//! Integration tests against a real ClickHouse server.
//!
//! Skipped unless NOMAD_TEST_URL is set, e.g.:
//!     docker compose up -d
//!     NOMAD_TEST_URL=http://localhost:8123 cargo test --test integration

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use clickhouse::Client;
use serde::Deserialize;

fn test_url() -> Option<String> {
    std::env::var("NOMAD_TEST_URL").ok().map(|u| u.trim_end_matches('/').to_string())
}

macro_rules! require_server {
    () => {
        match test_url() {
            Some(url) => url,
            None => {
                eprintln!("skipped: NOMAD_TEST_URL not set");
                return;
            }
        }
    };
}

struct TestEnv {
    database_url: String,
    db: String,
    source: PathBuf,
}

impl TestEnv {
    fn new(base_url: &str, name: &str) -> Self {
        let db = format!("nomad_test_{name}_{}", std::process::id());
        let source = std::env::temp_dir().join(format!("nomad_it_{db}"));
        let _ = fs::remove_dir_all(&source);
        fs::create_dir_all(&source).unwrap();
        Self { database_url: format!("{base_url}/{db}"), db, source }
    }

    fn write_migration(&self, version: u64, slug: &str, up: &str, down: &str) {
        fs::write(self.source.join(format!("{version}_{slug}.up.sql")), up).unwrap();
        fs::write(self.source.join(format!("{version}_{slug}.down.sql")), down).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_nomad"))
            .args(args)
            .arg("--database-url")
            .arg(&self.database_url)
            .env_remove("CLICKHOUSE_URL")
            .env_remove("DATABASE_URL")
            .output()
            .expect("failed to spawn nomad binary")
    }

    fn run_source(&self, args: &[&str]) -> Output {
        let source = self.source.to_str().unwrap().to_string();
        let mut full: Vec<&str> = args.to_vec();
        full.push("--source");
        full.push(&source);
        let out = Command::new(env!("CARGO_BIN_EXE_nomad"))
            .args(&full)
            .arg("--database-url")
            .arg(&self.database_url)
            .env_remove("CLICKHOUSE_URL")
            .env_remove("DATABASE_URL")
            .output()
            .expect("failed to spawn nomad binary");
        out
    }

    fn client(&self, base_url: &str) -> Client {
        // The clickhouse crate doesn't read userinfo out of the URL; split it off.
        let url = url::Url::parse(base_url).unwrap();
        let endpoint = format!(
            "{}://{}:{}",
            url.scheme(),
            url.host_str().unwrap(),
            url.port().unwrap_or(8123)
        );
        let mut client = Client::default().with_url(endpoint).with_database(&self.db);
        if !url.username().is_empty() {
            client = client.with_user(url.username());
        }
        if let Some(password) = url.password() {
            client = client.with_password(password);
        }
        client
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = self.run(&["database", "drop", "-y"]);
        let _ = fs::remove_dir_all(&self.source);
    }
}

fn assert_ok(out: &Output) {
    assert!(
        out.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[derive(Debug, clickhouse::Row, Deserialize)]
struct TrackingRow {
    version: i64,
    success: bool,
}

async fn tracking_rows(client: &Client) -> Vec<TrackingRow> {
    client
        .query("SELECT version, success FROM `_nomad_migrations` ORDER BY version")
        .fetch_all::<TrackingRow>()
        .await
        .unwrap()
}

#[tokio::test]
async fn full_lifecycle() {
    let base = require_server!();
    let env = TestEnv::new(&base, "lifecycle");

    env.write_migration(
        1,
        "create_events",
        "CREATE TABLE events (id UInt64, name String) ENGINE = MergeTree ORDER BY id;",
        "DROP TABLE events;",
    );
    env.write_migration(
        2,
        "add_index_table",
        "-- two statements in one migration\n\
         CREATE TABLE idx (id UInt64) ENGINE = MergeTree ORDER BY id;\n\
         INSERT INTO idx VALUES (1);",
        "DROP TABLE idx;",
    );

    assert_ok(&env.run(&["database", "create"]));
    assert_ok(&env.run_source(&["migrate", "run"]));

    let info = stdout(&env.run_source(&["migrate", "info"]));
    assert!(info.contains("1/installed create events"), "{info}");
    assert!(info.contains("2/installed add index table"), "{info}");

    // Idempotent second run.
    let out = env.run_source(&["migrate", "run"]);
    assert_ok(&out);
    assert!(stdout(&out).contains("no pending migrations"));

    let client = env.client(&base);
    let rows = tracking_rows(&client).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.success));

    // Revert the latest only.
    assert_ok(&env.run_source(&["migrate", "revert"]));
    let info = stdout(&env.run_source(&["migrate", "info"]));
    assert!(info.contains("2/pending"), "{info}");
    assert!(info.contains("1/installed"), "{info}");

    // Revert everything.
    assert_ok(&env.run_source(&["migrate", "revert", "--target-version", "0"]));
    assert!(tracking_rows(&client).await.is_empty());

    // And bring it all back with setup semantics (db already exists).
    assert_ok(&env.run_source(&["database", "setup"]));
    assert_eq!(tracking_rows(&client).await.len(), 2);
}

#[tokio::test]
async fn dirty_state_blocks_further_runs() {
    let base = require_server!();
    let env = TestEnv::new(&base, "dirty");

    env.write_migration(
        1,
        "bad_second_statement",
        "CREATE TABLE ok_table (id UInt64) ENGINE = MergeTree ORDER BY id;\n\
         CREATE TABLE broken (id UInt64) ENGINE = NoSuchEngine ORDER BY id;",
        "DROP TABLE IF EXISTS ok_table;",
    );

    assert_ok(&env.run(&["database", "create"]));
    let out = env.run_source(&["migrate", "run"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("statement 2 of 2"), "{err}");
    assert!(err.contains("dirty"), "{err}");

    let client = env.client(&base);
    let rows = tracking_rows(&client).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].success);

    // Subsequent run and revert must abort.
    for cmd in [["migrate", "run"], ["migrate", "revert"]] {
        let out = env.run_source(&cmd);
        assert!(!out.status.success());
        assert!(stderr(&out).contains("dirty state from migration 1"), "{}", stderr(&out));
    }

    let info = stdout(&env.run_source(&["migrate", "info"]));
    assert!(info.contains("(dirty!)"), "{info}");
}

#[tokio::test]
async fn checksum_mismatch_detected() {
    let base = require_server!();
    let env = TestEnv::new(&base, "checksum");

    env.write_migration(
        1,
        "create_t",
        "CREATE TABLE t (id UInt64) ENGINE = MergeTree ORDER BY id;",
        "DROP TABLE t;",
    );
    assert_ok(&env.run(&["database", "create"]));
    assert_ok(&env.run_source(&["migrate", "run"]));

    // Tamper with the applied up file.
    fs::write(
        env.source.join("1_create_t.up.sql"),
        "CREATE TABLE t (id UInt64, extra String) ENGINE = MergeTree ORDER BY id;",
    )
    .unwrap();

    let out = env.run_source(&["migrate", "run"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("has been modified"), "{}", stderr(&out));

    let info_out = env.run_source(&["migrate", "info"]);
    assert_ok(&info_out);
    assert!(stdout(&info_out).contains("(checksum mismatch!)"), "{}", stdout(&info_out));
}

#[tokio::test]
async fn literal_question_marks_survive() {
    let base = require_server!();
    let env = TestEnv::new(&base, "qmark");

    env.write_migration(
        1,
        "question_marks",
        "CREATE TABLE q (id UInt64, note String DEFAULT 'why?') ENGINE = MergeTree ORDER BY id;\n\
         INSERT INTO q (id, note) VALUES (1, 'really???');",
        "DROP TABLE q;",
    );
    assert_ok(&env.run(&["database", "create"]));
    assert_ok(&env.run_source(&["migrate", "run"]));

    let client = env.client(&base);
    let notes = client
        .query("SELECT note FROM q ORDER BY id")
        .fetch_all::<String>()
        .await
        .unwrap();
    assert_eq!(notes, vec!["really???".to_string()]);
}

#[tokio::test]
async fn target_version_and_dry_run() {
    let base = require_server!();
    let env = TestEnv::new(&base, "target");

    env.write_migration(1, "a", "CREATE TABLE a (x UInt8) ENGINE = Memory;", "DROP TABLE a;");
    env.write_migration(2, "b", "CREATE TABLE b (x UInt8) ENGINE = Memory;", "DROP TABLE b;");
    assert_ok(&env.run(&["database", "create"]));

    // Dry run executes nothing.
    let out = env.run_source(&["migrate", "run", "--dry-run"]);
    assert_ok(&out);
    assert!(stdout(&out).contains("would apply 1/a"), "{}", stdout(&out));
    let client = env.client(&base);
    assert!(tracking_rows(&client).await.is_empty());

    // Apply only up to version 1.
    assert_ok(&env.run_source(&["migrate", "run", "--target-version", "1"]));
    let rows = tracking_rows(&client).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].version, 1);

    // Unknown target errors.
    let out = env.run_source(&["migrate", "run", "--target-version", "99"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("does not match any local migration"), "{}", stderr(&out));

    assert_ok(&env.run_source(&["migrate", "run"]));
    assert_eq!(tracking_rows(&client).await.len(), 2);
}

//! Integration tests against a real ClickHouse server.
//!
//! Skipped unless NOMAD_TEST_URL is set, e.g.:
//!     docker compose up -d
//!     NOMAD_TEST_URL=http://localhost:8123 cargo test --test integration

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

use clickhouse::Client;
use serde::Deserialize;

fn test_url() -> Option<String> {
    std::env::var("NOMAD_TEST_URL")
        .ok()
        .map(|u| u.trim_end_matches('/').to_string())
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
    server_url: String,
    db: String,
    source: PathBuf,
    cleanup_cluster: Option<String>,
}

impl TestEnv {
    fn new(base_url: &str, name: &str) -> Self {
        let db = format!("nomad_test_{name}_{}", std::process::id());
        let source = std::env::temp_dir().join(format!("nomad_it_{db}"));
        let _ = fs::remove_dir_all(&source);
        fs::create_dir_all(&source).unwrap();
        Self {
            server_url: base_url.to_owned(),
            db,
            source,
            cleanup_cluster: None,
        }
    }

    fn write_migration(&self, version: u64, slug: &str, up: &str, down: &str) {
        fs::write(self.source.join(format!("{version}_{slug}.up.sql")), up).unwrap();
        fs::write(self.source.join(format!("{version}_{slug}.down.sql")), down).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_nomad"))
            .args(args)
            .arg("--url")
            .arg(&self.server_url)
            .arg("--database")
            .arg(&self.db)
            .env_remove("NOMAD_CLICKHOUSE_URL")
            .env_remove("DATABASE_URL")
            .env_remove("CLICKHOUSE_DATABASE")
            .env_remove("RUST_LOG")
            .output()
            .expect("failed to spawn nomad binary")
    }

    fn run_source(&self, args: &[&str]) -> Output {
        self.run_source_with_log(args, "")
    }

    fn run_source_with_log(&self, args: &[&str], log: &str) -> Output {
        let source = self.source.to_str().unwrap().to_string();
        let mut full: Vec<&str> = args.to_vec();
        full.push("--source");
        full.push(&source);
        Command::new(env!("CARGO_BIN_EXE_nomad"))
            .args(&full)
            .arg("--url")
            .arg(&self.server_url)
            .arg("--database")
            .arg(&self.db)
            .env_remove("NOMAD_CLICKHOUSE_URL")
            .env_remove("DATABASE_URL")
            .env_remove("CLICKHOUSE_DATABASE")
            .env("RUST_LOG", log)
            .output()
            .expect("failed to spawn nomad binary")
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
        if let Some(cluster) = &self.cleanup_cluster {
            let _ = self.run(&["database", "drop", "--cluster", cluster, "-y", "--force"]);
        } else {
            let _ = self.run(&["database", "drop", "-y", "--force"]);
        }
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
    assert_ok(&env.run(&["migrate", "init"]));
    let out = env.run_source(&["migrate", "run"]);
    assert_ok(&out);
    assert!(!stderr(&out).contains("executing statement"));
    assert_eq!(
        stderr(&out)
            .lines()
            .filter(|line| line.contains(" INFO "))
            .count(),
        2
    );
    assert!(!stderr(&out).contains("CREATE TABLE events"));
    assert!(stderr(&out).contains("recorded success in `_nomad_migrations`"));

    let info = stdout(&env.run_source(&["migrate", "info"]));
    assert!(info.contains("1/installed create events"), "{info}");
    assert!(info.contains("2/installed add index table"), "{info}");

    // Idempotent second run.
    let out = env.run_source(&["migrate", "run"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("no pending migrations"));

    let client = env.client(&base);
    let rows = tracking_rows(&client).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.success));

    // Revert the latest only.
    assert_ok(&env.run_source(&["migrate", "revert", "--force"]));
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
    assert_ok(&env.run(&["migrate", "init"]));
    let out = env.run_source(&["migrate", "run"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("statement 2 of 2"), "{err}");
    assert!(err.contains("dirty"), "{err}");
    assert_eq!(
        err.lines().filter(|line| line.contains(" INFO ")).count(),
        1
    );
    assert_eq!(err.lines().filter(|line| line.contains("ERROR")).count(), 1);
    assert!(err.contains("failed to apply 1/bad second statement"));

    let client = env.client(&base);
    let rows = tracking_rows(&client).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].success);

    // Subsequent run and revert must abort.
    for cmd in [["migrate", "run"], ["migrate", "revert"]] {
        let out = env.run_source(&cmd);
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("dirty state from migration 1"),
            "{}",
            stderr(&out)
        );
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
    assert_ok(&env.run(&["migrate", "init"]));
    assert_ok(&env.run_source(&["migrate", "run"]));

    // Tamper with the applied up file.
    fs::write(
        env.source.join("1_create_t.up.sql"),
        "CREATE TABLE t (id UInt64, extra String) ENGINE = MergeTree ORDER BY id;",
    )
    .unwrap();

    let out = env.run_source(&["migrate", "run"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("has been modified"),
        "{}",
        stderr(&out)
    );

    let info_out = env.run_source(&["migrate", "info"]);
    assert_ok(&info_out);
    assert!(
        stdout(&info_out).contains("(checksum mismatch!)"),
        "{}",
        stdout(&info_out)
    );
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
    assert_ok(&env.run(&["migrate", "init"]));
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

    env.write_migration(
        1,
        "a",
        "CREATE TABLE a (x UInt8) ENGINE = Memory;",
        "DROP TABLE a;",
    );
    env.write_migration(
        2,
        "b",
        "CREATE TABLE b (x UInt8) ENGINE = Memory;",
        "DROP TABLE b;",
    );
    assert_ok(&env.run(&["database", "create"]));
    assert_ok(&env.run(&["migrate", "init"]));

    // Dry run executes nothing.
    let out = env.run_source(&["migrate", "run", "--dry-run"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("would apply 1/a"), "{}", stderr(&out));
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
    assert!(
        stderr(&out).contains("does not match any local migration"),
        "{}",
        stderr(&out)
    );

    assert_ok(&env.run_source(&["migrate", "run"]));
    assert_eq!(tracking_rows(&client).await.len(), 2);
}

#[tokio::test]
async fn manifest_must_be_initialized_explicitly() {
    let base = require_server!();
    let env = TestEnv::new(&base, "init");
    env.write_migration(
        1,
        "events",
        "CREATE TABLE events (id UInt64) ENGINE = Memory;",
        "DROP TABLE events;",
    );
    assert_ok(&env.run(&["database", "create"]));
    let client = env.client(&base);

    for args in [
        vec!["migrate", "run"],
        vec!["migrate", "run", "--dry-run"],
        vec!["migrate", "revert"],
        vec!["migrate", "revert", "--dry-run"],
        vec!["migrate", "info"],
    ] {
        let out = env.run_source(&args);
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("nomad migrate init"),
            "{}",
            stderr(&out)
        );
        let count = client.query("SELECT count() FROM system.tables WHERE database = currentDatabase() AND name IN ('_nomad_migrations', 'events')")
            .fetch_one::<u64>().await.unwrap();
        assert_eq!(count, 0);
    }

    assert_ok(&env.run(&["migrate", "init"]));
    assert_ok(&env.run(&["migrate", "init"])); // Idempotent, before migrations.
    assert_ok(&env.run_source(&["migrate", "run"]));
    let out = env.run(&["migrate", "init"]);
    assert_ok(&out); // Does not clear history.
    assert!(stderr(&out).contains("already exists with engine MergeTree"));
    assert!(stderr(&out).contains("no DDL submitted"));
    assert!(!stderr(&out).contains("CREATE TABLE"));
    // A nonexistent cluster would fail if init submitted ON CLUSTER DDL.
    let out = env.run(&["migrate", "init", "--cluster=nomad_nonexistent_cluster"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("no DDL submitted"));
    assert_eq!(tracking_rows(&client).await.len(), 1);
    let out = env.run(&["database", "reset", "-y", "--force"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("no migrations applied"));
    let count = client
        .query("SELECT count() FROM system.tables WHERE database = currentDatabase()")
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert_eq!(count, 0);
    let out = env.run_source(&["database", "setup"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nomad migrate init"));
    assert_ok(&env.run(&["migrate", "init"]));
    assert_ok(&env.run_source(&["database", "setup"]));
    assert_eq!(tracking_rows(&client).await.len(), 1);
}

#[tokio::test]
async fn incompatible_manifest_engine_is_rejected() {
    let base = require_server!();
    let env = TestEnv::new(&base, "manifest_engine");
    assert_ok(&env.run(&["database", "create", "--engine=atomic"]));
    let client = env.client(&base);
    let out = env.run(&["migrate", "init", "--cluster=nomad_nonexistent_cluster"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nomad_nonexistent_cluster"));
    let count = client.query("SELECT count() FROM system.tables WHERE database = currentDatabase() AND name = '_nomad_migrations'").fetch_one::<u64>().await.unwrap();
    assert_eq!(count, 0); // Atomic databases honor the requested cluster.
    client
        .query("CREATE TABLE _nomad_migrations (id UInt64) ENGINE = Memory")
        .execute()
        .await
        .unwrap();
    client
        .query("INSERT INTO _nomad_migrations VALUES (1)")
        .execute()
        .await
        .unwrap();
    let out = env.run(&["migrate", "init"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("incompatible with inferred MergeTree"));
    let rows = client
        .query("SELECT count() FROM _nomad_migrations")
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert_eq!(rows, 1); // Incompatible tables and their data are preserved.
}

#[tokio::test]
async fn sql_is_only_logged_at_debug() {
    let base = require_server!();
    let env = TestEnv::new(&base, "logging");
    env.write_migration(
        1,
        "logging",
        "CREATE TABLE log_test (id UInt64) ENGINE = Memory;",
        "DROP TABLE log_test;",
    );
    let out = env.run(&["database", "create"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("INFO"));
    assert!(!stderr(&out).contains("CREATE DATABASE"));
    let out = env.run(&["migrate", "init"]);
    assert_ok(&out);
    assert!(!stderr(&out).contains("CREATE TABLE"));

    let out = env.run_source(&["migrate", "run", "--dry-run"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("would apply 1/logging"));
    assert_eq!(
        stderr(&out)
            .lines()
            .filter(|line| line.contains(" INFO "))
            .count(),
        1
    );
    assert!(!stderr(&out).contains("CREATE TABLE log_test"));
    assert!(stdout(&out).is_empty());
    for flag in ["-v", "-vv", "-vvv"] {
        let out = env.run_source(&["migrate", "run", "--dry-run", flag]);
        assert_ok(&out);
        assert!(stderr(&out).contains("CREATE TABLE log_test"));
        assert!(stderr(&out).contains("DEBUG"));
    }
    let out = env.run_source_with_log(&["migrate", "run", "--dry-run", "-v"], "nomad=error");
    assert_ok(&out);
    assert!(stderr(&out).contains("CREATE TABLE log_test")); // CLI verbosity overrides Nomad's env directive.
    let out = env.run_source_with_log(&["migrate", "run", "--dry-run"], "nomad=debug");
    assert_ok(&out);
    assert!(stderr(&out).contains("DEBUG"));
    assert!(stderr(&out).contains("CREATE TABLE log_test"));
    assert!(tracking_rows(&env.client(&base)).await.is_empty());

    let out = env.run_source_with_log(&["migrate", "run"], "nomad=debug");
    assert_ok(&out);
    assert!(stderr(&out).contains("CREATE TABLE log_test"));
    let out = env.run_source(&["migrate", "revert", "--dry-run"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("would revert 1/logging"));
    assert_eq!(
        stderr(&out)
            .lines()
            .filter(|line| line.contains(" INFO "))
            .count(),
        1
    );
    assert!(!stderr(&out).contains("DROP TABLE log_test"));
    let out = env.run_source_with_log(&["migrate", "revert", "--dry-run"], "debug");
    assert_ok(&out);
    assert!(stderr(&out).contains("DROP TABLE log_test"));
    let out = env.run_source(&["migrate", "revert"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("reverted 1/logging"));
    assert_eq!(
        stderr(&out)
            .lines()
            .filter(|line| line.contains(" INFO "))
            .count(),
        1
    );
    assert!(!stderr(&out).contains("DROP TABLE log_test"));
}

#[tokio::test]
async fn incompatible_database_engine_is_rejected_before_reset() {
    let base = require_server!();
    let env = TestEnv::new(&base, "db_engine");
    assert_ok(&env.run(&["database", "create"]));
    assert_ok(&env.run(&["migrate", "init"]));
    let out = env.run(&["database", "create"]);
    assert_ok(&out);
    assert!(stderr(&out).contains("no DDL submitted"));
    for args in [
        vec!["database", "create", "--engine=replicated"],
        vec!["database", "setup", "--engine=replicated"],
        vec!["database", "reset", "--engine=replicated", "-y"],
    ] {
        let out = if args[1] == "setup" {
            env.run_source(&args)
        } else {
            env.run(&args)
        };
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("incompatible with requested Replicated"),
            "{}",
            stderr(&out)
        );
    }
    assert!(tracking_rows(&env.client(&base)).await.is_empty()); // Reset did not remove the manifest.
}

#[tokio::test]
async fn replicated_database_on_cluster() {
    let base = require_server!();
    let Ok(cluster) = std::env::var("NOMAD_TEST_CLUSTER") else {
        eprintln!("skipped: NOMAD_TEST_CLUSTER not set");
        return;
    };
    let mut env = TestEnv::new(&base, "replicated_db");
    env.cleanup_cluster = Some(cluster.clone());
    let engine = || {
        env.client(&base)
            .query("SELECT engine FROM system.databases WHERE name = ?")
            .bind(&env.db)
            .fetch_one::<String>()
    };
    let result = async {
        assert_ok(&env.run(&[
            "database",
            "create",
            "--engine=replicated",
            "--cluster",
            &cluster,
        ]));
        assert_eq!(engine().await.unwrap(), "Replicated");
        let out = env.run(&["database", "reset", "-y", "--force"]);
        assert!(!out.status.success()); // No accidental downgrade to the default engine.
        assert_eq!(engine().await.unwrap(), "Replicated");
        let out = env.run(&["migrate", "init", "--cluster=nomad_nonexistent_cluster"]);
        assert_ok(&out);
        assert!(stderr(&out).contains("ignoring --cluster for Replicated database"));
        let manifest_engine = env.client(&base)
            .query("SELECT engine FROM system.tables WHERE database = currentDatabase() AND name = '_nomad_migrations'")
            .fetch_one::<String>().await.unwrap();
        assert_eq!(manifest_engine, "ReplicatedMergeTree");
        assert_ok(&env.run(&[
            "database",
            "reset",
            "--engine=replicated",
            "--cluster",
            &cluster,
            "-y",
        ]));
        assert_eq!(engine().await.unwrap(), "Replicated");
        let tables = env
            .client(&base)
            .query("SELECT count() FROM system.tables WHERE database = currentDatabase()")
            .fetch_one::<u64>()
            .await
            .unwrap();
        assert_eq!(tables, 0); // Reset never initializes the manifest.
        let ch = env.client(&base);
        ch.query("CREATE TABLE guard (id UInt64) ENGINE = ReplicatedMergeTree ORDER BY id")
            .execute()
            .await
            .unwrap();
        ch.query("INSERT INTO guard VALUES (1)")
            .execute()
            .await
            .unwrap();
        let out = env.run(&[
            "database",
            "reset",
            "--engine=replicated",
            "--cluster",
            &cluster,
            "-y",
        ]);
        assert!(!out.status.success());
        assert!(stderr(&out).contains("no DROP submitted"));
        assert_eq!(
            ch.query("SELECT count() FROM guard")
                .fetch_one::<u64>()
                .await
                .unwrap(),
            1
        );
        assert_ok(&env.run(&[
            "database",
            "reset",
            "--engine=replicated",
            "--cluster",
            &cluster,
            "-y",
            "--force",
        ]));
    };
    result.await;
    assert_ok(&env.run(&["database", "drop", "--cluster", &cluster, "-y", "--force"]));
}

#[tokio::test]
async fn destructive_commands_require_force_for_merge_tree_data() {
    let base = require_server!();
    let env = TestEnv::new(&base, "drop_guard");
    env.write_migration(1, "protected", "CREATE TABLE protected (id UInt64) ENGINE = MergeTree ORDER BY id; INSERT INTO protected VALUES (42);", "DROP TABLE protected;");
    assert_ok(&env.run(&["database", "create"]));
    assert_ok(&env.run(&["migrate", "init"]));
    assert_ok(&env.run_source(&["migrate", "run"]));
    let client = env.client(&base);
    let out = env.run_source(&["migrate", "revert", "--dry-run"]);
    assert_ok(&out);
    for args in [
        vec!["migrate", "revert"],
        vec!["database", "drop", "-y"],
        vec!["database", "reset", "-y"],
    ] {
        let out = if args[0] == "migrate" {
            env.run_source(&args)
        } else {
            env.run(&args)
        };
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("TABLE_SIZE_EXCEEDS_MAX_DROP_SIZE_LIMIT")
                || stderr(&out).contains("max_table_size_to_drop=1"),
            "{}",
            stderr(&out)
        );
        assert_eq!(
            client
                .query("SELECT count() FROM protected")
                .fetch_one::<u64>()
                .await
                .unwrap(),
            1
        );
        assert_eq!(tracking_rows(&client).await.len(), 1);
    }
    assert_ok(&env.run_source(&["migrate", "revert", "--force"]));
    assert!(tracking_rows(&client).await.is_empty());
    assert_ok(&env.run_source(&["migrate", "run"]));
    assert_ok(&env.run(&["database", "reset", "-y", "--force"]));
    assert_eq!(
        client
            .query("SELECT count() FROM system.tables WHERE database = currentDatabase()")
            .fetch_one::<u64>()
            .await
            .unwrap(),
        0
    );
    assert_ok(&env.run(&["migrate", "init"]));
    assert_ok(&env.run_source(&["migrate", "run"]));
    assert_ok(&env.run(&["database", "drop", "-y", "--force"]));
}

/// These CLI checks run without a ClickHouse server or a local .env file.
fn isolated_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nomad"));
    command
        .current_dir(std::env::temp_dir())
        .env_remove("NOMAD_CLICKHOUSE_URL")
        .env_remove("CLICKHOUSE_DATABASE")
        .env_remove("DATABASE_URL")
        .env_remove("RUST_LOG");
    command
}

#[test]
fn missing_connection_values_prompt_for_every_connection_command() {
    for (category, action) in [
        ("migrate", "init"),
        ("migrate", "run"),
        ("migrate", "revert"),
        ("migrate", "info"),
        ("database", "create"),
        ("database", "drop"),
        ("database", "setup"),
        ("database", "reset"),
    ] {
        for url_args in [vec!["--url", "http://localhost"], vec!["--ask"]] {
            let output = isolated_command()
                .args([category, action])
                .args(&url_args)
                .output()
                .unwrap();
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success());
            let prompt = if url_args.first() == Some(&"--ask") {
                "could not read server URL from masked prompt"
            } else {
                "could not read database name from prompt"
            };
            assert!(error.contains(prompt), "{error}");
        }
    }
    let output = isolated_command()
        .args(["migrate", "run", "--db", "explicit"])
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        error.contains("could not read server URL from masked prompt"),
        "{error}"
    );
    let output = isolated_command()
        .args(["migrate", "add", "--help"])
        .output()
        .unwrap();
    assert_ok(&output);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("--database"));
}

#[test]
fn url_flags_override_environment_and_url_paths_are_rejected() {
    for flag in ["--database", "--db"] {
        let output = isolated_command()
            .env("CLICKHOUSE_DATABASE", "")
            .env("NOMAD_CLICKHOUSE_URL", "invalid")
            .args([
                "migrate",
                "run",
                flag,
                "explicit",
                "--url",
                "http://localhost/mydb",
            ])
            .output()
            .unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(error.contains("must not contain a path"), "{error}");
        assert!(error.contains("--database/--db"), "{error}");
    }
    let output = isolated_command()
        .env("CLICKHOUSE_DATABASE", "from_env")
        .env("NOMAD_CLICKHOUSE_URL", "http://localhost/mydb")
        .args(["database", "create"])
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("must not contain a path"), "{error}");

    let output = isolated_command()
        .env("CLICKHOUSE_DATABASE", "from_env")
        .args(["database", "drop", "--db", "", "--url", "http://localhost"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--database must not be empty"));
}

#[test]
fn missing_source_prompts_before_any_database_operations() {
    for args in [
        vec!["migrate", "add", "events"],
        vec![
            "migrate",
            "run",
            "--url",
            "http://127.0.0.1:1",
            "--db",
            "test",
        ],
        vec![
            "migrate",
            "revert",
            "--url",
            "http://127.0.0.1:1",
            "--db",
            "test",
        ],
        vec![
            "migrate",
            "info",
            "--url",
            "http://127.0.0.1:1",
            "--db",
            "test",
        ],
        vec!["db", "setup", "--url", "http://127.0.0.1:1", "--db", "test"],
    ] {
        let output = isolated_command().args(args).output().unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            error.contains("could not read migration directory from prompt; pass --source"),
            "{error}"
        );
    }
}

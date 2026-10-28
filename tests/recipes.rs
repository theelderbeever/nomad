//! Recipe resolution through the CLI, without a ClickHouse server.
use std::{fs, process::Command};

fn nomad(directory: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nomad"));
    command.current_dir(directory);
    for key in [
        "NOMAD_CLICKHOUSE_URL",
        "CLICKHOUSE_URL",
        "CLICKHOUSE_DATABASE",
        "NOMAD_SOURCE",
        "NOMAD_REPLICATED",
        "NOMAD_CLUSTER",
        "NOMAD_ASK",
        "NOMAD_FORCE",
        "NOMAD_YES",
        "NOMAD_TARGET_VERSION",
        "NOMAD_DRY_RUN",
        "RUST_LOG",
    ] {
        command.env_remove(key);
    }
    command
}

#[test]
fn add_uses_recipe_or_cli_source_and_ignores_environment_source() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join(".nomad-recipes.toml"),
        "[app]\nsource = 'recipe'",
    )
    .unwrap();
    for (env, flag, expected) in [
        (None, None, "recipe"),
        (Some("environment"), None, "recipe"),
        (Some("environment"), Some("cli"), "cli"),
    ] {
        let mut command = nomad(directory.path());
        command.args([
            "migrate",
            "add",
            if env.is_some() {
                "with_environment"
            } else {
                "events"
            },
            "-r=app",
        ]);
        if let Some(source) = env {
            command.env("NOMAD_SOURCE", source);
        }
        if let Some(source) = flag {
            command.args(["--source", source]);
        }
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.contains("loaded recipe"), "{stderr}");
        assert_eq!(
            fs::read_dir(directory.path().join(expected))
                .unwrap()
                .count(),
            if expected == "recipe" && env.is_some() {
                4
            } else {
                2
            }
        );
    }
}

#[test]
fn cli_connection_values_override_environment_and_recipe() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join(".nomad-recipes.yaml"),
        "app:\n  url: invalid\n  database: ''\n  source: missing\n",
    )
    .unwrap();
    let output = nomad(directory.path())
        .args([
            "migrate",
            "run",
            "-r=app",
            "--url=http://localhost:1",
            "--db=cli",
        ])
        .env("NOMAD_CLICKHOUSE_URL", "invalid")
        .env("CLICKHOUSE_DATABASE", "")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    // Reaching the manifest check proves both overridden connection settings
    // passed their domain parsers. No ClickHouse server is required.
    assert!(
        stderr.contains("failed to check for the migration manifest table"),
        "{stderr}"
    );
    assert!(!stderr.contains("invalid server URL"), "{stderr}");
    assert!(!stderr.contains("--database must not be empty"), "{stderr}");
}

fn assert_prompt(output: std::process::Output, prompt: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains(prompt), "{stderr}");
}

#[test]
fn dotenv_files_and_legacy_environment_variables_are_ignored() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join(".env"),
        "NOMAD_CLICKHOUSE_URL=invalid\nCLICKHOUSE_URL=invalid\nCLICKHOUSE_DATABASE=from_dotenv\n",
    )
    .unwrap();
    assert_prompt(
        nomad(directory.path())
            .args(["database", "create", "--db=explicit"])
            .env("CLICKHOUSE_URL", "invalid")
            .env("CLICKHOUSE_DATABASE", "legacy")
            .env("NOMAD_URL", "invalid")
            .output()
            .unwrap(),
        "could not read server URL from masked prompt",
    );
    assert_prompt(
        nomad(directory.path())
            .args(["database", "create", "--url=http://localhost"])
            .env("CLICKHOUSE_DATABASE", "legacy")
            .env("NOMAD_CLICKHOUSE_DATABASE", "unsupported")
            .output()
            .unwrap(),
        "could not read database name from prompt",
    );
}

#[test]
fn only_nomad_clickhouse_url_overlays_recipe_and_cli_can_override_it() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join(".nomad-recipes.toml"),
        "[app]\nurl = 'invalid'",
    )
    .unwrap();
    assert_prompt(
        nomad(directory.path())
            .args(["database", "create", "-r=app"])
            .env("NOMAD_CLICKHOUSE_URL", "http://localhost")
            .output()
            .unwrap(),
        "could not read database name from prompt",
    );
    assert_prompt(
        nomad(directory.path())
            .args(["database", "create", "-r=app", "--url=http://localhost"])
            .env("NOMAD_CLICKHOUSE_URL", "invalid")
            .output()
            .unwrap(),
        "could not read database name from prompt",
    );
    assert_prompt(
        nomad(directory.path())
            .args(["database", "create", "-r=app", "--ask"])
            .env("NOMAD_CLICKHOUSE_URL", "invalid")
            .output()
            .unwrap(),
        "could not read server URL from masked prompt",
    );
}

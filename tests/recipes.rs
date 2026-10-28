//! Recipe resolution through the CLI, without a ClickHouse server.
use std::{fs, process::Command};

fn nomad(directory: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nomad"));
    command.current_dir(directory);
    for key in [
        "NOMAD_CLICKHOUSE_URL",
        "NOMAD_CLICKHOUSE_HOST",
        "NOMAD_CLICKHOUSE_PORT",
        "NOMAD_CLICKHOUSE_USER",
        "NOMAD_CLICKHOUSE_PASSWORD",
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
            .args(["database", "create", "-r=app", "--prompt"])
            .env("NOMAD_CLICKHOUSE_URL", "invalid")
            .output()
            .unwrap(),
        "could not read server URL from masked prompt",
    );
}

#[test]
fn component_environment_and_cli_overrides_reach_the_client() {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join(".nomad-recipes.toml"), "[app]\nurl = 'http://url-user:url-pass@url.invalid:1'\nhost = 'recipe.invalid'\nport = 1\nuser = 'recipe-user'\npassword = 'recipe-pass'\ndatabase = 'app'").unwrap();
    for explicit in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("no connection to test server: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\nstub").unwrap();
            headers.to_ascii_lowercase()
        });
        let mut command = nomad(directory.path());
        command
            .args([
                "database",
                "create",
                "-r=app",
                "--url=http://url-user:url-pass@url.invalid:1",
            ])
            .env("NOMAD_CLICKHOUSE_HOST", "127.0.0.1")
            .env("NOMAD_CLICKHOUSE_PORT", port.to_string())
            .env("NOMAD_CLICKHOUSE_USER", "env-user")
            .env("NOMAD_CLICKHOUSE_PASSWORD", "env-pass");
        if explicit {
            command.args(["--user=cli-user", "--password=p@ss:/%"]);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let headers = server.join().unwrap();
        let (user, password) = if explicit {
            ("cli-user", "p@ss:/%")
        } else {
            ("env-user", "env-pass")
        };
        assert!(
            headers.contains(&format!("x-clickhouse-user: {user}\r\n")),
            "{headers}"
        );
        assert!(
            headers.contains(&format!("x-clickhouse-key: {password}\r\n")),
            "{headers}"
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains(password));
    }
}

#[test]
fn prompted_components_skip_invalid_environment_values() {
    let directory = tempfile::tempdir().unwrap();
    assert_prompt(
        nomad(directory.path())
            .args(["database", "create", "--prompt=host"])
            .env("NOMAD_CLICKHOUSE_HOST", "invalid:host")
            .output()
            .unwrap(),
        "could not read host from prompt",
    );
    assert_prompt(
        nomad(directory.path())
            .args([
                "database",
                "create",
                "--url=http://localhost",
                "--prompt=port",
            ])
            .env("NOMAD_CLICKHOUSE_PORT", "invalid")
            .output()
            .unwrap(),
        "could not read port from prompt",
    );
}

mod cli;
mod client;
mod config;
mod db;
mod migrate;
mod prompt;
mod recipe;

use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::Parser;
use inquire::{Password, PasswordDisplayMode, Text};

use cli::{Cli, Command, ConnectionArgs, DatabaseCmd, MigrateCmd};
use config::{ConnectOptions, PromptField, ServerUrl};

fn connect_options(connection: ConnectionArgs) -> Result<ConnectOptions> {
    connect_options_with(connection, |field| {
        let label = match field {
            PromptField::Url => "Server URL:",
            PromptField::Host => "Server host:",
            PromptField::Port => "Server port:",
            PromptField::User => "User:",
            PromptField::Password => "Password:",
            PromptField::Database => "Database name:",
        };
        let value = if matches!(field, PromptField::Url | PromptField::Password) {
            Password::new(label)
                .with_display_mode(if field == PromptField::Url {
                    PasswordDisplayMode::Full
                } else {
                    PasswordDisplayMode::Masked
                })
                .without_confirmation()
                .prompt()
        } else {
            Text::new(label).prompt()
        };
        value.with_context(|| match field {
            PromptField::Url => "could not read server URL from masked prompt; pass --url/--host or set NOMAD_CLICKHOUSE_URL".into(),
            PromptField::Database => "could not read database name from prompt; pass --database/--db or select a recipe".into(),
            _ => format!("could not read {} from prompt; pass --{} or select a recipe", field.flag(), field.flag()),
        })
    })
}

fn connect_options_with(
    connection: ConnectionArgs,
    mut prompt: impl FnMut(PromptField) -> Result<String>,
) -> Result<ConnectOptions> {
    let selected = |field| connection.prompt.contains(&field);
    let server = match connection.url.filter(|_| !selected(PromptField::Url)) {
        Some(url) => Some(url),
        None if selected(PromptField::Url)
            || (connection.host.is_none() && !selected(PromptField::Host)) =>
        {
            Some(prompt(PromptField::Url)?.parse()?)
        }
        None => None,
    };
    let host = if selected(PromptField::Host) {
        Some(prompt(PromptField::Host)?.parse()?)
    } else {
        connection.host
    };
    let server = match server {
        Some(server) => server,
        None => ServerUrl::from_host(host.as_ref().expect("a supplied or prompted host"))?,
    };
    let port = if selected(PromptField::Port) {
        Some(prompt(PromptField::Port)?.parse()?)
    } else {
        connection.port
    };
    let user = if selected(PromptField::User) {
        Some(prompt(PromptField::User)?)
    } else {
        connection.user
    };
    let password = if selected(PromptField::Password) {
        Some(prompt(PromptField::Password)?)
    } else {
        connection.password
    };
    let server = server.with_overrides(host, port, user, password);
    let database = match connection
        .database
        .filter(|_| !selected(PromptField::Database))
    {
        Some(database) => database,
        None => prompt(PromptField::Database)?.parse()?,
    };
    Ok(ConnectOptions::new(server, database))
}

fn with_verbosity(
    filter: tracing_subscriber::EnvFilter,
    verbose: u8,
) -> tracing_subscriber::EnvFilter {
    let directive = match verbose {
        0 => return filter,
        1 => "nomad=debug",
        _ => "nomad=trace",
    };
    filter.add_directive(
        directive
            .parse()
            .expect("valid built-in Nomad log directive"),
    )
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(with_verbosity(filter, cli.verbose))
        .with_writer(std::io::stderr)
        .with_ansi(
            std::io::stderr().is_terminal()
                && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()),
        )
        .with_target(false)
        .init();

    if let Err(err) = run().await {
        tracing::error!("{err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = recipe::resolve(
        std::env::args_os(),
        &std::env::current_dir().context("could not access current directory")?,
        recipe::environment(),
    )?;
    match cli.command {
        Command::Migrate(cmd) => match cmd {
            MigrateCmd::Add {
                description,
                source,
            } => {
                let source = prompt::source_directory(source.source)?;
                let (up, down) = migrate::source::add_migration(source.as_path(), &description)?;
                tracing::info!("created {}", up.display());
                tracing::info!("created {}", down.display());
                Ok(())
            }
            MigrateCmd::Init { url, cluster } => {
                let opts = connect_options(url)?;
                let ch = client::connect(&opts);
                migrate::manifest::init(&ch, cluster.as_ref()).await
            }
            MigrateCmd::Run {
                source,
                url,
                target_version,
                dry_run,
            } => {
                let opts = connect_options(url)?;
                let source = prompt::source_directory(source.source)?;
                let ch = client::connect(&opts);
                migrate::runner::run(&ch, source.as_path(), target_version, dry_run).await
            }
            MigrateCmd::Revert {
                source,
                url,
                force,
                target_version,
                dry_run,
            } => {
                let opts = connect_options(url)?;
                let source = prompt::source_directory(source.source)?;
                let ch = force.policy().client(client::connect(&opts));
                migrate::runner::revert(&ch, source.as_path(), target_version, dry_run).await
            }
            MigrateCmd::Info { source, url } => {
                let opts = connect_options(url)?;
                let source = prompt::source_directory(source.source)?;
                let ch = client::connect(&opts);
                migrate::runner::info(&ch, source.as_path()).await
            }
        },
        Command::Database(cmd) => match cmd {
            DatabaseCmd::Create { url, definition } => {
                db::create(&connect_options(url)?, &definition.definition()).await
            }
            DatabaseCmd::Info { url, cluster } => {
                db::info(&connect_options(url)?, cluster.as_ref()).await
            }
            DatabaseCmd::Drop {
                url,
                cluster,
                yes,
                force,
            } => {
                db::drop(
                    &connect_options(url)?,
                    cluster.as_ref(),
                    yes,
                    force.policy(),
                )
                .await
            }
            DatabaseCmd::Setup {
                source,
                url,
                definition,
            } => {
                let opts = connect_options(url)?;
                let source = prompt::source_directory(source.source)?;
                db::create(&opts, &definition.definition()).await?;
                let ch = client::connect(&opts);
                migrate::runner::run(&ch, source.as_path(), None, false).await
            }
            DatabaseCmd::Reset {
                url,
                definition,
                yes,
                force,
            } => {
                db::reset(
                    &connect_options(url)?,
                    &definition.definition(),
                    yes,
                    force.policy(),
                )
                .await
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> ConnectionArgs {
        ConnectionArgs {
            url: Some("https://old:secret@localhost:8443".parse().unwrap()),
            database: Some("app".parse().unwrap()),
            ..Default::default()
        }
    }

    #[test]
    fn components_override_url_without_encoding_credentials_into_endpoint() {
        let opts = connect_options_with(
            ConnectionArgs {
                host: Some("[::1]".parse().unwrap()),
                port: Some("9999".parse().unwrap()),
                user: Some("user@corp".into()),
                password: Some("p@ss:/%".into()),
                ..connection()
            },
            |_| panic!("unexpected prompt"),
        )
        .unwrap();
        assert_eq!(opts.endpoint.as_str(), "https://[::1]:9999/");
        assert_eq!(opts.user.as_deref(), Some("user@corp"));
        assert_eq!(opts.password.as_deref(), Some("p@ss:/%"));
    }

    #[test]
    fn host_can_supply_an_endpoint_without_a_url_prompt() {
        let opts = connect_options_with(
            ConnectionArgs {
                url: None,
                host: Some("localhost".parse().unwrap()),
                ..connection()
            },
            |_| panic!("unexpected prompt"),
        )
        .unwrap();
        assert_eq!(opts.endpoint.as_str(), "http://localhost:8123/");
    }

    #[test]
    fn selected_prompt_fields_override_existing_values() {
        let mut fields = vec![PromptField::User, PromptField::Password].into_iter();
        let opts = connect_options_with(
            ConnectionArgs {
                prompt: vec![PromptField::User, PromptField::Password],
                ..connection()
            },
            |field| {
                assert_eq!(Some(field), fields.next());
                Ok(if field == PromptField::User {
                    "prompted"
                } else {
                    ""
                }
                .into())
            },
        )
        .unwrap();
        assert_eq!(opts.user.as_deref(), Some("prompted"));
        assert_eq!(opts.password.as_deref(), Some(""));
        assert!(fields.next().is_none());
        assert_eq!(opts.endpoint.as_str(), "https://localhost:8443/");
    }

    #[test]
    fn all_connection_fields_can_be_prompted() {
        let opts = connect_options_with(
            ConnectionArgs {
                prompt: vec![
                    PromptField::Url,
                    PromptField::Host,
                    PromptField::Port,
                    PromptField::User,
                    PromptField::Password,
                    PromptField::Database,
                ],
                ..connection()
            },
            |field| {
                Ok(match field {
                    PromptField::Url => "http://url-user:url-password@url-host",
                    PromptField::Host => "other-host",
                    PromptField::Port => "9000",
                    PromptField::User => "new-user",
                    PromptField::Password => "new-password",
                    PromptField::Database => "new-database",
                }
                .into())
            },
        )
        .unwrap();
        assert_eq!(opts.endpoint.as_str(), "http://other-host:9000/");
        assert_eq!(opts.user.as_deref(), Some("new-user"));
        assert_eq!(opts.password.as_deref(), Some("new-password"));
        assert_eq!(opts.database.as_str(), "new-database");
    }

    #[test]
    fn missing_url_and_database_prompt_automatically() {
        let mut fields = vec![PromptField::Url, PromptField::Database].into_iter();
        let opts = connect_options_with(ConnectionArgs::default(), |field| {
            assert_eq!(Some(field), fields.next());
            Ok(if field == PromptField::Url {
                "http://localhost"
            } else {
                "chosen"
            }
            .into())
        })
        .unwrap();
        assert_eq!(opts.database.as_str(), "chosen");
        assert_eq!(opts.endpoint.as_str(), "http://localhost:8123/");
    }

    #[test]
    fn prompt_errors_and_invalid_values_never_fall_back() {
        for (field, input) in [
            (PromptField::Url, "invalid"),
            (PromptField::Host, ""),
            (PromptField::Host, "localhost:8123"),
            (PromptField::Port, "0"),
            (PromptField::Port, "65536"),
            (PromptField::Database, " "),
        ] {
            assert!(
                connect_options_with(
                    ConnectionArgs {
                        prompt: vec![field],
                        ..connection()
                    },
                    |_| Ok(input.into())
                )
                .is_err()
            );
        }
        let error = connect_options_with(
            ConnectionArgs {
                prompt: vec![PromptField::Password],
                ..connection()
            },
            |_| anyhow::bail!("prompt canceled"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "prompt canceled");
    }

    #[test]
    fn recipe_environment_cli_and_prompts_resolve_into_one_connection() {
        use figment::providers::Serialized;
        use std::collections::BTreeMap;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(".nomad-recipes.toml"), "[app]\nurl = 'https://url-user:url-pass@url-host:9000'\nhost = 'recipe-host'\nport = 9001\nuser = 'recipe-user'\npassword = 'recipe-pass'\ndatabase = 'app'").unwrap();
        let env: BTreeMap<_, _> = [
            ("host", "env-host"),
            ("port", "9002"),
            ("user", "env-user"),
            ("password", "env-pass"),
        ]
        .into_iter()
        .collect();
        let cli = recipe::resolve(
            [
                "nomad",
                "migrate",
                "run",
                "-r=app",
                "--host=cli-host",
                "--port=9003",
                "--user=cli-user",
                "--prompt=password",
            ]
            .into_iter()
            .map(Into::into),
            directory.path(),
            Serialized::defaults(env),
        )
        .unwrap();
        let Command::Migrate(MigrateCmd::Run { url, .. }) = cli.command else {
            unreachable!()
        };
        let opts = connect_options_with(url, |field| {
            assert_eq!(field, PromptField::Password);
            Ok("prompted-password".into())
        })
        .unwrap();
        assert_eq!(opts.endpoint.as_str(), "https://cli-host:9003/");
        assert_eq!(opts.user.as_deref(), Some("cli-user"));
        assert_eq!(opts.password.as_deref(), Some("prompted-password"));
        assert_eq!(opts.database.as_str(), "app");
    }

    #[test]
    fn verbosity_only_increases_nomad_logging() {
        for verbose in 0..=3 {
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(with_verbosity(
                    tracing_subscriber::EnvFilter::new("info"),
                    verbose,
                ))
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                assert_eq!(
                    tracing::enabled!(target: "nomad::client", tracing::Level::DEBUG),
                    verbose > 0
                );
                assert_eq!(
                    tracing::enabled!(target: "nomad::client", tracing::Level::TRACE),
                    verbose > 1
                );
                assert!(!tracing::enabled!(target: "hyper", tracing::Level::DEBUG));
                assert!(tracing::enabled!(target: "hyper", tracing::Level::INFO));
            });
        }
    }
}

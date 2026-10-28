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
use config::ConnectOptions;

fn connect_options(connection: ConnectionArgs) -> Result<ConnectOptions> {
    connect_options_with(
        connection,
        || {
            Password::new("Server URL:")
                .with_display_mode(PasswordDisplayMode::Masked)
                .without_confirmation()
                .prompt()
                .context("could not read server URL from masked prompt; pass --url or set NOMAD_CLICKHOUSE_URL")
        },
        || {
            Text::new("Database name:").prompt().context(
                "could not read database name from prompt; pass --database/--db or select a recipe",
            )
        },
    )
}

fn connect_options_with(
    connection: ConnectionArgs,
    prompt_url: impl FnOnce() -> Result<String>,
    prompt_database: impl FnOnce() -> Result<String>,
) -> Result<ConnectOptions> {
    let server = match connection.url.filter(|_| !connection.ask) {
        Some(url) => url,
        None => prompt_url()?.parse()?,
    };
    let database = match connection.database {
        Some(database) => database,
        None => prompt_database()?.parse()?,
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
            MigrateCmd::Init { url, manifest } => {
                let manifest = manifest.table();
                let opts = connect_options(url)?;
                let ch = client::connect(&opts);
                migrate::manifest::init(&ch, &manifest).await
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

    #[test]
    fn ask_uses_prompted_url_and_parses_it() {
        let opts = connect_options_with(
            ConnectionArgs {
                url: None,
                database: Some("prompted".parse().unwrap()),
                ask: true,
            },
            || Ok("http://user:secret@localhost".into()),
            || panic!("unexpected database prompt"),
        )
        .unwrap();
        assert_eq!(opts.database.as_str(), "prompted");
        assert_eq!(opts.password.as_deref(), Some("secret"));
    }

    #[test]
    fn explicit_url_does_not_prompt() {
        let opts = connect_options_with(
            ConnectionArgs {
                url: Some("http://localhost".parse().unwrap()),
                database: Some("explicit".parse().unwrap()),
                ask: false,
            },
            || panic!("unexpected prompt"),
            || panic!("unexpected database prompt"),
        )
        .unwrap();
        assert_eq!(opts.database.as_str(), "explicit");
    }

    #[test]
    fn ask_errors_do_not_fall_back() {
        for input in ["", "invalid"] {
            assert!(
                connect_options_with(
                    ConnectionArgs {
                        url: None,
                        database: Some("prompted".parse().unwrap()),
                        ask: true
                    },
                    || Ok(input.into()),
                    || panic!("unexpected database prompt"),
                )
                .is_err()
            );
        }
        let error = connect_options_with(
            ConnectionArgs {
                url: None,
                database: Some("prompted".parse().unwrap()),
                ask: true,
            },
            || anyhow::bail!("prompt canceled"),
            || panic!("unexpected database prompt"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "prompt canceled");
    }

    #[test]
    fn missing_values_prompt_automatically() {
        for ask in [false, true] {
            let opts = connect_options_with(
                ConnectionArgs {
                    url: None,
                    database: None,
                    ask,
                },
                || Ok("https://localhost".into()),
                || Ok("chosen".into()),
            )
            .unwrap();
            assert_eq!(opts.endpoint.as_str(), "https://localhost:8443/");
            assert_eq!(opts.database.as_str(), "chosen");
        }
    }

    #[test]
    fn only_missing_database_prompts_and_invalid_input_fails() {
        for name in ["chosen", "", "  ", "db\0name"] {
            let result = connect_options_with(
                ConnectionArgs {
                    url: Some("http://localhost".parse().unwrap()),
                    database: None,
                    ask: false,
                },
                || panic!("unexpected URL prompt"),
                || Ok(name.into()),
            );
            if name == "chosen" {
                assert_eq!(result.unwrap().database.as_str(), name);
            } else {
                assert!(result.is_err());
            }
        }
        let error = connect_options_with(
            ConnectionArgs {
                url: Some("http://localhost".parse().unwrap()),
                database: None,
                ask: false,
            },
            || panic!("unexpected URL prompt"),
            || anyhow::bail!("database prompt canceled"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "database prompt canceled");
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

mod cli;
mod client;
mod config;
mod db;
mod migrate;

use std::{io::IsTerminal, path::Path};

use anyhow::{Context, Result};
use clap::Parser;
use inquire::{Password, PasswordDisplayMode};

use cli::{Cli, Command, DatabaseCmd, DatabaseUrlArg, MigrateCmd};
use config::ConnectOptions;

fn connect_options(url: DatabaseUrlArg) -> Result<ConnectOptions> {
    connect_options_with(url, || {
        Password::new("Database URL:")
            .with_display_mode(PasswordDisplayMode::Hidden)
            .without_confirmation()
            .prompt()
            .context("could not read database URL from hidden prompt")
    })
}

fn connect_options_with(
    url: DatabaseUrlArg,
    prompt: impl FnOnce() -> Result<String>,
) -> Result<ConnectOptions> {
    let raw = if url.ask {
        prompt()?
    } else {
        config::resolve_url(url.database_url)?
    };
    config::parse_url(&raw)
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
    // Best-effort .env loading, like sqlx-cli.
    let _ = dotenvy::dotenv();

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

    if let Err(err) = run(cli).await {
        tracing::error!("{err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Migrate(cmd) => match cmd {
            MigrateCmd::Add {
                description,
                source,
            } => {
                let (up, down) =
                    migrate::source::add_migration(Path::new(&source.source), &description)?;
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
                let ch = client::connect(&opts);
                migrate::runner::run(&ch, Path::new(&source.source), target_version, dry_run).await
            }
            MigrateCmd::Revert {
                source,
                url,
                force,
                target_version,
                dry_run,
            } => {
                let opts = connect_options(url)?;
                let ch = force.policy().client(client::connect(&opts));
                migrate::runner::revert(&ch, Path::new(&source.source), target_version, dry_run)
                    .await
            }
            MigrateCmd::Info { source, url } => {
                let opts = connect_options(url)?;
                let ch = client::connect(&opts);
                migrate::runner::info(&ch, Path::new(&source.source)).await
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
                db::create(&opts, &definition.definition()).await?;
                let ch = client::connect(&opts);
                migrate::runner::run(&ch, Path::new(&source.source), None, false).await
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
            DatabaseUrlArg {
                database_url: None,
                ask: true,
            },
            || Ok("http://user:secret@localhost/prompted".into()),
        )
        .unwrap();
        assert_eq!(opts.database, "prompted");
        assert_eq!(opts.password.as_deref(), Some("secret"));
    }

    #[test]
    fn explicit_url_does_not_prompt() {
        let opts = connect_options_with(
            DatabaseUrlArg {
                database_url: Some("http://localhost/explicit".into()),
                ask: false,
            },
            || panic!("unexpected prompt"),
        )
        .unwrap();
        assert_eq!(opts.database, "explicit");
    }

    #[test]
    fn ask_errors_do_not_fall_back() {
        for input in ["", "invalid"] {
            assert!(
                connect_options_with(
                    DatabaseUrlArg {
                        database_url: None,
                        ask: true
                    },
                    || Ok(input.into()),
                )
                .is_err()
            );
        }
        let error = connect_options_with(
            DatabaseUrlArg {
                database_url: None,
                ask: true,
            },
            || anyhow::bail!("prompt canceled"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "prompt canceled");
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

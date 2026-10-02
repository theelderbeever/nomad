mod cli;
mod client;
mod config;
mod db;
mod migrate;

use std::{io::IsTerminal, path::Path};

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command, DatabaseCmd, DatabaseUrlArg, MigrateCmd};
use config::ConnectOptions;

fn connect_options(url: DatabaseUrlArg) -> Result<ConnectOptions> {
    config::parse_url(&config::resolve_url(url.database_url)?)
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

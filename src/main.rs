mod cli;
mod client;
mod config;
mod db;
mod migrate;

use std::path::Path;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command, DatabaseCmd, DatabaseUrlArg, MigrateCmd};
use config::ConnectOptions;

fn connect_options(url: DatabaseUrlArg) -> Result<ConnectOptions> {
    config::parse_url(&config::resolve_url(url.database_url)?)
}

#[tokio::main]
async fn main() {
    // Best-effort .env loading, like sqlx-cli.
    let _ = dotenvy::dotenv();

    if let Err(err) = run(Cli::parse()).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Migrate(cmd) => match cmd {
            MigrateCmd::Add { description, source } => {
                let (up, down) = migrate::source::add_migration(Path::new(&source.source), &description)?;
                println!("created {}", up.display());
                println!("created {}", down.display());
                Ok(())
            }
            MigrateCmd::Run { source, url, target_version, dry_run } => {
                let opts = connect_options(url)?;
                let ch = client::connect(&opts);
                migrate::runner::run(&ch, Path::new(&source.source), target_version, dry_run).await
            }
            MigrateCmd::Revert { source, url, target_version, dry_run } => {
                let opts = connect_options(url)?;
                let ch = client::connect(&opts);
                migrate::runner::revert(&ch, Path::new(&source.source), target_version, dry_run).await
            }
            MigrateCmd::Info { source, url } => {
                let opts = connect_options(url)?;
                let ch = client::connect(&opts);
                migrate::runner::info(&ch, Path::new(&source.source)).await
            }
        },
        Command::Database(cmd) => match cmd {
            DatabaseCmd::Create { url } => db::create(&connect_options(url)?).await,
            DatabaseCmd::Drop { url, yes } => db::drop(&connect_options(url)?, yes).await,
            DatabaseCmd::Setup { source, url } => {
                let opts = connect_options(url)?;
                db::create(&opts).await?;
                let ch = client::connect(&opts);
                migrate::runner::run(&ch, Path::new(&source.source), None, false).await
            }
            DatabaseCmd::Reset { source, url, yes } => {
                let opts = connect_options(url)?;
                db::drop(&opts, yes).await?;
                db::create(&opts).await?;
                let ch = client::connect(&opts);
                migrate::runner::run(&ch, Path::new(&source.source), None, false).await
            }
        },
    }
}

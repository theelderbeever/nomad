use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nomad", version, about = "A thin sqlx-cli style migration tool for ClickHouse")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create and run migrations
    #[command(subcommand)]
    Migrate(MigrateCmd),
    /// Create and drop the database
    #[command(subcommand)]
    Database(DatabaseCmd),
}

#[derive(Args)]
pub struct SourceArg {
    /// Path to the directory containing migration files
    #[arg(long, default_value = "migrations")]
    pub source: String,
}

#[derive(Args)]
pub struct DatabaseUrlArg {
    /// ClickHouse URL, e.g. http://user:pass@localhost:8123/mydb
    /// (falls back to CLICKHOUSE_URL, then DATABASE_URL)
    #[arg(long)]
    pub database_url: Option<String>,
}

#[derive(Subcommand)]
pub enum MigrateCmd {
    /// Create a new pair of <version>_<description>.{up,down}.sql migration files
    Add {
        /// Short description of the migration; becomes part of the filename
        description: String,
        #[command(flatten)]
        source: SourceArg,
    },
    /// Apply pending migrations
    Run {
        #[command(flatten)]
        source: SourceArg,
        #[command(flatten)]
        url: DatabaseUrlArg,
        /// Apply pending migrations up to and including this version
        #[arg(long)]
        target_version: Option<i64>,
        /// Print what would be executed without running anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Revert applied migrations (default: only the latest)
    Revert {
        #[command(flatten)]
        source: SourceArg,
        #[command(flatten)]
        url: DatabaseUrlArg,
        /// Revert all applied migrations with a version greater than this (0 = revert everything)
        #[arg(long)]
        target_version: Option<i64>,
        /// Print what would be executed without running anything
        #[arg(long)]
        dry_run: bool,
    },
    /// List applied and pending migrations
    Info {
        #[command(flatten)]
        source: SourceArg,
        #[command(flatten)]
        url: DatabaseUrlArg,
    },
}

#[derive(Subcommand)]
pub enum DatabaseCmd {
    /// Create the database named in the connection URL
    Create {
        #[command(flatten)]
        url: DatabaseUrlArg,
    },
    /// Drop the database named in the connection URL
    Drop {
        #[command(flatten)]
        url: DatabaseUrlArg,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// Create the database and run all pending migrations
    Setup {
        #[command(flatten)]
        source: SourceArg,
        #[command(flatten)]
        url: DatabaseUrlArg,
    },
    /// Drop the database, then re-create it and run all pending migrations
    Reset {
        #[command(flatten)]
        source: SourceArg,
        #[command(flatten)]
        url: DatabaseUrlArg,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

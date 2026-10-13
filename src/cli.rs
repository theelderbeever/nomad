use clap::{Args, Parser, Subcommand};

use crate::{
    client::DropPolicy,
    config::ClusterName,
    db::{DatabaseDefinition, DatabaseEngine},
    migrate::manifest::ManifestTable,
};

#[derive(Parser)]
#[command(
    name = "nomad",
    version,
    about = "A thin sqlx-cli style migration tool for ClickHouse"
)]
pub struct Cli {
    /// Increase Nomad verbosity: -v = debug, -vv/-vvv = trace (dependencies unchanged)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
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
    /// Prompt for the database URL with input masked by asterisks (ignores environment URLs)
    #[arg(long, conflicts_with = "database_url")]
    pub ask: bool,
}

#[derive(Args)]
pub struct ManifestArgs {
    /// Use ReplicatedMergeTree for the migration manifest table
    #[arg(long)]
    pub replicated: bool,
    /// Create the migration manifest table ON CLUSTER with this cluster name
    #[arg(long)]
    pub cluster: Option<ClusterName>,
}

impl ManifestArgs {
    pub fn table(&self) -> ManifestTable {
        ManifestTable {
            replicated: self.replicated,
            cluster: self.cluster.clone(),
        }
    }
}

#[derive(Args)]
pub struct ForceArg {
    /// Disable the 1-byte MergeTree drop-size limit (max_table_size_to_drop=0)
    #[arg(long)]
    pub force: bool,
}

impl ForceArg {
    pub fn policy(self) -> DropPolicy {
        if self.force {
            DropPolicy::Forced
        } else {
            DropPolicy::Protected
        }
    }
}

#[derive(Args)]
pub struct DatabaseCreateArgs {
    /// Use the Replicated database engine with server-configured defaults
    #[arg(long)]
    pub replicated: bool,
    /// Create (and reset) the database ON CLUSTER with this cluster name
    #[arg(long)]
    pub cluster: Option<ClusterName>,
}

impl DatabaseCreateArgs {
    pub fn definition(self) -> DatabaseDefinition {
        DatabaseDefinition {
            engine: if self.replicated {
                DatabaseEngine::Replicated
            } else {
                DatabaseEngine::Default
            },
            cluster: self.cluster,
        }
    }
}

#[derive(Subcommand)]
pub enum MigrateCmd {
    /// Create the migration manifest table in an existing database
    Init {
        #[command(flatten)]
        url: DatabaseUrlArg,
        #[command(flatten)]
        manifest: ManifestArgs,
    },
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
        /// Preview migration actions without executing them (SQL logged at debug)
        #[arg(long)]
        dry_run: bool,
    },
    /// Revert applied migrations (default: only the latest)
    Revert {
        #[command(flatten)]
        source: SourceArg,
        #[command(flatten)]
        url: DatabaseUrlArg,
        #[command(flatten)]
        force: ForceArg,
        /// Revert all applied migrations with a version greater than this (0 = revert everything)
        #[arg(long)]
        target_version: Option<i64>,
        /// Preview migration actions without executing them (SQL logged at debug)
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
        #[command(flatten)]
        definition: DatabaseCreateArgs,
    },
    /// Drop the database named in the connection URL
    Drop {
        #[command(flatten)]
        url: DatabaseUrlArg,
        #[command(flatten)]
        force: ForceArg,
        /// Drop the database ON CLUSTER with this cluster name
        #[arg(long)]
        cluster: Option<ClusterName>,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// Create the database and run pending migrations (requires an initialized manifest)
    Setup {
        #[command(flatten)]
        source: SourceArg,
        #[command(flatten)]
        url: DatabaseUrlArg,
        #[command(flatten)]
        definition: DatabaseCreateArgs,
    },
    /// Drop and re-create the database; initialize the manifest separately before migrating
    Reset {
        #[command(flatten)]
        url: DatabaseUrlArg,
        #[command(flatten)]
        force: ForceArg,
        #[command(flatten)]
        definition: DatabaseCreateArgs,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    const MANIFEST_COMMANDS: [[&str; 2]; 1] = [["migrate", "init"]];

    #[test]
    fn ask_is_available_for_connection_commands_and_conflicts_with_url() {
        for (category, command) in [
            ("migrate", "init"),
            ("migrate", "run"),
            ("migrate", "revert"),
            ("migrate", "info"),
            ("database", "create"),
            ("database", "drop"),
            ("database", "setup"),
            ("database", "reset"),
        ] {
            assert!(Cli::try_parse_from(["nomad", category, command, "--ask"]).is_ok());
            let error = Cli::try_parse_from([
                "nomad",
                category,
                command,
                "--ask",
                "--database-url",
                "http://localhost",
            ])
            .err()
            .unwrap();
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
        }
        let error = Cli::try_parse_from(["nomad", "migrate", "add", "events", "--ask"])
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn force_is_available_only_for_destructive_commands() {
        for args in [
            vec!["nomad", "migrate", "revert", "--force"],
            vec!["nomad", "database", "drop", "--force"],
            vec!["nomad", "database", "reset", "--force"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        for args in [
            vec!["nomad", "migrate", "run", "--force"],
            vec!["nomad", "database", "create", "--force"],
            vec!["nomad", "database", "setup", "--force"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn database_creation_flags_are_independent_of_manifest_flags() {
        for command in ["create", "setup", "reset"] {
            for replicated in [false, true] {
                for cluster in [false, true] {
                    let mut args = vec!["nomad", "database", command];
                    if replicated {
                        args.push("--replicated");
                    }
                    if cluster {
                        args.push("--cluster=production");
                    }
                    let definition = match Cli::try_parse_from(args).unwrap().command {
                        Command::Database(
                            DatabaseCmd::Create { definition, .. }
                            | DatabaseCmd::Setup { definition, .. }
                            | DatabaseCmd::Reset { definition, .. },
                        ) => definition.definition(),
                        _ => unreachable!(),
                    };
                    assert_eq!(
                        definition.engine,
                        if replicated {
                            DatabaseEngine::Replicated
                        } else {
                            DatabaseEngine::Default
                        }
                    );
                    assert_eq!(definition.cluster.is_some(), cluster);
                }
            }
            assert!(Cli::try_parse_from(["nomad", "database", command, "--cluster="]).is_err());
        }
        assert!(Cli::try_parse_from(["nomad", "database", "drop", "--cluster=production"]).is_ok());
        assert!(Cli::try_parse_from(["nomad", "database", "drop", "--replicated"]).is_err());
    }

    #[test]
    fn verbosity_flags_are_global_and_repeatable() {
        for (args, expected) in [
            (vec!["nomad", "migrate", "run"], 0),
            (vec!["nomad", "-v", "migrate", "run"], 1),
            (vec!["nomad", "migrate", "-vv", "run"], 2),
            (vec!["nomad", "migrate", "run", "-vvv"], 3),
            (
                vec!["nomad", "database", "create", "--verbose", "--verbose"],
                2,
            ),
        ] {
            assert_eq!(Cli::try_parse_from(args).unwrap().verbose, expected);
        }
    }

    #[test]
    fn manifest_flags_select_all_four_combinations() {
        for command in MANIFEST_COMMANDS {
            for replicated in [false, true] {
                for cluster in [None, Some("production")] {
                    let mut args = vec!["nomad", command[0], command[1]];
                    if replicated {
                        args.push("--replicated");
                    }
                    if cluster.is_some() {
                        args.push("--cluster=production");
                    }
                    let cli = Cli::try_parse_from(args).unwrap();
                    let Command::Migrate(MigrateCmd::Init { manifest, .. }) = cli.command else {
                        unreachable!();
                    };
                    let manifest = manifest.table();
                    assert_eq!(manifest.replicated, replicated);
                    assert_eq!(manifest.cluster.as_ref().map(ClusterName::as_str), cluster);
                }
            }
        }
    }

    #[test]
    fn commands_without_a_manifest_do_not_require_selection() {
        for args in [
            vec!["nomad", "migrate", "add", "events"],
            vec!["nomad", "database", "create"],
            vec!["nomad", "database", "drop"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
    }

    #[test]
    fn migration_commands_only_use_the_existing_manifest() {
        for (category, command) in [
            ("migrate", "run"),
            ("migrate", "revert"),
            ("migrate", "info"),
        ] {
            assert!(Cli::try_parse_from(["nomad", category, command]).is_ok());
            for flag in ["--replicated", "--cluster=production"] {
                let error = Cli::try_parse_from(["nomad", category, command, flag])
                    .err()
                    .unwrap();
                assert_eq!(error.kind(), ErrorKind::UnknownArgument);
            }
        }
    }

    #[test]
    fn cluster_requires_a_nonempty_name() {
        let error = Cli::try_parse_from(["nomad", "migrate", "init", "--cluster"])
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        for command in MANIFEST_COMMANDS {
            for name in ["", "  "] {
                let error =
                    Cli::try_parse_from(["nomad", command[0], command[1], "--cluster", name])
                        .err()
                        .unwrap();
                assert_eq!(error.kind(), ErrorKind::ValueValidation);
                assert!(error.to_string().contains("--cluster must not be empty"));
            }
        }
    }
}

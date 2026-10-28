use super::checksum::CHECKSUM_LEN;
use anyhow::{Context, Result, bail, ensure};
use clickhouse::Client;

use crate::{client, config::ClusterName};

pub const TRACKING_TABLE: &str = "_nomad_migrations";

pub async fn init(ch: &Client, cluster: Option<&ClusterName>) -> Result<()> {
    let database_engine = ch
        .query("SELECT engine FROM system.databases WHERE name = currentDatabase()")
        .fetch_optional::<String>()
        .await
        .context("failed to inspect the database engine; no manifest DDL submitted")?
        .context(
            "selected database does not exist; create it before running `nomad migrate init`",
        )?;
    let manifest = ManifestTable::from_database(&database_engine, cluster)?;
    tracing::info!(
        "inspected database engine {database_engine}; selected manifest engine {}{}",
        manifest.engine(),
        if manifest.cluster().is_some() {
            " with ON CLUSTER"
        } else {
            " without ON CLUSTER"
        }
    );
    let existing = ch
        .query("SELECT engine FROM system.tables WHERE database = currentDatabase() AND name = ?")
        .bind(TRACKING_TABLE)
        .fetch_optional::<String>()
        .await
        .context("failed to inspect the migration manifest table")?;
    if let Some(engine) = &existing {
        ensure!(
            manifest.compatible(engine),
            "migration manifest `{TRACKING_TABLE}` already uses {engine}, incompatible with inferred {} for database engine {database_engine}; initialization does not convert existing engines",
            manifest.engine()
        );
        tracing::info!(
            "migration manifest `{TRACKING_TABLE}` already exists with engine {engine}; no DDL submitted; preserving migration history"
        );
        return Ok(());
    }
    tracing::info!(
        "initializing migration manifest with engine {}",
        manifest.engine()
    );
    client::execute(ch, &manifest.create_sql())
        .await
        .context("failed to create the migration tracking table")?;
    tracing::info!(
        "created migration manifest `{TRACKING_TABLE}` with engine {}",
        manifest.engine()
    );
    Ok(())
}

pub async fn require_tracking_table(ch: &Client) -> Result<()> {
    let count = ch
        .query("SELECT count() FROM system.tables WHERE database = currentDatabase() AND name = ?")
        .bind(TRACKING_TABLE)
        .fetch_one::<u64>()
        .await
        .context("failed to check for the migration manifest table")?;
    ensure!(
        count != 0,
        "migration manifest `{TRACKING_TABLE}` does not exist in the connected database; run `nomad migrate init` first"
    );
    Ok(())
}

/// The database engine determines manifest replication. Only local DDL can carry a cluster.
#[derive(Debug)]
enum ManifestTable {
    Local { cluster: Option<ClusterName> },
    Replicated,
    Shared,
}

impl ManifestTable {
    fn from_database(engine: &str, cluster: Option<&ClusterName>) -> Result<Self> {
        match engine {
            "Atomic" | "Ordinary" => Ok(Self::Local {
                cluster: cluster.cloned(),
            }),
            "Replicated" | "Shared" => {
                if let Some(cluster) = cluster {
                    tracing::warn!(
                        "Database uses Engine = {engine}. Skipping `--cluster={cluster}`",
                        cluster = cluster.as_str(),
                    );
                }
                Ok(if engine == "Replicated" {
                    Self::Replicated
                } else {
                    Self::Shared
                })
            }
            _ => bail!(
                "unsupported database engine {engine:?}; cannot infer a safe migration manifest engine; no manifest DDL submitted"
            ),
        }
    }

    fn cluster(&self) -> Option<&ClusterName> {
        match self {
            Self::Local { cluster } => cluster.as_ref(),
            Self::Replicated | Self::Shared => None,
        }
    }

    fn engine(&self) -> &'static str {
        match self {
            Self::Local { .. } => "MergeTree",
            Self::Replicated => "ReplicatedMergeTree()",
            Self::Shared => "SharedMergeTree()",
        }
    }

    fn compatible(&self, engine: &str) -> bool {
        match self {
            Self::Local { .. } => engine == "MergeTree",
            Self::Replicated => engine == "ReplicatedMergeTree",
            Self::Shared => engine == "SharedMergeTree",
        }
    }

    pub fn create_sql(&self) -> String {
        let scope = self.cluster().map_or_else(String::new, |cluster| {
            format!(" ON CLUSTER {}", client::quote_identifier(cluster.as_str()))
        });
        let engine = self.engine();
        format!(
            "CREATE TABLE IF NOT EXISTS `{TRACKING_TABLE}`{scope} (
            version           Int64,
            description       String,
            installed_on      DateTime64(3, 'UTC'),
            success           Bool,
            checksum          FixedString({CHECKSUM_LEN}),
            execution_time_ms UInt64
        ) ENGINE = {engine} ORDER BY version"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::splitter::split_statements;

    #[test]
    fn database_engine_selects_manifest_engine_and_ddl_scope() {
        for (database, engine) in [
            ("Atomic", "MergeTree"),
            ("Ordinary", "MergeTree"),
            ("Replicated", "ReplicatedMergeTree()"),
            ("Shared", "SharedMergeTree()"),
        ] {
            for cluster in [None, Some("production".parse::<ClusterName>().unwrap())] {
                let table = ManifestTable::from_database(database, cluster.as_ref()).unwrap();
                let sql = table.create_sql();
                let scope = if cluster.is_some() && matches!(database, "Atomic" | "Ordinary") {
                    " ON CLUSTER `production`"
                } else {
                    ""
                };
                assert!(sql.starts_with(&format!(
                    "CREATE TABLE IF NOT EXISTS `_nomad_migrations`{scope} ("
                )));
                assert!(sql.ends_with(&format!("ENGINE = {engine} ORDER BY version")));
                for column in [
                    "version           Int64",
                    "description       String",
                    "installed_on      DateTime64(3, 'UTC')",
                    "success           Bool",
                    "checksum          FixedString(48)",
                    "execution_time_ms UInt64",
                ] {
                    assert!(sql.contains(column), "missing {column}");
                }
            }
        }
        assert!(ManifestTable::from_database("Memory", None).is_err());
    }

    #[test]
    fn existing_manifest_must_match_inferred_engine() {
        for (database, expected) in [
            ("Atomic", "MergeTree"),
            ("Replicated", "ReplicatedMergeTree"),
            ("Shared", "SharedMergeTree"),
        ] {
            let table = ManifestTable::from_database(database, None).unwrap();
            for actual in [
                "MergeTree",
                "ReplicatedMergeTree",
                "SharedMergeTree",
                "Memory",
            ] {
                assert_eq!(table.compatible(actual), actual == expected);
            }
        }
    }

    #[test]
    fn cluster_name_cannot_escape_its_identifier() {
        let cluster = "prod`\\?; DROP TABLE important; --".parse().unwrap();
        let sql = ManifestTable::from_database("Atomic", Some(&cluster))
            .unwrap()
            .create_sql();
        assert!(sql.contains(&format!(
            "ON CLUSTER {}",
            client::quote_identifier(cluster.as_str())
        )));
        assert_eq!(split_statements(&sql).len(), 1);
        assert!(crate::client::escape_placeholders(&sql).contains("??"));
    }
}

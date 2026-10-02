use super::checksum::CHECKSUM_LEN;
use anyhow::{Context, Result, ensure};
use clickhouse::Client;

use crate::{client, config::ClusterName};

pub const TRACKING_TABLE: &str = "_nomad_migrations";

pub async fn init(ch: &Client, manifest: &ManifestTable) -> Result<()> {
    let existing = ch
        .query("SELECT engine FROM system.tables WHERE database = currentDatabase() AND name = ?")
        .bind(TRACKING_TABLE)
        .fetch_optional::<String>()
        .await
        .context("failed to inspect the migration manifest table")?;
    if let Some(engine) = &existing {
        let compatible = if manifest.replicated {
            matches!(engine.as_str(), "ReplicatedMergeTree" | "SharedMergeTree")
        } else {
            engine == "MergeTree"
        };
        ensure!(
            compatible,
            "migration manifest `{TRACKING_TABLE}` already uses {engine}, incompatible with requested {}; initialization does not convert existing engines",
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
        "migration manifest `{TRACKING_TABLE}` does not exist in the connected database; run `nomad migrate init` first (use --replicated and/or --cluster as needed)"
    );
    Ok(())
}

/// Controls only manifest creation; migration SQL and database DDL are unchanged.
pub struct ManifestTable {
    pub replicated: bool,
    pub cluster: Option<ClusterName>,
}

impl ManifestTable {
    fn engine(&self) -> &'static str {
        if self.replicated {
            "ReplicatedMergeTree()"
        } else {
            "MergeTree"
        }
    }

    pub fn create_sql(&self) -> String {
        let scope = self.cluster.as_ref().map_or_else(String::new, |cluster| {
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
    fn table_variants_preserve_the_manifest_schema() {
        for (replicated, engine) in [(false, "MergeTree"), (true, "ReplicatedMergeTree()")] {
            for cluster in [None, Some("production".parse::<ClusterName>().unwrap())] {
                let sql = ManifestTable {
                    replicated,
                    cluster: cluster.clone(),
                }
                .create_sql();
                let scope = if cluster.is_some() {
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
    }

    #[test]
    fn cluster_name_cannot_escape_its_identifier() {
        let sql = ManifestTable {
            replicated: false,
            cluster: Some("prod`\\?; DROP TABLE important; --".parse().unwrap()),
        }
        .create_sql();
        assert!(sql.contains("ON CLUSTER `prod\\`\\\\?; DROP TABLE important; --`"));
        assert_eq!(split_statements(&sql).len(), 1);
        assert!(crate::client::escape_placeholders(&sql).contains("??"));
    }
}

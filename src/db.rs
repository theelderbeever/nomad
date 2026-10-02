use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};

use crate::{
    client,
    config::{ClusterName, ConnectOptions},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseEngine {
    Default,
    Replicated,
}

pub struct DatabaseDefinition {
    pub engine: DatabaseEngine,
    pub cluster: Option<ClusterName>,
}

impl DatabaseDefinition {
    fn create_sql(&self, name: &str) -> String {
        let engine = match self.engine {
            DatabaseEngine::Default => "",
            DatabaseEngine::Replicated => " ENGINE = Replicated",
        };
        format!(
            "CREATE DATABASE IF NOT EXISTS {}{}{engine}",
            client::quote_identifier(name),
            cluster_scope(self.cluster.as_ref())
        )
    }

    fn check_engine(&self, engine: &str) -> Result<()> {
        let compatible = match self.engine {
            DatabaseEngine::Default => matches!(engine, "Atomic" | "Shared"),
            DatabaseEngine::Replicated => engine == "Replicated",
        };
        anyhow::ensure!(
            compatible,
            "existing database uses engine {engine}, incompatible with requested {:?}; use --replicated for Replicated databases. Nomad does not convert database engines",
            self.engine
        );
        Ok(())
    }
}

fn cluster_scope(cluster: Option<&ClusterName>) -> String {
    cluster.map_or_else(String::new, |name| {
        format!(" ON CLUSTER {}", client::quote_identifier(name.as_str()))
    })
}

async fn existing_engine(opts: &ConnectOptions) -> Result<Option<String>> {
    client::connect_without_db(opts)
        .query("SELECT engine FROM system.databases WHERE name = ?")
        .bind(&opts.database)
        .fetch_optional::<String>()
        .await
        .with_context(|| format!("failed to inspect database {:?}", opts.database))
}

pub async fn create(opts: &ConnectOptions, definition: &DatabaseDefinition) -> Result<()> {
    if let Some(engine) = existing_engine(opts).await? {
        definition.check_engine(&engine)?;
        tracing::info!(
            "database {:?} already exists with engine {engine}; no DDL submitted",
            opts.database
        );
        return Ok(());
    }
    let ch = client::connect_without_db(opts);
    client::execute(&ch, &definition.create_sql(&opts.database))
        .await
        .with_context(|| format!("failed to create database {:?}", opts.database))?;
    let engine = existing_engine(opts)
        .await?
        .context("created database was not found on the connected node")?;
    tracing::info!(
        "created database {:?} with engine {engine}{}",
        opts.database,
        cluster_scope(definition.cluster.as_ref())
    );
    Ok(())
}

async fn check_drop_size(
    opts: &ConnectOptions,
    cluster: Option<&ClusterName>,
    policy: client::DropPolicy,
) -> Result<()> {
    if !policy.is_protected() {
        return Ok(());
    }
    // Check before DROP DATABASE can shut down any table's background work.
    // The server-side limit remains in place to protect against concurrent writes.
    let ch = client::connect_without_db(opts);
    let source = if cluster.is_some() {
        "clusterAllReplicas(?, system.tables)"
    } else {
        "system.tables"
    };
    let sql = format!(
        "SELECT name FROM {source} WHERE database = ? AND endsWith(engine, 'MergeTree') AND (total_bytes > 1 OR total_rows > 0) LIMIT 1"
    );
    let mut query = ch.query(&sql);
    if let Some(cluster) = cluster {
        query = query.bind(cluster.as_str());
    }
    let table = query
        .bind(&opts.database)
        .fetch_optional::<String>()
        .await
        .context("failed to check database drop-size protection; no DROP submitted")?;
    if let Some(table) = table {
        bail!(
            "database {:?} contains MergeTree data in table {table:?}; max_table_size_to_drop=1 protection prevents dropping it. Pass --force to disable the limit; no DROP submitted",
            opts.database
        );
    }
    Ok(())
}

pub async fn drop(
    opts: &ConnectOptions,
    cluster: Option<&ClusterName>,
    yes: bool,
    policy: client::DropPolicy,
) -> Result<()> {
    check_drop_size(opts, cluster, policy).await?;
    let scope = cluster_scope(cluster);
    if !yes
        && !confirm(&format!(
            "Are you sure you want to drop the database {:?}{scope}?",
            opts.database
        ))?
    {
        bail!("aborted");
    }
    let ch = policy.client(client::connect_without_db(opts));
    client::execute(
        &ch,
        &format!(
            "DROP DATABASE IF EXISTS {}{scope}",
            client::quote_identifier(&opts.database)
        ),
    )
    .await
    .with_context(|| format!("failed to drop database {:?}", opts.database))?;
    tracing::info!("dropped database {:?}{scope}", opts.database);
    Ok(())
}

pub async fn reset(
    opts: &ConnectOptions,
    definition: &DatabaseDefinition,
    yes: bool,
    policy: client::DropPolicy,
) -> Result<()> {
    // Reject accidental engine changes before dropping anything.
    if let Some(engine) = existing_engine(opts).await? {
        definition.check_engine(&engine)?;
    }
    drop(opts, definition.cluster.as_ref(), yes, policy).await?;
    create(opts, definition).await?;
    tracing::info!(
        "database reset complete; no migrations applied. Run `nomad migrate init` with the desired replication flags, then `nomad migrate run`"
    );
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} (y/N) ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_ddl_supports_all_four_combinations() {
        for engine in [DatabaseEngine::Default, DatabaseEngine::Replicated] {
            for cluster in [None, Some("production".parse().unwrap())] {
                let definition = DatabaseDefinition { engine, cluster };
                let scope = if definition.cluster.is_some() {
                    " ON CLUSTER `production`"
                } else {
                    ""
                };
                let suffix = if engine == DatabaseEngine::Replicated {
                    " ENGINE = Replicated"
                } else {
                    ""
                };
                assert_eq!(
                    definition.create_sql("mydb"),
                    format!("CREATE DATABASE IF NOT EXISTS `mydb`{scope}{suffix}")
                );
            }
        }
    }

    #[test]
    fn existing_database_engine_must_match_the_request() {
        let default = DatabaseDefinition {
            engine: DatabaseEngine::Default,
            cluster: None,
        };
        let replicated = DatabaseDefinition {
            engine: DatabaseEngine::Replicated,
            cluster: None,
        };
        assert!(default.check_engine("Atomic").is_ok());
        assert!(replicated.check_engine("Replicated").is_ok());
        assert!(default.check_engine("Replicated").is_err());
        assert!(replicated.check_engine("Atomic").is_err());
    }

    #[test]
    fn database_and_cluster_names_are_quoted() {
        let definition = DatabaseDefinition {
            engine: DatabaseEngine::Replicated,
            cluster: Some("prod`\\?".parse().unwrap()),
        };
        let sql = definition.create_sql("db`\\?; DROP DATABASE important; --");
        assert!(sql.contains("`db\\`\\\\?; DROP DATABASE important; --`"));
        assert!(sql.contains("ON CLUSTER `prod\\`\\\\?`"));
        assert_eq!(crate::migrate::splitter::split_statements(&sql).len(), 1);
    }
}

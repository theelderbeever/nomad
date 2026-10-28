use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Write},
};

use anyhow::{Context, Result, bail, ensure};
use clickhouse::Client;
use serde::Deserialize;

use crate::{
    client,
    config::{ClusterName, ConnectOptions, DatabaseUuid},
    prompt,
};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseEngine {
    Atomic,
    Replicated,
}

impl DatabaseEngine {
    fn name(self) -> &'static str {
        match self {
            Self::Atomic => "Atomic",
            Self::Replicated => "Replicated",
        }
    }
}

#[derive(Clone)]
pub struct DatabaseDefinition {
    pub engine: Option<DatabaseEngine>,
    pub uuid: Option<DatabaseUuid>,
    pub cluster: Option<ClusterName>,
}

impl DatabaseDefinition {
    fn create_sql(&self, name: &str) -> String {
        let engine = match self.engine {
            None => "",
            Some(DatabaseEngine::Atomic) => " ENGINE = Atomic",
            Some(DatabaseEngine::Replicated) => " ENGINE = Replicated",
        };
        let uuid = self
            .uuid
            .map_or_else(String::new, |uuid| format!(" UUID '{uuid}'"));
        format!(
            "CREATE DATABASE IF NOT EXISTS {}{uuid}{}{engine}",
            client::quote_identifier(name),
            cluster_scope(self.cluster.as_ref())
        )
    }

    fn check_engine(&self, engine: &str) -> Result<()> {
        let compatible = match self.engine {
            None => matches!(engine, "Atomic" | "Shared"),
            Some(DatabaseEngine::Atomic) => engine == "Atomic",
            Some(DatabaseEngine::Replicated) => engine == "Replicated",
        };
        let requested = self
            .engine
            .map_or("server default (Atomic or Shared)", DatabaseEngine::name);
        anyhow::ensure!(
            compatible,
            "existing database uses engine {engine}, incompatible with requested {requested}; select the matching --engine. Nomad does not convert database engines"
        );
        Ok(())
    }
}

fn cluster_scope(cluster: Option<&ClusterName>) -> String {
    cluster.map_or_else(String::new, |name| {
        format!(" ON CLUSTER {}", client::quote_identifier(name.as_str()))
    })
}

#[derive(Debug, clickhouse::Row, Deserialize)]
struct DatabaseRecord {
    host: String,
    engine: String,
    uuid: String,
}

#[derive(Debug)]
struct NodeState {
    host: String,
    database: Option<DatabaseRecord>,
}

#[derive(clickhouse::Row, Deserialize)]
struct ClusterNode {
    host_name: String,
    port: u16,
    shard_num: u32,
    replica_num: u32,
}

async fn cluster_nodes(ch: &Client, cluster: &ClusterName) -> Result<Vec<ClusterNode>> {
    ch.query("SELECT host_name, port, shard_num, replica_num FROM system.clusters WHERE cluster = ? ORDER BY shard_num, replica_num")
        .bind(cluster.as_str()).fetch_all::<ClusterNode>().await.context("failed to inspect cluster membership")
}

async fn resolve_cluster(ch: &Client, cluster: &ClusterName) -> Result<ClusterName> {
    let mut name = cluster.as_str().to_owned();
    if name.contains('{') {
        let macros = ch
            .query("SELECT macro, substitution FROM system.macros")
            .fetch_all::<(String, String)>()
            .await
            .context("failed to resolve cluster macros")?;
        for (key, value) in macros {
            name = name.replace(&format!("{{{key}}}"), &value);
        }
        ensure!(
            !name.contains(['{', '}']),
            "unresolved cluster macro in {name:?}"
        );
    }
    let cluster: ClusterName = name.parse()?;
    ensure!(
        !cluster_nodes(ch, &cluster).await?.is_empty(),
        "cluster {:?} is not configured on the connected node",
        cluster.as_str()
    );
    Ok(cluster)
}

async fn database_states(
    opts: &ConnectOptions,
    cluster: Option<&ClusterName>,
) -> Result<Vec<NodeState>> {
    tracing::info!(database = opts.database.as_str(), endpoint = %opts.endpoint, "inspecting database identity and engine");
    let ch = client::connect_without_db(opts).with_setting("skip_unavailable_shards", "0");
    let (hosts, records) = if let Some(cluster) = cluster {
        let hosts = ch
            .query("SELECT hostName() FROM clusterAllReplicas(?, system.one)")
            .bind(cluster.as_str())
            .fetch_all::<String>()
            .await
            .context("failed to inspect all cluster nodes; unavailable nodes are not skipped")?;
        let records = ch.query("SELECT hostName() AS host, engine, toString(uuid) AS uuid FROM clusterAllReplicas(?, system.databases) WHERE name = ?")
            .bind(cluster.as_str()).bind(opts.database.as_str()).fetch_all::<DatabaseRecord>().await
            .context("failed to inspect database identities on all cluster nodes")?;
        (hosts, records)
    } else {
        let host = ch
            .query("SELECT hostName()")
            .fetch_one::<String>()
            .await
            .context("failed to inspect connected node")?;
        let records = ch.query("SELECT hostName() AS host, engine, toString(uuid) AS uuid FROM system.databases WHERE name = ?")
            .bind(opts.database.as_str()).fetch_all::<DatabaseRecord>().await.context("failed to inspect database identity")?;
        (vec![host], records)
    };
    let mut records: BTreeMap<_, _> = records
        .into_iter()
        .map(|row| (row.host.clone(), row))
        .collect();
    let hosts: BTreeSet<_> = hosts.into_iter().collect();
    ensure!(
        !hosts.is_empty(),
        "no nodes returned while inspecting database"
    );
    Ok(hosts
        .into_iter()
        .map(|host| NodeState {
            database: records.remove(&host),
            host,
        })
        .collect())
}

fn check_consistency(states: &[NodeState]) -> Result<()> {
    let records: Vec<_> = states
        .iter()
        .filter_map(|node| node.database.as_ref())
        .collect();
    if let Some(first) = records.first() {
        ensure!(
            records.iter().all(|row| row.engine == first.engine),
            "database engines differ across nodes; inspect `nomad db info` before proceeding"
        );
        if first.engine == "Replicated" {
            ensure!(
                records.iter().all(|row| row.uuid == first.uuid),
                "Replicated database UUIDs differ across nodes; inspect `nomad db info` before proceeding"
            );
        }
    }
    Ok(())
}

fn check_definition(states: &[NodeState], definition: &DatabaseDefinition) -> Result<()> {
    check_consistency(states)?;
    for row in states.iter().filter_map(|node| node.database.as_ref()) {
        definition.check_engine(&row.engine)?;
        if let Some(expected) = definition.uuid {
            ensure!(
                row.uuid == expected.to_string(),
                "database on {:?} has UUID {}, incompatible with requested {expected}; no identity conversion is performed",
                row.host,
                row.uuid
            );
        }
    }
    Ok(())
}

fn existing_uuid(states: &[NodeState]) -> Result<Option<DatabaseUuid>> {
    let mut records = states.iter().filter_map(|node| node.database.as_ref());
    let Some(first) = records.next() else {
        return Ok(None);
    };
    if records.all(|row| row.uuid == first.uuid) {
        Ok(Some(first.uuid.parse()?))
    } else {
        Ok(None)
    }
}

async fn submit_create(opts: &ConnectOptions, definition: &DatabaseDefinition) -> Result<()> {
    tracing::info!(
        "creating database {:?}{} with engine {} and UUID {}; waiting for ClickHouse to complete DDL",
        opts.database.as_str(),
        cluster_scope(definition.cluster.as_ref()),
        definition
            .engine
            .map_or("server default", DatabaseEngine::name),
        definition
            .uuid
            .map_or_else(|| "generated by ClickHouse".into(), |uuid| uuid.to_string())
    );
    client::execute(&client::connect_without_db(opts), &definition.create_sql(opts.database.as_str())).await
        .with_context(|| format!("failed while creating database {:?}; submitted DDL may continue on ClickHouse; inspect `nomad db info` before retrying", opts.database.as_str()))?;
    let states = database_states(opts, definition.cluster.as_ref()).await
        .context("database DDL submitted, but verification failed; inspect `nomad db info` before retrying")?;
    check_definition(&states, definition)?;
    for node in &states {
        let row = node.database.as_ref().with_context(|| format!("database was not found on node {:?} after DDL; inspect `nomad db info` before retrying", node.host))?;
        tracing::info!(
            host = row.host,
            engine = row.engine,
            uuid = row.uuid,
            "database creation verified"
        );
    }
    Ok(())
}

pub async fn create(opts: &ConnectOptions, definition: &DatabaseDefinition) -> Result<()> {
    let mut definition = definition.clone();
    if let Some(cluster) = &definition.cluster {
        definition.cluster =
            Some(resolve_cluster(&client::connect_without_db(opts), cluster).await?);
    }
    let states = database_states(opts, definition.cluster.as_ref())
        .await
        .context("database creation preflight failed; no DDL submitted")?;
    check_definition(&states, &definition)?;
    if states.iter().all(|node| node.database.is_some()) {
        for row in states.iter().filter_map(|node| node.database.as_ref()) {
            tracing::info!(
                "database {:?} already exists on {:?} with engine {} and UUID {}; no DDL submitted",
                opts.database.as_str(),
                row.host,
                row.engine,
                row.uuid
            );
        }
        return Ok(());
    }
    let existing = existing_uuid(&states)?;
    definition.uuid = prompt::database_uuid(definition.uuid, existing)?;
    // Generation must never create a second identity beside an existing group.
    check_definition(&states, &definition)?;
    submit_create(opts, &definition).await
}

pub async fn info(opts: &ConnectOptions, cluster: Option<&ClusterName>) -> Result<()> {
    let ch = client::connect_without_db(opts);
    let cluster = if let Some(cluster) = cluster {
        Some(resolve_cluster(&ch, cluster).await?)
    } else {
        let local = database_states(opts, None).await?;
        if local.iter().any(|node| {
            node.database
                .as_ref()
                .is_some_and(|row| row.engine == "Replicated")
        }) {
            let candidate: ClusterName = opts.database.as_str().parse()?;
            if cluster_nodes(&ch, &candidate).await?.is_empty() {
                tracing::warn!(
                    "could not discover the database replication cluster; reporting the connected node only; pass --cluster to inspect other nodes"
                );
                None
            } else {
                Some(candidate)
            }
        } else {
            None
        }
    };
    println!("Database: {:?}", opts.database.as_str());
    if let Some(cluster) = &cluster {
        println!("Cluster: {}", cluster.as_str());
        let nodes = cluster_nodes(&ch, cluster).await?;
        let width = nodes
            .iter()
            .map(|node| node.host_name.len())
            .max()
            .unwrap_or(0)
            .max(14);
        println!(
            "{:<width$}  {:>5}  {:>5}  {:>7}",
            "CONFIGURED HOST", "PORT", "SHARD", "REPLICA"
        );
        for node in nodes {
            println!(
                "{:<width$}  {:>5}  {:>5}  {:>7}",
                node.host_name, node.port, node.shard_num, node.replica_num
            );
        }
    } else {
        println!("Scope: connected node only (pass --cluster to inspect a cluster)");
    }
    let states = database_states(opts, cluster.as_ref()).await?;
    let width = states
        .iter()
        .map(|node| node.host.len())
        .max()
        .unwrap_or(0)
        .max(4);
    let engine_width = states
        .iter()
        .filter_map(|node| node.database.as_ref())
        .map(|row| row.engine.len())
        .max()
        .unwrap_or(0)
        .max(7);
    println!("{:<width$}  {:<engine_width$}  UUID", "HOST", "ENGINE");
    for node in &states {
        if let Some(row) = &node.database {
            println!(
                "{:<width$}  {:<engine_width$}  {}",
                node.host, row.engine, row.uuid
            );
        } else {
            println!("{:<width$}  {:<engine_width$}  -", node.host, "MISSING");
        }
    }
    check_consistency(&states)?;
    ensure!(
        states.iter().all(|node| node.database.is_some()),
        "database is missing on one or more nodes listed above"
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
        .bind(opts.database.as_str())
        .fetch_optional::<String>()
        .await
        .context("failed to check database drop-size protection; no DROP submitted")?;
    if let Some(table) = table {
        bail!(
            "database {:?} contains MergeTree data in table {table:?}; max_table_size_to_drop=1 protection prevents dropping it. Pass --force to disable the limit; no DROP submitted",
            opts.database.as_str()
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
            opts.database.as_str()
        ))?
    {
        bail!("aborted");
    }
    let ch = policy.client(client::connect_without_db(opts));
    client::execute(
        &ch,
        &format!(
            "DROP DATABASE IF EXISTS {}{scope}",
            client::quote_identifier(opts.database.as_str())
        ),
    )
    .await
    .with_context(|| format!("failed to drop database {:?}", opts.database.as_str()))?;
    tracing::info!("dropped database {:?}{scope}", opts.database.as_str());
    Ok(())
}

pub async fn reset(
    opts: &ConnectOptions,
    definition: &DatabaseDefinition,
    yes: bool,
    policy: client::DropPolicy,
) -> Result<()> {
    let mut definition = definition.clone();
    if let Some(cluster) = &definition.cluster {
        definition.cluster =
            Some(resolve_cluster(&client::connect_without_db(opts), cluster).await?);
    }
    let states = database_states(opts, definition.cluster.as_ref()).await?;
    check_consistency(&states)?;
    for row in states.iter().filter_map(|node| node.database.as_ref()) {
        definition.check_engine(&row.engine)?;
    }
    // Resolve the new identity before any DROP so canceling the prompt cannot remove data.
    definition.uuid = prompt::database_uuid(definition.uuid, None)?;
    drop(opts, definition.cluster.as_ref(), yes, policy).await?;
    submit_create(opts, &definition).await?;
    tracing::info!(
        "database reset complete; no migrations applied. Run `nomad migrate init`, then `nomad migrate run`"
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
    fn database_ddl_supports_explicit_engines_and_server_default() {
        for engine in [
            None,
            Some(DatabaseEngine::Atomic),
            Some(DatabaseEngine::Replicated),
        ] {
            for cluster in [None, Some("production".parse().unwrap())] {
                let definition = DatabaseDefinition {
                    engine,
                    cluster,
                    uuid: None,
                };
                let scope = if definition.cluster.is_some() {
                    " ON CLUSTER `production`"
                } else {
                    ""
                };
                let suffix = if engine == Some(DatabaseEngine::Replicated) {
                    " ENGINE = Replicated"
                } else if engine == Some(DatabaseEngine::Atomic) {
                    " ENGINE = Atomic"
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
            engine: None,
            cluster: None,
            uuid: None,
        };
        let replicated = DatabaseDefinition {
            engine: Some(DatabaseEngine::Replicated),
            cluster: None,
            uuid: None,
        };
        let atomic = DatabaseDefinition {
            engine: Some(DatabaseEngine::Atomic),
            cluster: None,
            uuid: None,
        };
        assert!(atomic.check_engine("Atomic").is_ok());
        assert!(atomic.check_engine("Shared").is_err());
        assert!(atomic.check_engine("Replicated").is_err());
        assert!(default.check_engine("Shared").is_ok());
        assert!(default.check_engine("Atomic").is_ok());
        assert!(replicated.check_engine("Replicated").is_ok());
        assert!(default.check_engine("Replicated").is_err());
        assert!(replicated.check_engine("Atomic").is_err());
    }

    #[test]
    fn explicit_uuid_is_part_of_database_ddl_and_existing_identity_is_checked() {
        let uuid: DatabaseUuid = "314487e9-ca39-4ff1-ae91-e82c9d3f07e3".parse().unwrap();
        let definition = DatabaseDefinition {
            engine: Some(DatabaseEngine::Replicated),
            cluster: Some("production".parse().unwrap()),
            uuid: Some(uuid),
        };
        assert_eq!(
            definition.create_sql("reporting"),
            "CREATE DATABASE IF NOT EXISTS `reporting` UUID '314487e9-ca39-4ff1-ae91-e82c9d3f07e3' ON CLUSTER `production` ENGINE = Replicated"
        );
        let mut states = vec![
            NodeState {
                host: "one".into(),
                database: Some(DatabaseRecord {
                    host: "one".into(),
                    engine: "Replicated".into(),
                    uuid: uuid.to_string(),
                }),
            },
            NodeState {
                host: "two".into(),
                database: None,
            },
        ];
        assert!(check_definition(&states, &definition).is_ok());
        assert_eq!(existing_uuid(&states).unwrap(), Some(uuid));
        states.push(NodeState {
            host: "three".into(),
            database: Some(DatabaseRecord {
                host: "three".into(),
                engine: "Replicated".into(),
                uuid: uuid::Uuid::new_v4().to_string(),
            }),
        });
        assert!(check_definition(&states, &definition).is_err());
    }

    #[test]
    fn database_and_cluster_names_are_quoted() {
        let definition = DatabaseDefinition {
            engine: Some(DatabaseEngine::Replicated),
            cluster: Some("prod`\\?".parse().unwrap()),
            uuid: None,
        };
        let sql = definition.create_sql("db`\\?; DROP DATABASE important; --");
        assert!(sql.contains("`db\\`\\\\?; DROP DATABASE important; --`"));
        assert!(sql.contains("ON CLUSTER `prod\\`\\\\?`"));
        assert_eq!(crate::migrate::splitter::split_statements(&sql).len(), 1);
    }
}

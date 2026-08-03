use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clickhouse::Client;
use serde::{Deserialize, Serialize};

use super::checksum::{CHECKSUM_LEN, checksum};
use super::source::{self, Migration};
use super::splitter::split_statements;
use crate::client;

pub const TRACKING_TABLE: &str = "_nomad_migrations";

#[derive(Debug, Clone, clickhouse::Row, Serialize, Deserialize)]
pub struct MigrationRow {
    pub version: i64,
    pub description: String,
    #[serde(with = "clickhouse::serde::chrono::datetime64::millis")]
    pub installed_on: DateTime<Utc>,
    pub success: bool,
    #[serde(with = "fixed_bytes")]
    pub checksum: [u8; CHECKSUM_LEN],
    pub execution_time_ms: u64,
}

/// Serde impl for `[u8; CHECKSUM_LEN]` as a fixed-length tuple, which the
/// clickhouse crate's RowBinary codec maps onto `FixedString(N)`. Needed
/// because serde only derives arrays up to 32 elements.
mod fixed_bytes {
    use super::CHECKSUM_LEN;
    use serde::de::{Error as _, SeqAccess, Visitor};
    use serde::ser::SerializeTuple;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; CHECKSUM_LEN], s: S) -> Result<S::Ok, S::Error> {
        let mut tuple = s.serialize_tuple(CHECKSUM_LEN)?;
        for byte in v {
            tuple.serialize_element(byte)?;
        }
        tuple.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; CHECKSUM_LEN], D::Error> {
        struct BytesVisitor;
        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = [u8; CHECKSUM_LEN];

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{CHECKSUM_LEN} bytes")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = [0u8; CHECKSUM_LEN];
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = seq.next_element()?.ok_or_else(|| A::Error::invalid_length(i, &self))?;
                }
                Ok(out)
            }
        }
        d.deserialize_tuple(CHECKSUM_LEN, BytesVisitor)
    }
}

pub async fn ensure_tracking_table(ch: &Client) -> Result<()> {
    ch.query(&format!(
        "CREATE TABLE IF NOT EXISTS `{TRACKING_TABLE}` (
            version           Int64,
            description       String,
            installed_on      DateTime64(3, 'UTC'),
            success           Bool,
            checksum          FixedString({CHECKSUM_LEN}),
            execution_time_ms UInt64
        ) ENGINE = MergeTree ORDER BY version"
    ))
    .execute()
    .await
    .context("failed to create the migration tracking table")
}

pub async fn fetch_applied(ch: &Client) -> Result<Vec<MigrationRow>> {
    ch.query(&format!(
        "SELECT version, description, installed_on, success, checksum, execution_time_ms
         FROM `{TRACKING_TABLE}` ORDER BY version"
    ))
    .fetch_all::<MigrationRow>()
    .await
    .context("failed to read the migration tracking table")
}

/// Hard-error checks shared by `run` and `revert`: dirty rows, applied
/// migrations missing locally, and checksum mismatches.
fn verify_applied(applied: &[MigrationRow], local: &BTreeMap<i64, &Migration>) -> Result<()> {
    for row in applied {
        if !row.success {
            bail!(
                "database is in a dirty state from migration {v}: a previous attempt failed partway.\n\
                 Fix the schema by hand, then either delete the tracking row to re-run the migration\n\
                 (DELETE FROM `{t}` WHERE version = {v}) or, if the remaining statements were applied\n\
                 manually, replace it with a success = true row.",
                v = row.version,
                t = TRACKING_TABLE,
            );
        }
        let Some(migration) = local.get(&row.version) else {
            bail!(
                "migration {} was applied to the database but is missing from the local migration directory",
                row.version
            );
        };
        if checksum(migration.up_sql.as_bytes()) != row.checksum {
            bail!(
                "migration {} was previously applied but its up file has been modified\n\
                 (applied checksum {}, local checksum {})",
                row.version,
                hex::encode(row.checksum),
                hex::encode(checksum(migration.up_sql.as_bytes())),
            );
        }
    }
    Ok(())
}

fn by_version(migrations: &[Migration]) -> BTreeMap<i64, &Migration> {
    migrations.iter().map(|m| (m.version, m)).collect()
}

pub async fn run(
    ch: &Client,
    source_dir: &Path,
    target_version: Option<i64>,
    dry_run: bool,
) -> Result<()> {
    let migrations = source::discover(source_dir)?;
    ensure_tracking_table(ch).await?;
    let applied = fetch_applied(ch).await?;
    verify_applied(&applied, &by_version(&migrations))?;

    if let Some(target) = target_version {
        if !migrations.iter().any(|m| m.version == target) {
            bail!("--target-version {target} does not match any local migration");
        }
        if let Some(max_applied) = applied.iter().map(|r| r.version).max() {
            if target < max_applied {
                bail!(
                    "--target-version {target} is below the latest applied migration ({max_applied}); \
                     use `migrate revert --target-version {target}` to go backwards"
                );
            }
        }
    }

    let applied_versions: Vec<i64> = applied.iter().map(|r| r.version).collect();
    let pending: Vec<&Migration> = migrations
        .iter()
        .filter(|m| !applied_versions.contains(&m.version))
        .filter(|m| target_version.is_none_or(|t| m.version <= t))
        .collect();

    if pending.is_empty() {
        println!("no pending migrations");
        return Ok(());
    }

    for migration in pending {
        apply_one(ch, migration, dry_run).await?;
    }
    Ok(())
}

async fn apply_one(ch: &Client, migration: &Migration, dry_run: bool) -> Result<()> {
    let statements = split_statements(&migration.up_sql);
    if dry_run {
        println!("would apply {}/{}:", migration.version, migration.description);
        for stmt in &statements {
            println!("    {stmt};");
        }
        println!("    -- then record version {} in `{TRACKING_TABLE}`", migration.version);
        return Ok(());
    }

    let started = Instant::now();
    let total = statements.len();
    for (i, stmt) in statements.iter().enumerate() {
        if let Err(err) = client::execute(ch, stmt).await {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            record(ch, migration, false, elapsed_ms)
                .await
                .context("additionally failed to record the dirty state in the tracking table")?;
            return Err(err.context(format!(
                "migration {} ({}) failed at statement {} of {total}; \
                 recorded a success = false row — the database is now in a dirty state \
                 (see README for recovery)",
                migration.version,
                migration.up_path.display(),
                i + 1,
            )));
        }
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    record(ch, migration, true, elapsed_ms).await?;
    println!("applied {}/{} ({elapsed_ms}ms)", migration.version, migration.description);
    Ok(())
}

async fn record(ch: &Client, migration: &Migration, success: bool, execution_time_ms: u64) -> Result<()> {
    let mut insert = ch.insert::<MigrationRow>(TRACKING_TABLE).await?;
    insert
        .write(&MigrationRow {
            version: migration.version,
            description: migration.description.clone(),
            installed_on: Utc::now(),
            success,
            checksum: checksum(migration.up_sql.as_bytes()),
            execution_time_ms,
        })
        .await?;
    insert.end().await.context("failed to record migration in the tracking table")
}

pub async fn revert(
    ch: &Client,
    source_dir: &Path,
    target_version: Option<i64>,
    dry_run: bool,
) -> Result<()> {
    let migrations = source::discover(source_dir)?;
    let local = by_version(&migrations);
    ensure_tracking_table(ch).await?;
    let applied = fetch_applied(ch).await?;
    verify_applied(&applied, &local)?;

    // Applied versions above the target, newest first. No target = latest only.
    let mut to_revert: Vec<i64> = applied
        .iter()
        .map(|r| r.version)
        .filter(|&v| target_version.is_none_or(|t| v > t))
        .collect();
    to_revert.sort_unstable_by(|a, b| b.cmp(a));
    if target_version.is_none() {
        to_revert.truncate(1);
    }

    if to_revert.is_empty() {
        println!("no migrations to revert");
        return Ok(());
    }

    for version in to_revert {
        let migration = local[&version]; // verified present above
        revert_one(ch, migration, dry_run).await?;
    }
    Ok(())
}

async fn revert_one(ch: &Client, migration: &Migration, dry_run: bool) -> Result<()> {
    let statements = split_statements(&migration.down_sql);
    if dry_run {
        println!("would revert {}/{}:", migration.version, migration.description);
        for stmt in &statements {
            println!("    {stmt};");
        }
        println!("    -- then delete version {} from `{TRACKING_TABLE}`", migration.version);
        return Ok(());
    }

    let started = Instant::now();
    let total = statements.len();
    for (i, stmt) in statements.iter().enumerate() {
        client::execute(ch, stmt).await.with_context(|| {
            format!(
                "revert of migration {} ({}) failed at statement {} of {total}; \
                 the migration is still recorded as applied — fix the schema by hand \
                 before retrying",
                migration.version,
                migration.down_path.display(),
                i + 1,
            )
        })?;
    }

    // Only unrecord after every down statement succeeded, so a failed revert
    // leaves the migration still marked applied.
    ch.query(&format!(
        "DELETE FROM `{TRACKING_TABLE}` WHERE version = ? SETTINGS lightweight_deletes_sync = 2"
    ))
    .bind(migration.version)
    .execute()
    .await
    .context(
        "failed to delete the tracking row (lightweight DELETE requires ClickHouse >= 22.8; \
         on older servers run: ALTER TABLE `_nomad_migrations` DELETE WHERE version = <v> \
         SETTINGS mutations_sync = 2)",
    )?;

    let elapsed_ms = started.elapsed().as_millis() as u64;
    println!("reverted {}/{} ({elapsed_ms}ms)", migration.version, migration.description);
    Ok(())
}

pub async fn info(ch: &Client, source_dir: &Path) -> Result<()> {
    let migrations = source::discover(source_dir)?;
    ensure_tracking_table(ch).await?;
    let applied = fetch_applied(ch).await?;
    let applied_by_version: BTreeMap<i64, &MigrationRow> =
        applied.iter().map(|r| (r.version, r)).collect();

    let mut seen = Vec::new();
    for migration in &migrations {
        seen.push(migration.version);
        let (status, note) = match applied_by_version.get(&migration.version) {
            None => ("pending  ", ""),
            Some(row) if !row.success => ("installed", " (dirty!)"),
            Some(row) if row.checksum != checksum(migration.up_sql.as_bytes()) => {
                ("installed", " (checksum mismatch!)")
            }
            Some(_) => ("installed", ""),
        };
        println!("{}/{status} {}{note}", migration.version, migration.description);
    }
    for row in &applied {
        if !seen.contains(&row.version) {
            println!("{}/installed {} (missing local file!)", row.version, row.description);
        }
    }
    Ok(())
}

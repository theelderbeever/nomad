# nomad

A thin, sqlx-cli-style migration tool built specifically for ClickHouse.

- Reversible migrations only: every migration is a `<version>_<description>.up.sql` /
  `.down.sql` pair. Single-file migrations are not supported.
- Talks to ClickHouse over the HTTP interface (port 8123/8443).
- Choose a plain or replicated manifest table, with optional `ON CLUSTER` creation,
  using `nomad migrate init --replicated --cluster <NAME>`.

## Usage

```console
$ nomad migrate add create_events
created migrations/20260803120000_create_events.up.sql
created migrations/20260803120000_create_events.down.sql

$ nomad database create # create the database
$ nomad migrate init    # create _nomad_migrations (plain MergeTree by default)
$ nomad database setup  # create database + run pending (manifest must already exist)
$ nomad migrate run     # apply pending migrations
$ nomad migrate info    # list applied/pending
$ nomad migrate revert  # revert the latest applied migration
$ nomad migrate revert --target-version 0   # revert everything
$ nomad database reset -y  # drop and re-create; then run migrate init and migrate run
```

Common flags:

- `--replicated` — use `ReplicatedMergeTree()` on `migrate init`, or the `Replicated`
  database engine on `database create`, `setup`, and `reset`
- `--cluster <NAME>` — scope manifest creation or database create/setup/reset/drop
  to `ON CLUSTER`; independent of `--replicated`
- `-v`, `-vv`, `-vvv` / `--verbose` — increase Nomad logging (`debug`, then `trace`;
  further repetitions stay at `trace`); accepted before or after subcommands
- `--source <DIR>` — migration directory (default `migrations`)
- `--database-url <URL>` — falls back to `CLICKHOUSE_URL`, then `DATABASE_URL`
  (a `.env` file is loaded if present)
- `--target-version <V>` — run up to / revert down to a version
- `--dry-run` — preview migration actions without executing them (SQL at debug)
- `--force` — disable the drop-size limit on `migrate revert`, `database drop`, and
  `database reset`; `-y` skips confirmation only and does not disable protection

Accepted URLs: `http://`, `https://`, `clickhouse://` (→ http, port 8123),
`clickhouses://` (→ https, port 8443). The first path segment is the database name:

```
CLICKHOUSE_URL=http://default:secret@localhost:8123/myapp
```

## Database provisioning

Database creation uses the server's default engine unless `--replicated` is supplied.
The Replicated engine uses configured defaults for its Keeper/ZooKeeper path, shard,
and replica names; Nomad does not expose custom engine arguments.

```console
$ nomad database create --replicated --cluster=production
$ nomad migrate init --replicated
$ nomad migrate run
```

This database creation emits:

```sql
CREATE DATABASE IF NOT EXISTS `myapp` ON CLUSTER `production` ENGINE = Replicated;
```

Database replication propagates metadata; the manifest still needs its own replicated
table engine to share migration records. Existing compatible databases produce no
DDL. An incompatible existing engine is an error; Nomad does not convert engines.

`database setup` accepts the same creation flags and requires an existing manifest.
`database reset` checks engine compatibility before dropping anything and uses the
selected engine and cluster for recreation. Repeat `--replicated` and `--cluster`
when resetting a replicated cluster database; reset never creates the manifest or
runs migrations. Custom engine parameters are not preserved by reset: provision
those databases externally.

```console
$ nomad database reset --replicated --cluster=production -y
$ nomad migrate init --replicated
$ nomad migrate run
$ nomad database drop --cluster=production -y
```

Destructive commands (`database drop`, `database reset`, and `migrate revert`) set
`max_table_size_to_drop=1` per request. Pass `--force` to set it to `0`, disabling
ClickHouse's size restriction. Database commands first check MergeTree data on the
connected node or all selected cluster replicas, before submitting DROP, to avoid
shutting down tables during a rejected database drop. A failed revert leaves its
migration recorded as applied.

```console
$ nomad migrate revert --force
$ nomad database reset --replicated --cluster=production --force -y
$ nomad database drop --cluster=production --force -y
```

The limit is in bytes and protects MergeTree-family drops/truncations; it is not a
universal row-count guard for engines such as Memory. Database protection also covers
nonempty `_nomad_migrations` tables, so a populated manifest requires `--force` to
drop/reset its database. Migration SQL may explicitly override query settings.
([ClickHouse drop-size setting](https://clickhouse.com/docs/operations/server-configuration-parameters/settings#max_table_size_to_drop)).

## How it works

State lives in a `_nomad_migrations` table inside the target database, holding the
version, description, timestamp, success flag, raw SHA-384 checksum of the up file,
and execution time of every applied migration. All variants use `ORDER BY version`.

Create the manifest explicitly with `nomad migrate init` after creating the database.
`migrate run`, `revert`, and `info` (including dry runs) require an existing manifest
and fail with an initialization hint if it is missing; they never create it.
Initialization is idempotent with matching engine flags and preserves existing migration
history. If a compatible manifest already exists on the connected node, initialization
submits no DDL, including with `--cluster`; it does not provision missing tables on
other nodes. Requesting a different engine for an existing manifest is an error.
Only `migrate init` creates the manifest. `database setup` creates the database if
needed and applies migrations using an existing manifest; it fails with the same
initialization hint if the manifest is missing. `database reset` drops and recreates
the database without applying migrations. After reset, run `migrate init` with the
desired replication flags, then `migrate run`.

| Flags | Engine | Creation scope |
| --- | --- | --- |
| Neither | `MergeTree` | Connected node |
| `--cluster production` | `MergeTree` | `ON CLUSTER production` |
| `--replicated` | `ReplicatedMergeTree()` | Connected node / database-managed DDL |
| `--replicated --cluster production` | `ReplicatedMergeTree()` | `ON CLUSTER production` |

```console
$ nomad migrate init --replicated --cluster=production
$ nomad migrate run
$ nomad migrate info
```

Replicated variants use the server's `default_replica_path` and `default_replica_name`
([ClickHouse replication documentation](https://clickhouse.com/docs/reference/engines/table-engines/mergetree-family/replication)).
Self-managed deployments need Keeper/ZooKeeper and suitable replica configuration.
The variant without `ON CLUSTER` is useful where the database or service manages
DDL propagation; otherwise ensure the manifest exists on each intended replica with
a shared replication path.

These flags control manifest creation only. They do not convert an existing table's
engine, add `ON CLUSTER` to migration SQL, or change database engines. Database commands accept their own flags.
For `ON CLUSTER`, the target database must already exist on every cluster node.
Manifest reads, inserts, and deletes use the connected node; `MergeTree` manifests
remain independent on each node, and replicated manifests share data within their
replication group, not across shards. Run migrations from a consistent endpoint and
one runner at a time; replication does not provide a migration lock.

Progress, warnings, and errors use `tracing` on stderr, with `info` as the default
level. Migration status listings (`migrate info`) remain on stdout. SQL is logged
only at `debug`, including during dry runs. Increase Nomad's verbosity with global
flags; dependency logs keep their existing levels:

```console
$ nomad -v migrate run --dry-run       # debug, including SQL
$ nomad migrate run --dry-run -vv      # trace
$ nomad -vvv migrate init --replicated # trace (maximum level)
```

`RUST_LOG` remains available for advanced filters. When verbosity flags are supplied,
the CLI overrides the `nomad` directive while preserving filters for dependencies.

At `info`, each migration produces one summary line with its manifest change,
including dry runs. A failed migration produces a failure summary plus an error with
the cause. Statement progress and SQL are logged at `debug`. Initialization reports whether the manifest was created
or already existed without submitting DDL.

Because the ClickHouse HTTP interface accepts one statement per request, each
migration file is split into statements on `;` (string literals, quoted identifiers,
and `--`/`/* */` comments are handled) and executed sequentially.

### Failure and recovery

ClickHouse has no transactional DDL, so a migration that fails partway cannot be
rolled back automatically. When statement *N* of a migration fails:

1. nomad records the migration with `success = false` and reports which statement
   failed. The database is now **dirty**: `migrate run` and `migrate revert` refuse to
   do anything until you intervene.
2. Inspect the schema and finish or undo the migration's work by hand.
3. Then either delete the dirty row to re-run the whole migration —
   `DELETE FROM _nomad_migrations WHERE version = <v>` — (writing up scripts with
   `IF NOT EXISTS` / `IF EXISTS` guards makes re-runs painless), or if you completed
   the remaining statements manually, insert a `success = true` row instead.

A failed `migrate revert` leaves the migration marked as applied (the tracking row is
only deleted after every down statement succeeds); fix the schema by hand and retry.

Checksums: modifying an already-applied up file is an error on `run`/`revert`, and is
flagged by `migrate info`. Reverting the tracking row requires ClickHouse ≥ 22.8
(lightweight `DELETE`); on older servers use
`ALTER TABLE _nomad_migrations DELETE WHERE version = <v> SETTINGS mutations_sync = 2`.

## Development

Unit tests run offline: `cargo test`.

Integration tests need a ClickHouse server and are skipped unless
`NOMAD_TEST_URL` is set:

```console
$ docker compose up -d
$ NOMAD_TEST_URL=http://default:nomad@localhost:8123 cargo test --test integration
# Set NOMAD_TEST_CLUSTER as well to test replicated database creation/reset/drop.
```
# nomad

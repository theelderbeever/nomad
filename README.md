# nomad

A thin, sqlx-cli-style migration tool built specifically for ClickHouse.

- Reversible migrations only: every migration is a `<version>_<description>.up.sql` /
  `.down.sql` pair. Single-file migrations are not supported.
- Talks to ClickHouse over the HTTP interface (port 8123/8443).
- Initialize a manifest with `nomad migrate init`; its table engine is inferred from
  the database engine, with optional `--cluster` for Atomic databases.

## Usage

```console
$ nomad migrate add create_events
created migrations/20260803120000_create_events.up.sql
created migrations/20260803120000_create_events.down.sql

$ export NOMAD_CLICKHOUSE_URL=http://default:secret@localhost:8123

$ nomad database create --db myapp # create the database
$ nomad migrate init    --db myapp # create _nomad_migrations (plain MergeTree by default)
$ nomad database setup  --db myapp # create database + run pending (manifest must already exist)
$ nomad migrate run     --db myapp # apply pending migrations
$ nomad migrate info    --db myapp # list applied/pending
$ nomad migrate revert  --db myapp # revert the latest applied migration
$ nomad migrate revert --target-version 0   --db myapp # revert everything
$ nomad database reset -y  --db myapp # drop and re-create; then run migrate init and migrate run
```

`database` can also be written as `db`, e.g. `nomad db create --db myapp`.

Common flags:

- `--engine <replicated|atomic>` — select the database engine on `database create`,
  `setup`, and `reset`; omit it to use the server default
- `--cluster <NAME>` — scope database create/setup/reset/drop or manifest creation
  to `ON CLUSTER`; `migrate init` warns and ignores it for Replicated/Shared databases
- `-v`, `-vv`, `-vvv` / `--verbose` — increase Nomad logging (`debug`, then `trace`;
  further repetitions stay at `trace`); accepted before or after subcommands
- `--source <DIR>` — migration directory; prompts in plain text if omitted, with
  Tab completion for directories and `migrations` as the suggested default.
- `--url <URL>` — server URL; falls back to `NOMAD_CLICKHOUSE_URL`
  and prompts with masked input if missing
- `--database <NAME>` / `--db <NAME>` — prompts in plain text if missing from flags
  and the selected recipe. There is no implicit `default` database.
- `--ask` — prompt for the server URL with input masked by asterisks; ignores environment
  URLs and conflicts with `--url`, e.g. `nomad migrate run --ask --db myapp`
- `--target-version <V>` — run up to / revert down to a version
- `--dry-run` — preview migration actions without executing them (SQL at debug)
- `--force` — disable the drop-size limit on `migrate revert`, `database drop`, and
  `database reset`; `-y` skips confirmation only and does not disable protection

Accepted URLs: `http://`, `https://`, `clickhouse://` (→ http, port 8123),
`clickhouses://` (→ https, port 8443). URLs accept a base server address, optional
credentials and port, and an optional trailing slash. Paths (including database names), query strings, and fragments are rejected.
Pass the database separately, e.g. `nomad migrate run --url http://localhost:8123 --db myapp`,
or export `NOMAD_CLICKHOUSE_URL` and pass `--db myapp`.

`--database-url` and the `DATABASE_URL` fallback are no longer supported. CLI flags
override `NOMAD_CLICKHOUSE_URL`; `--ask` forces the URL prompt.
Connection commands prompt for any missing URL or database name. For noninteractive
use, supply the URL through a flag, recipe, or `NOMAD_CLICKHOUSE_URL`, supply the
database through a flag or recipe, and pass `--source` to commands
that use migrations. `migrate add` only needs the migration directory.

## Recipes

Pass `--recipe <NAME>` / `-r=<NAME>` to any `database` or `migrate` command.
Nomad reads a map of recipe names from `.nomad-recipes.toml`,
`.nomad-recipes.yaml`, or `.nomad-recipes.yml` in the current directory. Keep only
one of these files. Recipes are loaded only when explicitly selected; a missing
file, unknown recipe, or unknown option is an error.

```toml
[analytics]
url = "http://localhost:8123"
database = "analytics"
engine = "replicated"
cluster = '{cluster}'
source = "./clickhouse/migrations/analytics"

[reporting]
name = "reporting"
url = "http://localhost:8123"
database = "reporting"
engine = "replicated"
cluster = '{cluster}'
source = "./clickhouse/migrations/reporting"
```

The equivalent YAML structure is:

```yaml
analytics:
  url: http://localhost:8123
  database: analytics
  engine: replicated
  cluster: '{cluster}'
  source: ./clickhouse/migrations/analytics
```

```console
$ nomad database create -r=analytics
$ nomad migrate init --recipe=analytics
$ nomad migrate run -r=analytics --dry-run
$ nomad migrate add create_events -r=analytics
$ nomad migrate run -r=reporting --database=reporting_test
```

Priority is **recipe < environment < explicit CLI flags**. `NOMAD_CLICKHOUSE_URL`
is the only environment variable for command options; it overrides the recipe URL.
`.env` files are not loaded. Legacy `CLICKHOUSE_URL`, `CLICKHOUSE_DATABASE`, and
other environment-based command options are ignored. Boolean flags accept an explicit
`=false`, such as `--force=false` or `--dry-run=false`, to override an inherited
`true`.

Recipes support `url`, `database`, `source`, `engine`, `cluster`, `ask`, `force`,
`yes`, `target_version`, and `dry_run`. `target-version` and `dry-run` are accepted
aliases. The optional `name` is a label; the map key selects the recipe. Only options
supported by the selected command are used, allowing one recipe to be shared across
commands. Relative source paths resolve from the current directory. `--ask` forces
a URL prompt, while an explicit `--url` overrides an inherited `ask` setting.

Recipe values use the same parsers as CLI flags. Recipes do not create migration
manifests automatically: run `migrate init` explicitly before applying migrations.

## Database provisioning

Database creation uses the server's default engine when `--engine` is omitted.
`--engine=atomic` explicitly selects Atomic; `--engine=replicated` selects Replicated.
The Replicated engine uses configured defaults for its Keeper/ZooKeeper path, shard,
and replica names; Nomad does not expose custom engine arguments.

```console
$ nomad database create --engine=replicated --cluster=production
$ nomad migrate init
$ nomad migrate run
```

This database creation emits:

```sql
CREATE DATABASE IF NOT EXISTS `myapp` ON CLUSTER `production` ENGINE = Replicated;
```

Database replication propagates metadata; `migrate init` inspects the database and
selects a replicated manifest table engine to share migration records. Existing compatible databases produce no
DDL. An incompatible existing engine is an error; Nomad does not convert engines.

`database setup` accepts the same creation flags and requires an existing manifest.
`database reset` checks engine compatibility before dropping anything and uses the
selected engine and cluster for recreation. Repeat `--engine=replicated` and `--cluster`
when resetting a replicated cluster database; reset never creates the manifest or
runs migrations. Custom engine parameters are not preserved by reset: provision
those databases externally.

```console
$ nomad database reset --engine=replicated --cluster=production -y
$ nomad migrate init
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
$ nomad database reset --engine=replicated --cluster=production --force -y
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
Initialization inspects `system.databases` to select the manifest engine:

| Database engine | Manifest engine | Creation scope |
| --- | --- | --- |
| Atomic / Ordinary | `MergeTree` | Connected node, or `ON CLUSTER` with `--cluster` |
| Replicated | `ReplicatedMergeTree()` | Database-managed DDL; `--cluster` is ignored with a warning |
| Shared | `SharedMergeTree()` | Shared catalog; `--cluster` is ignored with a warning |

Unknown database engines cause an error before any manifest DDL. Initialization is
idempotent with a compatible existing manifest and preserves migration history. An
incompatible manifest is an error; Nomad never converts its engine. If the manifest
already exists on the connected node, initialization submits no DDL, even with
`--cluster`; it does not provision missing tables on other nodes.

Only `migrate init` creates the manifest. `database setup` creates the database if
needed and applies migrations using an existing manifest. `database reset` drops
and recreates the database without initializing the manifest or applying migrations.
After reset, run `migrate init`, then `migrate run`.

```console
$ nomad database create --engine=replicated --cluster=production
$ nomad migrate init --cluster=production # warns; the Replicated database propagates DDL
$ nomad migrate run
```

Replicated manifests use the server's `default_replica_path` and `default_replica_name`.
Self-managed deployments need Keeper/ZooKeeper and suitable replica configuration.
For Atomic databases, `--cluster` requires the target database on every cluster node.
Each node's MergeTree manifest remains independent; creating it on a cluster does
not replicate migration records. Migration SQL is executed as written: Nomad does
not inject `ON CLUSTER` or alter table/database engines.

`--replicated` is no longer accepted on database or migration commands. Replace
recipe `replicated = true` with `engine = "replicated"` for database provisioning;
manifest replication is inferred from the actual database, independently of recipe
`engine`. The same recipe `cluster` remains available to database commands and
`migrate init`.

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
$ nomad -vvv migrate init # trace (maximum level)
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

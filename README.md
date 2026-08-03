# nomad

A thin, sqlx-cli-style migration tool built specifically for ClickHouse.

- Reversible migrations only: every migration is a `<version>_<description>.up.sql` /
  `.down.sql` pair. Single-file migrations are not supported.
- Talks to ClickHouse over the HTTP interface (port 8123/8443).
- Single-node by design. Your migration SQL may use `ON CLUSTER` clauses, but the
  tool itself does not manage clusters, and its tracking table is a plain MergeTree.

## Usage

```console
$ nomad migrate add create_events
created migrations/20260803120000_create_events.up.sql
created migrations/20260803120000_create_events.down.sql

$ nomad database setup            # CREATE DATABASE + run all pending
$ nomad migrate run               # apply pending migrations
$ nomad migrate info              # list applied/pending
$ nomad migrate revert            # revert the latest applied migration
$ nomad migrate revert --target-version 0   # revert everything
$ nomad database reset -y         # drop, re-create, re-run everything
```

Common flags:

- `--source <DIR>` — migration directory (default `migrations`)
- `--database-url <URL>` — falls back to `CLICKHOUSE_URL`, then `DATABASE_URL`
  (a `.env` file is loaded if present)
- `--target-version <V>` — run up to / revert down to a version
- `--dry-run` — print the statements that would run without executing them

Accepted URLs: `http://`, `https://`, `clickhouse://` (→ http, port 8123),
`clickhouses://` (→ https, port 8443). The first path segment is the database name:

```
CLICKHOUSE_URL=http://default:secret@localhost:8123/myapp
```

## How it works

State lives in a `_nomad_migrations` table (`MergeTree ORDER BY version`) inside the target
database, holding the version, description, timestamp, success flag, raw SHA-384
checksum of the up file, and execution time of every applied migration.

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
```
# nomad

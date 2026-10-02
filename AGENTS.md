`nomad` is a clickhouse migration tracking tool meant to mimic sqlx-cli.

- Parse don't validate: Use newtypes to guarantee a valid domain at construction rather than validating criteria in code.
- Be loud: communicate to the user during migrations being run via logging. A user should know exactly what was changed or not changed after running a command.
- Safe by design: Ensure the cli prevents users from accidental misconfiguration whenever possible. Example: _nomad_migrations must be created and exist before running migrations. It is not auto created which ensures the user has selected the correct type of replication.
- After changes make sure and run `prek` to lint and format code.

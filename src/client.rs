use anyhow::{Context, Result};
use clickhouse::Client;

use crate::config::ConnectOptions;

/// Restrict destructive commands unless the caller explicitly opts into data loss.
#[derive(Debug, Clone, Copy)]
pub enum DropPolicy {
    Protected,
    Forced,
}

impl DropPolicy {
    pub fn is_protected(self) -> bool {
        matches!(self, Self::Protected)
    }

    pub fn client(self, client: Client) -> Client {
        let limit = match self {
            Self::Protected => "1",
            Self::Forced => "0",
        };
        tracing::debug!(
            max_table_size_to_drop = limit,
            "setting drop-size protection"
        );
        client.with_setting("max_table_size_to_drop", limit)
    }
}

/// Client scoped to the target database, for running migrations.
pub fn connect(opts: &ConnectOptions) -> Client {
    base_client(opts)
        .with_database(&opts.database)
        // Surface mid-stream failures on long statements as real errors
        // instead of an HTTP 200 that dies partway through.
        .with_setting("wait_end_of_query", "1")
}

/// Client with no database selected, for CREATE/DROP DATABASE when the
/// target database may not exist yet.
pub fn connect_without_db(opts: &ConnectOptions) -> Client {
    base_client(opts)
}

fn base_client(opts: &ConnectOptions) -> Client {
    let mut client = Client::default().with_url(&opts.endpoint);
    if let Some(user) = &opts.user {
        client = client.with_user(user);
    }
    if let Some(password) = &opts.password {
        client = client.with_password(password);
    }
    client
}

/// Quote a ClickHouse identifier, preserving literal backslashes and backticks.
pub fn quote_identifier(name: &str) -> String {
    format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
}

/// The clickhouse crate treats `?` in query text as a bind placeholder and
/// `??` as an escaped literal `?`. Migration SQL is never bound, so escape
/// every `?` before sending user-authored statements.
pub fn escape_placeholders(stmt: &str) -> String {
    stmt.replace('?', "??")
}

pub async fn execute(client: &Client, stmt: &str) -> Result<()> {
    tracing::debug!("executing SQL:\n{stmt};");
    client
        .query(&escape_placeholders(stmt))
        .execute()
        .await
        .context("failed to execute statement")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_question_marks() {
        assert_eq!(escape_placeholders("SELECT 'why?'"), "SELECT 'why??'");
        assert_eq!(escape_placeholders("SELECT 1"), "SELECT 1");
    }
}

use anyhow::{Context, Result, bail, ensure};
use url::Url;

/// A nonempty cluster name, parsed before any database operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterName(String);

impl std::str::FromStr for ClusterName {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        ensure!(!raw.trim().is_empty(), "--cluster must not be empty");
        ensure!(
            !raw.contains('\0'),
            "--cluster must not contain a NUL character"
        );
        Ok(Self(raw.to_owned()))
    }
}

impl ClusterName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectOptions {
    /// scheme + host + port only, e.g. "http://localhost:8123"
    pub endpoint: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: String,
}

/// Resolve the connection URL: --database-url flag > CLICKHOUSE_URL > DATABASE_URL.
pub fn resolve_url(flag: Option<String>) -> Result<String> {
    resolve_url_from(
        flag,
        std::env::var("CLICKHOUSE_URL").ok(),
        std::env::var("DATABASE_URL").ok(),
    )
}

fn resolve_url_from(
    flag: Option<String>,
    clickhouse_url: Option<String>,
    database_url: Option<String>,
) -> Result<String> {
    flag.or(clickhouse_url).or(database_url).context(
        "no database URL provided; pass --database-url or set CLICKHOUSE_URL or DATABASE_URL",
    )
}

pub fn parse_url(raw: &str) -> Result<ConnectOptions> {
    let url = Url::parse(raw).with_context(|| format!("invalid database URL {raw:?}"))?;

    let (scheme, default_port) = match url.scheme() {
        "http" | "clickhouse" => ("http", 8123),
        "https" | "clickhouses" => ("https", 8443),
        other => bail!(
            "unsupported URL scheme {other:?} (expected http, https, clickhouse, or clickhouses)"
        ),
    };

    let host = url.host_str().context("database URL has no host")?;
    let port = url.port().unwrap_or(default_port);

    let user = match url.username() {
        "" => None,
        u => Some(percent_decode(u)?),
    };
    let password = url.password().map(percent_decode).transpose()?;

    let database = url
        .path_segments()
        .and_then(|mut segments| segments.next())
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .transpose()?
        .unwrap_or_else(|| "default".to_string());

    for (key, _) in url.query_pairs() {
        tracing::warn!("ignoring unsupported URL query parameter {key:?}");
    }

    Ok(ConnectOptions {
        endpoint: format!("{scheme}://{host}:{port}"),
        user,
        password,
        database,
    })
}

/// The url crate keeps userinfo and path segments percent-encoded; decode them.
fn percent_decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let (hi, lo) = match bytes.get(i + 1).zip(bytes.get(i + 2)) {
                Some((&hi, &lo)) => ((hi as char).to_digit(16), (lo as char).to_digit(16)),
                None => (None, None),
            };
            match hi.zip(lo) {
                Some((hi, lo)) => {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                }
                None => bail!("invalid percent-encoding in URL component {s:?}"),
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).with_context(|| format!("URL component {s:?} is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_flag_wins() {
        let got = resolve_url_from(Some("a".into()), Some("b".into()), Some("c".into())).unwrap();
        assert_eq!(got, "a");
    }

    #[test]
    fn precedence_clickhouse_url_beats_database_url() {
        let got = resolve_url_from(None, Some("b".into()), Some("c".into())).unwrap();
        assert_eq!(got, "b");
    }

    #[test]
    fn precedence_database_url_fallback() {
        let got = resolve_url_from(None, None, Some("c".into())).unwrap();
        assert_eq!(got, "c");
    }

    #[test]
    fn precedence_nothing_errors() {
        assert!(resolve_url_from(None, None, None).is_err());
    }

    #[test]
    fn parses_http_with_default_port() {
        let opts = parse_url("http://localhost/mydb").unwrap();
        assert_eq!(opts.endpoint, "http://localhost:8123");
        assert_eq!(opts.database, "mydb");
        assert_eq!(opts.user, None);
        assert_eq!(opts.password, None);
    }

    #[test]
    fn parses_https_default_port() {
        let opts = parse_url("https://ch.example.com/prod").unwrap();
        assert_eq!(opts.endpoint, "https://ch.example.com:8443");
    }

    #[test]
    fn maps_clickhouse_scheme_to_http() {
        let opts = parse_url("clickhouse://localhost/mydb").unwrap();
        assert_eq!(opts.endpoint, "http://localhost:8123");
    }

    #[test]
    fn maps_clickhouses_scheme_to_https() {
        let opts = parse_url("clickhouses://ch.example.com:9443/mydb").unwrap();
        assert_eq!(opts.endpoint, "https://ch.example.com:9443");
    }

    #[test]
    fn explicit_port_wins() {
        let opts = parse_url("http://localhost:9999/mydb").unwrap();
        assert_eq!(opts.endpoint, "http://localhost:9999");
    }

    #[test]
    fn empty_path_means_default_database() {
        assert_eq!(
            parse_url("http://localhost:8123").unwrap().database,
            "default"
        );
        assert_eq!(
            parse_url("http://localhost:8123/").unwrap().database,
            "default"
        );
    }

    #[test]
    fn decodes_userinfo() {
        let opts = parse_url("http://user%40corp:p%40ss@localhost/mydb").unwrap();
        assert_eq!(opts.user.as_deref(), Some("user@corp"));
        assert_eq!(opts.password.as_deref(), Some("p@ss"));
    }

    #[test]
    fn rejects_unknown_scheme() {
        assert!(parse_url("postgres://localhost/mydb").is_err());
    }
}

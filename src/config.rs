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

/// An explicitly selected, nonempty database name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseName(String);

impl std::str::FromStr for DatabaseName {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        ensure!(!raw.trim().is_empty(), "--database must not be empty");
        ensure!(
            !raw.contains('\0'),
            "--database must not contain a NUL character"
        );
        Ok(Self(raw.to_owned()))
    }
}

impl DatabaseName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A nonempty migration directory path; it may be created by `migrate add`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationDirectory(std::path::PathBuf);

impl std::str::FromStr for MigrationDirectory {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        ensure!(!raw.trim().is_empty(), "--source must not be empty");
        ensure!(
            !raw.contains('\0'),
            "--source must not contain a NUL character"
        );
        Ok(Self(raw.into()))
    }
}

impl MigrationDirectory {
    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

/// A server URL that cannot select a database or carry query settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrl {
    endpoint: Url,
    user: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectOptions {
    pub endpoint: Url,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: DatabaseName,
}

impl ConnectOptions {
    pub fn new(url: ServerUrl, database: DatabaseName) -> Self {
        Self {
            endpoint: url.endpoint,
            user: url.user,
            password: url.password,
            database,
        }
    }
}

/// Resolve the server URL: --url flag > CLICKHOUSE_URL.
pub fn resolve_url(flag: Option<ServerUrl>) -> Result<Option<ServerUrl>> {
    resolve_url_from(flag, std::env::var("CLICKHOUSE_URL").ok())
}

fn resolve_url_from(flag: Option<ServerUrl>, env: Option<String>) -> Result<Option<ServerUrl>> {
    if let Some(url) = flag {
        return Ok(Some(url));
    }
    env.map(|raw| raw.parse()).transpose()
}

impl std::str::FromStr for ServerUrl {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        let url = Url::parse(raw).context("invalid server URL")?;
        let (scheme, default_port) = match url.scheme() {
            "http" | "clickhouse" => ("http", 8123),
            "https" | "clickhouses" => ("https", 8443),
            _ => bail!("unsupported URL scheme (expected http, https, clickhouse, or clickhouses)"),
        };
        let host = url.host_str().context("server URL has no host")?;
        // Inspect the supplied path too: URL parsing normalizes /db/.. to /.
        let authority = raw
            .split_once("://")
            .context("server URL must include a scheme and host")?
            .1;
        let path = authority
            .find(['/', '?', '#', '\\'])
            .map(|start| &authority[start..])
            .unwrap_or("");
        let path = path.split(['?', '#']).next().unwrap_or("");
        ensure!(
            (path.is_empty() || path == "/") && (url.path().is_empty() || url.path() == "/"),
            "server URL must not contain a path; select the database with --database/--db or CLICKHOUSE_DATABASE"
        );
        ensure!(
            url.query().is_none(),
            "server URL must not contain query parameters; select the database with --database/--db or CLICKHOUSE_DATABASE"
        );
        ensure!(
            url.fragment().is_none(),
            "server URL must not contain a fragment"
        );
        let port = url.port().unwrap_or(default_port);
        let user = match url.username() {
            "" => None,
            u => Some(percent_decode(u)?),
        };
        let password = url.password().map(percent_decode).transpose()?;
        Ok(Self {
            endpoint: Url::parse(&format!("{scheme}://{host}:{port}"))
                .context("could not construct server endpoint")?,
            user,
            password,
        })
    }
}

/// The url crate keeps userinfo percent-encoded; decode it.
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
                None => bail!("invalid percent-encoding in URL component"),
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).context("URL component is not valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_precedence_and_missing_url() {
        let flag = "http://flag";
        let env = "http://env";
        assert_eq!(
            resolve_url_from(Some(flag.parse().unwrap()), Some(env.into())).unwrap(),
            Some(flag.parse().unwrap())
        );
        assert_eq!(
            resolve_url_from(None, Some(env.into())).unwrap(),
            Some(env.parse().unwrap())
        );
        assert_eq!(resolve_url_from(None, None).unwrap(), None);
        assert!(resolve_url_from(None, Some("invalid".into())).is_err());
    }

    #[test]
    fn parses_base_urls() {
        for (raw, endpoint) in [
            ("http://localhost", "http://localhost:8123/"),
            ("http://localhost/", "http://localhost:8123/"),
            ("https://ch.example.com", "https://ch.example.com:8443/"),
            ("clickhouse://localhost", "http://localhost:8123/"),
            (
                "clickhouses://ch.example.com:9443",
                "https://ch.example.com:9443/",
            ),
            ("http://localhost:9999", "http://localhost:9999/"),
            ("http://[::1]:8123/", "http://[::1]:8123/"),
        ] {
            assert_eq!(
                raw.parse::<ServerUrl>().unwrap().endpoint.as_str(),
                endpoint
            );
        }
    }

    #[test]
    fn rejects_paths_and_url_settings() {
        for raw in [
            "http://localhost/mydb",
            "http://localhost/mydb/extra",
            "http://localhost/mydb/..",
            "http://localhost/%2e",
            "http://localhost/./",
            "http://localhost\\mydb",
            "http://localhost/%6dydb",
            "http://localhost//",
            "clickhouse://localhost/mydb",
            "https://localhost/mydb",
            "http://localhost?database=mydb",
            "http://localhost?",
            "http://localhost/#mydb",
            "http://localhost/#",
            "postgres://localhost",
            "http://",
            "localhost",
        ] {
            assert!(raw.parse::<ServerUrl>().is_err(), "{raw}");
        }
    }

    #[test]
    fn database_names_are_explicit_and_preserved() {
        for raw in ["", "  ", "db\0name"] {
            assert!(raw.parse::<DatabaseName>().is_err());
        }
        for raw in ["default", "my app", "db`name", "percent%20name"] {
            assert_eq!(raw.parse::<DatabaseName>().unwrap().as_str(), raw);
        }
    }

    #[test]
    fn decodes_userinfo() {
        let opts = "http://user%40corp:p%40ss@localhost"
            .parse::<ServerUrl>()
            .unwrap();
        assert_eq!(opts.user.as_deref(), Some("user@corp"));
        assert_eq!(opts.password.as_deref(), Some("p@ss"));
    }

    #[test]
    fn parsing_errors_do_not_disclose_credentials() {
        for raw in [
            "http://user:secret@localhost:invalid",
            "secret://localhost",
            "http://user:secret%ZZ@localhost",
            "http://user:secret%FF@localhost",
            "http://user:secret@localhost/mydb",
        ] {
            let error = format!("{:#}", raw.parse::<ServerUrl>().unwrap_err());
            assert!(!error.contains("secret"), "{error}");
            assert!(!error.contains(raw), "{error}");
        }
    }
}

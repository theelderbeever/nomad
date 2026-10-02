use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub struct Migration {
    pub version: i64,
    pub description: String,
    pub up_path: PathBuf,
    pub down_path: PathBuf,
    pub up_sql: String,
    pub down_sql: String,
}

/// Parse `<version>_<description>.(up|down).sql`; returns (version, description, is_up).
fn parse_filename(name: &str) -> Option<(i64, String, bool)> {
    let stem = name.strip_suffix(".sql")?;
    let (stem, is_up) = match stem.strip_suffix(".up") {
        Some(s) => (s, true),
        None => (stem.strip_suffix(".down")?, false),
    };
    let (version, description) = stem.split_once('_')?;
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let version: i64 = version.parse().ok()?;
    Some((version, description.to_string(), is_up))
}

/// Scan a migration directory (non-recursive) and return migrations sorted by version.
/// Every version must have exactly one up and one down file.
pub fn discover(dir: &Path) -> Result<Vec<Migration>> {
    let entries = fs::read_dir(dir)
        .with_context(|| format!("cannot read migration directory {}", dir.display()))?;

    struct Pair {
        description: String,
        up: Option<PathBuf>,
        down: Option<PathBuf>,
    }
    let mut pairs: BTreeMap<i64, Pair> = BTreeMap::new();

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            tracing::warn!("skipping non-UTF-8 filename in {}", dir.display());
            continue;
        };
        let Some((version, description, is_up)) = parse_filename(name) else {
            tracing::warn!(
                "skipping {name:?} (not a <version>_<description>.{{up,down}}.sql file)"
            );
            continue;
        };

        let pair = pairs.entry(version).or_insert_with(|| Pair {
            description: description.clone(),
            up: None,
            down: None,
        });
        let slot = if is_up { &mut pair.up } else { &mut pair.down };
        if let Some(existing) = slot {
            bail!(
                "duplicate migration version {version}: {} and {name}",
                existing.file_name().unwrap().to_string_lossy()
            );
        }
        *slot = Some(entry.path());
        // Prefer the up file's description if the two ever differ.
        if is_up {
            pair.description = description;
        }
    }

    let mut migrations = Vec::with_capacity(pairs.len());
    for (version, pair) in pairs {
        let (up_path, down_path) = match (pair.up, pair.down) {
            (Some(up), Some(down)) => (up, down),
            (Some(_), None) => {
                bail!("migration {version} has an up file but no matching .down.sql")
            }
            (None, Some(_)) => bail!("migration {version} has a down file but no matching .up.sql"),
            (None, None) => unreachable!(),
        };
        let up_sql = fs::read_to_string(&up_path)
            .with_context(|| format!("cannot read {}", up_path.display()))?;
        let down_sql = fs::read_to_string(&down_path)
            .with_context(|| format!("cannot read {}", down_path.display()))?;
        migrations.push(Migration {
            version,
            description: pair.description.replace('_', " "),
            up_path,
            down_path,
            up_sql,
            down_sql,
        });
    }
    Ok(migrations)
}

/// Create a new `<timestamp>_<slug>.{up,down}.sql` pair. Returns the two paths.
pub fn add_migration(dir: &Path, description: &str) -> Result<(PathBuf, PathBuf)> {
    let version = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let slug = slugify(description);
    if slug.is_empty() {
        bail!("description {description:?} contains no usable characters");
    }

    fs::create_dir_all(dir)
        .with_context(|| format!("cannot create migration directory {}", dir.display()))?;

    let up_path = dir.join(format!("{version}_{slug}.up.sql"));
    let down_path = dir.join(format!("{version}_{slug}.down.sql"));
    for path in [&up_path, &down_path] {
        if path.exists() {
            bail!("refusing to overwrite existing file {}", path.display());
        }
    }
    fs::write(&up_path, "-- Add up migration script here\n")?;
    fs::write(&down_path, "-- Add down migration script here\n")?;
    Ok((up_path, down_path))
}

fn slugify(description: &str) -> String {
    let mut slug = String::with_capacity(description.len());
    let mut last_was_sep = true; // trims leading separators
    for c in description.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            slug.push('_');
            last_was_sep = true;
        }
    }
    slug.trim_end_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nomad_test_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_sorted_pairs() {
        let dir = temp_dir("sorted");
        write(&dir, "20260102000000_second.up.sql", "SELECT 2");
        write(&dir, "20260102000000_second.down.sql", "SELECT -2");
        write(&dir, "20260101000000_first.up.sql", "SELECT 1");
        write(&dir, "20260101000000_first.down.sql", "SELECT -1");

        let migrations = discover(&dir).unwrap();
        assert_eq!(migrations.len(), 2);
        assert_eq!(migrations[0].version, 20260101000000);
        assert_eq!(migrations[0].description, "first");
        assert_eq!(migrations[0].up_sql, "SELECT 1");
        assert_eq!(migrations[1].version, 20260102000000);
        assert_eq!(migrations[1].down_sql, "SELECT -2");
    }

    #[test]
    fn missing_down_is_an_error() {
        let dir = temp_dir("missing_down");
        write(&dir, "1_a.up.sql", "");
        let err = discover(&dir).unwrap_err().to_string();
        assert!(err.contains("no matching .down.sql"), "{err}");
    }

    #[test]
    fn missing_up_is_an_error() {
        let dir = temp_dir("missing_up");
        write(&dir, "1_a.down.sql", "");
        let err = discover(&dir).unwrap_err().to_string();
        assert!(err.contains("no matching .up.sql"), "{err}");
    }

    #[test]
    fn duplicate_version_is_an_error() {
        let dir = temp_dir("dup");
        write(&dir, "1_a.up.sql", "");
        write(&dir, "1_a.down.sql", "");
        write(&dir, "1_b.up.sql", "");
        let err = discover(&dir).unwrap_err().to_string();
        assert!(err.contains("duplicate migration version 1"), "{err}");
    }

    #[test]
    fn ignores_unrelated_files() {
        let dir = temp_dir("unrelated");
        write(&dir, "1_a.up.sql", "");
        write(&dir, "1_a.down.sql", "");
        write(&dir, "README.md", "");
        write(&dir, "notes.sql", "");
        assert_eq!(discover(&dir).unwrap().len(), 1);
    }

    #[test]
    fn parse_filename_grammar() {
        assert_eq!(
            parse_filename("20260101000000_add_users.up.sql"),
            Some((20260101000000, "add_users".into(), true))
        );
        assert_eq!(parse_filename("1_x.down.sql"), Some((1, "x".into(), false)));
        assert_eq!(parse_filename("nope.sql"), None);
        assert_eq!(parse_filename("1_x.sql"), None); // single-file migrations unsupported
        assert_eq!(parse_filename("x_1.up.sql"), None); // non-numeric version
        assert_eq!(parse_filename("_desc.up.sql"), None); // empty version
    }

    #[test]
    fn add_creates_pair_and_refuses_overwrite() {
        let dir = temp_dir("add");
        let (up, down) = add_migration(&dir, "Add Users!").unwrap();
        assert!(
            up.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("_add_users.up.sql")
        );
        assert!(down.exists());
        assert!(
            fs::read_to_string(&up)
                .unwrap()
                .contains("Add up migration")
        );
    }

    #[test]
    fn slugify_cases() {
        assert_eq!(slugify("Add Users!"), "add_users");
        assert_eq!(slugify("  weird--name  "), "weird_name");
        assert_eq!(slugify("!!!"), "");
    }
}

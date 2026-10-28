//! Named command defaults, overlaid before clap constructs domain types.
use std::{collections::BTreeMap, ffi::OsString, path::Path};

use anyhow::{Context, Result, bail, ensure};
use clap::{CommandFactory, FromArgMatches};
use figment::{
    Figment, Provider,
    providers::{Format, Serialized, Toml, Yaml},
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::cli::Cli;

/// Recipe keys are exact names, including dots; they are not configuration paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct RecipeName(String);

impl std::str::FromStr for RecipeName {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        ensure!(!raw.trim().is_empty(), "recipe name must not be empty");
        ensure!(
            !raw.contains('\0'),
            "recipe name must not contain a NUL character"
        );
        Ok(Self(raw.to_owned()))
    }
}

impl TryFrom<String> for RecipeName {
    type Error = anyhow::Error;

    fn try_from(raw: String) -> Result<Self> {
        raw.parse()
    }
}

/// Raw defaults: clap parses these into the same domain types used by explicit flags.
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    database: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cluster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "pass")]
    password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    force: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    yes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "target-version")]
    target_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "dry-run")]
    dry_run: Option<bool>,
}

fn load(directory: &Path, name: &RecipeName) -> Result<Recipe> {
    let files = [
        ".nomad-recipes.toml",
        ".nomad-recipes.yaml",
        ".nomad-recipes.yml",
    ]
    .map(|file| directory.join(file));
    let files: Vec<_> = files.iter().filter(|path| path.exists()).collect();
    ensure!(
        !files.is_empty(),
        "--recipe requires .nomad-recipes.toml, .nomad-recipes.yaml, or .nomad-recipes.yml in the current directory"
    );
    ensure!(
        files.len() == 1,
        "multiple recipe files in the current directory; keep only one of .nomad-recipes.toml, .nomad-recipes.yaml, or .nomad-recipes.yml"
    );
    let path = files[0];
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("could not read recipe file {}", path.display()))?;
    let config = if path.extension().is_some_and(|ext| ext == "toml") {
        Figment::from(Toml::string(&contents))
    } else {
        Figment::from(Yaml::string(&contents))
    };
    let mut recipes: IndexMap<RecipeName, Recipe> = config
        .extract()
        .with_context(|| format!("could not parse recipe file {}", path.display()))?;
    if let Some(recipe) = recipes.swap_remove(name) {
        return Ok(recipe);
    }
    let available = recipes
        .keys()
        .map(|key| key.0.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "recipe {:?} not found in {}; available recipes: {}",
        name.0,
        path.display(),
        available
    )
}

pub fn environment() -> impl Provider {
    let options: BTreeMap<_, _> = [
        ("NOMAD_CLICKHOUSE_URL", "url"),
        ("NOMAD_CLICKHOUSE_HOST", "host"),
        ("NOMAD_CLICKHOUSE_PORT", "port"),
        ("NOMAD_CLICKHOUSE_USER", "user"),
        ("NOMAD_CLICKHOUSE_PASSWORD", "password"),
    ]
    .into_iter()
    .filter_map(|(env, key)| std::env::var(env).ok().map(|value| (key, value)))
    .collect();
    Serialized::defaults(options)
}

pub fn resolve(
    args: impl IntoIterator<Item = OsString>,
    directory: &Path,
    environment: impl Provider,
) -> Result<Cli> {
    let args: Vec<_> = args.into_iter().collect();
    let mut command = Cli::command();
    let matches = command.clone().try_get_matches_from(&args)?;
    let recipe = match matches.get_one::<RecipeName>("recipe") {
        Some(name) => {
            let recipe = load(directory, name)?;
            tracing::info!(recipe = %name.0, "loaded recipe");
            recipe
        }
        None => Recipe::default(),
    };
    let (category, category_matches) = matches.subcommand().expect("required command");
    let (action, action_matches) = category_matches.subcommand().expect("required subcommand");
    let leaf = command
        .find_subcommand_mut(category)
        .unwrap()
        .find_subcommand_mut(action)
        .unwrap();
    let mut config = Figment::from(Serialized::defaults(recipe)).merge(environment);
    for arg in leaf.get_arguments() {
        let id = arg.get_id().as_str();
        if action_matches.value_source(id) != Some(clap::parser::ValueSource::CommandLine) {
            continue;
        }
        if arg.get_long().is_some() && !matches!(id, "recipe" | "verbose" | "help") {
            let values: Vec<_> = action_matches
                .get_raw(id)
                .unwrap()
                .map(|value| value.to_string_lossy().into_owned())
                .collect();
            let raw = if id == "prompt" {
                values.join(",")
            } else {
                values.into_iter().next().unwrap()
            };
            config = config.merge(Serialized::default(id, raw));
        }
    }
    // Only extract defaults used by this command. For example, `migrate add`
    // should not parse connection settings, and `migrate run` never provisions a manifest.
    let ids: Vec<_> = leaf
        .get_arguments()
        .filter(|arg| arg.get_long().is_some())
        .map(|arg| arg.get_id().as_str().to_owned())
        .filter(|id| !matches!(id.as_str(), "recipe" | "verbose" | "help"))
        .collect();
    let mut defaults: BTreeMap<String, figment::value::Value> = config.extract()?;
    if ids.iter().any(|id| id == "prompt")
        && let Some(figment::value::Value::String(_, fields)) = defaults.get("prompt")
    {
        let prompted: Vec<_> = fields
            .split(',')
            .map(|field| {
                <crate::config::PromptField as clap::ValueEnum>::from_str(field, false)
                    .map_err(anyhow::Error::msg)
            })
            .collect::<Result<_>>()?;
        // Prompted fields bypass inherited values before their domain parsers run.
        for field in prompted {
            defaults.remove(field.flag());
        }
    }
    for id in ids {
        if let Some(value) = defaults.get(&id) {
            let default = match value {
                figment::value::Value::String(_, value) => value.clone(),
                figment::value::Value::Bool(_, value) => value.to_string(),
                figment::value::Value::Num(_, value) => value
                    .to_i128()
                    .context("expected an integer recipe option")?
                    .to_string(),
                _ => bail!("invalid value for recipe option {id}"),
            };
            *leaf = leaf.clone().mut_arg(&id, |arg| arg.default_value(default));
        }
    }
    let matches = command.try_get_matches_from(args)?;
    Ok(Cli::from_arg_matches(&matches)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Command, DatabaseCmd, MigrateCmd};

    fn resolve_test(directory: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<Cli> {
        resolve(
            args.iter().map(OsString::from),
            directory,
            Serialized::defaults(env.iter().copied().collect::<BTreeMap<_, _>>()),
        )
    }

    fn fixture(extension: &str, contents: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(format!(".nomad-recipes.{extension}")),
            contents,
        )
        .unwrap();
        directory
    }

    #[test]
    fn toml_and_yaml_support_exact_keys_and_shared_options() {
        for (extension, contents) in [
            (
                "toml",
                "[\"app.prod\"]\nname = 'label'\nurl = 'http://localhost'\ndatabase = 'app'\nengine = 'replicated'\ncluster = '{cluster}'\nsource = './migrations'",
            ),
            (
                "yaml",
                "app.prod:\n  name: label\n  url: http://localhost\n  database: app\n  engine: replicated\n  cluster: '{cluster}'\n  source: ./migrations\n",
            ),
            (
                "yml",
                "app.prod:\n  name: label\n  url: http://localhost\n  database: app\n  engine: replicated\n  cluster: '{cluster}'\n  source: ./migrations\n",
            ),
        ] {
            let directory = fixture(extension, contents);
            for (category, action) in [
                ("database", "create"),
                ("db", "setup"),
                ("database", "reset"),
                ("database", "drop"),
                ("migrate", "init"),
                ("migrate", "run"),
                ("migrate", "revert"),
                ("migrate", "info"),
            ] {
                let cli = resolve_test(
                    directory.path(),
                    &["nomad", category, action, "-r=app.prod"],
                    &[],
                )
                .unwrap();
                let connection = match cli.command {
                    Command::Database(
                        DatabaseCmd::Create { url, definition }
                        | DatabaseCmd::Setup {
                            url, definition, ..
                        }
                        | DatabaseCmd::Reset {
                            url, definition, ..
                        },
                    ) => {
                        assert_eq!(
                            definition.engine,
                            Some(crate::db::DatabaseEngine::Replicated)
                        );
                        assert_eq!(definition.cluster.unwrap().as_str(), "{cluster}");
                        url
                    }
                    Command::Database(DatabaseCmd::Drop { url, cluster, .. }) => {
                        assert_eq!(cluster.unwrap().as_str(), "{cluster}");
                        url
                    }
                    Command::Migrate(MigrateCmd::Init { url, cluster }) => {
                        assert_eq!(cluster.unwrap().as_str(), "{cluster}");
                        url
                    }
                    Command::Migrate(
                        MigrateCmd::Run { url, source, .. }
                        | MigrateCmd::Revert { url, source, .. }
                        | MigrateCmd::Info { url, source },
                    ) => {
                        assert_eq!(source.source.unwrap().as_path(), Path::new("./migrations"));
                        url
                    }
                    _ => unreachable!(),
                };
                assert!(connection.url.is_some());
                assert_eq!(connection.database.unwrap().as_str(), "app");
            }
        }
    }

    #[test]
    fn init_retains_recipe_cluster_and_cli_can_override_it() {
        let directory = fixture(
            "toml",
            "[app]\nengine = 'replicated'\ncluster = 'recipe-cluster'",
        );
        for (extra, expected) in [
            (None, "recipe-cluster"),
            (Some("--cluster=cli-cluster"), "cli-cluster"),
        ] {
            let mut args = vec!["nomad", "migrate", "init", "-r=app"];
            args.extend(extra);
            let cli = resolve_test(directory.path(), &args, &[]).unwrap();
            let Command::Migrate(MigrateCmd::Init { cluster, .. }) = cli.command else {
                unreachable!()
            };
            assert_eq!(cluster.unwrap().as_str(), expected);
        }
    }

    #[test]
    fn recipe_engine_uses_the_cli_enum_parser() {
        let directory = fixture("toml", "[app]\nengine = 'unknown'");
        assert!(
            resolve_test(
                directory.path(),
                &["nomad", "database", "create", "-r=app"],
                &[]
            )
            .is_err()
        );
        let cli = resolve_test(
            directory.path(),
            &["nomad", "database", "create", "-r=app", "--engine=atomic"],
            &[],
        )
        .unwrap();
        let Command::Database(DatabaseCmd::Create { definition, .. }) = cli.command else {
            unreachable!()
        };
        assert_eq!(definition.engine, Some(crate::db::DatabaseEngine::Atomic));
    }

    #[test]
    fn uuid_recipe_defaults_use_the_same_parser_as_cli_flags() {
        let directory = fixture(
            "toml",
            "[app]\nuuid = '314487e9-ca39-4ff1-ae91-e82c9d3f07e3'\ncluster = 'production'",
        );
        let cli =
            resolve_test(directory.path(), &["nomad", "db", "create", "-r=app"], &[]).unwrap();
        let Command::Database(DatabaseCmd::Create { definition, .. }) = cli.command else {
            unreachable!()
        };
        assert_eq!(
            definition.uuid.unwrap().to_string(),
            "314487e9-ca39-4ff1-ae91-e82c9d3f07e3"
        );
        let cli = resolve_test(directory.path(), &["nomad", "db", "info", "-r=app"], &[]).unwrap();
        let Command::Database(DatabaseCmd::Info { cluster, .. }) = cli.command else {
            unreachable!()
        };
        assert_eq!(cluster.unwrap().as_str(), "production");
    }

    #[test]
    fn recipe_flag_is_global_and_add_only_uses_source() {
        let directory = fixture(
            "toml",
            "[app]\nurl = 'invalid'\nsource = 'recipe-migrations'",
        );
        for args in [
            vec!["nomad", "-r", "app", "migrate", "add", "events"],
            vec!["nomad", "migrate", "--recipe=app", "add", "events"],
            vec!["nomad", "migrate", "add", "events", "-r=app"],
        ] {
            let cli = resolve_test(directory.path(), &args, &[]).unwrap();
            let Command::Migrate(MigrateCmd::Add { source, .. }) = cli.command else {
                unreachable!()
            };
            assert_eq!(
                source.source.unwrap().as_path(),
                Path::new("recipe-migrations")
            );
        }
    }

    #[test]
    fn precedence_is_recipe_then_environment_then_explicit_flags() {
        let directory = fixture(
            "toml",
            "[app]\nurl = 'http://recipe'\ndatabase = 'recipe'\nsource = 'recipe'\nengine = 'atomic'\ncluster = 'recipe'",
        );
        let env = [("url", "http://environment")];
        let args = ["nomad", "database", "setup", "-r=app"];
        let cli = resolve_test(directory.path(), &args, &env).unwrap();
        let Command::Database(DatabaseCmd::Setup {
            url,
            source,
            definition,
        }) = cli.command
        else {
            unreachable!()
        };
        assert_eq!(url.database.unwrap().as_str(), "recipe");
        assert_eq!(url.url.unwrap(), "http://environment".parse().unwrap());
        assert_eq!(source.source.unwrap().as_path(), Path::new("recipe"));
        assert_eq!(definition.cluster.unwrap().as_str(), "recipe");
        let args = [
            "nomad",
            "database",
            "setup",
            "-r=app",
            "--url=http://cli",
            "--db=cli",
            "--source=cli",
            "--cluster=cli",
            "--engine=replicated",
        ];
        let cli = resolve_test(directory.path(), &args, &env).unwrap();
        let Command::Database(DatabaseCmd::Setup {
            url,
            source,
            definition,
        }) = cli.command
        else {
            unreachable!()
        };
        assert_eq!(url.database.unwrap().as_str(), "cli");
        assert_eq!(url.url.unwrap(), "http://cli".parse().unwrap());
        assert_eq!(source.source.unwrap().as_path(), Path::new("cli"));
        assert_eq!(definition.cluster.unwrap().as_str(), "cli");
        assert_eq!(
            definition.engine,
            Some(crate::db::DatabaseEngine::Replicated)
        );
    }

    #[test]
    fn lower_priority_invalid_domain_values_can_be_overridden() {
        let directory = fixture(
            "toml",
            "[app]\nurl = 'invalid'\ndatabase = ''\ncluster = ''\nsource = ''",
        );
        assert!(
            resolve_test(
                directory.path(),
                &["nomad", "database", "setup", "-r=app"],
                &[]
            )
            .is_err()
        );
        assert!(
            resolve_test(
                directory.path(),
                &[
                    "nomad",
                    "database",
                    "setup",
                    "-r=app",
                    "--url=http://cli",
                    "--database=cli",
                    "--source=cli",
                    "--cluster=cli"
                ],
                &[("url", "invalid")]
            )
            .is_ok()
        );
    }

    #[test]
    fn prompt_url_bypasses_invalid_recipe_url() {
        let directory = fixture("toml", "[app]\nurl = 'invalid'");
        let cli = resolve_test(
            directory.path(),
            &["nomad", "migrate", "run", "-r=app", "--prompt"],
            &[],
        )
        .unwrap();
        let Command::Migrate(MigrateCmd::Run { url, .. }) = cli.command else {
            unreachable!()
        };
        assert_eq!(url.prompt, [crate::config::PromptField::Url]);
        assert!(url.url.is_none());
    }

    #[test]
    fn recipe_can_set_action_options_and_cli_overrides_target() {
        let directory = fixture(
            "toml",
            "[app]\ntarget_version = 42\ndry_run = true\nforce = true\nyes = true",
        );
        let cli = resolve_test(
            directory.path(),
            &["nomad", "migrate", "revert", "-r=app", "--target-version=7"],
            &[],
        )
        .unwrap();
        let Command::Migrate(MigrateCmd::Revert {
            target_version,
            dry_run,
            force,
            ..
        }) = cli.command
        else {
            unreachable!()
        };
        assert_eq!(target_version, Some(7));
        assert!(dry_run);
        assert!(force.force);
        let cli = resolve_test(
            directory.path(),
            &["nomad", "database", "drop", "-r=app"],
            &[],
        )
        .unwrap();
        let Command::Database(DatabaseCmd::Drop { yes, .. }) = cli.command else {
            unreachable!()
        };
        assert!(yes);
    }

    #[test]
    fn explicit_false_overrides_inherited_boolean_values() {
        let directory = fixture(
            "toml",
            "[app]\nengine = 'replicated'\nforce = true\nyes = true\ndry_run = true",
        );
        let cli = resolve_test(
            directory.path(),
            &[
                "nomad",
                "database",
                "reset",
                "-r=app",
                "--engine=atomic",
                "--force=false",
                "--yes=false",
            ],
            &[],
        )
        .unwrap();
        let Command::Database(DatabaseCmd::Reset {
            definition,
            force,
            yes,
            ..
        }) = cli.command
        else {
            unreachable!()
        };
        assert_eq!(definition.engine, Some(crate::db::DatabaseEngine::Atomic));
        assert!(!force.force);
        assert!(!yes);
        let cli = resolve_test(
            directory.path(),
            &["nomad", "migrate", "run", "-r=app", "--dry-run=false"],
            &[],
        )
        .unwrap();
        let Command::Migrate(MigrateCmd::Run { dry_run, .. }) = cli.command else {
            unreachable!()
        };
        assert!(!dry_run);
    }

    #[test]
    fn prompt_list_overrides_invalid_inherited_component_values() {
        let directory = fixture(
            "toml",
            "[app]\nhost = 'invalid:host'\nport = 0\nuser = 'old'\npassword = 'old'",
        );
        let cli = resolve_test(
            directory.path(),
            &[
                "nomad",
                "migrate",
                "run",
                "-r=app",
                "--prompt=host,port,user,password",
            ],
            &[("port", "invalid")],
        )
        .unwrap();
        let Command::Migrate(MigrateCmd::Run { url, .. }) = cli.command else {
            unreachable!()
        };
        assert_eq!(url.prompt.len(), 4);
        assert!(url.host.is_none());
        assert!(url.port.is_none());
        assert!(url.user.is_none());
        assert!(url.password.is_none());
    }

    #[test]
    fn recipe_selection_is_explicit_and_errors_are_actionable() {
        let directory = tempfile::tempdir().unwrap();
        let args = ["nomad", "migrate", "add", "events", "-r=app"];
        let error = resolve_test(directory.path(), &args, &[])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("--recipe requires"), "{error}");
        let path = directory.path().join(".nomad-recipes.toml");
        std::fs::write(&path, "[other]\nsource = 'migrations'").unwrap();
        let error = resolve_test(directory.path(), &args, &[])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("available recipes: other"), "{error}");
        std::fs::write(&path, "[app]\nsoruce = 'typo'").unwrap();
        assert!(resolve_test(directory.path(), &args, &[]).is_err());
        std::fs::write(&path, "invalid TOML [").unwrap();
        assert!(resolve_test(directory.path(), &args, &[]).is_err());
        assert!(
            resolve_test(
                directory.path(),
                &["nomad", "migrate", "add", "events"],
                &[]
            )
            .is_ok()
        );
        std::fs::write(directory.path().join(".nomad-recipes.yml"), "app: {}").unwrap();
        let error = resolve_test(directory.path(), &args, &[])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("multiple recipe files"), "{error}");
        assert!(resolve_test(directory.path(), &["nomad", "migrate", "run", "-r="], &[]).is_err());
    }
}

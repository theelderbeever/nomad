use std::{fs, path::MAIN_SEPARATOR};

use anyhow::{Context, Result};
use inquire::{Autocomplete, CustomUserError, Text, autocompletion::Replacement};

use crate::config::MigrationDirectory;

pub fn source_directory(source: Option<MigrationDirectory>) -> Result<MigrationDirectory> {
    source_directory_with(source, || {
        Text::new("Migration directory:")
            .with_default("migrations")
            .with_autocomplete(DirectoryCompleter)
            .with_help_message("Tab to complete a directory; Enter to accept")
            .prompt()
            .context("could not read migration directory from prompt; pass --source")
    })
}

fn source_directory_with(
    source: Option<MigrationDirectory>,
    prompt: impl FnOnce() -> Result<String>,
) -> Result<MigrationDirectory> {
    match source {
        Some(source) => Ok(source),
        None => prompt()?.parse(),
    }
}

#[derive(Clone)]
struct DirectoryCompleter;

impl Autocomplete for DirectoryCompleter {
    fn get_suggestions(&mut self, input: &str) -> Result<Vec<String>, CustomUserError> {
        let (parent, prefix) = input
            .rfind(MAIN_SEPARATOR)
            .map(|index| input.split_at(index + MAIN_SEPARATOR.len_utf8()))
            .unwrap_or(("", input));
        let directory = if parent.is_empty() { "." } else { parent };
        // Missing or unreadable directories should still allow manual input.
        let Ok(entries) = fs::read_dir(directory) else {
            return Ok(Vec::new());
        };
        let mut suggestions = Vec::new();
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with(prefix) {
                suggestions.push(format!("{parent}{name}{MAIN_SEPARATOR}"));
            }
        }
        suggestions.sort();
        Ok(suggestions)
    }

    fn get_completion(
        &mut self,
        input: &str,
        highlighted_suggestion: Option<String>,
    ) -> Result<Replacement, CustomUserError> {
        if highlighted_suggestion.is_some() {
            return Ok(highlighted_suggestion);
        }
        let suggestions = self.get_suggestions(input)?;
        let Some(mut prefix) = suggestions.first().cloned() else {
            return Ok(None);
        };
        for suggestion in &suggestions[1..] {
            while !suggestion.starts_with(&prefix) {
                prefix.pop();
            }
        }
        Ok((prefix.len() > input.len()).then_some(prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_uses_flag_or_prompt_and_rejects_empty_paths() {
        let explicit = source_directory_with(Some("custom".parse().unwrap()), || {
            panic!("unexpected source prompt")
        })
        .unwrap();
        assert_eq!(explicit.as_path(), std::path::Path::new("custom"));
        let prompted = source_directory_with(None, || Ok("new directory".into())).unwrap();
        assert_eq!(prompted.as_path(), std::path::Path::new("new directory"));
        for input in ["", "  ", "dir\0name"] {
            assert!(source_directory_with(None, || Ok(input.into())).is_err());
        }
        let error = source_directory_with(None, || anyhow::bail!("canceled")).unwrap_err();
        assert_eq!(error.to_string(), "canceled");
    }

    #[test]
    fn directory_completion_supports_nested_paths_and_common_prefixes() {
        let root = std::env::temp_dir().join(format!("nomad_completion_{}", std::process::id()));
        fs::create_dir_all(root.join("migrations one/nested")).unwrap();
        fs::create_dir_all(root.join("migrations two")).unwrap();
        fs::write(root.join("migrations.sql"), "").unwrap();
        let input = format!("{}/mi", root.display());
        let one = format!("{}/migrations one/", root.display());
        let two = format!("{}/migrations two/", root.display());
        let mut completer = DirectoryCompleter;
        assert_eq!(
            completer.get_suggestions(&input).unwrap(),
            vec![one.clone(), two]
        );
        assert_eq!(
            completer.get_completion(&input, None).unwrap(),
            Some(format!("{}/migrations ", root.display()))
        );
        assert_eq!(
            completer.get_completion(&input, Some(one.clone())).unwrap(),
            Some(one.clone())
        );
        assert_eq!(
            completer.get_completion(&format!("{one}ne"), None).unwrap(),
            Some(format!("{one}nested/"))
        );
        assert!(
            completer
                .get_suggestions(&format!("{one}missing/"))
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }
}

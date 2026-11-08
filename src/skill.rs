use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::CliError;
use crate::fileutil;

/// The skill shipped with the binary and installed by `kanso skill install`.
pub const SKILL: &str = include_str!("../skills/kanso/SKILL.md");

#[derive(Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Created,
    Updated,
    UpToDate,
}

impl InstallOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "installed",
            Self::Updated => "updated",
            Self::UpToDate => "up to date",
        }
    }
}

pub fn default_dir() -> Result<PathBuf, CliError> {
    let home = std::env::var("HOME")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CliError::usage("HOME is not set; pass --dir to choose where to install the skill")
        })?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("opencode")
        .join("skills"))
}

/// Write `<dir>/kanso/SKILL.md`, creating parents, and report what happened.
///
/// An existing file with different content is replaced, because the skill is
/// owned by the binary; the outcome is always printed by the caller, so the
/// replacement is never silent.
pub fn install(dir: &Path) -> Result<(PathBuf, InstallOutcome), CliError> {
    let path = dir.join("kanso").join("SKILL.md");
    let outcome = match fs::read(&path) {
        Ok(existing) if existing == SKILL.as_bytes() => InstallOutcome::UpToDate,
        Ok(_) => InstallOutcome::Updated,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => InstallOutcome::Created,
        Err(error) => {
            return Err(CliError::Io(format!(
                "failed to read {}: {error}",
                path.display()
            )))
        }
    };

    if outcome != InstallOutcome::UpToDate {
        fileutil::write_atomic(&path, SKILL.as_bytes(), 0o644).map_err(|error| {
            CliError::Io(format!("failed to write {}: {error}", path.display()))
        })?;
    }
    Ok((path, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn installs_creates_parents_and_does_not_rewrite_an_identical_file() {
        let dir = TempDir::new();
        let base = dir.path().join("skills");
        let (path, outcome) = install(&base).expect("install");
        assert_eq!(outcome, InstallOutcome::Created);
        assert_eq!(path, base.join("kanso").join("SKILL.md"));
        assert_eq!(fs::read_to_string(&path).expect("read"), SKILL);

        let (_, outcome) = install(&base).expect("reinstall");
        assert_eq!(outcome, InstallOutcome::UpToDate);
    }

    #[test]
    fn replaces_different_content_and_reports_it() {
        let dir = TempDir::new();
        let base = dir.path().join("skills");
        let path = base.join("kanso").join("SKILL.md");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "old skill").expect("write old");

        let (_, outcome) = install(&base).expect("install");
        assert_eq!(outcome, InstallOutcome::Updated);
        assert_eq!(fs::read_to_string(&path).expect("read"), SKILL);
    }

    #[test]
    fn the_embedded_skill_documents_the_loop_and_exit_codes() {
        for phrase in [
            "kanso next --project ALP",
            "kanso claim ALP-42",
            "kanso move ALP-42 in_review",
            "kanso move ALP-42 blocker --reason",
            "answer arrives as a new message",
            "Do not poll for the",
            "Never move a card to `done`",
            "kanso propose",
            "current worktree",
            "has no label named `need_spec`",
            "blocker flow to report what is missing",
            "| `3` |",
        ] {
            assert!(SKILL.contains(phrase), "{phrase}");
        }
    }
}

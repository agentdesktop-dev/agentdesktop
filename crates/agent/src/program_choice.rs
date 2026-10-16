//! The device user's program choice, kept in the daemon's state directory.
//!
//! The choice is stored whether or not the policy honors it
//! (`programSelection.allowUserChoice`), so turning the policy off and on
//! again brings it back.

use std::path::{Path, PathBuf};

use agentdesktop_core::model::ProgramChoice;
use anyhow::Context;

use crate::{reconcile::MANAGED_PROGRAMS, secure_fs};

fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("program-choice.json")
}

/// The stored choice, or `All` when none is stored. A file that cannot be
/// read or parsed is logged and read as `All`, the policy's default.
pub(crate) fn load(state_dir: &Path) -> ProgramChoice {
    let path = path(state_dir);
    let contents = match std::fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ProgramChoice::All;
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read the program choice; managing every configured program");
            return ProgramChoice::All;
        }
    };
    match serde_json::from_slice(&contents) {
        Ok(choice) => choice,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot parse the program choice; managing every configured program");
            ProgramChoice::All
        }
    }
}

/// Stores `choice`, readable only by the daemon's user.
pub(crate) fn save(state_dir: &Path, choice: &ProgramChoice) -> anyhow::Result<()> {
    secure_fs::ensure_private_dir(state_dir)?;
    let mut contents = serde_json::to_vec_pretty(choice).context("encode program choice")?;
    contents.push(b'\n');
    secure_fs::atomic_write(&path(state_dir), &contents, 0o600)
}

/// Refuses a selection naming a program no configuration can manage.
pub(crate) fn validate(choice: &ProgramChoice) -> anyhow::Result<()> {
    if let ProgramChoice::Selected { programs } = choice
        && let Some(unknown) = programs
            .iter()
            .find(|program| !MANAGED_PROGRAMS.contains(&program.as_str()))
    {
        anyhow::bail!(
            "unknown program {unknown:?}; expected one of: {}",
            MANAGED_PROGRAMS.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use agentdesktop_core::model::ProgramChoice;

    use super::{load, save, validate};

    fn state_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "agentdesktop-program-choice-{name}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    #[test]
    fn a_saved_choice_loads_back_and_nothing_stored_is_all() {
        let dir = state_dir("round-trip");
        assert_eq!(load(&dir), ProgramChoice::All);

        let choice = ProgramChoice::Selected {
            programs: ["codex".to_owned()].into(),
        };
        save(&dir, &choice).unwrap();
        assert_eq!(load(&dir), choice);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("program-choice.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_choice_is_all() {
        let dir = state_dir("garbage");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("program-choice.json"), b"{not json").unwrap();
        assert_eq!(load(&dir), ProgramChoice::All);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_selection_names_only_managed_programs() {
        assert!(validate(&ProgramChoice::None).is_ok());
        let known = ProgramChoice::Selected {
            programs: ["claude-code".to_owned(), "vscode".to_owned()].into(),
        };
        assert!(validate(&known).is_ok());
        let cursor = ProgramChoice::Selected {
            programs: ["cursor".to_owned()].into(),
        };
        assert!(
            validate(&cursor)
                .unwrap_err()
                .to_string()
                .contains("unknown program \"cursor\"")
        );
    }
}

use crate::{
    config::PlayLock,
    files::{ensure_safe_path, valid_hash},
};
use anyhow::{Context, Result, ensure};
use std::{
    path::Path,
    process::{Child, Command},
};
use sysinfo::{ProcessesToUpdate, System};

pub struct ProcessMonitor {
    system: System,
    #[cfg(test)]
    fake_running: Option<bool>,
}

impl Default for ProcessMonitor {
    fn default() -> Self {
        Self {
            system: System::new(),
            #[cfg(test)]
            fake_running: None,
        }
    }
}

impl ProcessMonitor {
    #[cfg(test)]
    pub(crate) fn for_test(running: bool) -> Self {
        Self {
            fake_running: Some(running),
            ..Self::default()
        }
    }

    pub fn game_running(&mut self) -> bool {
        self.named_game_running("dusklight.exe")
    }

    pub fn named_game_running(&mut self, executable: &str) -> bool {
        #[cfg(test)]
        if let Some(running) = self.fake_running {
            return running;
        }
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        self.system
            .processes()
            .values()
            .any(|p| p.name().to_string_lossy().eq_ignore_ascii_case(executable))
    }
}

pub fn launch(path: &Path) -> Result<Child> {
    launch_game(path, crate::protocol::GameId::Dusklight)
}

pub fn launch_game(path: &Path, game: crate::protocol::GameId) -> Result<Child> {
    let executable = match game {
        crate::protocol::GameId::Dusklight => "dusklight.exe",
        crate::protocol::GameId::Totk => "totk.exe",
    };
    let context = match game {
        crate::protocol::GameId::Dusklight => "Launching Dusklight",
        crate::protocol::GameId::Totk => "Launching Tears of the Kingdom",
    };
    ensure_safe_path(path)?;
    ensure!(
        path.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(executable)),
        "Select the matching game executable"
    );
    ensure!(path.is_file(), "Game executable was not found");
    Command::new(path)
        .current_dir(path.parent().context("Executable has no directory")?)
        .spawn()
        .context(context)
}

pub fn acquire(
    existing: Option<&PlayLock>,
    running: bool,
    owner: &str,
    token: &str,
) -> Result<PlayLock> {
    ensure!(!running, "Dusklight is already running");
    ensure!(
        existing.is_none(),
        "The play lock is already held; reconnect or recover it"
    );
    ensure!(
        valid_hash(owner) && valid_hash(token),
        "Invalid play lock identity"
    );
    Ok(PlayLock {
        owner: owner.to_owned(),
        token: token.to_owned(),
    })
}

pub fn matches(lock: Option<&PlayLock>, owner: &str, token: &str) -> bool {
    lock.is_some_and(|lock| lock.owner == owner && lock.token == token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_do_not_expire_or_allow_a_second_owner() -> Result<()> {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let token = "c".repeat(64);
        let lock = acquire(None, false, &a, &token)?;
        assert!(acquire(Some(&lock), false, &b, &"d".repeat(64)).is_err());
        assert!(acquire(None, true, &a, &token).is_err());
        assert!(matches(Some(&lock), &a, &token));
        assert!(!matches(Some(&lock), &b, &token));
        assert!(!matches(Some(&lock), &a, &"d".repeat(64)));
        Ok(())
    }
}

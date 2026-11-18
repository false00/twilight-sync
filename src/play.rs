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
            // Synthetic fixtures must not depend on unrelated games running on the host.
            fake_running: Some(false),
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
            .filter(|process| {
                process
                    .name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(executable)
            })
            .any(|process| {
                #[cfg(windows)]
                match crate::process_job::is_source_build_process(process.pid().as_u32()) {
                    Ok(build_process) => !build_process,
                    Err(error) => {
                        eprintln!(
                            "Cannot distinguish gameplay from source-build process {}: {error:#}",
                            process.pid()
                        );
                        true
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = process;
                    true
                }
            })
    }
}

pub fn launch(path: &Path) -> Result<Child> {
    launch_game(path, crate::protocol::GameId::Dusklight)
}

pub fn launch_game(path: &Path, game: crate::protocol::GameId) -> Result<Child> {
    let executable = match game {
        crate::protocol::GameId::Dusklight => "dusklight.exe",
        crate::protocol::GameId::Totk => "totk.exe",
        crate::protocol::GameId::Triforce => "Triforce.exe",
    };
    let context = match game {
        crate::protocol::GameId::Dusklight => "Launching Dusklight",
        crate::protocol::GameId::Totk => "Launching Tears of the Kingdom",
        crate::protocol::GameId::Triforce => "Launching Triforce",
    };
    ensure_safe_path(path)?;
    ensure!(
        path.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(executable)),
        "Select the matching game executable"
    );
    ensure!(path.is_file(), "Game executable was not found");
    let mut command = Command::new(path);
    command.current_dir(path.parent().context("Executable has no directory")?);
    if game == crate::protocol::GameId::Triforce {
        command.env("HOOKSHOT_NO_UPDATE_CHECK", "1");
    }
    command.spawn().context(context)
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
    #[cfg(windows)]
    fn build_process_trees_do_not_hide_an_unrelated_running_game() -> Result<()> {
        use std::{
            fs,
            os::windows::process::CommandExt,
            time::{Duration, Instant},
        };
        let temporary = tempfile::tempdir()?;
        let name = format!(
            "twilight-process-{}.exe",
            crate::files::sha256(temporary.path().as_os_str().as_encoded_bytes())
        );
        let executable = temporary.path().join(&name);
        fs::copy(std::env::current_exe()?, &executable)?;
        let ready = temporary.path().join("build-ready");
        let descendant = temporary.path().join("descendant-ready");
        let go = temporary.path().join("go");
        let mut command = Command::new(&executable);
        command
            .args(["--exact", "play::tests::owned_process_fixture", "--ignored"])
            .env("TWILIGHT_PROCESS_READY", &ready)
            .env("TWILIGHT_PROCESS_DESCENDANT", &descendant)
            .env("TWILIGHT_PROCESS_GO", &go)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x08000000);
        let build_job = crate::process_job::Job::for_source_build()?;
        let mut child = command.spawn()?;
        build_job.assign(&child)?;
        fs::write(&go, b"go")?;
        let wait_for = |path: &Path| -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !path.try_exists()? {
                ensure!(
                    Instant::now() < deadline,
                    "Process fixture did not become ready"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        };
        wait_for(&descendant)?;
        assert!(crate::process_job::is_source_build_process(child.id())?);
        let descendant_pid = fs::read_to_string(&descendant)?.parse::<u32>()?;
        assert!(crate::process_job::is_source_build_process(descendant_pid)?);
        let mut monitor = ProcessMonitor {
            fake_running: None,
            ..Default::default()
        };
        assert!(!monitor.named_game_running(&name));
        let unrelated_ready = temporary.path().join("unrelated-ready");
        command
            .env_remove("TWILIGHT_PROCESS_DESCENDANT")
            .env_remove("TWILIGHT_PROCESS_GO")
            .env("TWILIGHT_PROCESS_READY", &unrelated_ready);
        let unrelated_job = crate::process_job::Job::new(None, None)?;
        let mut unrelated = command.spawn()?;
        unrelated_job.assign(&unrelated)?;
        wait_for(&unrelated_ready)?;
        assert!(!crate::process_job::is_source_build_process(
            unrelated.id()
        )?);
        assert!(
            monitor.named_game_running(&name),
            "A genuine matching process was hidden by the build"
        );
        drop(unrelated_job);
        unrelated.wait()?;
        assert!(!monitor.named_game_running(&name));
        drop(build_job);
        child.wait()?;
        Ok(())
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "Internal subprocess fixture for build/game process classification"]
    fn owned_process_fixture() -> Result<()> {
        use std::{
            os::windows::process::CommandExt,
            time::{Duration, Instant},
        };
        let ready =
            std::env::var_os("TWILIGHT_PROCESS_READY").context("Missing fixture ready path")?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut descendant = None;
        if let Some(path) = std::env::var_os("TWILIGHT_PROCESS_DESCENDANT") {
            let go =
                std::env::var_os("TWILIGHT_PROCESS_GO").context("Missing fixture start path")?;
            while !Path::new(&go).try_exists()? {
                ensure!(Instant::now() < deadline, "Fixture start timed out");
                std::thread::sleep(Duration::from_millis(20));
            }
            descendant = Some(
                Command::new(std::env::current_exe()?)
                    .args(["--exact", "play::tests::owned_process_fixture", "--ignored"])
                    .env_remove("TWILIGHT_PROCESS_DESCENDANT")
                    .env("TWILIGHT_PROCESS_READY", path)
                    .creation_flags(0x08000000)
                    .spawn()?,
            );
        }
        crate::files::atomic_write(Path::new(&ready), std::process::id().to_string().as_bytes())?;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Some(mut child) = descendant {
            child.wait()?;
        }
        Ok(())
    }

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

use crate::{
    files::ensure_safe_path, history::History, play::ProcessMonitor, protocol::GameId, totk,
    triforce,
};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Game {
    Totk,
    Triforce,
}

impl Game {
    pub fn id(self) -> GameId {
        match self {
            Self::Totk => GameId::Totk,
            Self::Triforce => GameId::Triforce,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Totk => "TotK",
            Self::Triforce => "Ocarina of Time",
        }
    }

    pub fn executable_name(self) -> &'static str {
        match self {
            Self::Totk => "totk.exe",
            Self::Triforce => "Triforce.exe",
        }
    }

    pub fn profile_name(self) -> &'static str {
        match self {
            Self::Totk => "profile",
            Self::Triforce => "user",
        }
    }

    pub fn executable(self, directory: &Path) -> Result<PathBuf> {
        if self == Self::Triforce {
            let longshot = directory.join("longshot.exe").try_exists()?
                || directory.join("bin").join("longshot.exe").try_exists()?;
            let triforce = directory.join("Triforce.exe").try_exists()?
                || directory.join("bin").join("Triforce.exe").try_exists()?;
            ensure!(
                !(longshot && triforce),
                "Keep Triforce and Longshot in separate installation folders"
            );
            for path in [
                directory.join("bin").join("longshot.exe"),
                directory.join("longshot.exe"),
            ] {
                ensure_safe_path(&path)?;
                if path.try_exists()? {
                    ensure!(
                        path.is_file()
                            && directory.join("image.bin").is_file()
                            && directory.join("romfs").is_dir(),
                        "Select the complete Longshot game folder, not its builder"
                    );
                    return Ok(path);
                }
            }
            let binary = directory.join("bin").join(self.executable_name());
            ensure_safe_path(&binary)?;
            if binary.try_exists()? {
                ensure!(binary.is_file(), "Triforce's game executable is not a file");
                ensure!(
                    directory.join("image.bin").is_file() && directory.join("romfs").is_dir(),
                    "Select the complete Triforce game folder, not its Setup launcher"
                );
                return Ok(binary);
            }
        }
        let executable = directory.join(self.executable_name());
        if self == Self::Triforce && executable.try_exists()? {
            ensure!(
                directory.join("image.bin").is_file() && directory.join("romfs").is_dir(),
                "Select the complete Triforce game folder, not its Setup launcher"
            );
        }
        Ok(executable)
    }

    pub fn installation(self, executable: &Path) -> Result<PathBuf> {
        ensure_safe_path(executable)?;
        ensure!(
            executable.is_file()
                && executable
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case(self.executable_name())
                        || (self == Self::Triforce && name.eq_ignore_ascii_case("longshot.exe"))),
            "Select {} from the native installation",
            self.executable_name()
        );
        let parent = executable
            .parent()
            .context("Game executable has no directory")?;
        if self == Self::Triforce
            && parent
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
        {
            let root = parent
                .parent()
                .context("Triforce bin directory has no installation root")?;
            ensure!(
                root.join("image.bin").is_file() && root.join("romfs").is_dir(),
                "Select the game executable from a complete OOT installation"
            );
            Ok(root.into())
        } else {
            if self == Self::Triforce {
                ensure!(
                    parent.join("image.bin").is_file() && parent.join("romfs").is_dir(),
                    "Select the complete Triforce game, not its Setup launcher"
                );
            }
            Ok(parent.into())
        }
    }

    pub fn running(self, monitor: &mut ProcessMonitor) -> bool {
        monitor.named_game_running(self.executable_name())
            || (self == Self::Triforce && monitor.named_game_running("hookshot.exe"))
            || (self == Self::Triforce && monitor.named_game_running("longshot.exe"))
    }

    pub fn ensure_stopped(self) -> Result<()> {
        ensure!(
            !self.running(&mut ProcessMonitor::default()),
            "Close {} before changing its saves or installation",
            self.name()
        );
        Ok(())
    }

    pub fn account(self, profile: &Path) -> Result<Option<[u8; 16]>> {
        match self {
            Self::Totk => totk::read_account(profile),
            Self::Triforce => Ok(Some(triforce::ACCOUNT.to_le_bytes())),
        }
    }

    pub fn committed_save(self, profile: &Path) -> Result<PathBuf> {
        match self {
            Self::Totk => totk::committed_save(profile),
            Self::Triforce => triforce::committed_save(profile),
        }
    }

    pub fn snapshot(self, profile: &Path) -> Result<Option<totk::Snapshot>> {
        match self {
            Self::Totk => totk::snapshot(profile),
            Self::Triforce => triforce::snapshot(profile),
        }
    }

    pub fn summary(self, bytes: &[u8]) -> Result<totk::Summary> {
        match self {
            Self::Totk => totk::summary(bytes),
            Self::Triforce => triforce::summary(bytes),
        }
    }

    pub fn apply(
        self,
        profile: &Path,
        bytes: &[u8],
        expected: Option<&str>,
    ) -> Result<Option<PathBuf>> {
        totk::apply_native(profile, bytes, expected, self)
    }

    pub fn history(self, root: &Path, username: &str, device: &str) -> Result<History> {
        match self {
            Self::Totk => History::open_totk(root, username, device),
            Self::Triforce => History::open_triforce(root, username, device),
        }
    }
}

use crate::files::{atomic_write, ensure_safe_path, sha256, valid_hash};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const OPEN_ARGUMENT: &str = "--open-dusklight";
pub const OPEN_TOTK_ARGUMENT: &str = "--open-totk";
const SHORTCUT_NAME: &str = "Dusklight (Twilight Sync).lnk";
const RECEIPT_NAME: &str = "dusklight-desktop-shortcut.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u8,
    shortcut: PathBuf,
    manager: PathBuf,
    game: PathBuf,
    sha256: String,
}

#[derive(Deserialize)]
struct LinkDetails {
    target: PathBuf,
    arguments: String,
    working: PathBuf,
    icon: String,
}

pub fn install_dusklight_shortcut(root: &Path, game: &Path) -> Result<PathBuf> {
    ensure!(cfg!(windows), "Desktop shortcuts require Windows");
    let user = directories::UserDirs::new().context("Cannot locate the current user's folders")?;
    let desktop = user
        .desktop_dir()
        .context("Windows did not return a desktop folder")?;
    create(root, desktop, &std::env::current_exe()?, game)
}

pub fn install_totk_shortcut(root: &Path, game: &Path) -> Result<PathBuf> {
    ensure!(cfg!(windows), "Desktop shortcuts require Windows");
    let user = directories::UserDirs::new().context("Cannot locate the current user's folders")?;
    let desktop = user
        .desktop_dir()
        .context("Windows did not return a desktop folder")?;
    create_game(
        root,
        desktop,
        &std::env::current_exe()?,
        game,
        crate::protocol::GameId::Totk,
    )
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    ensure_safe_path(path)?;
    let metadata = path.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "Desktop shortcut or receipt exceeds its file limit"
    );
    let bytes = fs::read(path)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Desktop shortcut file grew while reading"
    );
    Ok(bytes)
}

fn unchanged(path: &Path, expected: Option<&[u8]>) -> Result<bool> {
    ensure_safe_path(path)?;
    match expected {
        Some(bytes) => Ok(path.try_exists()? && read_bounded(path, 64 * 1024)? == bytes),
        None => Ok(!path.try_exists()?),
    }
}

fn same_path(actual: &Path, expected: &Path) -> Result<bool> {
    ensure_safe_path(actual)?;
    ensure_safe_path(expected)?;
    Ok(actual.canonicalize().context("Resolving shortcut path")?
        == expected
            .canonicalize()
            .context("Resolving expected shortcut path")?)
}

fn create(root: &Path, desktop: &Path, manager: &Path, game: &Path) -> Result<PathBuf> {
    create_game(
        root,
        desktop,
        manager,
        game,
        crate::protocol::GameId::Dusklight,
    )
}

fn create_game(
    root: &Path,
    desktop: &Path,
    manager: &Path,
    game: &Path,
    selected: crate::protocol::GameId,
) -> Result<PathBuf> {
    let (shortcut_name, receipt_name, argument) = match selected {
        crate::protocol::GameId::Dusklight => (SHORTCUT_NAME, RECEIPT_NAME, OPEN_ARGUMENT),
        crate::protocol::GameId::Totk => (
            "Tears of the Kingdom (Twilight Sync).lnk",
            "totk-desktop-shortcut.json",
            OPEN_TOTK_ARGUMENT,
        ),
    };
    let destination = desktop.join(shortcut_name);
    let receipt_path = root.join(receipt_name);
    for path in [root, desktop, manager, game, &destination, &receipt_path] {
        ensure_safe_path(path)?;
    }
    ensure!(
        root.is_dir() && desktop.is_dir(),
        "Shortcut folders must already exist"
    );
    ensure!(
        manager.is_file() && game.is_file(),
        "Shortcut executables are missing"
    );
    let old_receipt = if receipt_path.try_exists()? {
        Some(read_bounded(&receipt_path, 16 * 1024)?)
    } else {
        None
    };
    let receipt = old_receipt
        .as_deref()
        .map(serde_json::from_slice::<Receipt>)
        .transpose()?;
    if let Some(receipt) = &receipt {
        ensure!(
            receipt.version == 1 && valid_hash(&receipt.sha256),
            "Invalid desktop shortcut receipt"
        );
    }
    let previous = if destination.try_exists()? {
        let receipt = receipt.as_ref().context(
            "An unmanaged Dusklight desktop shortcut already exists; it was not overwritten",
        )?;
        let bytes = read_bounded(&destination, 64 * 1024)?;
        ensure!(
            receipt.shortcut == destination && receipt.sha256 == sha256(&bytes),
            "The Dusklight desktop shortcut was modified; it was not overwritten"
        );
        if receipt.manager == manager && receipt.game == game {
            return Ok(destination);
        }
        Some(bytes)
    } else {
        None
    };
    let stage = tempfile::Builder::new()
        .prefix(".desktop-shortcut-")
        .tempdir_in(root)?;
    let staged = stage.path().join("Dusklight.lnk");
    let details = write_link_argument(&staged, manager, game, argument)?;
    ensure!(
        same_path(&details.target, manager)?,
        "The desktop shortcut target is not the current manager"
    );
    ensure!(
        details.arguments == argument,
        "The desktop shortcut arguments do not open protected Play controls"
    );
    ensure!(
        same_path(
            &details.working,
            manager.parent().context("Manager has no parent folder")?
        )?,
        "The desktop shortcut working directory does not match the manager"
    );
    let (icon, index) = details
        .icon
        .rsplit_once(',')
        .context("The desktop shortcut icon has no resource index")?;
    let icon = icon
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
        .unwrap_or(icon);
    ensure!(
        index == "0" && same_path(Path::new(icon), game)?,
        "The desktop shortcut icon does not match the installed game"
    );
    let bytes = read_bounded(&staged, 64 * 1024)?;
    ensure!(
        bytes.starts_with(&[0x4c, 0, 0, 0]),
        "Windows did not create a shell link"
    );
    ensure!(
        unchanged(&destination, previous.as_deref())?
            && unchanged(&receipt_path, old_receipt.as_deref())?,
        "Desktop shortcut changed during installation; no files were overwritten"
    );
    let receipt = Receipt {
        version: 1,
        shortcut: destination.clone(),
        manager: manager.into(),
        game: game.into(),
        sha256: sha256(&bytes),
    };
    let receipt_bytes = serde_json::to_vec_pretty(&receipt)?;
    atomic_write(&destination, &bytes)?;
    if let Err(error) = atomic_write(&receipt_path, &receipt_bytes) {
        let rollback = (|| -> Result<()> {
            ensure!(
                unchanged(&destination, Some(&bytes))?,
                "Shortcut changed before rollback; external changes were preserved"
            );
            if let Some(previous) = &previous {
                atomic_write(&destination, previous)?;
            } else {
                fs::remove_file(&destination)?;
            }
            Ok(())
        })();
        if let Err(rollback) = rollback {
            anyhow::bail!(
                "Desktop shortcut receipt failed: {error:#}; rollback needs review: {rollback:#}"
            );
        }
        return Err(error)
            .context("Desktop shortcut receipt failed; the previous shortcut was restored");
    }
    Ok(destination)
}

#[cfg(windows)]
fn write_link_argument(
    destination: &Path,
    manager: &Path,
    game: &Path,
    argument: &str,
) -> Result<LinkDetails> {
    use std::{os::windows::process::CommandExt, process::Command};
    let script = r#"
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$shell = New-Object -ComObject WScript.Shell
$shortcut = $null
try {
    $shortcut = $shell.CreateShortcut($env:TWILIGHT_LINK)
    $shortcut.TargetPath = $env:TWILIGHT_MANAGER
    $shortcut.Arguments = $env:TWILIGHT_OPEN_ARGUMENT
    $shortcut.WorkingDirectory = [IO.Path]::GetDirectoryName($env:TWILIGHT_MANAGER)
    $shortcut.IconLocation = $env:TWILIGHT_GAME + ',0'
    $shortcut.Description = 'Open the game through Twilight Sync protected Play controls'
    $shortcut.Save()
    $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shortcut)
    $shortcut = $shell.CreateShortcut($env:TWILIGHT_LINK)
    @{target=$shortcut.TargetPath; arguments=$shortcut.Arguments; working=$shortcut.WorkingDirectory; icon=$shortcut.IconLocation} | ConvertTo-Json -Compress
} finally {
    if ($null -ne $shortcut) { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shortcut) }
    $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
}
"#;
    let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    let output = Command::new(
        PathBuf::from(windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
    )
    .args(["-NoProfile", "-NonInteractive", "-Command", script])
    .env("TWILIGHT_LINK", destination)
    .env("TWILIGHT_MANAGER", manager)
    .env("TWILIGHT_GAME", game)
    .env("TWILIGHT_OPEN_ARGUMENT", argument)
    .creation_flags(0x08000000)
    .output()?;
    ensure!(
        output.status.success(),
        "Windows desktop shortcut creation failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("Invalid Windows desktop shortcut result")
}

#[cfg(not(windows))]
fn write_link_argument(_: &Path, _: &Path, _: &Path, _: &str) -> Result<LinkDetails> {
    anyhow::bail!("Desktop shortcuts require Windows")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        desktop: PathBuf,
        manager: PathBuf,
        game: PathBuf,
    }

    fn fixture() -> Result<Fixture> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("private");
        let desktop = directory.path().join("redirected desktop");
        let manager_folder = directory.path().join("Manager's app");
        let game_folder = directory.path().join("game v1");
        for path in [&root, &desktop, &manager_folder, &game_folder] {
            fs::create_dir(path)?;
        }
        let manager = manager_folder.join("twilight-sync.exe");
        let game = game_folder.join("dusklight.exe");
        fs::write(&manager, b"manager fixture - never launched")?;
        fs::write(&game, b"game fixture - never launched")?;
        Ok(Fixture {
            _directory: directory,
            root,
            desktop,
            manager,
            game,
        })
    }

    #[test]
    fn both_games_have_independent_managed_shortcuts_and_receipts() -> Result<()> {
        let fixture = fixture()?;
        let dusklight = create(
            &fixture.root,
            &fixture.desktop,
            &fixture.manager,
            &fixture.game,
        )?;
        let game = fixture
            .game
            .parent()
            .context("Missing fixture folder")?
            .join("totk.exe");
        fs::write(&game, b"TotK fixture - never executed")?;
        let totk = create_game(
            &fixture.root,
            &fixture.desktop,
            &fixture.manager,
            &game,
            crate::protocol::GameId::Totk,
        )?;
        assert_ne!(dusklight, totk);
        assert!(fixture.root.join(RECEIPT_NAME).is_file());
        assert!(fixture.root.join("totk-desktop-shortcut.json").is_file());
        assert_eq!(
            create_game(
                &fixture.root,
                &fixture.desktop,
                &fixture.manager,
                &game,
                crate::protocol::GameId::Totk
            )?,
            totk
        );
        Ok(())
    }

    #[test]
    fn managed_desktop_shortcut_is_idempotent_and_tracks_game_updates() -> Result<()> {
        let f = fixture()?;
        let path = create(&f.root, &f.desktop, &f.manager, &f.game)?;
        let original = fs::read(&path)?;
        let receipt: Receipt = serde_json::from_slice(&fs::read(f.root.join(RECEIPT_NAME))?)?;
        assert_eq!(receipt.manager, f.manager);
        assert_eq!(receipt.game, f.game);
        assert_eq!(receipt.sha256, sha256(&original));
        assert_eq!(create(&f.root, &f.desktop, &f.manager, &f.game)?, path);
        assert_eq!(fs::read(&path)?, original);
        let next_folder = f.root.join("game v2");
        fs::create_dir(&next_folder)?;
        let next = next_folder.join("dusklight.exe");
        fs::write(&next, b"next game fixture - never launched")?;
        assert_eq!(create(&f.root, &f.desktop, &f.manager, &next)?, path);
        let receipt: Receipt = serde_json::from_slice(&fs::read(f.root.join(RECEIPT_NAME))?)?;
        assert_eq!(receipt.game, next);
        assert_eq!(receipt.sha256, sha256(&fs::read(path)?));
        Ok(())
    }

    #[test]
    fn shell_link_short_path_aliases_keep_the_same_protected_targets() -> Result<()> {
        use std::{os::windows::process::CommandExt, process::Command};
        #[derive(Deserialize)]
        struct ShortPaths {
            manager: PathBuf,
            game: PathBuf,
        }
        let f = fixture()?;
        let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
        let output = Command::new(
            PathBuf::from(windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
        )
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); $fs=New-Object -ComObject Scripting.FileSystemObject; try { @{manager=$fs.GetFile($env:TWILIGHT_MANAGER).ShortPath; game=$fs.GetFile($env:TWILIGHT_GAME).ShortPath} | ConvertTo-Json -Compress } finally { $null=[Runtime.InteropServices.Marshal]::FinalReleaseComObject($fs) }",
        ])
        .env("TWILIGHT_MANAGER", &f.manager)
        .env("TWILIGHT_GAME", &f.game)
        .creation_flags(0x08000000)
        .output()?;
        ensure!(
            output.status.success(),
            "Cannot resolve fixture short paths"
        );
        let paths: ShortPaths = serde_json::from_slice(&output.stdout)?;
        assert_eq!(paths.manager.canonicalize()?, f.manager.canonicalize()?);
        assert_eq!(paths.game.canonicalize()?, f.game.canonicalize()?);
        let shortcut = create(&f.root, &f.desktop, &paths.manager, &paths.game)?;
        assert!(shortcut.is_file());
        Ok(())
    }

    #[test]
    fn user_modified_shortcut_is_not_overwritten() -> Result<()> {
        let f = fixture()?;
        let path = create(&f.root, &f.desktop, &f.manager, &f.game)?;
        let custom = b"user modified shortcut";
        fs::write(&path, custom)?;
        let error = create(&f.root, &f.desktop, &f.manager, &f.game)
            .err()
            .context("Expected modified-shortcut rejection")?;
        assert!(format!("{error:#}").contains("was modified"));
        assert_eq!(fs::read(path)?, custom);
        Ok(())
    }

    #[test]
    fn a_deleted_managed_shortcut_is_recreated() -> Result<()> {
        let f = fixture()?;
        let path = create(&f.root, &f.desktop, &f.manager, &f.game)?;
        fs::remove_file(&path)?;
        assert_eq!(create(&f.root, &f.desktop, &f.manager, &f.game)?, path);
        let receipt: Receipt = serde_json::from_slice(&fs::read(f.root.join(RECEIPT_NAME))?)?;
        assert_eq!(receipt.sha256, sha256(&fs::read(path)?));
        Ok(())
    }

    #[test]
    fn unrelated_desktop_shortcut_is_not_overwritten() -> Result<()> {
        let f = fixture()?;
        let path = f.desktop.join(SHORTCUT_NAME);
        let original = b"unrelated shortcut";
        fs::write(&path, original)?;
        let error = create(&f.root, &f.desktop, &f.manager, &f.game)
            .err()
            .context("Expected unmanaged-shortcut rejection")?;
        assert!(format!("{error:#}").contains("unmanaged"));
        assert_eq!(fs::read(path)?, original);
        assert!(!f.root.join(RECEIPT_NAME).try_exists()?);
        Ok(())
    }
}

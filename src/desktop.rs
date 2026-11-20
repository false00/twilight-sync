use crate::files::{atomic_write, ensure_safe_path, sha256, valid_hash};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const OPEN_ARGUMENT: &str = "--open-dusklight";
pub const OPEN_TOTK_ARGUMENT: &str = "--open-totk";
pub const OPEN_TRIFORCE_ARGUMENT: &str = "--open-triforce";
const SHORTCUT_NAME: &str = "Twilight Sync.lnk";
const RECEIPT_NAME: &str = "desktop-shortcut.json";
const LEGACY: [(&str, &str); 3] = [
    (
        "Dusklight (Twilight Sync).lnk",
        "dusklight-desktop-shortcut.json",
    ),
    (
        "Tears of the Kingdom (Twilight Sync).lnk",
        "totk-desktop-shortcut.json",
    ),
    (
        "Ocarina of Time (Twilight Sync).lnk",
        "triforce-desktop-shortcut.json",
    ),
];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u8,
    shortcut: PathBuf,
    manager: PathBuf,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyReceipt {
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

pub fn install_shortcut(root: &Path) -> Result<PathBuf> {
    ensure!(cfg!(windows), "Desktop shortcuts require Windows");
    let user = directories::UserDirs::new().context("Cannot locate the current user's folders")?;
    let desktop = user
        .desktop_dir()
        .context("Windows did not return a desktop folder")?;
    create(root, desktop, &std::env::current_exe()?)
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

fn verify_link(details: &LinkDetails, manager: &Path) -> Result<()> {
    ensure!(
        same_path(&details.target, manager)?,
        "The desktop shortcut targets a different app; it was not overwritten"
    );
    ensure!(
        details.arguments.is_empty(),
        "The desktop shortcut has custom arguments; it was not overwritten"
    );
    ensure!(
        same_path(
            &details.working,
            manager.parent().context("Manager has no parent folder")?
        )?,
        "The desktop shortcut has a custom working directory; it was not overwritten"
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
        index == "0" && same_path(Path::new(icon), manager)?,
        "The desktop shortcut has a custom icon; it was not overwritten"
    );
    Ok(())
}

fn create(root: &Path, desktop: &Path, manager: &Path) -> Result<PathBuf> {
    let path = create_master(root, desktop, manager)?;
    retire_legacy(root, desktop)
        .context("The Twilight Sync shortcut is ready, but old game shortcuts need attention")?;
    Ok(path)
}

fn create_master(root: &Path, desktop: &Path, manager: &Path) -> Result<PathBuf> {
    let destination = desktop.join(SHORTCUT_NAME);
    let receipt_path = root.join(RECEIPT_NAME);
    for path in [root, desktop, manager, &destination, &receipt_path] {
        ensure_safe_path(path)?;
    }
    ensure!(
        root.is_dir() && desktop.is_dir(),
        "Shortcut folders must already exist"
    );
    ensure!(manager.is_file(), "The Twilight Sync executable is missing");
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
            receipt.version == 2 && valid_hash(&receipt.sha256) && receipt.shortcut == destination,
            "Invalid Twilight Sync desktop shortcut receipt"
        );
    }
    let previous = if destination.try_exists()? {
        Some(read_bounded(&destination, 64 * 1024)?)
    } else {
        None
    };
    let owned = receipt.as_ref().is_some_and(|receipt| {
        previous
            .as_ref()
            .is_some_and(|bytes| receipt.sha256 == sha256(bytes))
    });
    if owned
        && receipt
            .as_ref()
            .is_some_and(|receipt| receipt.manager == manager)
    {
        return Ok(destination);
    }
    if !owned && let Some(bytes) = &previous {
        ensure!(
            bytes.starts_with(&[0x4c, 0, 0, 0]),
            "An unmanaged or modified desktop shortcut exists; it was not overwritten"
        );
        verify_link(&link_details(&destination, None)?, manager)?;
        ensure!(
            unchanged(&destination, Some(bytes))?
                && unchanged(&receipt_path, old_receipt.as_deref())?,
            "Desktop shortcut changed during verification; no files were overwritten"
        );
        // The installer can recreate the same app link without sharing the private receipt.
        atomic_write(
            &receipt_path,
            &serde_json::to_vec_pretty(&Receipt {
                version: 2,
                shortcut: destination.clone(),
                manager: manager.into(),
                sha256: sha256(bytes),
            })?,
        )?;
        return Ok(destination);
    }
    let stage = tempfile::Builder::new()
        .prefix(".desktop-shortcut-")
        .tempdir_in(root)?;
    let staged = stage.path().join(SHORTCUT_NAME);
    verify_link(&link_details(&staged, Some(manager))?, manager)?;
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
    let receipt = serde_json::to_vec_pretty(&Receipt {
        version: 2,
        shortcut: destination.clone(),
        manager: manager.into(),
        sha256: sha256(&bytes),
    })?;
    atomic_write(&destination, &bytes)?;
    if let Err(error) = atomic_write(&receipt_path, &receipt) {
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

fn retire_legacy(root: &Path, desktop: &Path) -> Result<()> {
    let mut errors = Vec::new();
    for (name, receipt_name) in LEGACY {
        if let Err(error) = retire_legacy_link(&desktop.join(name), &root.join(receipt_name)) {
            errors.push(format!("{name}: {error:#}"));
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(())
}

fn retire_legacy_link(path: &Path, receipt_path: &Path) -> Result<()> {
    if !receipt_path.try_exists()? {
        return Ok(());
    }
    let bytes = read_bounded(receipt_path, 16 * 1024)?;
    let receipt: LegacyReceipt = serde_json::from_slice(&bytes)?;
    ensure!(
        receipt.version == 1
            && valid_hash(&receipt.sha256)
            && receipt.shortcut.is_absolute()
            && receipt.shortcut.file_name() == path.file_name(),
        "Invalid legacy shortcut receipt; no legacy shortcut was removed"
    );
    if receipt.shortcut != path {
        ensure!(
            !path.try_exists()? && !receipt.shortcut.try_exists()?,
            "Legacy shortcut desktop location changed; no legacy shortcut was removed"
        );
    } else if path.try_exists()? {
        let shortcut = read_bounded(path, 64 * 1024)?;
        ensure!(
            sha256(&shortcut) == receipt.sha256,
            "The old shortcut {} was modified and was preserved",
            path.display()
        );
        ensure!(
            unchanged(path, Some(&shortcut))? && unchanged(receipt_path, Some(&bytes))?,
            "Legacy desktop shortcut changed; it was preserved"
        );
        fs::remove_file(path)?;
    }
    ensure!(
        unchanged(receipt_path, Some(&bytes))?,
        "Legacy shortcut receipt changed; it was preserved"
    );
    fs::remove_file(receipt_path)?;
    Ok(())
}

#[cfg(windows)]
fn link_details(destination: &Path, manager: Option<&Path>) -> Result<LinkDetails> {
    use std::{os::windows::process::CommandExt, process::Command};
    let script = r#"
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$shell = New-Object -ComObject WScript.Shell
$shortcut = $null
try {
    $shortcut = $shell.CreateShortcut($env:TWILIGHT_LINK)
    if (-not [string]::IsNullOrEmpty($env:TWILIGHT_MANAGER)) {
        $shortcut.TargetPath = $env:TWILIGHT_MANAGER
        $shortcut.Arguments = ''
        $shortcut.WorkingDirectory = [IO.Path]::GetDirectoryName($env:TWILIGHT_MANAGER)
        $shortcut.IconLocation = $env:TWILIGHT_MANAGER + ',0'
        $shortcut.Description = 'Open Twilight Sync to manage and safely play your games'
        $shortcut.Save()
        $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shortcut)
        $shortcut = $shell.CreateShortcut($env:TWILIGHT_LINK)
    }
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
    .env("TWILIGHT_MANAGER", manager.unwrap_or_else(|| Path::new("")))
    .creation_flags(0x08000000)
    .output()?;
    ensure!(
        output.status.success(),
        "Windows desktop shortcut operation failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("Invalid Windows desktop shortcut result")
}

#[cfg(not(windows))]
fn link_details(_: &Path, _: Option<&Path>) -> Result<LinkDetails> {
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
    }

    fn fixture() -> Result<Fixture> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("private");
        let desktop = directory.path().join("redirected desktop");
        let folder = directory.path().join("Manager's app");
        for path in [&root, &desktop, &folder] {
            fs::create_dir(path)?;
        }
        let manager = folder.join("twilight-sync.exe");
        fs::write(&manager, b"manager fixture - never launched")?;
        Ok(Fixture {
            _directory: directory,
            root,
            desktop,
            manager,
        })
    }

    fn legacy(f: &Fixture, name: &str, receipt_name: &str) -> Result<PathBuf> {
        let path = f.desktop.join(name);
        fs::write(&path, b"receipt-owned legacy link fixture")?;
        atomic_write(
            &f.root.join(receipt_name),
            &serde_json::to_vec(&LegacyReceipt {
                version: 1,
                shortcut: path.clone(),
                manager: f.manager.clone(),
                game: f.root.join("old-game.exe"),
                sha256: sha256(&fs::read(&path)?),
            })?,
        )?;
        Ok(path)
    }

    #[test]
    fn one_app_shortcut_is_idempotent_recreated_and_tracks_app_moves() -> Result<()> {
        let f = fixture()?;
        let path = create(&f.root, &f.desktop, &f.manager)?;
        let bytes = fs::read(&path)?;
        verify_link(&link_details(&path, None)?, &f.manager)?;
        assert_eq!(create(&f.root, &f.desktop, &f.manager)?, path);
        assert_eq!(fs::read(&path)?, bytes);
        fs::remove_file(&path)?;
        create(&f.root, &f.desktop, &f.manager)?;
        let next = f.root.join("updated-manager.exe");
        fs::write(&next, b"updated fixture - never executed")?;
        create(&f.root, &f.desktop, &next)?;
        verify_link(&link_details(&path, None)?, &next)?;
        assert_eq!(fs::read_dir(&f.desktop)?.count(), 1);
        Ok(())
    }

    #[test]
    fn app_shortcut_accepts_windows_short_path_aliases() -> Result<()> {
        use std::{os::windows::process::CommandExt, process::Command};
        let f = fixture()?;
        let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
        let output = Command::new(PathBuf::from(windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"))
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); $fs=New-Object -ComObject Scripting.FileSystemObject; try { $fs.GetFile($env:TWILIGHT_MANAGER).ShortPath | ConvertTo-Json -Compress } finally { $null=[Runtime.InteropServices.Marshal]::FinalReleaseComObject($fs) }"])
            .env("TWILIGHT_MANAGER", &f.manager).creation_flags(0x08000000).output()?;
        ensure!(
            output.status.success(),
            "Cannot resolve the fixture's short path"
        );
        let manager: PathBuf = serde_json::from_slice(&output.stdout)?;
        assert_eq!(manager.canonicalize()?, f.manager.canonicalize()?);
        let path = create(&f.root, &f.desktop, &manager)?;
        verify_link(&link_details(&path, None)?, &f.manager)?;
        Ok(())
    }

    #[test]
    fn installer_created_master_is_adopted_without_rewriting_it() -> Result<()> {
        let f = fixture()?;
        let path = f.desktop.join(SHORTCUT_NAME);
        link_details(&path, Some(&f.manager))?;
        let bytes = fs::read(&path)?;
        create(&f.root, &f.desktop, &f.manager)?;
        assert_eq!(fs::read(&path)?, bytes);
        fs::remove_file(&path)?;
        link_details(&path, Some(&f.manager))?;
        let recreated = fs::read(&path)?;
        create(&f.root, &f.desktop, &f.manager)?;
        assert_eq!(fs::read(path)?, recreated);
        Ok(())
    }

    #[test]
    fn migration_removes_only_unchanged_receipt_owned_game_icons() -> Result<()> {
        let f = fixture()?;
        for (name, receipt) in LEGACY {
            legacy(&f, name, receipt)?;
        }
        let unrelated = f.desktop.join("My own game shortcut.lnk");
        fs::write(&unrelated, b"user shortcut")?;
        create(&f.root, &f.desktop, &f.manager)?;
        for (name, receipt) in LEGACY {
            assert!(!f.desktop.join(name).try_exists()?);
            assert!(!f.root.join(receipt).try_exists()?);
        }
        assert_eq!(fs::read(unrelated)?, b"user shortcut");
        Ok(())
    }

    #[test]
    fn desktop_move_retires_stale_receipts_without_touching_links() -> Result<()> {
        let f = fixture()?;
        let new_desktop = f.root.join("redirected-desktop");
        fs::create_dir(&new_desktop)?;
        let (name, receipt_name) = LEGACY[0];
        let old = legacy(&f, name, receipt_name)?;
        let receipt = f.root.join(receipt_name);
        let current = new_desktop.join(name);
        assert!(retire_legacy_link(&current, &receipt).is_err());
        fs::remove_file(old)?;
        fs::write(&current, b"unrelated shortcut at the new desktop")?;
        assert!(retire_legacy_link(&current, &receipt).is_err());
        assert_eq!(
            fs::read(&current)?,
            b"unrelated shortcut at the new desktop"
        );
        fs::remove_file(&current)?;
        retire_legacy_link(&current, &receipt)?;
        assert!(!receipt.try_exists()?);
        Ok(())
    }

    #[test]
    fn modified_or_unreceipted_legacy_icons_are_preserved() -> Result<()> {
        let f = fixture()?;
        let owned = legacy(&f, LEGACY[0].0, LEGACY[0].1)?;
        fs::write(&owned, b"customized link")?;
        let unmanaged = f.desktop.join(LEGACY[1].0);
        fs::write(&unmanaged, b"unmanaged link")?;
        let unchanged = legacy(&f, LEGACY[2].0, LEGACY[2].1)?;
        let error = create(&f.root, &f.desktop, &f.manager)
            .err()
            .context("Expected customized icon warning")?;
        assert!(format!("{error:#}").contains("was modified and was preserved"));
        assert!(f.desktop.join(SHORTCUT_NAME).is_file());
        assert_eq!(fs::read(owned)?, b"customized link");
        assert_eq!(fs::read(unmanaged)?, b"unmanaged link");
        assert!(!unchanged.try_exists()?);
        Ok(())
    }

    #[test]
    fn unrelated_or_modified_master_is_never_overwritten() -> Result<()> {
        let f = fixture()?;
        let path = f.desktop.join(SHORTCUT_NAME);
        fs::write(&path, b"unmanaged shortcut")?;
        assert!(create(&f.root, &f.desktop, &f.manager).is_err());
        assert_eq!(fs::read(&path)?, b"unmanaged shortcut");
        fs::remove_file(&path)?;
        create(&f.root, &f.desktop, &f.manager)?;
        fs::write(&path, b"customized shortcut")?;
        assert!(create(&f.root, &f.desktop, &f.manager).is_err());
        assert_eq!(fs::read(path)?, b"customized shortcut");
        Ok(())
    }
}

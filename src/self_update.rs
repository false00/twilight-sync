use crate::{
    files::{self, ensure_safe_path, valid_hash},
    install,
};
use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const REPOSITORY: &str = "false00/twilight-sync";
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const BINARY: &str = "twilight-sync-windows-x86_64.exe";
const LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub size: u64,
    pub sha256: String,
    pub notes_url: String,
    pub update_available: bool,
    binary_url: String,
    sums_url: String,
    sums_hash: String,
}

#[derive(Clone, Debug)]
pub struct State {
    pub enabled: bool,
    pub checking: bool,
    pub preparing: bool,
    pub latest: Option<Release>,
    pub checked_at: Option<String>,
    pub error: Option<String>,
    pub message: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            enabled: true, checking: false, preparing: false, latest: None,
            checked_at: None, error: None, message: "Automatic checks run at startup and every six hours. Installation always needs your approval.".into(),
        }
    }
}

impl State {
    pub fn available(&self) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|release| release.update_available)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    version: u8,
    automatic_checks: bool,
}

#[derive(Deserialize)]
struct Metadata {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    html_url: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u8,
    tag: String,
    parent_pid: u32,
    destination: PathBuf,
    old_hash: String,
    new_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    format: u8,
    repository: String,
    version: String,
    architecture: String,
    executable: String,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    status: String,
    helper_pid: u32,
    detail: String,
}

pub struct Prepared {
    plan: PathBuf,
    child: Child,
}

impl Prepared {
    pub fn cancel(mut self) -> Result<()> {
        let stage = self.plan.parent().context("Update has no stage")?;
        files::atomic_write(&stage.join("cancel"), b"cancel")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
        bail!(
            "Update cancellation was requested; helper logs remain at {}",
            stage.display()
        )
    }
}

pub fn load_enabled(root: &Path) -> Result<bool> {
    let path = root.join("update-settings.json");
    ensure_safe_path(&path)?;
    if !path.try_exists()? {
        return Ok(true);
    }
    let preferences: Preferences = serde_json::from_slice(&bounded(&path, 4096)?)?;
    ensure!(preferences.version == 1, "Unsupported update preferences");
    Ok(preferences.automatic_checks)
}

pub fn set_enabled(root: &Path, enabled: bool) -> Result<()> {
    files::atomic_write(
        &root.join("update-settings.json"),
        &serde_json::to_vec(&Preferences {
            version: 1,
            automatic_checks: enabled,
        })?,
    )
}

fn newer_than(candidate: &Version, current: &str) -> Result<bool> {
    Ok(candidate.cmp_precedence(&Version::parse(current)?).is_gt())
}

fn asset<'a>(metadata: &'a Metadata, name: &str, maximum: u64) -> Result<&'a Asset> {
    let matches: Vec<&Asset> = metadata
        .assets
        .iter()
        .filter(|asset| asset.name == name)
        .collect();
    ensure!(
        matches.len() == 1,
        "Release must contain exactly one {name}"
    );
    let asset = matches[0];
    ensure!(
        asset.size > 0 && asset.size <= maximum,
        "Invalid {name} asset size"
    );
    ensure!(
        asset.browser_download_url
            == format!(
                "https://github.com/{REPOSITORY}/releases/download/{}/{name}",
                metadata.tag_name
            ),
        "Update asset is outside the official repository"
    );
    digest(asset)?;
    Ok(asset)
}

fn digest(asset: &Asset) -> Result<String> {
    let hash = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .context("Update asset has no published SHA-256")?;
    ensure!(valid_hash(hash), "Invalid update SHA-256");
    Ok(hash.to_ascii_lowercase())
}

fn parse_metadata(bytes: &[u8]) -> Result<Release> {
    ensure!(bytes.len() <= 128 * 1024, "Update metadata exceeds limit");
    let metadata: Metadata = serde_json::from_slice(bytes)?;
    ensure!(
        !metadata.draft && !metadata.prerelease && metadata.assets.len() <= 32,
        "Only stable published updates are accepted"
    );
    let version = Version::parse(
        metadata
            .tag_name
            .strip_prefix('v')
            .context("Update tag must begin with v")?,
    )?;
    ensure!(
        version.pre.is_empty(),
        "Pre-release updates are not installed automatically"
    );
    let notes_url = format!(
        "https://github.com/{REPOSITORY}/releases/tag/{}",
        metadata.tag_name
    );
    ensure!(
        metadata.html_url == notes_url,
        "Update notes point outside the official repository"
    );
    let binary = asset(&metadata, BINARY, LIMIT)?;
    let sums = asset(&metadata, "SHA256SUMS.txt", 64 * 1024)?;
    Ok(Release {
        tag: metadata.tag_name.clone(),
        update_available: newer_than(&version, env!("CARGO_PKG_VERSION"))?,
        version,
        size: binary.size,
        sha256: digest(binary)?,
        notes_url,
        binary_url: binary.browser_download_url.clone(),
        sums_url: sums.browser_download_url.clone(),
        sums_hash: digest(sums)?,
    })
}

pub fn check() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let response = install::official_client()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()?
        .error_for_status()?;
    ensure!(
        response.url().as_str() == url,
        "Unexpected update metadata redirect"
    );
    let mut bytes = Vec::new();
    response.take(128 * 1024 + 1).read_to_end(&mut bytes)?;
    parse_metadata(&bytes)
}

fn bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    ensure_safe_path(path)?;
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "Invalid update metadata file"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Update file grew while reading"
    );
    Ok(bytes)
}

fn file_hash(path: &Path) -> Result<String> {
    ensure_safe_path(path)?;
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    ensure!(
        file.metadata()?.is_file() && length > 0 && length <= LIMIT,
        "Executable is invalid or oversized"
    );
    let mut hash = Sha256::new();
    let mut total = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= length, "Executable changed while hashing");
        hash.update(&buffer[..count]);
    }
    ensure!(total == length, "Executable was truncated");
    Ok(hex::encode(hash.finalize()))
}

fn checksum(text: &str, filename: &str) -> Result<String> {
    let matches: Vec<&str> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let hash = fields.next()?;
            let name = fields.next()?.trim_start_matches('*');
            (name == filename && fields.next().is_none()).then_some(hash)
        })
        .collect();
    ensure!(
        matches.len() == 1 && valid_hash(matches[0]),
        "Checksum list must have exactly one valid {filename} entry"
    );
    Ok(matches[0].to_ascii_lowercase())
}

fn validate_pe(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut dos = [0; 64];
    file.read_exact(&mut dos)?;
    ensure!(&dos[..2] == b"MZ", "Update is not a Windows executable");
    let offset = u32::from_le_bytes(dos[60..64].try_into()?) as u64;
    ensure!(
        offset >= 64 && offset + 26 <= length,
        "Invalid update PE offset"
    );
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0; 26];
    file.read_exact(&mut header)?;
    ensure!(
        &header[..4] == b"PE\0\0"
            && u16::from_le_bytes(header[4..6].try_into()?) == 0x8664
            && u16::from_le_bytes(header[24..26].try_into()?) == 0x20b,
        "Update is not Windows x64 PE32+"
    );
    Ok(())
}

fn updates_directory(root: &Path) -> Result<PathBuf> {
    let updates = root.join("updates");
    ensure_safe_path(&updates)?;
    fs::create_dir_all(&updates)?;
    Ok(updates)
}

fn stage_for(root: &Path, plan: &Path) -> Result<PathBuf> {
    ensure_safe_path(plan)?;
    let stage = plan.parent().context("Update plan has no folder")?;
    ensure!(
        plan.file_name().is_some_and(|name| name == "plan.json")
            && stage.parent() == Some(updates_directory(root)?.as_path()),
        "Update plan must be in the private updater directory"
    );
    let name = stage
        .file_name()
        .and_then(|name| name.to_str())
        .context("Invalid update folder")?;
    ensure!(
        name.starts_with("stage-")
            && name.len() <= 64
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid update stage name"
    );
    Ok(stage.into())
}

pub fn prepare(root: &Path, release: &Release) -> Result<Prepared> {
    ensure!(
        newer_than(&release.version, env!("CARGO_PKG_VERSION"))?,
        "This version is already current"
    );
    ensure!(
        cfg!(all(windows, target_arch = "x86_64")),
        "Automatic installation is currently Windows x64 only"
    );
    let destination = std::env::current_exe()?;
    ensure_safe_path(&destination)?;
    let stage = tempfile::Builder::new()
        .prefix("stage-")
        .tempdir_in(updates_directory(root)?)?;
    let client = install::official_client()?;
    let mut sums = Vec::new();
    client
        .get(&release.sums_url)
        .timeout(Duration::from_secs(30))
        .send()?
        .error_for_status()?
        .take(64 * 1024 + 1)
        .read_to_end(&mut sums)?;
    ensure!(
        sums.len() <= 64 * 1024 && files::sha256(&sums) == release.sums_hash,
        "Update checksum list failed integrity validation"
    );
    ensure!(
        checksum(std::str::from_utf8(&sums)?, BINARY)? == release.sha256,
        "Published update checksums disagree"
    );
    let binary = stage.path().join("twilight-sync.exe");
    let mut output = File::options().create_new(true).write(true).open(&binary)?;
    let mut response = client.get(&release.binary_url).send()?.error_for_status()?;
    let mut count = 0_u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = response.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        count += length as u64;
        ensure!(count <= release.size, "Update exceeds its declared size");
        output.write_all(&buffer[..length])?;
    }
    output.sync_all()?;
    drop(output);
    ensure!(
        count == release.size && file_hash(&binary)? == release.sha256,
        "Update executable SHA-256 or size mismatch"
    );
    validate_pe(&binary)?;
    let plan = Plan {
        version: 1,
        tag: release.tag.clone(),
        parent_pid: std::process::id(),
        old_hash: file_hash(&destination)?,
        destination,
        new_hash: release.sha256.clone(),
    };
    let plan_path = stage.path().join("plan.json");
    files::atomic_write(&plan_path, &serde_json::to_vec(&plan)?)?;
    let log = File::create(stage.path().join("helper.log"))?;
    let mut child = Command::new(&binary)
        .arg("--apply-update")
        .arg(&plan_path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("Starting the verified update helper")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if stage.path().join("ready").try_exists()? {
            ensure!(
                bounded(&stage.path().join("ready"), 16)? == b"ready",
                "Invalid update readiness acknowledgement"
            );
            let _owned_stage = stage.keep();
            return Ok(Prepared {
                plan: plan_path,
                child,
            });
        }
        if let Some(status) = child.try_wait()? {
            let detail =
                String::from_utf8_lossy(&bounded(&stage.path().join("helper.log"), 16 * 1024)?)
                    .into_owned();
            bail!("Update helper exited before readiness ({status}): {detail}");
        }
        if Instant::now() > deadline {
            files::atomic_write(&stage.path().join("cancel"), b"cancel")?;
            let kept = stage.keep();
            bail!(
                "Update helper did not acknowledge readiness; cancellation requested. Details: {}",
                kept.join("helper.log").display()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn load_plan(root: &Path, path: &Path) -> Result<(PathBuf, Plan)> {
    let stage = stage_for(root, path)?;
    let plan: Plan = serde_json::from_slice(&bounded(path, 16 * 1024)?)?;
    ensure!(
        plan.version == 1 && plan.parent_pid > 0 && plan.parent_pid != std::process::id(),
        "Invalid updater parent"
    );
    let version = Version::parse(plan.tag.strip_prefix('v').context("Invalid update tag")?)?;
    ensure!(
        version.pre.is_empty() && valid_hash(&plan.old_hash) && valid_hash(&plan.new_hash),
        "Invalid update plan digests or version"
    );
    ensure_safe_path(&plan.destination)?;
    ensure!(
        plan.destination
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe")),
        "Update destination must be the original executable"
    );
    Ok((stage, plan))
}

fn replace_files(stage: &Path, plan: &Plan) -> Result<()> {
    let lock_path = plan
        .destination
        .parent()
        .context("Executable has no directory")?
        .join(".install.lock");
    ensure_safe_path(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("Another app installer is running; update was not applied")?;
    let source = stage.join("twilight-sync.exe");
    ensure!(
        file_hash(&source)? == plan.new_hash,
        "Staged update changed after verification"
    );
    validate_pe(&source)?;
    ensure!(
        file_hash(&plan.destination)? == plan.old_hash,
        "Original executable changed; update refused"
    );
    let receipt_path = plan
        .destination
        .parent()
        .context("Executable has no directory")?
        .join(".twilight-sync-install.json");
    let old_receipt = if receipt_path.try_exists()? {
        let bytes = bounded(&receipt_path, 16 * 1024)?;
        let receipt: Receipt = serde_json::from_slice(&bytes)?;
        ensure!(
            receipt.format == 1
                && receipt.repository == REPOSITORY
                && receipt.sha256 == plan.old_hash
                && plan
                    .destination
                    .file_name()
                    .is_some_and(|name| name == receipt.executable.as_str()),
            "Installer receipt differs from the running app"
        );
        Some((bytes, receipt))
    } else {
        None
    };
    let backup = stage.join("previous.exe");
    ensure!(!backup.try_exists()?, "Updater backup path is occupied");
    fs::copy(&plan.destination, &backup)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&backup)?
        .sync_all()
        .context("Flushing the previous app backup")?;
    ensure!(
        file_hash(&backup)? == plan.old_hash && file_hash(&plan.destination)? == plan.old_hash,
        "Original executable changed during backup"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(
        plan.destination.parent().context("No install directory")?,
    )?;
    std::io::copy(&mut File::open(&source)?, &mut temporary)?;
    temporary.as_file().sync_all()?;
    ensure!(
        file_hash(temporary.path())? == plan.new_hash,
        "Copied update differs"
    );
    ensure!(
        file_hash(&plan.destination)? == plan.old_hash,
        "Original executable changed before replacement"
    );
    temporary
        .persist(&plan.destination)
        .map_err(|error| error.error)
        .context("Replacing app; close other instances and retry if the file is in use")?;
    let committed = (|| -> Result<()> {
        ensure!(
            file_hash(&plan.destination)? == plan.new_hash,
            "Installed update verification failed"
        );
        if let Some((_, receipt)) = &old_receipt {
            let next = Receipt {
                format: receipt.format,
                repository: receipt.repository.clone(),
                version: plan.tag.clone(),
                architecture: receipt.architecture.clone(),
                executable: receipt.executable.clone(),
                sha256: plan.new_hash.clone(),
            };
            files::atomic_write(&receipt_path, &serde_json::to_vec_pretty(&next)?)?;
        }
        Ok(())
    })();
    if let Err(error) = committed {
        let restored = (|| -> Result<()> {
            files::atomic_write(&plan.destination, &bounded(&backup, LIMIT)?)?;
            if let Some((bytes, _)) = old_receipt
                && bounded(&receipt_path, 16 * 1024)? != bytes
            {
                files::atomic_write(&receipt_path, &bytes)?;
            }
            Ok(())
        })();
        return match restored {
            Ok(()) => Err(error).context("Update failed; previous app restored"),
            Err(restore) => Err(error).context(format!(
                "Rollback needs review; backup retained at {}: {restore:#}",
                backup.display()
            )),
        };
    }
    Ok(())
}

#[cfg(windows)]
fn open_parent(pid: u32, expected: &Path) -> Result<Option<std::os::windows::io::OwnedHandle>> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        QueryFullProcessImageNameW,
    };
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(87) {
                return Ok(None);
            }
            return Err(error).context("Opening the original app process");
        }
        let handle = OwnedHandle::from_raw_handle(handle);
        let mut buffer = vec![0_u16; 32768];
        let mut length = buffer.len() as u32;
        ensure!(
            QueryFullProcessImageNameW(handle.as_raw_handle(), 0, buffer.as_mut_ptr(), &mut length)
                != 0,
            "Cannot verify the original app process"
        );
        let actual = PathBuf::from(String::from_utf16(&buffer[..length as usize])?);
        ensure!(
            actual.canonicalize()? == expected.canonicalize()?,
            "Updater parent executable differs; no process was stopped"
        );
        Ok(Some(handle))
    }
}

#[cfg(windows)]
fn wait_parent(parent: Option<&std::os::windows::io::OwnedHandle>, stage: &Path) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        ensure!(
            !stage.join("cancel").try_exists()?,
            "Update was cancelled; original app remains in place"
        );
        let Some(parent) = parent else {
            return Ok(());
        };
        let status = unsafe { WaitForSingleObject(parent.as_raw_handle(), 250) };
        if status == 0 {
            return Ok(());
        }
        ensure!(status == 258, "Cannot wait for the original app to exit");
        ensure!(
            Instant::now() < deadline,
            "Original app did not exit; update was not applied"
        );
    }
}

#[cfg(windows)]
pub fn apply(root: &Path, path: &Path) -> Result<()> {
    apply_with(root, path, |target| {
        Command::new(target)
            .arg("--background")
            .current_dir(target.parent().context("No executable directory")?)
            .spawn()
            .context("Restarting Twilight Sync")?;
        Ok(())
    })
}

#[cfg(windows)]
fn apply_with(root: &Path, path: &Path, restart: impl Fn(&Path) -> Result<()>) -> Result<()> {
    let (stage, plan) = load_plan(root, path)?;
    ensure!(
        plan.tag.strip_prefix('v') == Some(env!("CARGO_PKG_VERSION")),
        "Release tag does not match the verified helper's compiled version"
    );
    ensure!(
        std::env::current_exe()?.canonicalize()?
            == stage.join("twilight-sync.exe").canonicalize()?,
        "Only the verified staged app may act as updater"
    );
    ensure!(
        file_hash(&stage.join("twilight-sync.exe"))? == plan.new_hash
            && file_hash(&plan.destination)? == plan.old_hash,
        "Update plan does not match the executable bytes"
    );
    let parent = open_parent(plan.parent_pid, &plan.destination)?;
    files::atomic_write(&stage.join("ready"), b"ready")?;
    let mut parent_exited = false;
    let result = (|| -> Result<()> {
        wait_parent(parent.as_ref(), &stage)?;
        parent_exited = true;
        replace_files(&stage, &plan)?;
        let completion = Completion {
            status: "completed".into(),
            helper_pid: std::process::id(),
            detail: format!("Updated to {}", plan.tag),
        };
        files::atomic_write(
            &stage.join("result.json"),
            &serde_json::to_vec(&completion)?,
        )?;
        restart(&plan.destination)?;
        Ok(())
    })();
    if let Err(error) = result {
        let cancelled = !parent_exited && stage.join("cancel").try_exists()?;
        let completion = Completion {
            status: if cancelled { "cancelled" } else { "failed" }.into(),
            helper_pid: std::process::id(),
            detail: format!("{error:#}"),
        };
        files::atomic_write(
            &stage.join("result.json"),
            &serde_json::to_vec(&completion)?,
        )?;
        if cancelled {
            return Ok(());
        }
        if parent_exited && file_hash(&plan.destination)? == plan.old_hash {
            restart(&plan.destination)
                .context("Restarting the original app after the update failed")?;
        }
        return Err(error).context(format!(
            "Update logs and backups are retained at {}",
            stage.display()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn apply(_: &Path, _: &Path) -> Result<()> {
    bail!("Self-updates currently require Windows x64")
}

#[cfg(windows)]
fn helper_running(pid: u32) -> Result<bool> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    unsafe {
        let raw = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if raw.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(87) {
                return Ok(false);
            }
            return Err(error).context("Checking completed update helper");
        }
        let handle = OwnedHandle::from_raw_handle(raw);
        let state = WaitForSingleObject(handle.as_raw_handle(), 0);
        ensure!(
            state == 0 || state == 258,
            "Cannot inspect updater process state"
        );
        Ok(state == 258)
    }
}

pub fn cleanup(root: &Path) -> Result<usize> {
    let updates = updates_directory(root)?;
    let mut count = 0;
    for entry in fs::read_dir(&updates)? {
        let entry = entry?;
        let stage = entry.path();
        if !entry.file_type()?.is_dir() || !stage.join("result.json").try_exists()? {
            continue;
        }
        stage_for(root, &stage.join("plan.json"))?;
        let completion: Completion =
            serde_json::from_slice(&bounded(&stage.join("result.json"), 16 * 1024)?)?;
        if completion.status != "completed" && completion.status != "cancelled" {
            continue;
        }
        #[cfg(windows)]
        if helper_running(completion.helper_pid)? {
            continue;
        }
        let known = [
            "twilight-sync.exe",
            "previous.exe",
            "plan.json",
            "ready",
            "cancel",
            "result.json",
            "helper.log",
        ];
        for file in fs::read_dir(&stage)? {
            let file = file?;
            ensure!(
                file.file_type()?.is_file() && known.iter().any(|name| file.file_name() == *name),
                "Unknown file in completed update; directory preserved"
            );
            ensure_safe_path(&file.path())?;
        }
        for name in known {
            let path = stage.join(name);
            if path.try_exists()? {
                fs::remove_file(path)?;
            }
        }
        fs::remove_dir(stage)?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pe(marker: u8) -> Vec<u8> {
        let mut bytes = vec![0; 512];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&128_u32.to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&0x8664_u16.to_le_bytes());
        bytes[152..154].copy_from_slice(&0x20b_u16.to_le_bytes());
        bytes[300] = marker;
        bytes
    }

    fn metadata(tag: &str) -> Result<Vec<u8>> {
        let prefix = format!("https://github.com/{REPOSITORY}/releases/download/{tag}");
        Ok(serde_json::to_vec(&serde_json::json!({
            "tag_name": tag, "draft": false, "prerelease": false,
            "html_url": format!("https://github.com/{REPOSITORY}/releases/tag/{tag}"),
            "assets": [
                {"name": BINARY, "size": 512, "browser_download_url": format!("{prefix}/{BINARY}"), "digest": format!("sha256:{}", "a".repeat(64))},
                {"name": "SHA256SUMS.txt", "size": 100, "browser_download_url": format!("{prefix}/SHA256SUMS.txt"), "digest": format!("sha256:{}", "b".repeat(64))}
            ]
        }))?)
    }

    #[test]
    fn checks_semver_precedence_and_rejects_prereleases_or_spoofed_assets() -> Result<()> {
        let release = parse_metadata(&metadata("v0.10.0")?)?;
        assert!(newer_than(&release.version, "0.9.9")?);
        assert!(!newer_than(&Version::parse("0.3.0+new")?, "0.3.0+old")?);
        assert!(parse_metadata(&metadata("v0.4.0-beta.1")?).is_err());
        let mut value: serde_json::Value = serde_json::from_slice(&metadata("v1.0.0")?)?;
        value["assets"][0]["browser_download_url"] = "https://evil.example/app.exe".into();
        assert!(parse_metadata(&serde_json::to_vec(&value)?).is_err());
        Ok(())
    }

    #[test]
    fn checksum_lists_cannot_be_ambiguous() {
        assert!(
            checksum(
                &format!("{}  {BINARY}\n{}  {BINARY}", "a".repeat(64), "b".repeat(64)),
                BINARY
            )
            .is_err()
        );
        assert!(checksum("invalid  twilight-sync-windows-x86_64.exe", BINARY).is_err());
    }

    #[test]
    fn update_preferences_are_separate_from_game_settings_and_survive_restart() -> Result<()> {
        let root = tempfile::tempdir()?;
        assert!(load_enabled(root.path())?);
        set_enabled(root.path(), false)?;
        assert!(!load_enabled(root.path())?);
        assert!(!root.path().join("settings.json").try_exists()?);
        Ok(())
    }

    #[test]
    fn installs_verified_bytes_and_updates_receipt_without_touching_data() -> Result<()> {
        let root = tempfile::tempdir()?;
        let stage = updates_directory(root.path())?.join("stage-test");
        fs::create_dir(&stage)?;
        let target = root.path().join("app");
        fs::create_dir(&target)?;
        let exe = target.join("twilight-sync.exe");
        files::atomic_write(&exe, &pe(1))?;
        files::atomic_write(&stage.join("twilight-sync.exe"), &pe(2))?;
        let plan = Plan {
            version: 1,
            tag: "v0.4.0".into(),
            parent_pid: 42,
            destination: exe.clone(),
            old_hash: files::sha256(&pe(1)),
            new_hash: files::sha256(&pe(2)),
        };
        let receipt = Receipt {
            format: 1,
            repository: REPOSITORY.into(),
            version: "v0.3.0".into(),
            architecture: "x86_64".into(),
            executable: "twilight-sync.exe".into(),
            sha256: plan.old_hash.clone(),
        };
        files::atomic_write(
            &target.join(".twilight-sync-install.json"),
            &serde_json::to_vec(&receipt)?,
        )?;
        files::atomic_write(
            &root.path().join("settings.json"),
            b"private settings must survive",
        )?;
        replace_files(&stage, &plan)?;
        assert_eq!(fs::read(&exe)?, pe(2));
        let updated: Receipt =
            serde_json::from_slice(&fs::read(target.join(".twilight-sync-install.json"))?)?;
        assert_eq!(updated.version, "v0.4.0");
        assert_eq!(updated.sha256, plan.new_hash);
        assert_eq!(
            fs::read(root.path().join("settings.json"))?,
            b"private settings must survive"
        );
        assert_eq!(fs::read(stage.join("previous.exe"))?, pe(1));
        Ok(())
    }

    #[test]
    fn tampered_downloads_or_changed_targets_never_replace_existing_app() -> Result<()> {
        let root = tempfile::tempdir()?;
        let stage = root.path().join("stage");
        fs::create_dir(&stage)?;
        let target = root.path().join("app.exe");
        files::atomic_write(&target, &pe(1))?;
        files::atomic_write(&stage.join("twilight-sync.exe"), &pe(9))?;
        let plan = Plan {
            version: 1,
            tag: "v0.4.0".into(),
            parent_pid: 42,
            destination: target.clone(),
            old_hash: files::sha256(&pe(1)),
            new_hash: files::sha256(&pe(2)),
        };
        assert!(replace_files(&stage, &plan).is_err());
        assert_eq!(fs::read(&target)?, pe(1));
        Ok(())
    }

    #[test]
    fn update_plans_must_remain_in_the_private_updates_namespace() -> Result<()> {
        let root = tempfile::tempdir()?;
        assert!(stage_for(root.path(), &root.path().join("plan.json")).is_err());
        assert!(
            stage_for(
                root.path(),
                &updates_directory(root.path())?
                    .join("foreign")
                    .join("plan.json")
            )
            .is_err()
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn receipt_commit_failure_rolls_back_the_executable_and_preserves_settings() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tempfile::tempdir()?;
        let stage = root.path().join("stage");
        fs::create_dir(&stage)?;
        let exe = root.path().join("twilight-sync.exe");
        files::atomic_write(&exe, &pe(1))?;
        files::atomic_write(&stage.join("twilight-sync.exe"), &pe(2))?;
        let plan = Plan {
            version: 1,
            tag: "v0.4.0".into(),
            parent_pid: 42,
            destination: exe.clone(),
            old_hash: files::sha256(&pe(1)),
            new_hash: files::sha256(&pe(2)),
        };
        let receipt = Receipt {
            format: 1,
            repository: REPOSITORY.into(),
            version: "v0.3.0".into(),
            architecture: "x86_64".into(),
            executable: "twilight-sync.exe".into(),
            sha256: plan.old_hash.clone(),
        };
        let path = root.path().join(".twilight-sync-install.json");
        let original = serde_json::to_vec(&receipt)?;
        files::atomic_write(&path, &original)?;
        let _held = OpenOptions::new().read(true).share_mode(1).open(&path)?;
        assert!(replace_files(&stage, &plan).is_err());
        assert_eq!(fs::read(exe)?, pe(1));
        assert_eq!(fs::read(path)?, original);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Internal subprocess fixture used by verified_helper_waits_for_clean_exit"]
    fn simulated_old_app_process() -> Result<()> {
        let ready = PathBuf::from(
            std::env::var_os("TWILIGHT_TEST_PARENT_READY")
                .context("Internal fixture needs a ready path")?,
        );
        let stop = PathBuf::from(
            std::env::var_os("TWILIGHT_TEST_PARENT_STOP")
                .context("Internal fixture needs a stop path")?,
        );
        files::atomic_write(&ready, b"parent ready")?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while !stop.try_exists()? {
            ensure!(
                Instant::now() < deadline,
                "Fixture parent did not receive stop"
            );
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Internal subprocess fixture used by verified_helper_waits_for_clean_exit"]
    fn verified_update_helper_process() -> Result<()> {
        let root = PathBuf::from(
            std::env::var_os("TWILIGHT_TEST_UPDATE_ROOT")
                .context("Internal fixture needs an update root")?,
        );
        let plan = PathBuf::from(
            std::env::var_os("TWILIGHT_TEST_UPDATE_PLAN")
                .context("Internal fixture needs a plan")?,
        );
        apply_with(&root, &plan, |_| {
            files::atomic_write(&root.join("restart-requested"), b"verified restart")
        })
    }

    #[cfg(windows)]
    struct TestChild(Child);

    #[cfg(windows)]
    impl Drop for TestChild {
        fn drop(&mut self) {
            match self.0.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = self.0.kill() {
                        eprintln!("Cannot clean up owned test subprocess: {error}");
                    }
                    if let Err(error) = self.0.wait() {
                        eprintln!("Cannot reap owned test subprocess: {error}");
                    }
                }
                Err(error) => eprintln!("Cannot inspect owned test subprocess: {error}"),
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn verified_helper_waits_for_clean_exit_before_replacing_a_running_app() -> Result<()> {
        let root = tempfile::tempdir()?;
        let stage = updates_directory(root.path())?.join("stage-integration");
        fs::create_dir(&stage)?;
        let install = root.path().join("app");
        fs::create_dir(&install)?;
        let destination = install.join("twilight-sync.exe");
        let original = fs::read(std::env::current_exe()?)?;
        let mut older = original.clone();
        older.extend_from_slice(b"synthetic older PE overlay");
        files::atomic_write(&destination, &older)?;
        files::atomic_write(&stage.join("twilight-sync.exe"), &original)?;
        let parent_ready = root.path().join("parent-ready");
        let parent_stop = root.path().join("parent-stop");
        let mut parent = TestChild(
            Command::new(&destination)
                .args([
                    "--ignored",
                    "--exact",
                    "self_update::tests::simulated_old_app_process",
                    "--nocapture",
                ])
                .env("TWILIGHT_TEST_PARENT_READY", &parent_ready)
                .env("TWILIGHT_TEST_PARENT_STOP", &parent_stop)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !parent_ready.try_exists()? {
            ensure!(
                parent.0.try_wait()?.is_none() && Instant::now() < deadline,
                "Fixture parent did not start"
            );
            thread::sleep(Duration::from_millis(50));
        }
        let plan = Plan {
            version: 1,
            tag: format!("v{}", env!("CARGO_PKG_VERSION")),
            parent_pid: parent.0.id(),
            destination: destination.clone(),
            old_hash: files::sha256(&older),
            new_hash: files::sha256(&original),
        };
        let plan_path = stage.join("plan.json");
        files::atomic_write(&plan_path, &serde_json::to_vec(&plan)?)?;
        let mut helper = TestChild(
            Command::new(stage.join("twilight-sync.exe"))
                .args([
                    "--ignored",
                    "--exact",
                    "self_update::tests::verified_update_helper_process",
                    "--nocapture",
                ])
                .env("TWILIGHT_TEST_UPDATE_ROOT", root.path())
                .env("TWILIGHT_TEST_UPDATE_PLAN", &plan_path)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !stage.join("ready").try_exists()? {
            ensure!(
                helper.0.try_wait()?.is_none() && Instant::now() < deadline,
                "Verified helper did not acknowledge readiness"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            parent.0.try_wait()?.is_none(),
            "Helper must never terminate its parent"
        );
        assert_eq!(
            file_hash(&destination)?,
            plan.old_hash,
            "App changed before clean exit"
        );
        files::atomic_write(&parent_stop, b"stop normally")?;
        ensure!(
            parent.0.wait()?.success(),
            "Fixture parent did not exit cleanly"
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = helper.0.try_wait()? {
                ensure!(status.success(), "Native update helper failed");
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "Native update helper did not finish"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(file_hash(&destination)?, plan.new_hash);
        assert_eq!(file_hash(&stage.join("previous.exe"))?, plan.old_hash);
        assert!(root.path().join("restart-requested").try_exists()?);
        Ok(())
    }
}

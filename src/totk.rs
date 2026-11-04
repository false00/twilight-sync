use crate::{
    config::PlayLock,
    files::{CHUNK_SIZE, ensure_safe_path, sha256, valid_hash},
    history::{self, History},
    play::{self, ProcessMonitor},
    protocol::Event,
    sync_policy::{self, Decision},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    process::Child,
    time::{Duration, Instant},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub const MAX_FILES: usize = 4096;
pub const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
pub const MAX_SAVE_SIZE: u64 = 128 * 1024 * 1024;
pub const MAX_BUNDLE_SIZE: usize = MAX_SAVE_SIZE as usize + MAX_FILES * 1024;
const CANONICAL_ACCOUNT: &str = "00000000000000000000000000000001";

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub hash: String,
    pub bytes: Vec<u8>,
    pub files: usize,
}

pub fn default_install() -> Result<PathBuf> {
    let home = directories::UserDirs::new().context("Cannot find the TotK user directory")?;
    let path = home.home_dir().join("Games").join("TotKRecomp");
    ensure_safe_path(&path)?;
    Ok(path)
}

pub fn committed_save(profile: &Path) -> Result<PathBuf> {
    ensure_safe_path(profile)?;
    let transactions = profile.join("save-tx");
    ensure_safe_path(&transactions)?;
    if transactions.try_exists()? {
        for entry in fs::read_dir(transactions)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            ensure!(
                name != "old" && name != "new" && !name.starts_with("ready-"),
                "TotK has an unfinished save commit; launch and close the game before syncing"
            );
        }
    }
    let path = profile.join("save");
    ensure_safe_path(&path)?;
    Ok(path)
}

fn validate_name(name: &str) -> Result<()> {
    let parts: Vec<_> = name.split('/').collect();
    ensure!(
        (2..=12).contains(&parts.len())
            && parts[0].len() == 32
            && parts[0].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "TotK saves must be inside a 32-digit account directory"
    );
    for part in parts {
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        ensure!(
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.len() <= 240
                && !part.ends_with(['.', ' '])
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9')),
            "Unsafe TotK save filename"
        );
    }
    Ok(())
}

fn collect(
    directory: &Path,
    root: &Path,
    output: &mut BTreeMap<String, Vec<u8>>,
    total: &mut u64,
) -> Result<()> {
    ensure_safe_path(directory)?;
    ensure!(
        directory.components().count() <= root.components().count() + 12,
        "TotK save is nested too deeply"
    );
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        ensure_safe_path(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect(&path, root, output, total)?;
        } else {
            ensure!(metadata.is_file(), "TotK save contains a non-file entry");
            ensure!(
                output.len() < MAX_FILES && metadata.len() <= MAX_FILE_SIZE,
                "TotK save exceeds its file limit"
            );
            let name = path
                .strip_prefix(root)?
                .to_str()
                .context("TotK save filename is not UTF-8")?
                .replace('\\', "/");
            validate_name(&name)?;
            let mut bytes = Vec::new();
            File::open(&path)?
                .take(MAX_FILE_SIZE + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 == metadata.len(),
                "TotK save changed while being read"
            );
            *total += bytes.len() as u64;
            ensure!(
                *total <= MAX_SAVE_SIZE,
                "TotK save exceeds its total size limit"
            );
            ensure!(
                output.insert(name, bytes).is_none(),
                "Duplicate TotK save filename"
            );
        }
    }
    Ok(())
}

fn read_account(profile: &Path) -> Result<Option<[u8; 16]>> {
    let path = profile.join("account.uid");
    ensure_safe_path(&path)?;
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Reading TotK's local account identity"),
    };
    let mut bytes = Vec::new();
    file.take(17).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() == 16 && bytes.iter().any(|byte| *byte != 0),
        "TotK account.uid must contain one nonzero 16-byte account identity"
    );
    Ok(Some(bytes.try_into().map_err(|_| {
        anyhow::anyhow!("Invalid TotK account identity")
    })?))
}

fn account_name(account: &[u8; 16]) -> String {
    format!("{:032x}", u128::from_le_bytes(*account))
}

pub fn snapshot(profile: &Path) -> Result<Option<Snapshot>> {
    let root = committed_save(profile)?;
    if !root.try_exists()? {
        return Ok(None);
    }
    ensure!(root.is_dir(), "TotK committed save must be a directory");
    let Some(account) = read_account(profile)? else {
        ensure!(
            fs::read_dir(&root)?.next().is_none(),
            "TotK saves exist without account.uid; restore the game's local account metadata before syncing"
        );
        return Ok(None);
    };
    let active = root.join(account_name(&account));
    ensure_safe_path(&active)?;
    if !active.try_exists()? {
        return Ok(None);
    }
    let mut files = BTreeMap::new();
    collect(&active, &root, &mut files, &mut 0)?;
    if files.is_empty() {
        return Ok(None);
    }
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, bytes) in &files {
        let (_, relative) = name
            .split_once('/')
            .context("TotK save is outside its account directory")?;
        writer.start_file(format!("{CANONICAL_ACCOUNT}/{relative}"), options)?;
        writer.write_all(bytes)?;
    }
    let bytes = writer.finish()?.into_inner();
    ensure!(
        bytes.len() <= MAX_BUNDLE_SIZE,
        "TotK save bundle exceeds its size limit"
    );
    Ok(Some(Snapshot {
        hash: sha256(&bytes),
        bytes,
        files: files.len(),
    }))
}

pub fn decode(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    ensure!(
        bytes.len() <= MAX_BUNDLE_SIZE,
        "TotK save bundle exceeds its size limit"
    );
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    ensure!(
        (1..=MAX_FILES).contains(&archive.len()),
        "Invalid TotK save file count"
    );
    let mut files = BTreeMap::new();
    let mut total = 0;
    let mut folded = std::collections::BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        validate_name(entry.name())?;
        ensure!(
            entry.is_file() && entry.size() <= MAX_FILE_SIZE,
            "Invalid TotK save file"
        );
        ensure!(
            entry
                .unix_mode()
                .is_none_or(|mode| mode & 0o170000 != 0o120000),
            "TotK save contains a link"
        );
        ensure!(
            folded.insert(entry.name().to_ascii_lowercase()),
            "Duplicate TotK save filename"
        );
        total += entry.size();
        ensure!(
            total <= MAX_SAVE_SIZE,
            "TotK save exceeds its total size limit"
        );
        let name = entry.name().to_owned();
        let expected = entry.size();
        let mut content = Vec::new();
        entry
            .by_ref()
            .take(MAX_FILE_SIZE + 1)
            .read_to_end(&mut content)?;
        ensure!(content.len() as u64 == expected, "Invalid TotK save length");
        files.insert(name, content);
    }
    Ok(files)
}

pub fn ensure_stopped() -> Result<()> {
    ensure!(
        !ProcessMonitor::default().named_game_running("totk.exe"),
        "Close Tears of the Kingdom before syncing its saves"
    );
    Ok(())
}

pub fn apply(profile: &Path, bytes: &[u8], expected: Option<&str>) -> Result<Option<PathBuf>> {
    ensure_stopped()?;
    let files = decode(bytes)?;
    ensure_safe_path(profile)?;
    fs::create_dir_all(profile)?;
    ensure!(
        snapshot(profile)?.as_ref().map(|save| save.hash.as_str()) == expected,
        "TotK progress changed; no save was replaced"
    );
    let original_account = read_account(profile)?;
    let mut account = original_account.unwrap_or_else(rand::random::<[u8; 16]>);
    while account.iter().all(|byte| *byte == 0) {
        account = rand::random();
    }
    let account_directory = account_name(&account);
    let stage = tempfile::Builder::new()
        .prefix(".totk-sync-")
        .tempdir_in(profile)?;
    fs::write(stage.path().join("account.uid"), account)?;
    let prepared = stage.path().join("save").join(&account_directory);
    fs::create_dir_all(&prepared)?;
    for (name, content) in files {
        let (source_account, relative) =
            name.split_once('/').context("TotK save has no account")?;
        ensure!(
            source_account == CANONICAL_ACCOUNT,
            "TotK save bundle has a noncanonical account"
        );
        let destination = prepared.join(relative);
        fs::create_dir_all(destination.parent().context("TotK save has no parent")?)?;
        let mut file = File::create_new(destination)?;
        file.write_all(&content)?;
        file.sync_all()?;
    }
    let staged = snapshot(stage.path())?.context("Prepared TotK save is empty")?;
    ensure!(
        staged.hash == sha256(bytes),
        "TotK bundle is not canonical or changed while staging"
    );
    ensure_stopped()?;
    ensure!(
        snapshot(profile)?.as_ref().map(|save| save.hash.as_str()) == expected,
        "TotK progress changed during staging; no save was replaced"
    );
    ensure!(
        read_account(profile)? == original_account,
        "TotK's local account changed during staging"
    );
    let save_root = committed_save(profile)?;
    fs::create_dir_all(&save_root)?;
    if original_account.is_none() {
        crate::files::atomic_write(&profile.join("account.uid"), &account)?;
    }
    let destination = save_root.join(&account_directory);
    let backup = if destination.try_exists()? {
        let backups = profile.join("save-sync-backups");
        ensure_safe_path(&backups)?;
        fs::create_dir_all(&backups)?;
        let backup = backups.join(format!(
            "{}-{:016x}",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
            rand::random::<u64>()
        ));
        fs::create_dir(&backup)?;
        fs::rename(&destination, backup.join(&account_directory))
            .context("Retaining the previous TotK save")?;
        Some(backup)
    } else {
        None
    };
    if let Err(error) = fs::rename(&prepared, &destination) {
        if let Some(backup) = &backup {
            fs::rename(backup.join(&account_directory), &destination)
                .context("Restoring the original TotK save after a failed commit")?;
        }
        return Err(error).context("Committing the received TotK save");
    }
    ensure!(
        snapshot(profile)?.is_some_and(|save| save.hash == staged.hash),
        "Received TotK save verification failed; rollback copy was retained"
    );
    Ok(backup)
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub checked: bool,
    pub installed: bool,
    pub install_path: Option<PathBuf>,
    pub running: bool,
    pub local_hash: Option<String>,
    pub local_files: usize,
    pub local_size: u64,
    pub peer_checked: bool,
    pub remote_installed: bool,
    pub remote_running: bool,
    pub remote_hash: Option<String>,
    pub remote_files: usize,
    pub pending: bool,
    pub busy: bool,
    pub message: String,
    pub conflict: bool,
    pub base: Option<String>,
    pub local_summary: Option<Summary>,
    pub remote_summary: Option<Summary>,
    pub history: history::Page,
    pub lock: Option<PlayLock>,
    pub remote_lock: Option<PlayLock>,
    pub launch_pending: bool,
    pub recovery_pending: bool,
    pub peer_branch: Option<String>,
    pub comparison_verified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotSummary {
    pub number: u8,
    pub files: usize,
    pub bytes: u64,
    pub hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub files: usize,
    pub bytes: u64,
    pub album_files: usize,
    pub slots: Vec<SlotSummary>,
}

pub fn summary(bytes: &[u8]) -> Result<Summary> {
    let files = decode(bytes)?;
    let mut slots: BTreeMap<u8, (usize, u64, Vec<u8>)> = BTreeMap::new();
    let mut album_files = 0;
    let mut total = 0;
    for (name, content) in &files {
        total += content.len() as u64;
        let relative = name.split_once('/').context("Save has no account")?.1;
        if relative.starts_with("album/") {
            album_files += 1;
        }
        if let Some(slot) = relative
            .split('/')
            .next()
            .and_then(|part| part.strip_prefix("slot_"))
        {
            let number: u8 = slot.parse().context("Invalid TotK slot number")?;
            let entry = slots.entry(number).or_default();
            entry.0 += 1;
            entry.1 += content.len() as u64;
            entry.2.extend_from_slice(relative.as_bytes());
            entry.2.extend_from_slice(sha256(content).as_bytes());
        }
    }
    Ok(Summary {
        files: files.len(),
        bytes: total,
        album_files,
        slots: slots
            .into_iter()
            .map(|(number, (files, bytes, identity))| SlotSummary {
                number,
                files,
                bytes,
                hash: sha256(&identity),
            })
            .collect(),
    })
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    version: u8,
    base: Option<String>,
    lock: Option<PlayLock>,
    peer: Option<String>,
    #[serde(default)]
    release: Option<String>,
    #[serde(default)]
    install: Option<PathBuf>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: 1,
            base: None,
            lock: None,
            peer: None,
            release: None,
            install: None,
        }
    }
}

impl Session {
    fn load(root: &Path) -> Result<Self> {
        let path = root.join("session.json");
        ensure_safe_path(&path)?;
        if !path.try_exists()? {
            return Ok(Self::default());
        }
        ensure!(
            path.metadata()?.len() <= 16 * 1024,
            "TotK session metadata is oversized"
        );
        let session: Self = serde_json::from_slice(&fs::read(path)?)?;
        ensure!(
            session.version == 1
                && session.base.as_ref().is_none_or(|hash| valid_hash(hash))
                && session.peer.as_ref().is_none_or(|hash| valid_hash(hash)),
            "Invalid TotK session metadata"
        );
        ensure!(
            session
                .lock
                .as_ref()
                .is_none_or(|lock| valid_hash(&lock.owner) && valid_hash(&lock.token)),
            "Invalid TotK play lock"
        );
        ensure!(
            session
                .release
                .as_ref()
                .is_none_or(|token| valid_hash(token)),
            "Invalid TotK release token"
        );
        if let Some(install) = &session.install {
            ensure_safe_path(install)?;
        }
        Ok(session)
    }
}

struct Offer {
    hash: String,
    expected: Option<String>,
    files: usize,
    size: u64,
    updated: Instant,
    base: Option<String>,
    lease: Option<String>,
    preview: bool,
}

struct Incoming {
    offer: Offer,
    temporary: tempfile::NamedTempFile,
    received: u64,
}

pub struct Channel {
    root: PathBuf,
    install: Result<PathBuf>,
    pub status: Status,
    monitor: ProcessMonitor,
    pending: Option<Offer>,
    incoming: Option<Incoming>,
    outgoing: Option<(Snapshot, Instant)>,
    sent: Option<(String, Instant)>,
    session: Session,
    state_error: Option<String>,
    device: String,
    username: String,
    peer: Option<String>,
    history: Option<History>,
    history_hash: Option<String>,
    seen_hash: Option<String>,
    stable_since: Instant,
    last_poll: Instant,
    preview_requested: Option<String>,
    branch_requested: Option<String>,
    preview_sent: bool,
    outgoing_preview: bool,
    selected_remote: Option<(String, String, Instant)>,
    child: Option<Child>,
    managed_token: Option<String>,
    launch_request: Option<(String, Option<String>, Instant)>,
    launch_after_sync: bool,
    recovery_request: Option<String>,
    pub recovery_pending: Option<String>,
    #[cfg(test)]
    test_launch: bool,
}

impl Channel {
    pub fn new(root: PathBuf, install: Result<PathBuf>) -> Self {
        let (session, state_error) = match Session::load(&root) {
            Ok(session) => (session, None),
            Err(error) => (Session::default(), Some(format!("{error:#}"))),
        };
        let status = Status {
            base: session.base.clone(),
            lock: session.lock.clone(),
            ..Status::default()
        };
        let install = session.install.clone().map(Ok).unwrap_or(install);
        Self {
            root,
            install,
            status,
            monitor: ProcessMonitor::default(),
            pending: None,
            incoming: None,
            outgoing: None,
            sent: None,
            session,
            state_error,
            device: "0".repeat(64),
            username: "Local user".into(),
            peer: None,
            history: None,
            history_hash: None,
            seen_hash: None,
            stable_since: Instant::now(),
            last_poll: Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            preview_requested: None,
            branch_requested: None,
            preview_sent: false,
            outgoing_preview: false,
            selected_remote: None,
            child: None,
            managed_token: None,
            launch_request: None,
            launch_after_sync: false,
            recovery_request: None,
            recovery_pending: None,
            #[cfg(test)]
            test_launch: false,
        }
    }

    pub fn identify(&mut self, device: &str, username: &str) {
        self.device = device.into();
        self.username = username.into();
    }

    pub fn bind_peer(&mut self, peer: &str) -> Result<()> {
        ensure!(valid_hash(peer), "Invalid TotK peer identity");
        if self.session.peer.as_deref() != Some(peer) {
            ensure!(
                self.session.lock.is_none() && !self.status.busy,
                "Recover the existing TotK session before changing its partner"
            );
            self.session.base = None;
            self.session.peer = Some(peer.into());
            self.persist()?;
        }
        self.peer = Some(peer.into());
        Ok(())
    }

    fn persist(&mut self) -> Result<()> {
        ensure!(
            self.state_error.is_none(),
            "TotK safety metadata needs attention before syncing"
        );
        ensure_safe_path(&self.root)?;
        fs::create_dir_all(&self.root)?;
        if let Err(error) = crate::files::atomic_write(
            &self.root.join("session.json"),
            &serde_json::to_vec(&self.session)?,
        ) {
            self.state_error = Some(format!("{error:#}"));
            return Err(error);
        }
        self.status.base = self.session.base.clone();
        self.status.lock = self.session.lock.clone();
        Ok(())
    }

    fn history(&mut self) -> Result<&mut History> {
        if self.history.is_none() {
            self.history = Some(History::open_totk(
                &self.root,
                &self.username,
                &self.device,
            )?);
        }
        self.history.as_mut().context("TotK history is unavailable")
    }

    fn capture(&mut self, save: &Snapshot, reason: history::Reason) -> Result<()> {
        let profile = self.profile()?;
        let peer = self.peer.clone();
        self.history()?
            .capture(&save.bytes, &profile, reason, peer.as_deref())?;
        self.history_hash = Some(save.hash.clone());
        self.status.history = self.history()?.page(None)?;
        Ok(())
    }

    pub fn history_page(&mut self, before: Option<&str>) -> Result<()> {
        self.status.history = self.history()?.page(before)?;
        Ok(())
    }

    pub fn resolve(
        &mut self,
        local_hash: &str,
        remote_hash: &str,
        use_remote: bool,
    ) -> Result<Vec<Event>> {
        self.require_update_idle()?;
        self.refresh()?;
        ensure!(
            self.status.local_hash.as_deref() == Some(local_hash)
                && self.status.remote_hash.as_deref() == Some(remote_hash)
                && !self.status.remote_running
                && self.status.remote_lock.is_none(),
            "TotK progress or session changed; compare again"
        );
        let profile = self.profile()?;
        let peer = self.peer.clone().context("No bound TotK partner")?;
        if use_remote {
            if let Some(id) = self.status.peer_branch.clone() {
                let bytes = self.history()?.read(&id)?;
                ensure!(
                    sha256(&bytes) == remote_hash,
                    "Retained TotK branch is stale"
                );
                if let Some(current) = snapshot(&profile)? {
                    self.capture(&current, history::Reason::BeforeReceive)?;
                }
                apply(&profile, &bytes, Some(local_hash))?;
                self.history()?
                    .record_local_resolution(&bytes, &profile, &peer, remote_hash)?;
                self.session.base = Some(remote_hash.into());
                self.persist()?;
                self.refresh()?;
                self.status.history = self.history()?.page(None)?;
                self.status.conflict = false;
                return Ok(vec![self.shared_state()]);
            }
            self.selected_remote = Some((local_hash.into(), remote_hash.into(), Instant::now()));
            return Ok(vec![Event::SavePull {
                hash: remote_hash.into(),
                expected: Some(local_hash.into()),
                preview: false,
            }]);
        }
        let save = snapshot(&profile)?.context("No local TotK branch")?;
        self.history()?
            .record_local_resolution(&save.bytes, &profile, &peer, remote_hash)?;
        self.status.history = self.history()?.page(None)?;
        self.offer_mode(true)
    }

    pub fn continue_local(
        &mut self,
        connected: bool,
        expected: Option<&str>,
    ) -> Result<Vec<Event>> {
        self.idle()?;
        ensure!(
            snapshot(&self.profile()?)?
                .as_ref()
                .map(|save| save.hash.as_str())
                == expected,
            "Local TotK progress changed before the branch choice"
        );
        let mut frames = if self.pending.is_some() && self.incoming.is_none() {
            self.accept(false)?
        } else {
            Vec::new()
        };
        frames.extend(self.launch(connected, true)?);
        Ok(frames)
    }

    pub fn set_executable(&mut self, executable: &Path) -> Result<()> {
        self.require_update_idle()?;
        ensure!(
            self.session.lock.is_none() && self.session.release.is_none(),
            "Finish or recover the TotK play session before changing installations"
        );
        ensure_safe_path(executable)?;
        ensure!(
            executable.is_file()
                && executable
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("totk.exe")),
            "Select totk.exe from the native portable installation"
        );
        let install = executable
            .parent()
            .context("Game executable has no directory")?
            .to_path_buf();
        self.install = Ok(install.clone());
        self.session.install = Some(install);
        self.session.base = None;
        self.history_hash = None;
        self.persist()?;
        self.refresh()
    }

    pub fn restore(&mut self, id: &str, expected: Option<&str>) -> Result<()> {
        self.require_update_idle()?;
        ensure!(
            self.session.lock.is_none()
                && self.status.remote_lock.is_none()
                && !self.status.remote_running,
            "Stop both games and finish the TotK play session before restoring"
        );
        let profile = self.profile()?;
        let current = snapshot(&profile)?;
        ensure!(
            current.as_ref().map(|save| save.hash.as_str()) == expected,
            "TotK progress changed before restore"
        );
        let bytes = self.history()?.read(id)?;
        if let Some(current) = &current {
            self.capture(current, history::Reason::BeforeRestore)?;
        }
        apply(&profile, &bytes, expected)?;
        self.history()?.capture_restored(&bytes, &profile, id)?;
        self.session.base = None;
        self.persist()?;
        self.refresh()?;
        self.status.history = self.history()?.page(None)?;
        self.status.message =
            "Selected TotK snapshot restored; the previous branch remains in history.".into();
        Ok(())
    }

    fn profile(&self) -> Result<PathBuf> {
        let install = self
            .install
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        ensure_safe_path(install)?;
        ensure!(
            install.join("totk.exe").is_file(),
            "The portable TotK installation was not found"
        );
        Ok(install.join("profile"))
    }

    pub fn refresh(&mut self) -> Result<()> {
        ensure!(
            self.state_error.is_none(),
            "TotK safety metadata needs attention"
        );
        let install = self
            .install
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        ensure_safe_path(install)?;
        self.status.checked = true;
        self.status.install_path = Some(install.clone());
        self.status.installed = install.join("totk.exe").is_file();
        self.status.running = self.monitor.named_game_running("totk.exe");
        let save = if self.status.installed {
            match snapshot(&install.join("profile")) {
                Ok(save) => save,
                Err(_) if self.status.running => return Ok(()),
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        self.status.local_hash = save.as_ref().map(|save| save.hash.clone());
        self.status.local_files = save.as_ref().map_or(0, |save| save.files);
        self.status.local_size = save.as_ref().map_or(0, |save| save.bytes.len() as u64);
        let hash = save.as_ref().map(|save| save.hash.clone());
        if self.seen_hash != hash {
            self.seen_hash = hash;
            self.stable_since = Instant::now();
            self.status.local_summary =
                save.as_ref().map(|save| summary(&save.bytes)).transpose()?;
        } else if self.stable_since.elapsed() >= Duration::from_secs(2)
            && let Some(save) = &save
            && self.history_hash.as_ref() != Some(&save.hash)
        {
            self.capture(save, history::Reason::Observed)?;
        }
        Ok(())
    }

    pub fn state_event(&mut self) -> Result<Event> {
        self.refresh()?;
        Ok(Event::TotkState {
            installed: self.status.installed,
            running: self.status.running,
            hash: if self.status.running {
                None
            } else {
                self.status.local_hash.clone()
            },
            files: self.status.local_files,
            size: self.status.local_size,
        })
    }

    fn idle(&mut self) -> Result<()> {
        ensure!(
            self.state_error.is_none(),
            "TotK safety metadata needs attention"
        );
        ensure!(
            !self.monitor.named_game_running("totk.exe"),
            "Close Tears of the Kingdom before syncing its saves"
        );
        Ok(())
    }

    pub fn require_update_idle(&mut self) -> Result<()> {
        self.idle()?;
        ensure!(
            !self.status.busy
                && self.launch_request.is_none()
                && self.child.is_none()
                && self.session.lock.is_none()
                && self.session.release.is_none(),
            "Finish or recover the TotK play session and pending transfers first"
        );
        Ok(())
    }

    pub fn require_app_update_idle(&mut self) -> Result<()> {
        ensure!(
            !self.monitor.named_game_running("totk.exe") && !self.status.remote_running,
            "Stop TotK on both devices before updating Twilight Sync"
        );
        ensure!(
            !self.status.busy
                && self.launch_request.is_none()
                && self.child.is_none()
                && self.recovery_request.is_none()
                && self.recovery_pending.is_none(),
            "Finish the current TotK launch, transfer, or recovery before updating Twilight Sync"
        );
        Ok(())
    }

    pub fn game_running(&mut self) -> bool {
        self.monitor.named_game_running("totk.exe")
    }

    pub fn launch(&mut self, connected: bool, local_only: bool) -> Result<Vec<Event>> {
        self.refresh()?;
        self.idle()?;
        ensure!(
            self.child.is_none() && self.launch_request.is_none(),
            "TotK launch is already pending"
        );
        ensure!(
            !self.status.busy && !self.status.remote_running,
            "Finish the current TotK transfer or partner session first"
        );
        ensure!(
            self.session.lock.is_none()
                && (self.session.release.is_none() || !connected || local_only)
                && self.status.remote_lock.is_none(),
            "A TotK play lock is held; recover the completed session first"
        );
        if !connected || local_only {
            self.start_game(None)?;
            self.status.message = "Playing this PC's TotK branch. Offline progress is preserved for later reconciliation.".into();
            return Ok(if connected {
                vec![self.shared_state()]
            } else {
                Vec::new()
            });
        }
        ensure!(
            self.status.peer_checked && self.status.remote_installed,
            "Wait for the partner's TotK state before protected play"
        );
        if self.status.local_hash != self.status.remote_hash {
            let decision = sync_policy::reconcile(
                self.status.local_hash.as_deref(),
                self.status.remote_hash.as_deref(),
                self.session.base.as_deref(),
            );
            ensure!(
                decision != Decision::Conflict,
                "Compare the two TotK branches or explicitly continue this PC's progress"
            );
            self.launch_after_sync = true;
            self.status.launch_pending = true;
            return if decision == Decision::Send {
                self.offer_mode(false)
            } else {
                Ok(vec![self.shared_state()])
            };
        }
        let token = hex::encode(rand::random::<[u8; 32]>());
        self.session.lock = Some(play::acquire(None, false, &self.device, &token)?);
        self.persist()?;
        self.launch_request = Some((
            token.clone(),
            self.status.local_hash.clone(),
            Instant::now(),
        ));
        self.status.launch_pending = true;
        self.status.message = "Waiting for the paired PC to reserve TotK play.".into();
        Ok(vec![Event::LeaseRequest {
            token,
            hash: self.status.local_hash.clone(),
        }])
    }

    fn start_game(&mut self, token: Option<String>) -> Result<()> {
        self.idle()?;
        if let Some(save) = snapshot(&self.profile()?)? {
            self.capture(&save, history::Reason::Observed)?;
        }
        #[cfg(test)]
        if self.test_launch {
            self.monitor = ProcessMonitor::for_test(true);
            self.status.running = true;
            self.managed_token = token;
            return Ok(());
        }
        let executable = self
            .install
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?
            .join("totk.exe");
        self.child = Some(play::launch_game(
            &executable,
            crate::protocol::GameId::Totk,
        )?);
        self.managed_token = token;
        self.status.running = true;
        Ok(())
    }

    pub fn recover(&mut self) -> Result<Vec<Event>> {
        self.idle()?;
        ensure!(
            !self.status.busy && !self.status.remote_running,
            "Stop both TotK games and finish transfers before recovery"
        );
        let token = self
            .session
            .lock
            .as_ref()
            .map(|lock| lock.token.clone())
            .or_else(|| self.session.release.clone())
            .context("No TotK session needs recovery")?;
        self.recovery_request = Some(token.clone());
        Ok(vec![Event::Recover { token }])
    }

    pub fn recover_offline(&mut self) -> Result<()> {
        self.idle()?;
        ensure!(
            !self.status.busy && !self.status.remote_running,
            "Stop TotK and finish transfers before recovery"
        );
        let lock = self
            .session
            .lock
            .as_ref()
            .context("No local TotK reservation needs recovery")?;
        ensure!(
            lock.owner == self.device,
            "Only this PC's completed TotK reservation can be recovered offline"
        );
        self.session.release = Some(lock.token.clone());
        self.session.lock = None;
        self.launch_request = None;
        self.status.launch_pending = false;
        self.persist()?;
        self.status.message = "This PC's completed TotK session was recovered. The partner's reservation is retained until reconnection.".into();
        Ok(())
    }

    pub fn approve_recovery(&mut self, approved: bool) -> Result<Vec<Event>> {
        self.idle()?;
        ensure!(
            !self.status.busy && !self.status.remote_running,
            "Stop both TotK games before recovery"
        );
        let token = self
            .recovery_pending
            .take()
            .context("No TotK recovery request")?;
        self.status.recovery_pending = false;
        if !approved {
            return Ok(Vec::new());
        }
        self.session.lock = None;
        self.session.release = None;
        self.persist()?;
        Ok(vec![Event::RecoveryApproved { token }, self.shared_state()])
    }

    pub fn shared_state(&self) -> Event {
        Event::State {
            hash: self.status.local_hash.clone(),
            save_ready: self.status.installed,
            game: self.status.running,
            lock: self.session.lock.clone(),
        }
    }

    pub fn tick(&mut self, connected: bool) -> Result<Vec<Event>> {
        if self.last_poll.elapsed() < Duration::from_secs(3) {
            return Ok(Vec::new());
        }
        self.last_poll = Instant::now();
        self.expire();
        self.refresh()?;
        if self
            .launch_request
            .as_ref()
            .is_some_and(|(_, _, created)| created.elapsed() >= Duration::from_secs(30))
        {
            let (token, _, _) = self
                .launch_request
                .take()
                .context("Missing expired TotK launch")?;
            if play::matches(self.session.lock.as_ref(), &self.device, &token) {
                self.session.lock = None;
                self.session.release = Some(token);
                self.persist()?;
            }
            self.status.launch_pending = false;
            self.status.message = "TotK play approval expired. No game was launched.".into();
        }
        if let Some(child) = &mut self.child
            && child.try_wait()?.is_some()
        {
            self.child = None;
        }
        if !self.status.running
            && self.child.is_none()
            && self.stable_since.elapsed() >= Duration::from_secs(2)
            && let Some(token) = self.managed_token.take()
        {
            ensure!(
                play::matches(self.session.lock.as_ref(), &self.device, &token),
                "TotK session ownership changed"
            );
            self.session.lock = None;
            self.session.release = Some(token);
            self.persist()?;
        }
        if !connected || !self.status.installed {
            return Ok(Vec::new());
        }
        let mut frames = vec![self.shared_state(), self.state_event()?];
        if let Some(token) = &self.session.release
            && !self.status.running
            && self.child.is_none()
        {
            frames.push(Event::LeaseRelease {
                token: token.clone(),
                hash: self.status.local_hash.clone(),
            });
        }
        if !self.status.peer_checked
            || self.status.running
            || self.status.remote_running
            || self.status.busy
            || self.stable_since.elapsed() < Duration::from_secs(2)
        {
            return Ok(frames);
        }
        match sync_policy::reconcile(
            self.status.local_hash.as_deref(),
            self.status.remote_hash.as_deref(),
            self.session.base.as_deref(),
        ) {
            Decision::Matching => {
                self.status.conflict = false;
                if self.session.base != self.status.local_hash {
                    self.session.base = self.status.local_hash.clone();
                    self.persist()?;
                }
            }
            Decision::Send if self.session.lock.is_none() && self.status.remote_lock.is_none() => {
                frames.extend(self.offer_mode(false)?)
            }
            Decision::Conflict => {
                self.status.conflict = true;
                self.status.message = "Both PCs have different TotK progress. Choose a complete save or keep playing this PC's branch.".into();
                if self.preview_requested != self.status.remote_hash {
                    if let Some(hash) = &self.status.remote_hash {
                        frames.push(Event::SavePreviewRequest { hash: hash.clone() });
                    }
                    self.preview_requested = self.status.remote_hash.clone();
                }
                let may_preview =
                    self.peer.as_ref().is_some_and(|peer| self.device < *peer) || self.preview_sent;
                if may_preview
                    && self.branch_requested != self.status.remote_hash
                    && self.incoming.is_none()
                    && self.outgoing.is_none()
                    && let Some(hash) = &self.status.remote_hash
                {
                    frames.push(Event::SavePull {
                        hash: hash.clone(),
                        expected: self.status.local_hash.clone(),
                        preview: true,
                    });
                    self.branch_requested = Some(hash.clone());
                }
            }
            _ => {}
        }
        if self.launch_after_sync
            && !self.status.busy
            && self.session.release.is_none()
            && self.status.local_hash == self.status.remote_hash
        {
            self.launch_after_sync = false;
            self.status.launch_pending = false;
            frames.extend(self.launch(true, false)?);
        }
        Ok(frames)
    }

    pub fn offer(&mut self) -> Result<Vec<Event>> {
        self.offer_mode(true)
    }

    fn offer_mode(&mut self, manual: bool) -> Result<Vec<Event>> {
        self.prepare_offer(manual, false, self.status.remote_hash.clone())
    }

    fn prepare_offer(
        &mut self,
        manual: bool,
        preview: bool,
        expected: Option<String>,
    ) -> Result<Vec<Event>> {
        self.idle()?;
        ensure!(!self.status.busy, "A TotK save transfer is already pending");
        ensure!(
            self.status.peer_checked && self.status.remote_installed,
            "Check TotK saves on the paired device first; both PCs need TotK support"
        );
        ensure!(
            !self.status.remote_running,
            "Your partner is playing Tears of the Kingdom"
        );
        let save =
            snapshot(&self.profile()?)?.context("This PC has no committed TotK save to send")?;
        if self.status.remote_hash.as_ref() == Some(&save.hash) {
            self.status.message = "TotK saves already match.".into();
            return Ok(Vec::new());
        }
        let event = Event::SaveOffer {
            hash: save.hash.clone(),
            base: self.session.base.clone(),
            expected,
            manual,
            lease: None,
            preview,
            files: save.files,
            size: save.bytes.len() as u64,
        };
        self.outgoing_preview = preview;
        self.outgoing = Some((save, Instant::now()));
        self.status.busy = true;
        self.status.message = "Waiting for your partner to approve the TotK save.".into();
        Ok(vec![event])
    }

    pub fn accept(&mut self, approved: bool) -> Result<Vec<Event>> {
        if approved {
            self.idle()?;
            let offer = self
                .pending
                .as_ref()
                .context("No incoming TotK save offer")?;
            ensure!(
                offer.updated.elapsed() < Duration::from_secs(120),
                "TotK save offer expired"
            );
            ensure!(
                snapshot(&self.profile()?)?.as_ref().map(|save| &save.hash)
                    == offer.expected.as_ref(),
                "Local TotK progress changed since the offer"
            );
        }
        let offer = self.pending.take().context("No incoming TotK save offer")?;
        self.status.pending = false;
        if !approved {
            self.status.busy = false;
            self.status.message = "Incoming TotK save declined; local progress was kept.".into();
            return Ok(vec![Event::TotkDeclined { hash: offer.hash }]);
        }
        ensure_safe_path(&self.root)?;
        fs::create_dir_all(&self.root)?;
        let hash = offer.hash.clone();
        self.incoming = Some(Incoming {
            offer,
            temporary: tempfile::NamedTempFile::new_in(&self.root)?,
            received: 0,
        });
        self.status.message = "Receiving approved TotK save...".into();
        Ok(vec![Event::TotkAccept { hash }])
    }

    pub fn handle(&mut self, event: Event) -> Result<Vec<Event>> {
        match event {
            Event::SavePull {
                hash,
                expected,
                preview,
            } => {
                self.idle()?;
                ensure!(
                    valid_hash(&hash) && expected.as_ref().is_none_or(|hash| valid_hash(hash)),
                    "Invalid game-scoped snapshot request"
                );
                ensure!(
                    snapshot(&self.profile()?)?.is_some_and(|save| save.hash == hash),
                    "Requested TotK branch changed"
                );
                return self.prepare_offer(true, preview, expected);
            }
            Event::LeaseRequest { token, hash } => {
                ensure!(
                    valid_hash(&token) && hash.as_ref().is_none_or(|hash| valid_hash(hash)),
                    "Invalid game-scoped lease request"
                );
                self.refresh()?;
                let peer = self
                    .peer
                    .clone()
                    .context("TotK peer identity is not bound")?;
                if self.status.running
                    || self.status.busy
                    || self.session.lock.is_some()
                    || self.session.release.is_some()
                    || self.status.local_hash != hash
                {
                    return Ok(vec![Event::LeaseDenied {
                        token,
                        reason: "TotK is busy, locked, or its save differs".into(),
                    }]);
                }
                self.session.lock = Some(play::acquire(None, false, &peer, &token)?);
                self.persist()?;
                return Ok(vec![Event::LeaseGranted { token }, self.shared_state()]);
            }
            Event::LeaseGranted { token } => {
                let (requested, hash, created) = self
                    .launch_request
                    .take()
                    .context("No pending TotK launch")?;
                ensure!(
                    token == requested
                        && created.elapsed() < Duration::from_secs(30)
                        && play::matches(self.session.lock.as_ref(), &self.device, &token),
                    "Stale TotK launch approval"
                );
                ensure!(
                    snapshot(&self.profile()?)?.map(|save| save.hash) == hash,
                    "TotK progress changed before launch approval"
                );
                self.status.launch_pending = false;
                if let Err(error) = self.start_game(Some(token.clone())) {
                    self.session.lock = None;
                    self.session.release = Some(token.clone());
                    self.persist()?;
                    self.status.message = format!("TotK did not launch: {error:#}");
                    return Ok(vec![Event::LeaseRelease {
                        token,
                        hash: self.status.local_hash.clone(),
                    }]);
                }
                self.status.message =
                    "TotK is running with coordinated online play protection.".into();
                return Ok(vec![self.shared_state()]);
            }
            Event::LeaseDenied { token, reason } => {
                ensure!(
                    self.launch_request
                        .as_ref()
                        .is_some_and(|(requested, _, _)| requested == &token)
                        && play::matches(self.session.lock.as_ref(), &self.device, &token),
                    "Unexpected TotK launch denial"
                );
                self.launch_request = None;
                self.status.launch_pending = false;
                self.session.lock = None;
                self.persist()?;
                self.status.message = format!("TotK play was not reserved: {reason}");
            }
            Event::LeaseRelease { token, hash } => {
                self.idle()?;
                ensure!(
                    !self.status.busy && hash.as_ref().is_none_or(|hash| valid_hash(hash)),
                    "TotK save transfer is still active"
                );
                let peer = self.peer.as_ref().context("Missing TotK peer")?;
                if self.session.lock.is_some() {
                    ensure!(
                        play::matches(self.session.lock.as_ref(), peer, &token),
                        "TotK release does not own the active session"
                    );
                    self.session.lock = None;
                    self.persist()?;
                } else {
                    ensure!(valid_hash(&token), "Invalid TotK release token");
                }
                return Ok(vec![Event::LeaseReleased { token }, self.shared_state()]);
            }
            Event::LeaseReleased { token } => {
                ensure!(
                    self.session.release.as_ref() == Some(&token),
                    "Unexpected TotK release receipt"
                );
                self.session.release = None;
                self.persist()?;
            }
            Event::Recover { token } => {
                self.idle()?;
                ensure!(
                    valid_hash(&token) && !self.status.busy && !self.status.remote_running,
                    "TotK recovery is unsafe while a game or transfer is active"
                );
                ensure!(
                    self.session
                        .lock
                        .as_ref()
                        .is_none_or(|lock| lock.token == token),
                    "TotK recovery targets another session"
                );
                self.recovery_pending = Some(token);
                self.status.recovery_pending = true;
                self.status.message =
                    "Partner requests recovery of the completed TotK play session.".into();
            }
            Event::RecoveryApproved { token } => {
                self.idle()?;
                ensure!(
                    self.recovery_request.as_ref() == Some(&token) && !self.status.busy,
                    "Unexpected TotK recovery approval"
                );
                self.session.lock = None;
                self.session.release = None;
                self.recovery_request = None;
                self.launch_request = None;
                self.status.launch_pending = false;
                self.persist()?;
                return Ok(vec![self.shared_state()]);
            }
            Event::State {
                hash,
                save_ready,
                game,
                lock,
            } => {
                ensure!(
                    hash.as_ref().is_none_or(|hash| valid_hash(hash))
                        && (save_ready || hash.is_none()),
                    "Invalid game-scoped save state"
                );
                ensure!(
                    lock.as_ref().is_none_or(|lock| valid_hash(&lock.owner)
                        && valid_hash(&lock.token)
                        && (lock.owner == self.device || self.peer.as_ref() == Some(&lock.owner))),
                    "Invalid game-scoped play lock"
                );
                if self.status.remote_hash != hash {
                    self.status.remote_summary = None;
                    self.status.peer_branch = None;
                    self.status.comparison_verified = false;
                }
                self.status.peer_checked = true;
                self.status.remote_installed = save_ready;
                self.status.remote_running = game;
                self.status.remote_hash = hash;
                self.status.remote_lock = lock;
            }
            Event::SavePreviewRequest { hash } => {
                let save = snapshot(&self.profile()?)?.context("No TotK save to preview")?;
                ensure!(save.hash == hash, "TotK save changed before preview");
                return Ok(vec![Event::SavePreview {
                    hash,
                    base: self.session.base.clone(),
                    bytes: serde_json::to_vec(&summary(&save.bytes)?)?,
                }]);
            }
            Event::SavePreview {
                hash,
                base: _,
                bytes,
            } => {
                ensure!(
                    self.status.remote_hash.as_ref() == Some(&hash),
                    "Stale TotK comparison"
                );
                let preview: Summary = serde_json::from_slice(&bytes)?;
                ensure!(
                    preview.files <= MAX_FILES
                        && preview.bytes <= MAX_SAVE_SIZE
                        && preview.slots.len() <= 64
                        && preview.slots.iter().all(|slot| valid_hash(&slot.hash)
                            && slot.files <= MAX_FILES
                            && slot.bytes <= MAX_SAVE_SIZE),
                    "Invalid TotK comparison metadata"
                );
                self.status.remote_summary = Some(preview);
            }
            Event::SaveOffer {
                hash,
                base,
                expected,
                manual,
                lease,
                files,
                size,
                preview,
            } => {
                let current = snapshot(&self.profile()?)?;
                let chosen =
                    self.selected_remote
                        .as_ref()
                        .is_some_and(|(local, remote, selected)| {
                            expected.as_ref() == Some(local)
                                && &hash == remote
                                && selected.elapsed() < Duration::from_secs(120)
                        });
                let automatic = chosen
                    || (preview && self.branch_requested.as_ref() == Some(&hash))
                    || (!manual
                        && sync_policy::fast_forward(
                            current.as_ref().map(|save| save.hash.as_str()),
                            &hash,
                            self.session.base.as_deref(),
                            base.as_deref(),
                        ));
                self.receive_allowed(lease.as_deref())?;
                let frames = self.handle(Event::TotkOffer {
                    hash,
                    expected,
                    files,
                    size,
                })?;
                if let Some(pending) = &mut self.pending {
                    pending.base = base;
                    pending.lease = lease;
                    pending.preview = preview;
                }
                if automatic && self.pending.is_some() {
                    return self.accept(true);
                }
                return Ok(frames);
            }
            Event::TotkRequest => return Ok(vec![self.state_event()?]),
            Event::TotkState {
                installed,
                running,
                hash,
                files,
                size,
            } => {
                ensure!(
                    hash.as_ref().is_none_or(|hash| valid_hash(hash))
                        && files <= MAX_FILES
                        && size <= MAX_BUNDLE_SIZE as u64,
                    "Invalid TotK peer state"
                );
                ensure!(
                    hash.is_none() || (installed && !running && files > 0 && size > 0),
                    "Invalid TotK save readiness"
                );
                self.status.peer_checked = true;
                self.status.remote_installed = installed;
                self.status.remote_running = running;
                if !running {
                    self.status.remote_hash = hash;
                }
                self.status.remote_files = files;
            }
            Event::TotkOffer {
                hash,
                expected,
                files,
                size,
            } => {
                self.idle()?;
                ensure!(
                    !self.status.busy,
                    "Another TotK transfer is already pending"
                );
                ensure!(
                    valid_hash(&hash)
                        && expected.as_ref().is_none_or(|hash| valid_hash(hash))
                        && (1..=MAX_FILES).contains(&files)
                        && (1..=MAX_BUNDLE_SIZE as u64).contains(&size),
                    "Invalid TotK save offer"
                );
                let current = snapshot(&self.profile()?)?;
                if current.as_ref().is_some_and(|save| save.hash == hash) {
                    return Ok(vec![Event::TotkSaved { hash }]);
                }
                ensure!(
                    current.as_ref().map(|save| &save.hash) == expected.as_ref(),
                    "TotK progress changed; refresh both saves before offering a replacement"
                );
                self.pending = Some(Offer {
                    hash,
                    expected,
                    files,
                    size,
                    updated: Instant::now(),
                    base: None,
                    lease: None,
                    preview: false,
                });
                self.status.pending = true;
                self.status.busy = true;
                self.status.message = format!(
                    "Partner offered {files} TotK save files. Local progress will be backed up before replacement."
                );
            }
            Event::TotkAccept { hash } => {
                self.idle()?;
                let (save, offered) = self
                    .outgoing
                    .take()
                    .context("No outgoing TotK save offer")?;
                ensure!(
                    save.hash == hash && offered.elapsed() < Duration::from_secs(120),
                    "Stale TotK approval"
                );
                ensure!(
                    snapshot(&self.profile()?)?.is_some_and(|current| current.hash == hash),
                    "TotK save changed after it was offered"
                );
                let mut events: Vec<_> = save
                    .bytes
                    .chunks(CHUNK_SIZE)
                    .map(|bytes| Event::TotkChunk {
                        hash: hash.clone(),
                        bytes: bytes.to_vec(),
                    })
                    .collect();
                events.push(Event::TotkDone { hash: hash.clone() });
                self.sent = Some((hash, Instant::now()));
                self.status.message = "Sending approved TotK save...".into();
                return Ok(events);
            }
            Event::TotkChunk { hash, bytes } => {
                let incoming = self.incoming.as_mut().context("Unapproved TotK transfer")?;
                ensure!(
                    hash == incoming.offer.hash
                        && !bytes.is_empty()
                        && bytes.len() <= CHUNK_SIZE
                        && incoming.received + bytes.len() as u64 <= incoming.offer.size,
                    "Invalid TotK transfer chunk"
                );
                incoming.temporary.write_all(&bytes)?;
                incoming.received += bytes.len() as u64;
                incoming.offer.updated = Instant::now();
            }
            Event::TotkDone { hash } => {
                self.idle()?;
                let mut incoming = self.incoming.take().context("No approved TotK transfer")?;
                ensure!(
                    hash == incoming.offer.hash && incoming.received == incoming.offer.size,
                    "Incomplete TotK transfer"
                );
                incoming.temporary.flush()?;
                let bytes = fs::read(incoming.temporary.path())?;
                ensure!(
                    sha256(&bytes) == hash && decode(&bytes)?.len() == incoming.offer.files,
                    "TotK transfer checksum or file count mismatch"
                );
                self.receive_allowed(incoming.offer.lease.as_deref())?;
                let profile = self.profile()?;
                if let Some(current) = snapshot(&profile)? {
                    self.capture(&current, history::Reason::BeforeReceive)?;
                }
                let peer = self.peer.clone();
                if let Some(peer) = &peer {
                    let capture = self.history()?.capture_branch(
                        &bytes,
                        &profile,
                        peer,
                        incoming.offer.base.as_deref(),
                    )?;
                    self.status.peer_branch = Some(capture.id);
                }
                if incoming.offer.preview {
                    self.status.remote_summary = Some(summary(&bytes)?);
                    self.status.comparison_verified = true;
                    self.status.history = self.history()?.page(None)?;
                    self.status.busy = false;
                    self.status.message = "Both TotK branches are retained for comparison; local progress was not changed.".into();
                    return Ok(vec![Event::TotkSaved { hash }]);
                }
                let previous = snapshot(&profile)?.map(|save| save.hash);
                let backup = apply(&profile, &bytes, incoming.offer.expected.as_deref())?;
                if let (Some(peer), Some(_)) = (&peer, &previous)
                    && self
                        .selected_remote
                        .as_ref()
                        .is_some_and(|(_, selected, _)| selected == &hash)
                {
                    self.history()?
                        .record_local_resolution(&bytes, &profile, peer, &hash)?;
                }
                self.selected_remote = None;
                let received = Snapshot {
                    hash: hash.clone(),
                    files: incoming.offer.files,
                    bytes,
                };
                self.capture(&received, history::Reason::Received)?;
                self.session.base = Some(hash.clone());
                self.persist()?;
                self.refresh()?;
                self.status.busy = false;
                self.status.message = backup.map_or_else(
                    || "TotK saves received and verified.".into(),
                    |path| {
                        format!(
                            "TotK saves received. Previous progress retained at {}",
                            path.display()
                        )
                    },
                );
                return Ok(vec![Event::TotkSaved { hash }]);
            }
            Event::TotkSaved { hash } => {
                let expected = self
                    .sent
                    .as_ref()
                    .map(|(hash, _)| hash)
                    .or_else(|| self.outgoing.as_ref().map(|(save, _)| &save.hash));
                ensure!(expected == Some(&hash), "Unexpected TotK transfer receipt");
                self.sent = None;
                self.outgoing = None;
                self.status.busy = false;
                if self.outgoing_preview {
                    self.outgoing_preview = false;
                    self.preview_sent = true;
                    self.status.message =
                        "Partner retained the comparison snapshot; progress was not replaced."
                            .into();
                    return Ok(Vec::new());
                }
                self.status.remote_hash = Some(hash);
                self.session.base = self.status.remote_hash.clone();
                self.persist()?;
                self.status.message = "Partner received and verified the TotK saves.".into();
            }
            Event::TotkDeclined { hash } => {
                ensure!(
                    self.outgoing
                        .as_ref()
                        .is_some_and(|(save, _)| save.hash == hash),
                    "Unexpected TotK decline"
                );
                self.outgoing = None;
                self.status.busy = false;
                self.status.message = "Partner kept their existing TotK progress.".into();
            }
            _ => anyhow::bail!("Not a TotK sync event"),
        }
        Ok(Vec::new())
    }

    pub fn expire(&mut self) {
        let timed_out = self
            .pending
            .as_ref()
            .is_some_and(|offer| offer.updated.elapsed() > Duration::from_secs(120))
            || self.incoming.as_ref().is_some_and(|incoming| {
                incoming.offer.updated.elapsed() > Duration::from_secs(120)
            })
            || self
                .outgoing
                .as_ref()
                .is_some_and(|(_, updated)| updated.elapsed() > Duration::from_secs(120))
            || self
                .sent
                .as_ref()
                .is_some_and(|(_, updated)| updated.elapsed() > Duration::from_secs(120));
        if timed_out {
            self.reset();
            self.status.message =
                "TotK transfer expired; committed progress was not changed.".into();
        }
    }

    pub fn reset(&mut self) {
        self.pending = None;
        self.incoming = None;
        self.outgoing = None;
        self.sent = None;
        self.status.pending = false;
        self.status.busy = false;
        self.status.peer_checked = false;
        self.status.remote_hash = None;
        self.status.remote_installed = false;
        self.status.remote_running = false;
        self.status.remote_lock = None;
        self.status.remote_summary = None;
        self.preview_requested = None;
        self.branch_requested = None;
        self.preview_sent = false;
        self.outgoing_preview = false;
        self.selected_remote = None;
        self.status.peer_branch = None;
        self.status.comparison_verified = false;
        self.launch_request = None;
        self.launch_after_sync = false;
        self.status.launch_pending = false;
        self.recovery_request = None;
        self.recovery_pending = None;
        self.status.recovery_pending = false;
    }

    fn receive_allowed(&mut self, lease: Option<&str>) -> Result<()> {
        self.idle()?;
        ensure!(
            self.session
                .lock
                .as_ref()
                .is_none_or(|lock| self.peer.as_ref() == Some(&lock.owner)
                    && lease == Some(lock.token.as_str())),
            "Finish the TotK play session before replacing a save"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "00000000000000000000000000000001";

    #[test]
    fn snapshot_includes_only_committed_save_files() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let profile = temporary.path();
        let slot = profile.join("save").join(ACCOUNT).join("slot_00");
        fs::create_dir_all(&slot)?;
        fs::write(profile.join("account.uid"), 1_u128.to_le_bytes())?;
        fs::write(slot.join("progress.sav"), b"opaque TotK progress")?;
        fs::write(profile.join("totk.ini"), b"local settings")?;
        fs::create_dir_all(profile.join("cache"))?;
        fs::write(profile.join("cache/GPU"), b"device cache")?;
        let snapshot = snapshot(profile)?.context("Missing snapshot")?;
        let files = decode(&snapshot.bytes)?;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[&format!("{ACCOUNT}/slot_00/progress.sav")],
            b"opaque TotK progress"
        );
        assert_eq!(snapshot.hash, sha256(&snapshot.bytes));
        Ok(())
    }

    #[test]
    fn unchanged_save_has_a_stable_hash() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let slot = temporary.path().join("save").join(ACCOUNT);
        fs::create_dir_all(&slot)?;
        fs::write(temporary.path().join("account.uid"), 1_u128.to_le_bytes())?;
        fs::write(slot.join("progress.sav"), b"same progress")?;
        assert_eq!(
            snapshot(temporary.path())?.map(|save| save.hash),
            snapshot(temporary.path())?.map(|save| save.hash)
        );
        Ok(())
    }

    #[test]
    fn unfinished_game_commit_prevents_sync() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        fs::create_dir_all(temporary.path().join("save-tx/ready-7"))?;
        assert!(snapshot(temporary.path()).is_err());
        Ok(())
    }

    #[test]
    fn unsafe_or_non_save_paths_are_rejected() {
        for name in [
            "../totk.ini",
            "/etc/passwd",
            "totk.ini",
            "save/../settings",
            "C:/save",
            "00000000000000000000000000000001/CON.txt",
            "00000000000000000000000000000001/../cache",
        ] {
            assert!(validate_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn absent_save_is_not_an_empty_save_to_transmit() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        assert!(snapshot(temporary.path())?.is_none());
        Ok(())
    }

    fn fixture(root: &Path, progress: Option<&[u8]>) -> Result<Channel> {
        let install = root.join("game");
        fs::create_dir_all(install.join("profile"))?;
        fs::write(install.join("totk.exe"), b"not executed")?;
        fs::write(install.join("profile/account.uid"), 1_u128.to_le_bytes())?;
        if let Some(progress) = progress {
            let slot = install.join("profile/save").join(ACCOUNT).join("slot_00");
            fs::create_dir_all(&slot)?;
            fs::write(slot.join("progress.sav"), progress)?;
        }
        let mut channel = Channel::new(root.join("sync"), Ok(install));
        channel.monitor = ProcessMonitor::for_test(false);
        Ok(channel)
    }

    #[test]
    fn approved_transfer_seeds_an_empty_pc_without_copying_settings() -> Result<()> {
        let left_root = tempfile::tempdir()?;
        let right_root = tempfile::tempdir()?;
        let mut left = fixture(left_root.path(), Some(&vec![7; CHUNK_SIZE * 3]))?;
        let mut right = fixture(right_root.path(), None)?;
        fs::write(
            right
                .profile()?
                .parent()
                .context("Missing install")?
                .join("totk.ini"),
            b"local GPU settings",
        )?;
        left.handle(right.state_event()?)?;
        for event in left.offer()? {
            right.handle(event)?;
        }
        assert!(snapshot(&right.profile()?)?.is_none());
        for approval in right.accept(true)? {
            for frame in left.handle(approval)? {
                for receipt in right.handle(frame)? {
                    left.handle(receipt)?;
                }
            }
        }
        assert_eq!(
            snapshot(&left.profile()?)?.map(|save| save.hash),
            snapshot(&right.profile()?)?.map(|save| save.hash)
        );
        assert_eq!(
            fs::read(
                right
                    .profile()?
                    .parent()
                    .context("Missing install")?
                    .join("totk.ini")
            )?,
            b"local GPU settings"
        );
        assert!(!left.status.busy && !right.status.busy);
        Ok(())
    }

    #[test]
    fn declined_transfer_leaves_both_branches_unchanged() -> Result<()> {
        let left_root = tempfile::tempdir()?;
        let right_root = tempfile::tempdir()?;
        let mut left = fixture(left_root.path(), Some(b"left progress"))?;
        let mut right = fixture(right_root.path(), Some(b"right progress"))?;
        left.handle(right.state_event()?)?;
        let old = snapshot(&right.profile()?)?.map(|save| save.hash);
        for event in left.offer()? {
            right.handle(event)?;
        }
        for event in right.accept(false)? {
            left.handle(event)?;
        }
        assert_eq!(snapshot(&right.profile()?)?.map(|save| save.hash), old);
        Ok(())
    }

    #[test]
    fn replaced_progress_has_an_intact_rollback_copy() -> Result<()> {
        let source_root = tempfile::tempdir()?;
        let destination_root = tempfile::tempdir()?;
        let source = fixture(source_root.path(), Some(b"incoming progress"))?;
        let destination = fixture(destination_root.path(), Some(b"existing progress"))?;
        let incoming = snapshot(&source.profile()?)?.context("Missing incoming save")?;
        let current = snapshot(&destination.profile()?)?.context("Missing existing save")?;
        let backup = apply(
            &destination.profile()?,
            &incoming.bytes,
            Some(&current.hash),
        )?
        .context("Missing backup")?;
        assert_eq!(
            fs::read(backup.join(ACCOUNT).join("slot_00/progress.sav"))?,
            b"existing progress"
        );
        assert_eq!(
            snapshot(&destination.profile()?)?.map(|save| save.hash),
            Some(incoming.hash)
        );
        Ok(())
    }

    #[test]
    fn changed_progress_rejects_a_stale_approval() -> Result<()> {
        let left_root = tempfile::tempdir()?;
        let right_root = tempfile::tempdir()?;
        let mut left = fixture(left_root.path(), Some(b"left progress"))?;
        let mut right = fixture(right_root.path(), Some(b"right progress"))?;
        left.handle(right.state_event()?)?;
        for event in left.offer()? {
            right.handle(event)?;
        }
        fs::write(
            right
                .profile()?
                .join("save")
                .join(ACCOUNT)
                .join("slot_00/progress.sav"),
            b"newer local progress",
        )?;
        assert!(right.accept(true).is_err());
        assert!(right.status.pending);
        right.accept(false)?;
        assert!(!right.status.busy);
        assert_eq!(
            fs::read(
                right
                    .profile()?
                    .join("save")
                    .join(ACCOUNT)
                    .join("slot_00/progress.sav")
            )?,
            b"newer local progress"
        );
        Ok(())
    }

    #[test]
    fn chunks_without_receiver_approval_are_rejected() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut channel = fixture(root.path(), Some(b"existing progress"))?;
        assert!(
            channel
                .handle(Event::TotkChunk {
                    hash: "a".repeat(64),
                    bytes: vec![1]
                })
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn incoming_progress_maps_to_the_local_account_without_replacing_its_identity() -> Result<()> {
        let source_root = tempfile::tempdir()?;
        let destination_root = tempfile::tempdir()?;
        let source = fixture(source_root.path(), Some(b"incoming progress"))?;
        let destination = fixture(destination_root.path(), None)?;
        let profile = destination.profile()?;
        let local_account = 2_u128.to_le_bytes();
        fs::write(profile.join("account.uid"), local_account)?;
        let inactive = profile.join("save").join(ACCOUNT).join("slot_00");
        fs::create_dir_all(&inactive)?;
        fs::write(
            inactive.join("progress.sav"),
            b"inactive account, do not overwrite",
        )?;
        let incoming = snapshot(&source.profile()?)?.context("Missing source save")?;
        apply(&profile, &incoming.bytes, None)?;
        assert_eq!(fs::read(profile.join("account.uid"))?, local_account);
        assert_eq!(
            fs::read(
                profile
                    .join("save")
                    .join(account_name(&local_account))
                    .join("slot_00/progress.sav")
            )?,
            b"incoming progress"
        );
        assert_eq!(
            fs::read(inactive.join("progress.sav"))?,
            b"inactive account, do not overwrite"
        );
        assert_eq!(
            snapshot(&profile)?.map(|save| save.hash),
            Some(incoming.hash)
        );
        Ok(())
    }

    #[test]
    fn first_import_creates_a_local_account_that_can_load_the_received_progress() -> Result<()> {
        let source_root = tempfile::tempdir()?;
        let receiver_root = tempfile::tempdir()?;
        let source = fixture(source_root.path(), Some(b"incoming progress"))?;
        let incoming = snapshot(&source.profile()?)?.context("Missing source save")?;
        let profile = receiver_root.path().join("profile");
        assert!(apply(&profile, &incoming.bytes, None)?.is_none());
        let account = read_account(&profile)?.context("Missing receiving account")?;
        assert_eq!(
            fs::read(
                profile
                    .join("save")
                    .join(account_name(&account))
                    .join("slot_00/progress.sav")
            )?,
            b"incoming progress"
        );
        assert_eq!(
            snapshot(&profile)?.map(|save| save.hash),
            Some(incoming.hash)
        );
        Ok(())
    }

    #[test]
    fn missing_account_metadata_never_guesses_which_existing_save_to_replace() -> Result<()> {
        let root = tempfile::tempdir()?;
        let channel = fixture(root.path(), Some(b"existing progress"))?;
        fs::remove_file(channel.profile()?.join("account.uid"))?;
        assert!(snapshot(&channel.profile()?).is_err());
        Ok(())
    }

    #[test]
    fn compact_history_restores_a_complete_prior_save_and_retains_the_newer_branch() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut channel = fixture(root.path(), Some(b"first progress"))?;
        let profile = channel.profile()?;
        let first = snapshot(&profile)?.context("Missing first save")?;
        channel.capture(&first, history::Reason::Observed)?;
        let id = channel
            .history()?
            .find_hash(&first.hash)?
            .context("Missing history ID")?;
        fs::write(
            profile
                .join("save")
                .join(ACCOUNT)
                .join("slot_00/progress.sav"),
            b"newer progress",
        )?;
        let newer = snapshot(&profile)?.context("Missing newer save")?;
        channel.capture(&newer, history::Reason::Observed)?;
        channel.restore(&id, Some(&newer.hash))?;
        assert_eq!(snapshot(&profile)?.map(|save| save.hash), Some(first.hash));
        let newer_id = channel
            .history()?
            .find_hash(&newer.hash)?
            .context("Newer branch was lost")?;
        assert_eq!(channel.history()?.read(&newer_id)?, newer.bytes);
        assert!(channel.status.history.stats.logical_bytes > 0);
        assert!(
            channel
                .status
                .history
                .entries
                .iter()
                .all(|entry| entry.game == "TOTK")
        );
        Ok(())
    }

    #[test]
    fn common_base_fast_forward_is_automatic_but_two_changed_branches_conflict() -> Result<()> {
        use std::collections::VecDeque;
        let left_root = tempfile::tempdir()?;
        let right_root = tempfile::tempdir()?;
        let mut left = fixture(left_root.path(), Some(b"common base"))?;
        let mut right = fixture(right_root.path(), Some(b"common base"))?;
        left.refresh()?;
        right.refresh()?;
        left.handle(right.shared_state())?;
        right.handle(left.shared_state())?;
        for channel in [&mut left, &mut right] {
            channel.stable_since = Instant::now() - Duration::from_secs(3);
            channel.tick(true)?;
        }
        assert!(left.session.base.is_some() && left.session.base == right.session.base);
        fs::write(
            left.profile()?
                .join("save")
                .join(ACCOUNT)
                .join("slot_00/progress.sav"),
            b"one-sided progress",
        )?;
        left.refresh()?;
        left.stable_since = Instant::now() - Duration::from_secs(3);
        left.last_poll = Instant::now() - Duration::from_secs(4);
        let mut frames: VecDeque<_> = left
            .tick(true)?
            .into_iter()
            .map(|event| (false, event))
            .collect();
        while let Some((to_left, event)) = frames.pop_front() {
            let replies = if to_left {
                left.handle(event)?
            } else {
                right.handle(event)?
            };
            frames.extend(replies.into_iter().map(|event| (!to_left, event)));
        }
        assert_eq!(
            snapshot(&left.profile()?)?.map(|save| save.hash),
            snapshot(&right.profile()?)?.map(|save| save.hash)
        );
        assert!(!right.status.pending);
        fs::write(
            left.profile()?
                .join("save")
                .join(ACCOUNT)
                .join("slot_00/progress.sav"),
            b"left branch",
        )?;
        fs::write(
            right
                .profile()?
                .join("save")
                .join(ACCOUNT)
                .join("slot_00/progress.sav"),
            b"right branch",
        )?;
        left.refresh()?;
        right.refresh()?;
        left.handle(right.shared_state())?;
        left.stable_since = Instant::now() - Duration::from_secs(3);
        left.last_poll = Instant::now() - Duration::from_secs(4);
        assert!(
            !left
                .tick(true)?
                .iter()
                .any(|frame| matches!(frame, Event::SaveOffer { .. }))
        );
        assert!(left.status.conflict);
        Ok(())
    }

    fn paired_channels(left_root: &Path, right_root: &Path) -> Result<(Channel, Channel)> {
        let mut left = fixture(left_root, Some(b"matching progress"))?;
        let mut right = fixture(right_root, Some(b"matching progress"))?;
        left.identify(&"a".repeat(64), "left");
        right.identify(&"b".repeat(64), "right");
        left.bind_peer(&"b".repeat(64))?;
        right.bind_peer(&"a".repeat(64))?;
        left.refresh()?;
        right.refresh()?;
        left.handle(right.shared_state())?;
        right.handle(left.shared_state())?;
        left.test_launch = true;
        right.test_launch = true;
        Ok((left, right))
    }

    #[test]
    fn coordinated_play_requires_a_grant_and_releases_both_durable_locks() -> Result<()> {
        use std::collections::VecDeque;
        let left_root = tempfile::tempdir()?;
        let right_root = tempfile::tempdir()?;
        let (mut left, mut right) = paired_channels(left_root.path(), right_root.path())?;
        let requests = left.launch(true, false)?;
        assert!(!left.status.running && left.session.lock.is_some());
        let mut frames: VecDeque<_> = requests.into_iter().map(|event| (false, event)).collect();
        while let Some((to_left, event)) = frames.pop_front() {
            let replies = if to_left {
                left.handle(event)?
            } else {
                right.handle(event)?
            };
            frames.extend(replies.into_iter().map(|event| (!to_left, event)));
        }
        assert!(left.status.running);
        assert!(right.launch(true, false).is_err());
        assert!(
            Session::load(&left.root)?.lock.is_some() && Session::load(&right.root)?.lock.is_some()
        );
        left.monitor = ProcessMonitor::for_test(false);
        left.stable_since = Instant::now() - Duration::from_secs(3);
        left.last_poll = Instant::now() - Duration::from_secs(4);
        frames.extend(left.tick(true)?.into_iter().map(|event| (false, event)));
        while let Some((to_left, event)) = frames.pop_front() {
            let replies = if to_left {
                left.handle(event)?
            } else {
                right.handle(event)?
            };
            frames.extend(replies.into_iter().map(|event| (!to_left, event)));
        }
        assert!(left.session.lock.is_none() && right.session.lock.is_none());
        assert!(left.session.release.is_none());
        Ok(())
    }

    #[test]
    fn interrupted_reservations_survive_restart_and_require_bilateral_recovery() -> Result<()> {
        let left_root = tempfile::tempdir()?;
        let right_root = tempfile::tempdir()?;
        let (mut left, mut right) = paired_channels(left_root.path(), right_root.path())?;
        for request in left.launch(true, false)? {
            right.handle(request)?;
        }
        left.reset();
        let session = Session::load(&left.root)?;
        assert!(session.lock.is_some());
        let mut restarted = Channel::new(left.root.clone(), Ok(left_root.path().join("game")));
        restarted.identify(&"a".repeat(64), "left");
        restarted.monitor = ProcessMonitor::for_test(false);
        restarted.bind_peer(&"b".repeat(64))?;
        assert!(restarted.launch(false, true).is_err());
        for request in restarted.recover()? {
            right.handle(request)?;
        }
        assert!(right.session.lock.is_some() && restarted.session.lock.is_some());
        assert!(right.status.recovery_pending);
        for approval in right.approve_recovery(true)? {
            restarted.handle(approval)?;
        }
        assert!(right.session.lock.is_none() && restarted.session.lock.is_none());
        assert!(!right.status.recovery_pending);
        Ok(())
    }

    #[test]
    fn offline_recovery_only_releases_this_pcs_lock_and_keeps_the_partner_reservation() -> Result<()>
    {
        let left_root = tempfile::tempdir()?;
        let right_root = tempfile::tempdir()?;
        let (mut left, mut right) = paired_channels(left_root.path(), right_root.path())?;
        for request in left.launch(true, false)? {
            right.handle(request)?;
        }
        left.reset();
        right.reset();
        assert!(right.recover_offline().is_err());
        left.recover_offline()?;
        assert!(left.session.lock.is_none() && left.session.release.is_some());
        assert!(right.session.lock.is_some());
        left.launch(false, true)?;
        assert!(left.status.running && left.session.release.is_some());
        Ok(())
    }

    fn exchange(
        left: &mut Channel,
        right: &mut Channel,
        frames: Vec<Event>,
        to_left: bool,
    ) -> Result<()> {
        let mut frames: std::collections::VecDeque<_> =
            frames.into_iter().map(|event| (to_left, event)).collect();
        let mut count = 0;
        while let Some((to_left, event)) = frames.pop_front() {
            count += 1;
            ensure!(count < 20_000, "Game protocol did not settle");
            let replies = if to_left {
                left.handle(event)?
            } else {
                right.handle(event)?
            };
            frames.extend(replies.into_iter().map(|event| (!to_left, event)));
        }
        Ok(())
    }

    #[test]
    fn verified_fork_previews_preserve_both_branches_and_merge_an_explicit_choice() -> Result<()> {
        let left_root = tempfile::tempdir()?;
        let right_root = tempfile::tempdir()?;
        let (mut left, mut right) = paired_channels(left_root.path(), right_root.path())?;
        for channel in [&mut left, &mut right] {
            channel.stable_since = Instant::now() - Duration::from_secs(3);
            channel.tick(true)?;
        }
        fs::write(
            left.profile()?
                .join("save")
                .join(ACCOUNT)
                .join("slot_00/progress.sav"),
            b"left branch",
        )?;
        fs::write(
            right
                .profile()?
                .join("save")
                .join(ACCOUNT)
                .join("slot_00/progress.sav"),
            b"right branch",
        )?;
        left.refresh()?;
        right.refresh()?;
        left.handle(right.shared_state())?;
        right.handle(left.shared_state())?;
        let left_hash = left.status.local_hash.clone().context("No local branch")?;
        let right_hash = right
            .status
            .local_hash
            .clone()
            .context("No remote branch")?;
        left.stable_since = Instant::now() - Duration::from_secs(3);
        left.last_poll = Instant::now() - Duration::from_secs(4);
        let frames = left.tick(true)?;
        exchange(&mut left, &mut right, frames, false)?;
        assert!(left.status.comparison_verified && left.status.peer_branch.is_some());
        assert_eq!(
            snapshot(&left.profile()?)?.map(|save| save.hash),
            Some(left_hash.clone())
        );
        assert_eq!(
            snapshot(&right.profile()?)?.map(|save| save.hash),
            Some(right_hash.clone())
        );
        let frames = left.resolve(&left_hash, &right_hash, true)?;
        exchange(&mut left, &mut right, frames, false)?;
        assert_eq!(
            snapshot(&left.profile()?)?.map(|save| save.hash),
            Some(right_hash)
        );
        assert!(left.history()?.find_hash(&left_hash)?.is_some());
        assert!(
            left.status
                .history
                .entries
                .iter()
                .any(|entry| entry.merge_parent.is_some())
        );
        Ok(())
    }

    #[test]
    fn app_updates_preserve_durable_totk_reservations_without_unlocking_play() -> Result<()> {
        let left_root = tempfile::tempdir()?;
        let right_root = tempfile::tempdir()?;
        let (mut left, mut right) = paired_channels(left_root.path(), right_root.path())?;
        for request in left.launch(true, false)? {
            right.handle(request)?;
        }
        left.reset();
        right.reset();
        let left_record = fs::read(left.root.join("session.json"))?;
        let right_record = fs::read(right.root.join("session.json"))?;
        assert!(left.require_app_update_idle().is_ok());
        assert!(right.require_app_update_idle().is_ok());
        assert!(left.require_update_idle().is_err());
        assert!(right.require_update_idle().is_err());
        assert_eq!(fs::read(left.root.join("session.json"))?, left_record);
        assert_eq!(fs::read(right.root.join("session.json"))?, right_record);
        left.status.busy = true;
        assert!(left.require_app_update_idle().is_err());
        left.status.busy = false;
        left.monitor = ProcessMonitor::for_test(true);
        assert!(left.require_app_update_idle().is_err());
        Ok(())
    }

    #[test]
    fn running_game_prevents_save_offers() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut channel = fixture(root.path(), Some(b"existing progress"))?;
        channel.monitor = ProcessMonitor::for_test(true);
        assert!(channel.offer().is_err());
        Ok(())
    }
}

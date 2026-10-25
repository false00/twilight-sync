use crate::{
    audit::Audit,
    config::{self, Identity, Peer, PlayLock, Settings},
    disc::{self, Format as DiscFormat, Verified as VerifiedDisc},
    files::{self, CHUNK_SIZE, ensure_safe_path, valid_hash},
    history::{self, History},
    install::{self, Installed, ReleaseInfo},
    lan::{self, Advertisement, Discovered, Network},
    play::{self, ProcessMonitor},
    protocol::{self, Event, Handshake, SecureWriter},
    save_data, save_location, self_update,
    textures::{self, Activation, Pack},
};
use anyhow::{Context, Result, bail, ensure};
use crossbeam_channel::{Receiver, Sender, bounded};
use rand::RngCore;
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    process::Child,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::{NamedTempFile, TempPath};
use xxhash_rust::xxh3::Xxh3;

#[derive(Clone)]
pub struct Snapshot {
    pub settings: Settings,
    pub public_key: String,
    pub network: Option<Network>,
    pub devices: Vec<Discovered>,
    pub peer_name: Option<String>,
    pub comparison_code: Option<String>,
    pub local_approved: bool,
    pub remote_approved: bool,
    pub ready: bool,
    pub connecting: bool,
    pub pairing_open: bool,
    pub local_hash: Option<String>,
    pub local_save: Option<save_data::Summary>,
    pub conflict: Option<SaveConflict>,
    pub save_profile: Option<PathBuf>,
    pub save_ready: bool,
    pub remote_hash: Option<String>,
    pub game_running: bool,
    pub remote_game: bool,
    pub pending_save: bool,
    pub pending_iso: bool,
    pub pending_disc_format: Option<DiscFormat>,
    pub pending_recovery: bool,
    pub transfer: String,
    pub install_busy: bool,
    pub latest_release: Option<ReleaseInfo>,
    pub selected_release: Option<String>,
    pub texture_busy: bool,
    pub textures: textures::State,
    pub history: history::Page,
    pub self_updates: self_update::State,
    pub message: String,
    pub error: Option<String>,
    pub shutdown: bool,
}

#[derive(Clone, Debug)]
pub struct SaveConflict {
    pub local_hash: String,
    pub remote_hash: String,
    pub base_hash: Option<String>,
    pub base: Option<save_data::Summary>,
    pub remote: Option<save_data::Summary>,
}

pub enum Command {
    DesktopError(String),
    DesktopRecovered(String),
    TrustNetwork,
    OpenPairing,
    Connect(String),
    ApprovePairing,
    RejectPairing,
    SetExecutable(PathBuf),
    SetIso(PathBuf),
    SetIsoEnabled(bool),
    SetStartup(bool),
    SendSave,
    AcceptSave(bool),
    Launch,
    ContinueLocal {
        expected: Option<String>,
    },
    ResolveConflict {
        local_hash: String,
        remote_hash: String,
        use_remote: bool,
    },
    Recover,
    AcceptRecovery(bool),
    SendIso,
    AcceptIso(Option<PathBuf>),
    Install,
    CheckRelease,
    CheckSelfUpdate,
    SetAutoUpdateChecks(bool),
    UpdateSelf,
    OpenTextureCreator(String),
    CancelTextures,
    ImportTextures {
        source: String,
        version: String,
        path: PathBuf,
        preserve_unmanaged: bool,
    },
    ActivateTextures {
        id: Option<String>,
        preserve_unmanaged: bool,
    },
    RecoverTextures,
    RefreshTextures,
    HistoryPage(Option<String>),
    RestoreSnapshot {
        id: String,
        expected: Option<String>,
    },
    Unpair,
    Shutdown,
}

#[derive(Clone)]
pub struct Backend {
    sender: Sender<Input>,
    pub view: Arc<Mutex<Snapshot>>,
}

fn clear_desktop_error(error: &mut Option<String>, detail: &str) {
    if error.as_deref() == Some(detail) {
        *error = None;
    }
}

impl Backend {
    pub fn command(&self, command: Command) -> Result<()> {
        self.sender
            .try_send(Input::Command(command))
            .context("The manager is busy or has stopped")
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        self.view
            .lock()
            .map(|s| s.clone())
            .map_err(|_| anyhow::anyhow!("Dashboard state is unavailable"))
    }
}

enum Input {
    Command(Command),
    Network(Result<Network>),
    Connected {
        id: u64,
        session: Box<Handshake>,
        expected: Option<String>,
        network: String,
    },
    ConnectFailed(String),
    Frame {
        id: u64,
        event: Event,
    },
    Disconnected {
        id: u64,
        reason: String,
    },
    Installed(Result<(Installed, PathBuf)>),
    ReleaseChecked(Result<ReleaseInfo>),
    SelfUpdateChecked(Result<self_update::Release>),
    SelfUpdatePrepared(Result<self_update::Prepared>),
    SelfUpdateCancelled(Result<()>),
    TextureProgress {
        files: usize,
        bytes: u64,
    },
    TextureStatus {
        message: String,
        cancellable: bool,
    },
    TextureDone(Result<TextureOutcome>),
    IsoPrepared {
        id: u64,
        path: PathBuf,
        result: Result<VerifiedDisc>,
    },
    IsoVerified {
        channel: u64,
        pending: Box<PendingDisc>,
        result: Result<VerifiedDisc>,
    },
    IsoSent {
        id: u64,
        result: Result<()>,
    },
}

enum TextureOutcome {
    Installed { pack: Pack, activation: Activation },
    Activated(Activation),
    Recovered(Vec<PathBuf>),
}

struct Link {
    id: u64,
    key: String,
    name: Option<String>,
    writer: Arc<Mutex<SecureWriter>>,
    comparison_code: String,
    local_approved: bool,
    remote_approved: bool,
    committed: bool,
    ready: bool,
    deadline: Instant,
    last_seen: Instant,
}

#[derive(Clone)]
struct RemoteState {
    hash: Option<String>,
    save_ready: bool,
    game: bool,
    lock: Option<PlayLock>,
}

struct PendingSave {
    hash: String,
    expected: Option<String>,
    bytes: Vec<u8>,
    lease: Option<String>,
    deadline: Instant,
}

struct PeerPreview {
    hash: String,
    base: Option<String>,
    summary: save_data::Summary,
    base_summary: Option<save_data::Summary>,
    bytes: Vec<u8>,
}

struct OutgoingIso {
    id: String,
    path: PathBuf,
    hash: String,
    size: u64,
    format: DiscFormat,
    started: bool,
    sent: bool,
    deadline: Instant,
}

struct IsoOffer {
    id: String,
    hash: String,
    size: u64,
    format: DiscFormat,
    deadline: Instant,
}

struct IncomingIso {
    id: String,
    hash: String,
    size: u64,
    format: DiscFormat,
    received: u64,
    sha: Sha256,
    official: Xxh3,
    header: Vec<u8>,
    temporary: NamedTempFile,
    destination: PathBuf,
    last_chunk: Instant,
}

struct PendingDisc {
    id: String,
    hash: String,
    size: u64,
    format: DiscFormat,
    temporary: TempPath,
    destination: PathBuf,
}

struct DiscVerification {
    channel: u64,
    id: String,
    deadline: Instant,
}

struct Engine {
    root: PathBuf,
    settings: Settings,
    default_profile: PathBuf,
    save_profile: Option<PathBuf>,
    save_location_error: Option<String>,
    identity: Arc<Identity>,
    audit: Audit,
    sender: Sender<Input>,
    view: Arc<Mutex<Snapshot>>,
    network: Option<Network>,
    listener: Option<TcpListener>,
    discovery: Option<UdpSocket>,
    devices: HashMap<String, Discovered>,
    link: Option<Link>,
    remote: Option<RemoteState>,
    remote_preview: Option<PeerPreview>,
    preview_requested: Option<(String, Instant)>,
    pairing_until: Option<Instant>,
    pairing_attempts: u8,
    connecting: Arc<AtomicBool>,
    network_job: bool,
    last_network: Instant,
    last_verified: Instant,
    last_discovery: Instant,
    last_connect: Instant,
    last_ping: Instant,
    last_process: Instant,
    process: ProcessMonitor,
    game_running: bool,
    local_hash: Option<String>,
    local_save: Option<save_data::Summary>,
    previous_hash: Option<String>,
    save_valid: bool,
    stable: u8,
    stable_since: Instant,
    child: Option<Child>,
    managed_token: Option<String>,
    launch_pending: Option<String>,
    release_sent: bool,
    pending_save: Option<PendingSave>,
    sent_save: Option<(String, Instant)>,
    pending_recovery: Option<String>,
    requested_recovery: Option<String>,
    iso_offer: Option<IsoOffer>,
    incoming_iso: Option<IncomingIso>,
    outgoing_iso: Option<OutgoingIso>,
    iso_preparing: bool,
    iso_verifying: Option<DiscVerification>,
    cancel_transfer: Arc<AtomicBool>,
    install_busy: bool,
    latest_release: Option<ReleaseInfo>,
    selected_release: Option<String>,
    texture_busy: bool,
    texture_cancel: Arc<AtomicBool>,
    texture_state: textures::State,
    textures_loaded: bool,
    history: Option<History>,
    history_page: history::Page,
    history_candidate: Option<(String, Instant)>,
    history_last_hash: Option<String>,
    history_cursor: Option<String>,
    migrated_backups: bool,
    self_updates: self_update::State,
    last_self_update_check: Option<Instant>,
    first_self_update_check: Instant,
    last_self_update_cleanup: Instant,
    transfer: String,
    message: String,
    error: Option<String>,
    fatal: bool,
    shutdown: bool,
    discovery_rejections: u64,
    last_rejection_log: Instant,
}

fn token() -> String {
    let mut bytes = [0; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn texture_game_stopped() -> Result<()> {
    ensure!(
        !ProcessMonitor::default().game_running(),
        "Stop every Dusklight process before changing textures"
    );
    Ok(())
}

fn texture_profile_stopped(
    view: &Arc<Mutex<Snapshot>>,
    default_profile: &Path,
    executable: Option<&Path>,
    profile: &Path,
) -> Result<()> {
    texture_game_stopped()?;
    let snapshot = view
        .lock()
        .map_err(|_| anyhow::anyhow!("Texture status lock is poisoned"))?;
    ensure!(
        !snapshot.remote_game,
        "Stop Dusklight on the paired device before applying textures"
    );
    drop(snapshot);
    ensure!(
        save_location::profile(default_profile, executable)? == profile,
        "Dusklight's profile changed during texture installation; no pack was applied to another profile"
    );
    Ok(())
}

pub fn start() -> Result<Backend> {
    let root = config::root_directory()?;
    let instance = config::acquire_instance(&root)?;
    let settings = Settings::load(&root)?;
    let default_profile = save_location::default_profile()?;
    let audit = Audit::new(&root)?;
    let identity = Arc::new(Identity::load(&root)?);
    settings.persist(&root)?;
    config::set_startup(settings.startup)?;
    audit.record(
        "manager-started",
        None,
        None,
        None,
        "Startup preference applied; no network trusted implicitly",
    )?;
    let (sender, receiver) = bounded(128);
    let mut initial = initial_snapshot(&settings, &identity.public);
    initial.settings.save_file = None;
    let view = Arc::new(Mutex::new(initial));
    let mut engine = Engine::new(
        root,
        settings,
        default_profile,
        identity,
        audit,
        sender.clone(),
        view.clone(),
    );
    thread::Builder::new()
        .name("twilight-manager".into())
        .spawn(move || {
            let _instance = instance;
            if let Err(error) = engine.refresh_default_save() {
                engine.report(error);
            }
            if let Err(error) = engine.refresh_history() {
                engine.report(error);
            }
            if let Err(error) = engine.reload_textures() {
                engine.report(error);
            }
            if let Err(error) = engine.initialize_self_updates() {
                engine.report(error);
            }
            engine.message = "Local stores checked. Checking your private LAN...".into();
            engine.publish();
            engine.run(receiver);
        })?;
    Ok(Backend { sender, view })
}

#[cfg(any(debug_assertions, test))]
pub fn ui_smoke_backend(scenario: &str) -> Result<Backend> {
    ensure!(
        matches!(
            scenario,
            "offline" | "ready" | "pairing" | "connecting" | "attention"
        ),
        "Unknown UI smoke scenario"
    );
    let settings = Settings {
        startup: false,
        name: "UI smoke test".into(),
        ..Settings::default()
    };
    let mut snapshot = initial_snapshot(&settings, &"0".repeat(64));
    snapshot.message =
        "Approve your home network, then pair your other PC to start syncing.".into();
    snapshot.settings.save_file = Some(PathBuf::from(
        r"C:\Users\Player\AppData\Roaming\TwilitRealm\Dusklight\USA\Card A\01-GZ2E-gczelda2.gci",
    ));
    snapshot.save_profile = Some(PathBuf::from(
        r"C:\Users\Player\AppData\Roaming\TwilitRealm\Dusklight",
    ));
    snapshot.save_ready = true;
    if scenario != "offline" {
        let network = Network::from_parts(
            "192.168.10.20".parse()?,
            24,
            "192.168.10.1".parse()?,
            "02-AA-BB-CC-DD-EE",
            "Wi-Fi".into(),
        )?;
        snapshot.settings.network = Some(network.fingerprint.clone());
        snapshot.network = Some(network);
        snapshot.settings.game_exe = Some(PathBuf::from(r"C:\Games\Dusklight\dusklight.exe"));
        snapshot.settings.texture_profile =
            Some(PathBuf::from(r"C:\Users\Player\AppData\Roaming\Dusklight"));
        snapshot.selected_release = Some("v2.0.3".into());
        snapshot.local_hash = Some("a".repeat(64));
        snapshot.history.stats.commits = 12;
        snapshot.history.stats.blocks = 24;
        snapshot.history.stats.stored_bytes = 104_256;
        snapshot.history.stats.logical_bytes = 12 * files::SAVE_SIZE as u64;
        for sequence in (10..=12).rev() {
            snapshot.history.entries.push(history::Entry {
                id: format!("{sequence:064x}"),
                sequence,
                parent: Some(format!("{:064x}", sequence - 1)),
                merge_parent: None,
                game: "GZ2E".into(),
                name: "Adventure".into(),
                hash: if sequence == 12 {
                    "a".repeat(64)
                } else {
                    format!("{sequence:064x}")
                },
                timestamp: format!("2026-10-20T14:{sequence:02}:00Z"),
                username: "Player".into(),
                device: "0".repeat(64),
                peer: None,
                reason: history::Reason::Observed,
                restored_from: None,
                summary: None,
                changes: Vec::new(),
            });
        }
        snapshot.textures.packs.push(Pack {
            id: "c".repeat(64),
            source: "tp-definitive-edition-plus".into(),
            version: "1.2.0".into(),
            imported: "2026-10-20T12:00:00Z".into(),
            files: 4200,
            bytes: 512 * 1024 * 1024,
            skipped: 2,
        });
        snapshot.textures.active = Some("c".repeat(64));
        if matches!(scenario, "ready" | "attention") {
            snapshot.settings.peer = Some(Peer {
                name: "Legion Go".into(),
                public_key: "b".repeat(64),
            });
            snapshot.peer_name = Some("Legion Go".into());
            snapshot.ready = true;
            snapshot.remote_hash = snapshot.local_hash.clone();
            snapshot.message = "Save copies match. Protected play is available.".into();
        } else if scenario == "pairing" {
            snapshot.peer_name = Some("Legion Go".into());
            snapshot.comparison_code = Some("1234 5678".into());
            snapshot.pairing_open = true;
            snapshot.remote_approved = true;
        } else if scenario == "connecting" {
            snapshot.connecting = true;
            snapshot.pairing_open = true;
        }
        if scenario == "attention" {
            snapshot.pending_save = true;
            snapshot.error =
                Some("Preview: the two save copies differ. Choose which copy to keep.".into());
        }
    }
    let view = Arc::new(Mutex::new(snapshot));
    let (sender, receiver) = bounded(16);
    let worker_view = view.clone();
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(Input::Command(Command::Shutdown)) => break,
                Ok(Input::Command(Command::DesktopError(detail))) => {
                    eprintln!("{detail}");
                    match worker_view.lock() {
                        Ok(mut view) => view.error = Some(detail),
                        Err(error) => {
                            eprintln!("UI smoke state failure: {error}");
                            return;
                        }
                    }
                }
                Ok(Input::Command(Command::DesktopRecovered(detail))) => match worker_view.lock() {
                    Ok(mut view) => {
                        clear_desktop_error(&mut view.error, &detail);
                    }
                    Err(error) => {
                        eprintln!("UI smoke state failure: {error}");
                        return;
                    }
                },
                Ok(_) => match worker_view.lock() {
                    Ok(mut view) => {
                        view.error = Some("UI smoke mode does not perform real actions.".into())
                    }
                    Err(error) => {
                        eprintln!("UI smoke state failure: {error}");
                        return;
                    }
                },
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
        }
        match worker_view.lock() {
            Ok(mut view) => view.shutdown = true,
            Err(error) => eprintln!("UI smoke state failure: {error}"),
        }
    });
    Ok(Backend { sender, view })
}

fn initial_snapshot(settings: &Settings, key: &str) -> Snapshot {
    Snapshot {
        settings: settings.clone(),
        public_key: key.into(),
        network: None,
        devices: Vec::new(),
        peer_name: None,
        comparison_code: None,
        local_approved: false,
        remote_approved: false,
        ready: false,
        connecting: false,
        pairing_open: false,
        local_hash: None,
        local_save: None,
        conflict: None,
        save_profile: None,
        save_ready: false,
        remote_hash: None,
        game_running: false,
        remote_game: false,
        pending_save: false,
        pending_iso: false,
        pending_disc_format: None,
        pending_recovery: false,
        transfer: String::new(),
        install_busy: false,
        latest_release: None,
        selected_release: None,
        texture_busy: false,
        textures: textures::State::default(),
        history: history::Page::default(),
        self_updates: self_update::State::default(),
        message: "Loading compact history and checking your private LAN...".into(),
        error: None,
        shutdown: false,
    }
}

impl Engine {
    fn new(
        root: PathBuf,
        settings: Settings,
        default_profile: PathBuf,
        identity: Arc<Identity>,
        audit: Audit,
        sender: Sender<Input>,
        view: Arc<Mutex<Snapshot>>,
    ) -> Self {
        let now = Instant::now();
        let past = now.checked_sub(Duration::from_secs(60)).unwrap_or(now);
        Self {
            root,
            settings,
            default_profile,
            save_profile: None,
            save_location_error: None,
            identity,
            audit,
            sender,
            view,
            network: None,
            listener: None,
            discovery: None,
            devices: HashMap::new(),
            link: None,
            remote: None,
            remote_preview: None,
            preview_requested: None,
            pairing_until: None,
            pairing_attempts: 0,
            connecting: Arc::new(AtomicBool::new(false)),
            network_job: false,
            last_network: past,
            last_verified: past,
            last_discovery: past,
            last_connect: past,
            last_ping: past,
            last_process: past,
            process: ProcessMonitor::default(),
            game_running: false,
            local_hash: None,
            local_save: None,
            previous_hash: None,
            save_valid: false,
            stable: 0,
            stable_since: Instant::now(),
            child: None,
            managed_token: None,
            launch_pending: None,
            release_sent: false,
            pending_save: None,
            sent_save: None,
            pending_recovery: None,
            requested_recovery: None,
            iso_offer: None,
            incoming_iso: None,
            outgoing_iso: None,
            iso_preparing: false,
            iso_verifying: None,
            cancel_transfer: Arc::new(AtomicBool::new(false)),
            install_busy: false,
            latest_release: None,
            selected_release: None,
            texture_busy: false,
            texture_cancel: Arc::new(AtomicBool::new(false)),
            texture_state: textures::State::default(),
            textures_loaded: false,
            history: None,
            history_page: history::Page::default(),
            history_candidate: None,
            history_last_hash: None,
            history_cursor: None,
            migrated_backups: false,
            self_updates: self_update::State::default(),
            last_self_update_check: None,
            first_self_update_check: now + Duration::from_secs(15),
            last_self_update_cleanup: Instant::now(),
            transfer: String::new(),
            message: "Loading compact history and checking your private LAN...".into(),
            error: None,
            fatal: false,
            shutdown: false,
            discovery_rejections: 0,
            last_rejection_log: past,
        }
    }

    fn run(&mut self, receiver: Receiver<Input>) {
        while !self.shutdown {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(input) => {
                    let wire = matches!(&input, Input::Frame { .. });
                    if let Err(error) = self.input(input) {
                        if wire {
                            self.disconnect(&format!("Protocol request rejected: {error:#}"));
                        }
                        self.report(error);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            if !self.fatal
                && !self.shutdown
                && let Err(error) = self.tick()
            {
                self.report(error);
            }
            self.publish();
        }
        self.disconnect("Manager stopped; any play lock remains durable");
        if let Err(error) = self.audit.record(
            "manager-stopped",
            None,
            None,
            None,
            "Persisted play locks were not cleared",
        ) {
            self.report(error);
        }
        self.shutdown = true;
        self.publish();
    }

    fn store(&mut self, update: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut next = self.settings.clone();
        update(&mut next);
        if let Err(error) = next.persist(&self.root) {
            self.fatal = true;
            self.disconnect("Settings persistence failed; networking disabled");
            return Err(error).context("Cannot persist safety state; restart after fixing storage");
        }
        self.settings = next;
        Ok(())
    }

    fn report(&mut self, error: anyhow::Error) {
        let detail = format!("{error:#}");
        if self.error.as_deref() != Some(&detail)
            && let Err(log_error) = self.audit.record(
                "error",
                None,
                None,
                self.settings.peer.as_ref().map(|p| p.public_key.as_str()),
                &detail,
            )
        {
            self.fatal = true;
            self.listener = None;
            self.discovery = None;
            self.disconnect("Audit storage failed; networking disabled");
            self.error = Some(format!("{detail}; AUDIT FAILURE: {log_error:#}"));
            return;
        }
        self.error = Some(detail);
    }

    fn input(&mut self, input: Input) -> Result<()> {
        match input {
            Input::Command(command) => {
                ensure!(
                    !self.fatal || matches!(command, Command::Shutdown),
                    "Safety storage failed; only Quit is available"
                );
                self.command(command)
            }
            Input::Network(result) => {
                self.network_job = false;
                match result {
                    Ok(network) => {
                        self.last_verified = Instant::now();
                        let changed = self.network.as_ref().is_some_and(|old| {
                            old.fingerprint != network.fingerprint || old.address != network.address
                        });
                        if changed {
                            self.stop_network("Router or local address changed");
                        }
                        self.network = Some(network);
                        self.configure_sockets()
                    }
                    Err(error) => {
                        self.stop_network("LAN fingerprint could not be verified");
                        self.network = None;
                        Err(error)
                    }
                }
            }
            Input::Connected {
                id,
                session,
                expected,
                network,
            } => {
                self.connecting.store(false, Ordering::Release);
                ensure!(
                    !self.fatal && self.trusted_network(),
                    "Network is no longer trusted"
                );
                ensure!(
                    self.network
                        .as_ref()
                        .is_some_and(|n| n.fingerprint == network),
                    "Network changed during handshake"
                );
                ensure!(self.link.is_none(), "Another channel is already active");
                self.establish(id, *session, expected)
            }
            Input::ConnectFailed(reason) => {
                self.connecting.store(false, Ordering::Release);
                bail!("Secure connection failed: {reason}")
            }
            Input::Frame { id, event } => {
                if self.link.as_ref().is_some_and(|l| l.id == id) {
                    self.link.as_mut().context("Missing channel")?.last_seen = Instant::now();
                    self.event(event)?;
                }
                Ok(())
            }
            Input::Disconnected { id, reason } => {
                if self.link.as_ref().is_some_and(|l| l.id == id) {
                    let waiting_rvz = self
                        .outgoing_iso
                        .as_ref()
                        .is_some_and(|image| image.format == DiscFormat::Rvz && !image.started);
                    self.disconnect(&reason);
                    if waiting_rvz {
                        bail!(
                            "Peer disconnected before RVZ approval: {reason}. RVZ transfer requires support on both devices; update both managers. Play locks were preserved."
                        );
                    }
                    bail!("Peer disconnected: {reason}. Play locks were preserved.");
                }
                Ok(())
            }
            Input::Installed(result) => {
                self.install_busy = false;
                let (installed, shortcut) = result?;
                self.audit.record(
                    "dusklight-installed",
                    Some(&installed.executable),
                    Some(&installed.sha256),
                    None,
                    &installed.tag,
                )?;
                self.audit.record(
                    "dusklight-desktop-shortcut",
                    Some(&shortcut),
                    None,
                    None,
                    "Opens Twilight Sync protected Play controls; does not launch the game directly",
                )?;
                self.selected_release = Some(installed.tag.clone());
                if self.latest_release.as_ref().is_some_and(|release| {
                    release.tag != installed.tag || release.sha256 != installed.sha256
                }) {
                    self.latest_release = None;
                }
                self.store(|s| s.game_exe = Some(installed.executable))?;
                self.refresh_default_save()?;
                self.message = format!(
                    "Dusklight {} {}. Desktop shortcut opens protected Play controls. Saves, profile, and texture selection were preserved; nothing was launched.",
                    installed.tag,
                    if installed.reused {
                        "is already installed; managed files re-verified"
                    } else {
                        "installed side-by-side"
                    },
                );
                Ok(())
            }
            Input::ReleaseChecked(result) => {
                self.install_busy = false;
                let release = result?;
                self.audit.record(
                    "dusklight-release-checked",
                    None,
                    Some(&release.sha256),
                    None,
                    &release.tag,
                )?;
                self.selected_release =
                    install::selected_release(&self.root, self.settings.game_exe.as_deref())?;
                self.message = format!(
                    "Latest official Dusklight release: {}. Install/update when both games are stopped.",
                    release.tag
                );
                self.latest_release = Some(release);
                Ok(())
            }
            Input::SelfUpdateChecked(result) => {
                self.self_updates.checking = false;
                self.self_updates.checked_at = Some(time::OffsetDateTime::now_utc().to_string());
                match result {
                    Ok(release) => {
                        self.audit.record(
                            "app-update-checked",
                            None,
                            Some(&release.sha256),
                            None,
                            &release.tag,
                        )?;
                        self.self_updates.error = None;
                        self.self_updates.message = if release.update_available {
                            format!(
                                "Twilight Sync {} is available. Use Update Twilight Sync in the tray when both games are stopped.",
                                release.tag
                            )
                        } else {
                            format!(
                                "Running Twilight Sync {}; latest published release is {}.",
                                env!("CARGO_PKG_VERSION"),
                                release.tag
                            )
                        };
                        self.self_updates.latest = Some(release);
                    }
                    Err(error) => {
                        let detail = format!("Cannot check Twilight Sync updates: {error:#}");
                        self.audit
                            .record("app-update-check-failed", None, None, None, &detail)?;
                        self.self_updates.latest = None;
                        self.self_updates.error = Some(detail);
                        self.self_updates.message = "Update status is unknown. LAN syncing is not affected; retry the check when connected.".into();
                    }
                }
                Ok(())
            }
            Input::SelfUpdatePrepared(result) => {
                self.self_updates.preparing = false;
                let prepared = match result {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        self.self_updates.error = Some(format!("{error:#}"));
                        self.self_updates.message =
                            "Update preparation failed. The current app remains running.".into();
                        return Err(error);
                    }
                };
                self.game_running = self.process.game_running();
                let safe = (|| -> Result<()> {
                    self.require_idle()?;
                    ensure!(
                        self.no_transfer()
                            && !self.iso_preparing
                            && self.child.is_none()
                            && self.launch_pending.is_none(),
                        "Finish all gameplay and transfers before updating"
                    );
                    self.audit.record("app-update-handoff", None, None, None, "Verified helper acknowledged readiness; clean shutdown requested. Persistent state is unchanged")?;
                    Ok(())
                })();
                if let Err(error) = safe {
                    self.self_updates.error = Some(format!("Update deferred: {error:#}"));
                    self.self_updates.message = "Gameplay or a transfer began during preparation. The update was cancelled without stopping the game.".into();
                    self.self_updates.preparing = true;
                    let sender = self.sender.clone();
                    thread::spawn(move || {
                        if sender
                            .send(Input::SelfUpdateCancelled(prepared.cancel()))
                            .is_err()
                        {
                            eprintln!("Cannot deliver self-update cancellation result");
                        }
                    });
                } else {
                    drop(prepared);
                    self.self_updates.message =
                        "Restarting to install the verified update...".into();
                    self.shutdown = true;
                }
                Ok(())
            }
            Input::SelfUpdateCancelled(result) => {
                self.self_updates.preparing = false;
                if let Err(error) = result {
                    self.self_updates.error = Some(format!("{error:#}"));
                    return Err(error);
                }
                self.audit.record(
                    "app-update-cancelled",
                    None,
                    None,
                    None,
                    "Current app and game were not terminated",
                )?;
                Ok(())
            }
            Input::TextureProgress { files, bytes } => {
                if self.texture_busy {
                    self.texture_state.notice = format!(
                        "Importing {files} textures ({:.1} MiB)...",
                        bytes as f64 / 1_048_576.0
                    );
                }
                Ok(())
            }
            Input::TextureStatus {
                message,
                cancellable,
            } => {
                if self.texture_busy {
                    self.texture_state.notice = message;
                    self.texture_state.cancellable = cancellable;
                }
                Ok(())
            }
            Input::TextureDone(result) => {
                self.texture_busy = false;
                let refresh = self.reload_textures();
                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        if let Err(refresh) = refresh {
                            return Err(error)
                                .context(format!("Texture state also needs review: {refresh:#}"));
                        }
                        return Err(error);
                    }
                };
                refresh?;
                match outcome {
                    TextureOutcome::Installed { pack, activation } => {
                        self.audit.record(
                            "texture-pack-imported",
                            None,
                            Some(&pack.id),
                            None,
                            &format!(
                                "{} {}; {} images; {} entries not selected by native texture rules",
                                pack.source, pack.version, pack.files, pack.skipped
                            ),
                        )?;
                        self.audit.record(
                            "texture-pack-switched",
                            self.settings.texture_profile.as_deref(),
                            activation.active.as_deref(),
                            None,
                            "Texture pack installed and activated in the detected profile",
                        )?;
                        self.texture_state.notice = format!(
                            "{} {} installed and active: {} textures. Changes apply on the next game launch.",
                            textures::catalog(&pack.source)?.name,
                            pack.version,
                            pack.files
                        );
                    }
                    TextureOutcome::Activated(activation) => {
                        self.audit.record(
                            "texture-pack-switched",
                            self.settings.texture_profile.as_deref(),
                            activation.active.as_deref(),
                            None,
                            &format!(
                                "{} files copied instead of linked; preserved directories: {:?}",
                                activation.copied_files, activation.preserved
                            ),
                        )?;
                        self.texture_state.notice = format!(
                            "Texture selection applied for the next Dusklight launch. {} files needed copies; {} original/modified directories preserved.",
                            activation.copied_files,
                            activation.preserved.len()
                        );
                    }
                    TextureOutcome::Recovered(paths) => {
                        self.audit.record(
                            "texture-switch-recovered",
                            self.settings.texture_profile.as_deref(),
                            None,
                            None,
                            &format!("Preserved {} modified directories", paths.len()),
                        )?;
                        self.texture_state.notice = "Interrupted switch recovered. Previous textures and configuration restored.".into();
                    }
                }
                Ok(())
            }
            Input::IsoPrepared { id, path, result } => {
                self.iso_preparing = false;
                ensure!(
                    self.link.as_ref().is_some_and(|l| l.id == id && l.ready),
                    "Peer changed during disc verification"
                );
                let verified = match result {
                    Ok(verified) => verified,
                    Err(error) => {
                        self.transfer.clear();
                        return Err(error)
                            .context("Disc image verification failed before any offer");
                    }
                };
                self.require_idle()?;
                let transfer_id = token();
                let event = match verified.format {
                    DiscFormat::Iso => Event::IsoOffer {
                        id: transfer_id.clone(),
                        hash: verified.sha256.clone(),
                        size: verified.size,
                    },
                    DiscFormat::Rvz => Event::RvzOffer {
                        id: transfer_id.clone(),
                        hash: verified.sha256.clone(),
                        size: verified.size,
                    },
                };
                self.send(&event)?;
                self.outgoing_iso = Some(OutgoingIso {
                    id: transfer_id,
                    path,
                    hash: verified.sha256,
                    size: verified.size,
                    format: verified.format,
                    started: false,
                    sent: false,
                    deadline: Instant::now() + Duration::from_secs(120),
                });
                self.transfer = format!(
                    "Verified {}; waiting for approval on the other device",
                    verified.format.label()
                );
                Ok(())
            }
            Input::IsoVerified {
                channel,
                pending,
                result,
            } => {
                let current = self
                    .iso_verifying
                    .as_ref()
                    .is_some_and(|job| job.channel == channel && job.id == pending.id)
                    && self
                        .link
                        .as_ref()
                        .is_some_and(|link| link.id == channel && link.ready);
                if !current {
                    self.audit.record(
                        "disc-verification-discarded", Some(&pending.destination), Some(&pending.hash),
                        None, "Verification completed after cancellation or a channel change; staging discarded",
                    )?;
                    return Ok(());
                }
                self.iso_verifying = None;
                let validation = result.and_then(|verified| {
                    ensure!(
                        verified.format == pending.format
                            && verified.size == pending.size
                            && verified.sha256 == pending.hash,
                        "Verified RVZ differs from the approved transfer"
                    );
                    Ok(())
                });
                if let Err(error) = validation {
                    self.transfer.clear();
                    self.send(&Event::Error {
                        message: "RVZ validation failed; the receiver did not commit the image"
                            .into(),
                    })?;
                    return Err(error).context("Received RVZ was rejected");
                }
                if let Err(error) = self.commit_disc(*pending) {
                    self.disconnect("Disc commit could not complete; check the receiver log");
                    return Err(error);
                }
                Ok(())
            }
            Input::IsoSent { id, result } => {
                if self.link.as_ref().is_some_and(|l| l.id == id) {
                    if let Err(error) = result {
                        self.disconnect(
                            "Disc sender failed; receiver must discard its staged image",
                        );
                        return Err(error);
                    }
                    if let Some(outgoing) = &mut self.outgoing_iso {
                        outgoing.sent = true;
                        outgoing.deadline = Instant::now()
                            + if outgoing.format == DiscFormat::Rvz {
                                disc::VERIFY_TIMEOUT + Duration::from_secs(60)
                            } else {
                                Duration::from_secs(60)
                            };
                        self.transfer =
                            "Disc image sent; waiting for the receiver's verified commit".into();
                    }
                }
                Ok(())
            }
        }
    }

    fn trusted_network(&self) -> bool {
        !self.fatal
            && self.last_verified.elapsed() <= Duration::from_secs(12)
            && self
                .network
                .as_ref()
                .is_some_and(|n| self.settings.network.as_deref() == Some(n.fingerprint.as_str()))
    }

    fn pairing_open(&self) -> bool {
        self.pairing_until
            .is_some_and(|until| until > Instant::now())
            && self.settings.peer.is_none()
    }

    fn ready(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.ready)
    }

    fn peer_key(&self) -> Result<String> {
        Ok(self
            .link
            .as_ref()
            .context("No secure peer channel")?
            .key
            .clone())
    }

    fn require_idle(&self) -> Result<()> {
        ensure!(
            !self.install_busy && !self.texture_busy && !self.self_updates.preparing,
            "Wait for the current installation, update, or texture operation"
        );
        ensure!(
            !self.game_running && !self.remote.as_ref().is_some_and(|r| r.game),
            "Stop Dusklight on both devices first"
        );
        ensure!(
            self.settings.play_lock.is_none()
                && self.remote.as_ref().is_none_or(|r| r.lock.is_none()),
            "An exclusive-play lock is held"
        );
        Ok(())
    }

    fn no_transfer(&self) -> bool {
        self.pending_save.is_none()
            && self.sent_save.is_none()
            && self.iso_offer.is_none()
            && self.incoming_iso.is_none()
            && self.outgoing_iso.is_none()
            && !self.iso_preparing
            && self.iso_verifying.is_none()
    }

    fn install_texture(
        &mut self,
        source: String,
        version: String,
        path: PathBuf,
        preserve_unmanaged: bool,
    ) -> Result<()> {
        self.game_running = self.process.game_running();
        self.require_idle()?;
        ensure!(
            self.no_transfer(),
            "Finish pending transfers before installing textures"
        );
        textures::catalog(&source)?;
        ensure!(
            self.settings.game_exe.is_some(),
            "Locate or install Dusklight before installing its textures"
        );
        self.reload_textures()?;
        let profile = self
            .settings
            .texture_profile
            .clone()
            .context("Automatic Dusklight profile detection is not ready")?;
        self.audit.record(
            "texture-install-requested",
            Some(&path),
            None,
            None,
            &format!("{source}; user approved authorized texture installation"),
        )?;
        self.texture_busy = true;
        self.texture_cancel = Arc::new(AtomicBool::new(false));
        self.texture_state.cancellable = true;
        self.texture_state.notice = "Preparing texture installation...".into();
        let cancel = self.texture_cancel.clone();
        let root = self.root.clone();
        let default_profile = self.default_profile.clone();
        let executable = self.settings.game_exe.clone();
        let view = self.view.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let status = |message: String, cancellable: bool| -> Result<()> {
                sender
                    .send(Input::TextureStatus {
                        message,
                        cancellable,
                    })
                    .map_err(|_| anyhow::anyhow!("Texture manager stopped during installation"))
            };
            let check_idle = || -> Result<()> {
                texture_profile_stopped(&view, &default_profile, executable.as_deref(), &profile)
            };
            let result = (|| -> Result<TextureOutcome> {
                status("Installing the validated texture selection using Dusklight's own priority rules...".into(), true)?;
                let mut reported = 0;
                let pack = textures::import_cancellable(
                    &root,
                    &source,
                    &version,
                    &path,
                    cancel.clone(),
                    |files, bytes| {
                        if files >= reported + 20 {
                            reported = files;
                            if let Err(error) = sender.send(Input::TextureProgress { files, bytes })
                            {
                                eprintln!("Cannot deliver texture progress: {error}");
                                cancel.store(true, Ordering::Release);
                            }
                        }
                    },
                )?;
                ensure!(
                    !cancel.load(Ordering::Acquire),
                    "Texture pack is installed; activation was cancelled"
                );
                status(
                    "Applying textures to the detected Dusklight profile...".into(),
                    false,
                )?;
                textures::prepare_profile(&profile, check_idle)
                    .context("Texture pack is installed, but profile setup could not complete")?;
                let activation = textures::activate(
                    &root,
                    &profile,
                    Some(&pack.id),
                    preserve_unmanaged,
                    check_idle,
                )
                .context("Texture pack is installed, but activation could not complete")?;
                Ok(TextureOutcome::Installed { pack, activation })
            })();
            if sender.send(Input::TextureDone(result)).is_err() {
                eprintln!("Twilight Sync stopped before texture installation completed");
            }
        });
        Ok(())
    }

    fn command(&mut self, command: Command) -> Result<()> {
        if !matches!(command, Command::DesktopRecovered(_)) {
            self.error = None;
        }
        match command {
            Command::DesktopError(detail) => bail!("{detail}"),
            Command::DesktopRecovered(detail) => clear_desktop_error(&mut self.error, &detail),
            Command::TrustNetwork => {
                self.require_idle()?;
                let network = self
                    .network
                    .as_ref()
                    .context("No verifiable private LAN")?
                    .clone();
                self.audit.record(
                    "router-approved",
                    None,
                    Some(&network.fingerprint),
                    None,
                    &format!(
                        "{}; {}; {}",
                        network.adapter, network.gateway, network.router_mac
                    ),
                )?;
                self.stop_network("Router trust changed");
                self.store(|s| s.network = Some(network.fingerprint))?;
                self.configure_sockets()?;
                self.message = "Router approved. Discovery is limited to this subnet.".into();
            }
            Command::OpenPairing => {
                ensure!(self.trusted_network(), "Approve the current router first");
                ensure!(
                    self.settings.peer.is_none(),
                    "Unpair the existing partner before pairing another"
                );
                self.require_idle()?;
                self.pairing_until = Some(Instant::now() + Duration::from_secs(120));
                self.pairing_attempts = 0;
                self.audit.record(
                    "pairing-opened",
                    None,
                    None,
                    None,
                    "Two-minute, five-attempt consent window",
                )?;
                self.message = "Open pairing on both devices. Choose Compare codes on ONE device, then approve matching codes on BOTH.".into();
            }
            Command::Connect(key) => {
                ensure!(self.link.is_none(), "A channel is already open");
                let device = self
                    .devices
                    .get(&key)
                    .context("Device announcement expired")?
                    .clone();
                ensure!(
                    self.pairing_open()
                        || self
                            .settings
                            .peer
                            .as_ref()
                            .is_some_and(|p| p.public_key == key),
                    "Open pairing on both devices"
                );
                if self.settings.peer.is_none() {
                    ensure!(device.pairing, "The other device has not opened pairing");
                }
                self.connect(device.address, Some(key), None)?;
            }
            Command::ApprovePairing => {
                ensure!(self.pairing_open(), "The pairing window expired");
                let link = self.link.as_mut().context("No pairing request")?;
                ensure!(
                    link.name.is_some() && !link.committed,
                    "Wait for the authenticated device details"
                );
                link.local_approved = true;
                self.send(&Event::ApprovePairing)?;
                self.complete_pairing()?;
            }
            Command::RejectPairing => {
                self.audit.record(
                    "pairing-rejected",
                    None,
                    None,
                    self.link.as_ref().map(|l| l.key.as_str()),
                    "Local user rejected the code",
                )?;
                self.pairing_until = None;
                self.disconnect("Pairing rejected by the local user");
            }
            Command::SetExecutable(path) => {
                self.require_idle()?;
                ensure!(
                    self.no_transfer(),
                    "Finish pending transfers before changing the game executable"
                );
                ensure_safe_path(&path)?;
                ensure!(
                    path.is_file()
                        && path
                            .file_name()
                            .is_some_and(|name| name.eq_ignore_ascii_case("dusklight.exe")),
                    "Select dusklight.exe"
                );
                self.store(|s| s.game_exe = Some(path))?;
                self.refresh_default_save()?;
                self.selected_release =
                    install::selected_release(&self.root, self.settings.game_exe.as_deref())?;
            }
            Command::SetIso(path) => {
                self.require_idle()?;
                ensure!(self.no_transfer(), "Another transfer is active");
                ensure_safe_path(&path)?;
                ensure!(path.is_file(), "Select your own disc image file");
                let format = DiscFormat::from_path(&path)?;
                self.store(|s| s.iso_file = Some(path))?;
                self.message = format!(
                    "{} selected. Full original-disc verification happens before any offer.",
                    format.label()
                );
            }
            Command::SetIsoEnabled(enabled) => {
                self.require_idle()?;
                ensure!(
                    self.no_transfer(),
                    "Finish the current transfer before changing disc-image permissions"
                );
                self.store(|s| s.iso_enabled = enabled)?;
                self.audit.record(
                    "iso-permission-changed",
                    None,
                    None,
                    None,
                    if enabled {
                        "Explicitly enabled by user"
                    } else {
                        "Disabled"
                    },
                )?;
            }
            Command::SetStartup(enabled) => {
                config::set_startup(enabled)?;
                self.store(|s| s.startup = enabled)?;
                self.audit.record(
                    "startup-changed",
                    None,
                    None,
                    None,
                    if enabled { "Enabled" } else { "Disabled" },
                )?;
            }
            Command::SendSave => {
                ensure!(self.ready(), "Connect your paired device first");
                self.refresh_save()?;
                self.offer_save(true)?;
            }
            Command::AcceptSave(accept) => {
                if accept {
                    let pending = self
                        .pending_save
                        .take()
                        .context("No incoming save to approve")?;
                    ensure!(pending.deadline > Instant::now(), "Save approval expired");
                    self.receive_save(pending, true)?;
                } else {
                    let pending = self
                        .pending_save
                        .take()
                        .context("No incoming save to reject")?;
                    self.send(&Event::SaveDeclined { hash: pending.hash })?;
                    self.message =
                        "Incoming save declined. Your local save was not changed.".into();
                }
            }
            Command::Launch => self.request_launch()?,
            Command::ContinueLocal { expected } => {
                ensure!(
                    self.current_hash()? == expected,
                    "Your save changed; review it before playing"
                );
                if let Some(pending) = self.pending_save.take() {
                    self.send(&Event::SaveDeclined { hash: pending.hash })?;
                }
                self.request_launch_mode(true)?;
            }
            Command::ResolveConflict {
                local_hash,
                remote_hash,
                use_remote,
            } => {
                self.resolve_conflict(&local_hash, &remote_hash, use_remote)?;
            }
            Command::Recover => {
                self.game_running = self.process.game_running();
                ensure!(
                    !self.game_running && !self.remote.as_ref().is_some_and(|r| r.game),
                    "Stop every Dusklight process before recovery"
                );
                ensure!(
                    self.child.is_none() && self.launch_pending.is_none(),
                    "A managed launch is still active"
                );
                ensure!(
                    self.no_transfer(),
                    "Finish or decline transfers before recovery"
                );
                let lock = self
                    .settings
                    .play_lock
                    .as_ref()
                    .or_else(|| self.remote.as_ref().and_then(|r| r.lock.as_ref()))
                    .context("No stale play lock")?;
                let token = lock.token.clone();
                if !self.ready() {
                    self.audit.record("lock-recovered-offline", None, self.local_hash.as_deref(), None,
                        "Explicit local recovery; partner activity is unknown and divergent saves will be preserved")?;
                    self.store(|settings| settings.play_lock = None)?;
                    self.managed_token = None;
                    self.release_sent = false;
                    self.message = "Local session recovered. You can play offline; any divergent progress will need reconciliation later.".into();
                    return Ok(());
                }
                self.send(&Event::Recover {
                    token: token.clone(),
                })?;
                self.requested_recovery = Some(token);
                self.message = "Recovery requested. The other device must explicitly confirm both games are stopped.".into();
            }
            Command::AcceptRecovery(accept) => {
                self.game_running = self.process.game_running();
                let token = self
                    .pending_recovery
                    .take()
                    .context("No recovery request")?;
                ensure!(
                    !self.game_running && !self.remote.as_ref().is_some_and(|r| r.game),
                    "Recovery is unsafe while a game is running"
                );
                if accept {
                    ensure!(
                        self.child.is_none() && self.launch_pending.is_none(),
                        "A managed launch is still active"
                    );
                    self.audit.record(
                        "lock-recovery-approved",
                        None,
                        None,
                        Some(&self.peer_key()?),
                        &token,
                    )?;
                    self.store(|s| s.play_lock = None)?;
                    self.managed_token = None;
                    self.send(&Event::RecoveryApproved { token })?;
                    self.send_state()?;
                } else {
                    self.send(&Event::Error {
                        message: "Play-lock recovery was declined".into(),
                    })?;
                }
            }
            Command::SendIso => {
                ensure!(
                    self.ready() && self.settings.iso_enabled,
                    "Connect your partner and explicitly enable disc-image transfer"
                );
                self.require_idle()?;
                ensure!(self.no_transfer(), "Another transfer is active");
                let path = self
                    .settings
                    .iso_file
                    .clone()
                    .context("Select your own ISO or RVZ first")?;
                let id = self.link.as_ref().context("No peer")?.id;
                let sender = self.sender.clone();
                let cancel = self.cancel_transfer.clone();
                self.iso_preparing = true;
                self.transfer =
                    "Verifying the entire reconstructed disc against the official hash...".into();
                thread::spawn(move || {
                    let result = disc::verify_with_cancel(&path, cancel);
                    if sender
                        .send(Input::IsoPrepared { id, path, result })
                        .is_err()
                    {
                        eprintln!("Twilight Sync stopped before disc verification completed");
                    }
                });
            }
            Command::AcceptIso(destination) => self.accept_iso(destination)?,
            Command::CheckRelease => {
                ensure!(
                    !self.install_busy && !self.texture_busy && !self.self_updates.preparing,
                    "A management operation is already running"
                );
                self.audit.record(
                    "dusklight-update-check-requested",
                    None,
                    None,
                    None,
                    "Explicit official GitHub metadata request",
                )?;
                self.install_busy = true;
                self.message = "Checking the latest official Dusklight release...".into();
                let sender = self.sender.clone();
                thread::spawn(move || {
                    if sender
                        .send(Input::ReleaseChecked(install::latest_release()))
                        .is_err()
                    {
                        eprintln!("Twilight Sync stopped during the release check");
                    }
                });
            }
            Command::CheckSelfUpdate => self.begin_self_update_check()?,
            Command::SetAutoUpdateChecks(enabled) => {
                self_update::set_enabled(&self.root, enabled)?;
                self.audit.record(
                    "automatic-app-update-checks-changed",
                    None,
                    None,
                    None,
                    if enabled {
                        "Enabled: startup and every six hours; never automatic installation"
                    } else {
                        "Disabled; manual checks remain available"
                    },
                )?;
                self.self_updates.enabled = enabled;
                if enabled {
                    self.last_self_update_check = None;
                    self.first_self_update_check = Instant::now();
                }
            }
            Command::UpdateSelf => {
                self.game_running = self.process.game_running();
                self.require_idle()?;
                ensure!(
                    !self.self_updates.checking,
                    "Wait for the current update check"
                );
                ensure!(
                    self.no_transfer()
                        && !self.iso_preparing
                        && self.child.is_none()
                        && self.launch_pending.is_none(),
                    "Finish gameplay and transfers before updating"
                );
                ensure!(
                    self.self_updates.available(),
                    "Check for a newer Twilight Sync release first"
                );
                let release = self
                    .self_updates
                    .latest
                    .clone()
                    .context("No verified release metadata")?;
                self.audit.record(
                    "app-update-approved",
                    None,
                    Some(&release.sha256),
                    None,
                    &release.tag,
                )?;
                self.self_updates.preparing = true;
                self.self_updates.error = None;
                self.self_updates.message = format!(
                    "Downloading and verifying Twilight Sync {}. This app remains running until the helper is ready.",
                    release.tag
                );
                let root = self.root.clone();
                let sender = self.sender.clone();
                thread::spawn(move || {
                    if sender
                        .send(Input::SelfUpdatePrepared(self_update::prepare(
                            &root, &release,
                        )))
                        .is_err()
                    {
                        eprintln!("Cannot deliver the verified self-update preparation result");
                    }
                });
            }
            Command::CancelTextures => {
                ensure!(
                    self.texture_busy && self.texture_state.cancellable,
                    "Texture activation cannot be interrupted; wait for it to finish"
                );
                self.texture_cancel.store(true, Ordering::Release);
                self.texture_state.notice = "Cancelling texture installation...".into();
                self.texture_state.cancellable = false;
            }
            Command::OpenTextureCreator(source) => {
                let entry = textures::catalog(&source)?;
                self.audit.record(
                    "texture-creator-page-opened",
                    None,
                    None,
                    None,
                    entry
                        .creator_url
                        .context("This source is local-import only")?,
                )?;
                textures::open_creator(&source)?;
            }
            Command::ImportTextures {
                source,
                version,
                path,
                preserve_unmanaged,
            } => {
                self.install_texture(source, version, path, preserve_unmanaged)?;
            }
            Command::ActivateTextures {
                id,
                preserve_unmanaged,
            } => {
                self.game_running = self.process.game_running();
                self.require_idle()?;
                self.reload_textures()?;
                let profile = self
                    .settings
                    .texture_profile
                    .clone()
                    .context("Automatic Dusklight profile detection is not ready")?;
                self.audit.record(
                    "texture-switch-requested",
                    Some(&profile),
                    id.as_deref(),
                    None,
                    "Switch is blocked while either game runs; unrelated originals are preserved",
                )?;
                self.texture_busy = true;
                self.texture_state.notice =
                    "Verifying and switching texture files; do not launch Dusklight...".into();
                let root = self.root.clone();
                let sender = self.sender.clone();
                let view = self.view.clone();
                let default_profile = self.default_profile.clone();
                let executable = self.settings.game_exe.clone();
                thread::spawn(move || {
                    let check_idle = || {
                        texture_profile_stopped(
                            &view,
                            &default_profile,
                            executable.as_deref(),
                            &profile,
                        )
                    };
                    let result = (|| {
                        textures::prepare_profile(&profile, check_idle)?;
                        textures::activate(
                            &root,
                            &profile,
                            id.as_deref(),
                            preserve_unmanaged,
                            check_idle,
                        )
                        .map(TextureOutcome::Activated)
                    })();
                    if sender.send(Input::TextureDone(result)).is_err() {
                        eprintln!("Twilight Sync stopped during texture activation");
                    }
                });
            }
            Command::RecoverTextures => {
                self.game_running = self.process.game_running();
                self.require_idle()?;
                self.texture_busy = true;
                self.texture_state.notice =
                    "Recovering the interrupted texture transaction...".into();
                let root = self.root.clone();
                let sender = self.sender.clone();
                thread::spawn(move || {
                    let result = textures::recover(&root, texture_game_stopped)
                        .map(TextureOutcome::Recovered);
                    if sender.send(Input::TextureDone(result)).is_err() {
                        eprintln!("Twilight Sync stopped during texture recovery");
                    }
                });
            }
            Command::RefreshTextures => {
                ensure!(!self.texture_busy, "Wait for the texture operation");
                self.reload_textures()?;
            }
            Command::HistoryPage(before) => {
                self.history_cursor = before;
                self.refresh_history()?;
            }
            Command::RestoreSnapshot { id, expected } => {
                self.game_running = self.process.game_running();
                self.require_idle()?;
                ensure!(
                    self.no_transfer(),
                    "Finish or reject pending transfers before restoring"
                );
                self.refresh_save()?;
                self.ensure_history()?;
                let bytes = self
                    .history
                    .as_ref()
                    .context("History is unavailable")?
                    .read(&id)?;
                let path = self.save_destination(&bytes)?;
                save_location::prepare(&path)?;
                self.audit.record(
                    "snapshot-restore-approved",
                    Some(&path),
                    expected.as_deref(),
                    None,
                    &id,
                )?;
                let (hash, capture) = self
                    .history
                    .as_mut()
                    .context("History is unavailable")?
                    .restore(&id, &path, expected.as_deref())?;
                self.audit.record(
                    "snapshot-restored",
                    Some(&path),
                    Some(&hash),
                    None,
                    &format!("New history commit {}", capture.id),
                )?;
                self.history_last_hash = Some(hash.clone());
                self.history_candidate = None;
                self.history_cursor = None;
                self.local_hash = Some(hash.clone());
                self.local_save = Some(save_data::inspect(&bytes)?);
                self.previous_hash = Some(hash);
                self.save_valid = true;
                self.stable = 2;
                self.stable_since = Instant::now() - Duration::from_secs(2);
                self.refresh_history()?;
                self.send_state()?;
                self.message = "Snapshot restored as a new commit. Sync still requires the normal conflict and play-lock checks.".into();
            }
            Command::Install => {
                self.require_idle()?;
                ensure!(!self.install_busy, "An installation is already running");
                self.audit.record(
                    "dusklight-install-requested",
                    None,
                    None,
                    None,
                    "Explicit Internet exception: official GitHub release only",
                )?;
                self.install_busy = true;
                self.message = "Downloading and verifying the official Windows release...".into();
                let root = self.root.clone();
                let sender = self.sender.clone();
                thread::spawn(move || {
                    let result = (|| {
                        let installed = install::install_official(&root)?;
                        let shortcut =
                            crate::desktop::install_dusklight_shortcut(&root, &installed.executable)
                                .context("Dusklight files were installed/verified, but its desktop shortcut could not be created")?;
                        Ok((installed, shortcut))
                    })();
                    if sender.send(Input::Installed(result)).is_err() {
                        eprintln!("Twilight Sync stopped before installation completed");
                    }
                });
            }
            Command::Unpair => {
                self.require_idle()?;
                ensure!(
                    self.no_transfer(),
                    "Finish pending transfers before unpairing"
                );
                self.audit.record(
                    "peer-revoked",
                    None,
                    None,
                    self.settings.peer.as_ref().map(|p| p.public_key.as_str()),
                    "Local user revoked trust",
                )?;
                self.disconnect("Device unpaired");
                self.store(|s| {
                    s.peer = None;
                    s.sync_base = None;
                })?;
            }
            Command::Shutdown => {
                ensure!(
                    !self.game_running && self.child.is_none(),
                    "Stop Dusklight before quitting; close the window to the tray instead"
                );
                ensure!(
                    !self.install_busy
                        && !self.texture_busy
                        && !self.self_updates.preparing
                        && !self.iso_preparing,
                    "Wait for installation, textures, or disc verification to finish"
                );
                self.shutdown = true;
            }
        }
        Ok(())
    }

    fn configure_sockets(&mut self) -> Result<()> {
        if self.trusted_network() && self.listener.is_none() {
            let network = self.network.as_ref().context("Missing LAN")?;
            let listener = TcpListener::bind((network.address, protocol::PORT))
                .context("Binding the private-LAN listener")?;
            listener.set_nonblocking(true)?;
            let discovery = network
                .discovery_socket()
                .context("Binding local discovery")?;
            self.listener = Some(listener);
            self.discovery = Some(discovery);
            self.audit.record(
                "lan-enabled",
                None,
                Some(&network.fingerprint),
                None,
                &format!("Bound only to {}", network.address),
            )?;
            self.message = "Trusted LAN ready. No public listeners, cloud relay, or UPnP.".into();
        }
        Ok(())
    }

    fn stop_network(&mut self, reason: &str) {
        self.disconnect(reason);
        self.listener = None;
        self.discovery = None;
        self.devices.clear();
    }

    fn disconnect(&mut self, reason: &str) {
        self.cancel_transfer.store(true, Ordering::Release);
        if let Some(link) = self.link.take() {
            if let Err(error) =
                self.audit
                    .record("peer-disconnected", None, None, Some(&link.key), reason)
            {
                self.fatal = true;
                self.error = Some(format!("Cannot audit channel closure: {error:#}"));
            }
            let close = link
                .writer
                .lock()
                .map_err(|_| anyhow::anyhow!("Channel lock poisoned"))
                .and_then(|writer| writer.close());
            if let Err(error) = close
                && let Err(log_error) = self.audit.record(
                    "channel-close-error",
                    None,
                    None,
                    Some(&link.key),
                    &format!("{error:#}"),
                )
            {
                self.fatal = true;
                self.error = Some(format!(
                    "Audit failure while closing channel: {log_error:#}"
                ));
            }
        }
        self.remote = None;
        self.remote_preview = None;
        self.preview_requested = None;
        self.pending_save = None;
        self.sent_save = None;
        self.pending_recovery = None;
        self.requested_recovery = None;
        self.iso_offer = None;
        self.incoming_iso = None;
        self.outgoing_iso = None;
        self.iso_verifying = None;
        self.release_sent = false;
        self.launch_pending = None;
        self.transfer.clear();
        self.message = reason.into();
    }

    fn connect(
        &mut self,
        address: SocketAddr,
        expected: Option<String>,
        accepted: Option<TcpStream>,
    ) -> Result<()> {
        ensure!(self.trusted_network(), "Network is not trusted");
        let network = self.network.as_ref().context("No LAN")?.clone();
        let std::net::IpAddr::V4(ip) = address.ip() else {
            bail!("Only local IPv4 is supported");
        };
        ensure!(
            network.allows(ip),
            "Connection target is not on the directly connected subnet"
        );
        ensure!(
            self.last_connect.elapsed() >= Duration::from_secs(2),
            "Connection attempts are rate limited"
        );
        ensure!(
            !self.connecting.swap(true, Ordering::AcqRel),
            "A handshake is already running"
        );
        if self.settings.peer.is_none() {
            if !self.pairing_open() || self.pairing_attempts >= 5 {
                self.connecting.store(false, Ordering::Release);
                bail!("Pairing is closed or its attempt budget is exhausted");
            }
            self.pairing_attempts += 1;
        }
        self.last_connect = Instant::now();
        let identity = self.identity.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let result = (|| -> Result<Handshake> {
                let current =
                    lan::probe().context("Fresh router verification before connection")?;
                ensure!(
                    current.fingerprint == network.fingerprint
                        && current.address == network.address,
                    "Router or source interface changed before connection"
                );
                let initiator = accepted.is_none();
                let stream = match accepted {
                    Some(stream) => stream,
                    None => {
                        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
                        socket.bind(&SocketAddr::from((network.address, 0)).into())?;
                        socket.connect_timeout(&address.into(), Duration::from_secs(5))?;
                        TcpStream::from(socket)
                    }
                };
                protocol::handshake(
                    stream,
                    identity.private_key(),
                    &network.fingerprint,
                    initiator,
                )
            })();
            let input = match result {
                Ok(session) => Input::Connected {
                    id: rand::random(),
                    session: Box::new(session),
                    expected,
                    network: network.fingerprint,
                },
                Err(error) => Input::ConnectFailed(format!("{error:#}")),
            };
            if sender.send(input).is_err() {
                eprintln!("Twilight Sync stopped during a handshake");
            }
        });
        Ok(())
    }

    fn establish(&mut self, id: u64, session: Handshake, expected: Option<String>) -> Result<()> {
        ensure!(
            session.remote_key != self.identity.public,
            "Refusing a reflected self-connection"
        );
        ensure!(
            expected
                .as_ref()
                .is_none_or(|key| *key == session.remote_key),
            "Authenticated identity differs from the selected announcement"
        );
        let committed = if let Some(peer) = &self.settings.peer {
            ensure!(
                peer.public_key == session.remote_key,
                "Peer identity has changed; trust was not replaced"
            );
            true
        } else {
            ensure!(
                self.pairing_open(),
                "Unknown identity outside a pairing window"
            );
            false
        };
        self.cancel_transfer = Arc::new(AtomicBool::new(false));
        self.link = Some(Link {
            id,
            key: session.remote_key,
            name: None,
            writer: Arc::new(Mutex::new(session.writer)),
            comparison_code: session.comparison_code,
            local_approved: committed,
            remote_approved: committed,
            committed,
            ready: false,
            deadline: Instant::now() + Duration::from_secs(120),
            last_seen: Instant::now(),
        });
        self.remote = None;
        self.send(&Event::Hello {
            name: self.settings.name.clone(),
        })?;
        let sender = self.sender.clone();
        thread::spawn(move || {
            let mut reader = session.reader;
            loop {
                match reader.receive() {
                    Ok(event) => {
                        if sender.send(Input::Frame { id, event }).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        if sender
                            .send(Input::Disconnected {
                                id,
                                reason: format!("{error:#}"),
                            })
                            .is_err()
                        {
                            eprintln!("Twilight Sync stopped while the channel was closing");
                        }
                        break;
                    }
                }
            }
        });
        if committed {
            self.send(&Event::PairingReady)?;
        }
        self.message = if committed {
            "Authenticating your pinned partner..."
        } else {
            "Compare the code on BOTH devices before approving."
        }
        .into();
        Ok(())
    }

    fn send(&mut self, event: &Event) -> Result<()> {
        let result = {
            let link = self.link.as_ref().context("No secure channel")?;
            link.writer
                .lock()
                .map_err(|_| anyhow::anyhow!("Channel writer lock poisoned"))?
                .send(event)
        };
        if let Err(error) = result {
            self.disconnect("Secure channel write failed");
            return Err(error);
        }
        Ok(())
    }

    fn complete_pairing(&mut self) -> Result<()> {
        let link = self.link.as_ref().context("No pairing channel")?;
        if link.local_approved && link.remote_approved && !link.committed {
            ensure!(link.deadline > Instant::now(), "Pairing expired");
            let peer = Peer {
                public_key: link.key.clone(),
                name: link.name.clone().context("Missing authenticated name")?,
            };
            self.audit.record(
                "pairing-approved-both",
                None,
                None,
                Some(&peer.public_key),
                "Both users confirmed the session-bound comparison code",
            )?;
            self.store(|s| s.peer = Some(peer))?;
            self.link
                .as_mut()
                .context("Missing pairing channel")?
                .committed = true;
            self.pairing_until = None;
            self.send(&Event::PairingReady)?;
        }
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<()> {
        let ready = self.ready();
        match event {
            Event::Hello { name } => {
                ensure!(config::valid_label(&name), "Invalid peer name");
                let link = self.link.as_mut().context("No channel")?;
                ensure!(link.name.is_none(), "Duplicate hello");
                link.name = Some(name);
            }
            Event::ApprovePairing => {
                ensure!(
                    self.pairing_open(),
                    "Approval outside an active pairing window"
                );
                let link = self.link.as_mut().context("No channel")?;
                ensure!(
                    link.name.is_some() && !link.remote_approved,
                    "Unexpected pairing approval"
                );
                link.remote_approved = true;
                self.complete_pairing()?;
            }
            Event::PairingReady => {
                let link = self.link.as_mut().context("No channel")?;
                ensure!(
                    link.committed && link.name.is_some() && !link.ready,
                    "Pairing is not bilaterally committed"
                );
                link.ready = true;
                self.audit.record(
                    "peer-connected",
                    None,
                    None,
                    Some(&link.key),
                    "Noise identity matches the pinned partner",
                )?;
                self.error = None;
                self.message =
                    "Secure partner connected. Saves sync when idle and conflict-free.".into();
                self.send_state()?;
            }
            Event::Ping => {}
            Event::Error { message } => {
                ensure!(
                    config::valid_label(&message)
                        || (!message.is_empty()
                            && message.len() <= 256
                            && !message.chars().any(char::is_control)),
                    "Invalid remote error"
                );
                ensure!(ready, "Unpaired peer sent an error");
                if self.iso_verifying.is_some()
                    || self
                        .iso_offer
                        .as_ref()
                        .is_some_and(|image| image.format == DiscFormat::Rvz)
                    || self
                        .incoming_iso
                        .as_ref()
                        .is_some_and(|image| image.format == DiscFormat::Rvz)
                {
                    self.iso_offer = None;
                    self.incoming_iso = None;
                    self.iso_verifying = None;
                    self.cancel_transfer.store(true, Ordering::Release);
                    self.cancel_transfer = Arc::new(AtomicBool::new(false));
                }
                self.sent_save = None;
                self.outgoing_iso = None;
                self.requested_recovery = None;
                self.transfer.clear();
                self.error = Some(format!("Partner: {message}"));
                self.audit.record(
                    "peer-reported-error",
                    None,
                    None,
                    Some(&self.peer_key()?),
                    &message,
                )?;
            }
            event => {
                ensure!(
                    ready,
                    "Unpaired devices cannot exchange saves, disc images, or play locks"
                );
                self.authenticated_event(event)?;
            }
        }
        Ok(())
    }

    fn authenticated_event(&mut self, event: Event) -> Result<()> {
        let peer = self.peer_key()?;
        match event {
            Event::State {
                hash,
                save_ready,
                game,
                lock,
            } => {
                ensure!(
                    hash.as_ref().is_none_or(|h| valid_hash(h)),
                    "Invalid peer save hash"
                );
                ensure!(save_ready || hash.is_none(), "Invalid peer save readiness");
                if let Some(lock) = &lock {
                    ensure!(
                        valid_hash(&lock.token)
                            && (lock.owner == peer || lock.owner == self.identity.public),
                        "Invalid remote play lock"
                    );
                }
                if !game
                    && lock.is_none()
                    && !self.game_running
                    && self.child.is_none()
                    && self
                        .settings
                        .play_lock
                        .as_ref()
                        .is_some_and(|held| held.owner == peer)
                {
                    self.audit.record(
                        "peer-session-completed-offline",
                        None,
                        hash.as_deref(),
                        Some(&peer),
                        "Owner reports no active session; saves still require normal reconciliation",
                    )?;
                    self.store(|settings| settings.play_lock = None)?;
                    self.send_state()?;
                }
                if self
                    .remote_preview
                    .as_ref()
                    .is_some_and(|preview| Some(&preview.hash) != hash.as_ref())
                {
                    self.remote_preview = None;
                }
                self.remote = Some(RemoteState {
                    hash,
                    save_ready,
                    game,
                    lock,
                });
            }
            Event::Save {
                hash,
                base,
                expected,
                manual: _,
                lease,
                bytes,
            } => {
                ensure!(
                    valid_hash(&hash)
                        && base.as_ref().is_none_or(|h| valid_hash(h))
                        && expected.as_ref().is_none_or(|h| valid_hash(h)),
                    "Invalid transfer hashes"
                );
                ensure!(
                    files::sha256(&bytes) == hash,
                    "Save payload SHA-256 mismatch"
                );
                let identity = files::validate_save(&bytes)?;
                save_location::path_for_identity(
                    self.save_profile
                        .as_deref()
                        .context("Automatic save profile is unavailable")?,
                    &identity,
                )?;
                self.ensure_save_receive_allowed(lease.as_deref())?;
                self.ensure_save_profile_current()?;
                ensure!(
                    self.pending_save.is_none()
                        && self.incoming_iso.is_none()
                        && self.iso_offer.is_none(),
                    "Another incoming transfer is pending"
                );
                let current = self.current_hash()?;
                ensure!(
                    current == expected,
                    "Local save changed since the offer was prepared"
                );
                let automatic = current.is_none()
                    || current.as_ref() == Some(&hash)
                    || (current == base && self.settings.sync_base == base);
                let pending = PendingSave {
                    hash,
                    expected,
                    bytes,
                    lease,
                    deadline: Instant::now() + Duration::from_secs(120),
                };
                if automatic {
                    self.receive_save(pending, false)?;
                } else {
                    self.preserve_preview(pending.hash.clone(), base, pending.bytes.clone())?;
                    self.pending_save = Some(pending);
                    self.message = "Save fork detected. Both versions are preserved; compare progress before choosing a branch.".into();
                }
            }
            Event::Saved { hash } => {
                ensure!(valid_hash(&hash), "Invalid save receipt");
                ensure!(
                    self.sent_save
                        .as_ref()
                        .is_some_and(|(sent, _)| *sent == hash),
                    "Unexpected save receipt"
                );
                self.audit.record(
                    "save-acknowledged",
                    self.settings.save_file.as_deref(),
                    Some(&hash),
                    Some(&peer),
                    "Peer validated and atomically committed the save",
                )?;
                if let Some(other_hash) = self
                    .remote_preview
                    .as_ref()
                    .map(|preview| preview.hash.clone())
                    .filter(|other| other != &hash)
                    && self.current_hash()?.as_ref() == Some(&hash)
                {
                    let path = self
                        .settings
                        .save_file
                        .clone()
                        .context("Missing save location")?;
                    let bytes = files::read_save(&path)?;
                    ensure!(
                        files::sha256(&bytes) == hash,
                        "Local save changed during reconciliation acknowledgement"
                    );
                    self.ensure_history()?;
                    let captured = self
                        .history
                        .as_mut()
                        .context("History is unavailable")?
                        .record_local_resolution(&bytes, &path, &peer, &other_hash)?;
                    self.audit.record(
                        "save-fork-reconciled",
                        Some(&path),
                        Some(&hash),
                        Some(&peer),
                        &captured.id,
                    )?;
                    self.history_cursor = None;
                    self.refresh_history()?;
                }
                self.store(|s| s.sync_base = Some(hash.clone()))?;
                if let Some(remote) = &mut self.remote {
                    remote.hash = Some(hash);
                }
                self.sent_save = None;
                self.transfer.clear();
                self.message = "Save synced, verified, and backed up.".into();
            }
            Event::SavePreviewRequest { hash } => {
                ensure!(valid_hash(&hash), "Invalid preview request hash");
                if self.game_running
                    || !self.save_valid
                    || self.stable < 2
                    || self.local_hash.as_deref() != Some(hash.as_str())
                {
                    self.send_state()?;
                    return Ok(());
                }
                self.ensure_save_profile_current()?;
                let path = self
                    .settings
                    .save_file
                    .as_deref()
                    .context("Missing save location")?;
                let bytes = files::read_save(path)?;
                if files::sha256(&bytes) != hash {
                    self.send_state()?;
                    return Ok(());
                }
                self.send(&Event::SavePreview {
                    hash,
                    base: self.settings.sync_base.clone(),
                    bytes,
                })?;
            }
            Event::SavePreview { hash, base, bytes } => {
                ensure!(
                    valid_hash(&hash) && base.as_deref().is_none_or(valid_hash),
                    "Invalid preview hashes"
                );
                ensure!(
                    files::sha256(&bytes) == hash,
                    "Save preview SHA-256 mismatch"
                );
                if self
                    .preview_requested
                    .as_ref()
                    .is_none_or(|(requested, _)| requested != &hash)
                    || self
                        .remote
                        .as_ref()
                        .is_none_or(|remote| remote.hash.as_ref() != Some(&hash))
                {
                    return Ok(());
                }
                self.preserve_preview(hash, base, bytes)?;
                self.preview_requested = None;
            }
            Event::SaveDeclined { hash } => {
                ensure!(valid_hash(&hash), "Invalid declined save hash");
                if let Some((sent, _)) = &self.sent_save {
                    ensure!(sent == &hash, "Decline does not match the offered save");
                    self.sent_save = None;
                    self.transfer.clear();
                    self.message =
                        "Partner kept their save. Both branches remain unchanged.".into();
                }
            }
            Event::LeaseRequest { token, hash } => {
                ensure!(valid_hash(&token), "Invalid play-lock token");
                self.game_running = self.process.game_running();
                self.refresh_save()?;
                let result = (|| -> Result<PlayLock> {
                    ensure!(
                        self.no_transfer()
                            && !self.install_busy
                            && !self.texture_busy
                            && !self.self_updates.preparing,
                        "A transfer or installation is active"
                    );
                    ensure!(
                        self.save_valid
                            && self.local_hash == hash
                            && self.remote.as_ref().is_some_and(|r| r.save_ready)
                            && self.stable >= 2,
                        "Saves must match and be stable before play"
                    );
                    ensure!(
                        !self.remote.as_ref().is_some_and(|r| r.game),
                        "Peer already reports a running game"
                    );
                    play::acquire(
                        self.settings.play_lock.as_ref(),
                        self.game_running,
                        &peer,
                        &token,
                    )
                })();
                match result {
                    Ok(lock) => {
                        self.audit.record(
                            "play-lock-granted",
                            None,
                            hash.as_deref(),
                            Some(&peer),
                            &token,
                        )?;
                        self.store(|s| s.play_lock = Some(lock))?;
                        self.send(&Event::LeaseGranted { token })?;
                        self.send_state()?;
                    }
                    Err(error) => {
                        self.audit.record(
                            "play-lock-denied",
                            None,
                            hash.as_deref(),
                            Some(&peer),
                            &format!("{error:#}"),
                        )?;
                        self.send(&Event::LeaseDenied {
                            token,
                            reason: error.to_string().chars().take(200).collect(),
                        })?;
                    }
                }
            }
            Event::LeaseGranted { token } => {
                ensure!(
                    self.launch_pending.as_deref() == Some(&token)
                        && play::matches(
                            self.settings.play_lock.as_ref(),
                            &self.identity.public,
                            &token
                        ),
                    "Unexpected play-lock grant"
                );
                ensure!(
                    !self.process.game_running(),
                    "An external Dusklight launch was detected"
                );
                ensure!(
                    self.current_hash()? == self.local_hash
                        && self.save_valid
                        && self
                            .remote
                            .as_ref()
                            .is_some_and(|r| r.save_ready && r.hash == self.local_hash),
                    "Save changed while waiting for the play-lock grant"
                );
                self.launch_pending = None;
                self.start_game(Some(token))?;
            }
            Event::LeaseDenied { token, reason } => {
                ensure!(
                    reason.len() <= 256 && !reason.chars().any(char::is_control),
                    "Invalid lock denial"
                );
                ensure!(
                    self.launch_pending.as_deref() == Some(&token),
                    "Unexpected lock denial"
                );
                self.store(|s| s.play_lock = None)?;
                self.launch_pending = None;
                self.send_state()?;
                self.error = Some(format!("Launch denied by partner: {reason}"));
            }
            Event::LeaseRelease { token, hash } => {
                ensure!(
                    valid_hash(&token) && hash.as_ref().is_none_or(|h| valid_hash(h)),
                    "Invalid release"
                );
                ensure!(
                    !self.game_running && !self.remote.as_ref().is_some_and(|r| r.game),
                    "Cannot release a running game's lock"
                );
                ensure!(
                    self.save_valid && self.current_hash()? == hash,
                    "Final save must be committed before releasing play"
                );
                ensure!(
                    play::matches(self.settings.play_lock.as_ref(), &peer, &token)
                        || self.settings.play_lock.is_none(),
                    "Play lock owner or token mismatch"
                );
                self.audit.record(
                    "play-lock-released-peer",
                    None,
                    hash.as_deref(),
                    Some(&peer),
                    &token,
                )?;
                self.store(|s| s.play_lock = None)?;
                self.send(&Event::LeaseReleased { token })?;
                self.send_state()?;
            }
            Event::LeaseReleased { token } => {
                ensure!(
                    self.managed_token.as_deref() == Some(&token)
                        && self.release_sent
                        && self.child.is_none()
                        && !self.game_running,
                    "Unexpected release acknowledgement"
                );
                ensure!(
                    play::matches(
                        self.settings.play_lock.as_ref(),
                        &self.identity.public,
                        &token
                    ),
                    "Local lock changed"
                );
                self.store(|s| s.play_lock = None)?;
                self.managed_token = None;
                self.release_sent = false;
                self.audit.record(
                    "play-lock-released-local",
                    None,
                    self.local_hash.as_deref(),
                    Some(&peer),
                    &token,
                )?;
                self.send_state()?;
                self.message =
                    "Game stopped, final save synced, exclusive-play lock released.".into();
            }
            Event::Recover { token } => {
                ensure!(valid_hash(&token), "Invalid recovery token");
                ensure!(
                    !self.game_running && !self.remote.as_ref().is_some_and(|r| r.game),
                    "Cannot recover while a game is running"
                );
                ensure!(
                    self.settings
                        .play_lock
                        .as_ref()
                        .is_some_and(|l| l.token == token)
                        || self
                            .remote
                            .as_ref()
                            .and_then(|r| r.lock.as_ref())
                            .is_some_and(|l| l.token == token),
                    "Recovery token does not match a durable lock"
                );
                self.pending_recovery = Some(token);
                self.message =
                    "Partner requests crash recovery. Confirm every Dusklight process is stopped."
                        .into();
            }
            Event::RecoveryApproved { token } => {
                ensure!(
                    self.requested_recovery.as_deref() == Some(&token),
                    "Unexpected recovery approval"
                );
                ensure!(
                    !self.process.game_running() && !self.remote.as_ref().is_some_and(|r| r.game),
                    "A game started during recovery"
                );
                self.audit
                    .record("lock-recovered-both", None, None, Some(&peer), &token)?;
                self.store(|s| s.play_lock = None)?;
                self.requested_recovery = None;
                self.managed_token = None;
                self.launch_pending = None;
                self.release_sent = false;
                self.send_state()?;
                self.message =
                    "Both users approved recovery. Reconcile any save conflict before playing."
                        .into();
            }
            Event::IsoOffer { id, hash, size } => {
                self.receive_disc_offer(DiscFormat::Iso, id, hash, size)?
            }
            Event::RvzOffer { id, hash, size } => {
                self.receive_disc_offer(DiscFormat::Rvz, id, hash, size)?
            }
            Event::IsoAccept { id } => self.stream_iso(&id)?,
            Event::IsoChunk { id, bytes } => {
                let incoming = self
                    .incoming_iso
                    .as_mut()
                    .context("No approved disc-image transfer")?;
                ensure!(
                    incoming.id == id && !bytes.is_empty() && bytes.len() <= CHUNK_SIZE,
                    "Unexpected disc-image chunk"
                );
                ensure!(
                    !self.game_running,
                    "An external game started during disc-image transfer"
                );
                incoming.received += bytes.len() as u64;
                ensure!(
                    incoming.received <= incoming.size,
                    "Disc image exceeds approved size"
                );
                if incoming.header.len() < 0x440 {
                    let take = (0x440 - incoming.header.len()).min(bytes.len());
                    incoming.header.extend_from_slice(&bytes[..take]);
                }
                if incoming.format == DiscFormat::Rvz && incoming.header.len() >= 4 {
                    ensure!(
                        &incoming.header[..4] == b"RVZ\x01",
                        "Incoming image is not RVZ"
                    );
                }
                incoming.temporary.write_all(&bytes)?;
                incoming.sha.update(&bytes);
                if incoming.format == DiscFormat::Iso {
                    incoming.official.update(&bytes);
                }
                incoming.last_chunk = Instant::now();
                self.transfer = format!(
                    "Receiving {}: {:.1}%",
                    incoming.format.label(),
                    incoming.received as f64 * 100.0 / incoming.size as f64
                );
            }
            Event::IsoDone { id } => self.finish_iso(&id)?,
            Event::IsoReceived { id } => {
                let outgoing = self
                    .outgoing_iso
                    .as_ref()
                    .context("No outgoing disc image")?;
                ensure!(
                    outgoing.id == id && outgoing.started,
                    "Unexpected disc-image receipt"
                );
                self.audit.record(
                    "iso-acknowledged",
                    Some(&outgoing.path),
                    Some(&outgoing.hash),
                    Some(&peer),
                    "Peer verified official disc hash and SHA-256 before committing",
                )?;
                self.outgoing_iso = None;
                self.transfer = "Disc image synced and verified on the other device.".into();
            }
            _ => bail!("Unexpected twilightProto event in the paired state"),
        }
        Ok(())
    }

    fn current_hash(&self) -> Result<Option<String>> {
        save_location::hash(
            self.save_profile
                .as_deref()
                .context("Dusklight's automatic save profile is unavailable")?,
            self.settings
                .save_file
                .as_ref()
                .context("Dusklight's automatic save location is unavailable")?,
        )
    }

    fn save_destination(&mut self, bytes: &[u8]) -> Result<PathBuf> {
        let profile = self
            .save_profile
            .clone()
            .context("Dusklight's automatic save profile is unavailable")?;
        let identity = files::validate_save(bytes)?;
        let current = self
            .settings
            .save_file
            .as_ref()
            .context("Dusklight's automatic save location is unavailable")?;
        let target = save_location::path_for_storage(
            &profile,
            &identity,
            save_location::storage_for_path(current),
        )?;
        if current != &target {
            ensure!(
                self.current_hash()?.is_none(),
                "The existing default save belongs to another game region; it will not be replaced"
            );
            let discovered = save_location::discover(&profile)?;
            ensure!(
                files::read_optional_save(&discovered)?.is_none()
                    && files::read_optional_save(&target)?.is_none(),
                "A default save appeared after approval; refresh and retry rather than overwriting it"
            );
            self.apply_automatic_save_location(profile, target.clone())?;
        }
        Ok(target)
    }

    fn ensure_save_receive_allowed(&self, lease: Option<&str>) -> Result<()> {
        ensure!(
            !self.game_running && !self.remote.as_ref().is_some_and(|r| r.game),
            "Never replace saves while either game is running"
        );
        if let Some(lock) = &self.settings.play_lock {
            ensure!(
                lock.owner == self.peer_key()? && lease == Some(lock.token.as_str()),
                "Save transfer does not own the finalization lock"
            );
        } else {
            ensure!(lease.is_none(), "Unexpected finalization token");
        }
        Ok(())
    }

    fn ensure_save_profile_current(&self) -> Result<()> {
        if self.settings.play_lock.is_none() {
            ensure!(
                self.save_profile.as_ref()
                    == Some(&save_location::profile(
                        &self.default_profile,
                        self.settings.game_exe.as_deref(),
                    )?),
                "Dusklight's active profile changed after the save offer; retry after automatic detection completes"
            );
            ensure!(
                save_location::binding_is_current(
                    self.save_profile
                        .as_deref()
                        .context("Missing save profile")?,
                    self.settings
                        .save_file
                        .as_deref()
                        .context("Missing save location")?,
                )?,
                "Dusklight's memory-card format changed after the save offer; retry after detection completes"
            );
        }
        Ok(())
    }

    fn receive_save(&mut self, pending: PendingSave, manual: bool) -> Result<()> {
        self.commit_save(pending, manual, true)
    }

    fn commit_save(&mut self, pending: PendingSave, manual: bool, acknowledge: bool) -> Result<()> {
        self.game_running = self.process.game_running();
        self.ensure_save_receive_allowed(pending.lease.as_deref())?;
        self.ensure_save_profile_current()?;
        let path = self.save_destination(&pending.bytes)?;
        save_location::prepare(&path)?;
        self.audit.record(
            "save-receive-approved",
            Some(&path),
            Some(&pending.hash),
            Some(&self.peer_key()?),
            if manual {
                "Explicit conflict replacement approved"
            } else {
                "Conflict-free automatic sync"
            },
        )?;
        self.ensure_history()?;
        let peer = self.peer_key()?;
        let (hash, capture) = self
            .history
            .as_mut()
            .context("History is unavailable")?
            .replace_save(
                &path,
                &pending.bytes,
                pending.expected.as_deref(),
                history::Reason::Received,
                Some(&peer),
                None,
            )?;
        self.store(|s| s.sync_base = Some(hash.clone()))?;
        self.local_hash = Some(hash.clone());
        self.local_save = Some(save_data::inspect(&pending.bytes)?);
        self.previous_hash = Some(hash.clone());
        self.save_valid = true;
        self.stable = 2;
        self.stable_since = Instant::now() - Duration::from_secs(2);
        self.history_last_hash = Some(hash.clone());
        self.history_candidate = None;
        self.history_cursor = None;
        self.refresh_history()?;
        self.audit.record(
            "save-committed",
            Some(&path),
            Some(&hash),
            Some(&self.peer_key()?),
            &format!("Validated, snapshotted, atomically replaced, and re-read; commit {}; {} old commits pruned", capture.id, capture.pruned),
        )?;
        if acknowledge {
            self.send(&Event::Saved { hash })?;
        }
        self.send_state()?;
        self.message =
            "Received save committed safely; previous version retained in compact history.".into();
        Ok(())
    }

    fn offer_save(&mut self, manual: bool) -> Result<()> {
        ensure!(
            self.ready() && self.save_valid && self.local_hash.is_some() && self.stable >= 2,
            "Wait for a stable valid save and a connected partner"
        );
        ensure!(
            !self.game_running && !self.remote.as_ref().is_some_and(|r| r.game),
            "Wait until both games are stopped"
        );
        ensure!(self.no_transfer(), "Another transfer is pending");
        let remote = self
            .remote
            .as_ref()
            .context("Waiting for partner save status")?
            .clone();
        ensure!(
            remote.save_ready,
            "Wait for automatic save detection on the other device"
        );
        let lease = if let Some(lock) = &self.settings.play_lock {
            ensure!(
                lock.owner == self.identity.public
                    && self.managed_token.as_deref() == Some(&lock.token)
                    && self.child.is_none(),
                "A play lock is held"
            );
            Some(lock.token.clone())
        } else {
            ensure!(remote.lock.is_none(), "Partner holds a play lock");
            None
        };
        let path = self
            .settings
            .save_file
            .as_ref()
            .context("Dusklight's automatic save location is unavailable")?;
        let bytes = files::read_save(path)?;
        save_location::validate_at_path(
            self.save_profile
                .as_deref()
                .context("Automatic save profile is unavailable")?,
            path,
            &bytes,
        )?;
        let hash = files::sha256(&bytes);
        if remote.hash.as_ref() == Some(&hash) {
            return Ok(());
        }
        ensure!(
            manual || remote.hash.is_none() || remote.hash == self.settings.sync_base,
            "Both saves changed; choose which save to send"
        );
        self.audit.record(
            "save-offered",
            Some(path),
            Some(&hash),
            Some(&self.peer_key()?),
            if manual {
                "User initiated; receiver must approve any conflict"
            } else {
                "Automatic, common-base checked"
            },
        )?;
        self.send(&Event::Save {
            hash: hash.clone(),
            base: self.settings.sync_base.clone(),
            expected: remote.hash,
            manual,
            lease,
            bytes,
        })?;
        self.sent_save = Some((hash, Instant::now() + Duration::from_secs(120)));
        self.transfer = "Waiting for partner save approval...".into();
        self.message = "Save sent; waiting for validated commit or conflict approval.".into();
        Ok(())
    }

    fn preserve_preview(
        &mut self,
        hash: String,
        base: Option<String>,
        bytes: Vec<u8>,
    ) -> Result<()> {
        ensure!(files::sha256(&bytes) == hash, "Save preview hash differs");
        self.ensure_save_profile_current()?;
        let summary = save_data::inspect(&bytes)?;
        let profile = self
            .save_profile
            .as_deref()
            .context("Missing save profile")?;
        save_location::path_for_identity(profile, &files::validate_save(&bytes)?)?;
        let path = self
            .settings
            .save_file
            .clone()
            .context("Missing local save location")?;
        let peer = self.peer_key()?;
        self.ensure_history()?;
        let history = self.history.as_mut().context("History is unavailable")?;
        if !self.game_running
            && self.save_valid
            && let Some(current) = files::read_optional_save(&path)?
        {
            history.capture(&current, &path, history::Reason::Observed, None)?;
        }
        let base_summary = if self.settings.sync_base == base {
            base.as_deref()
                .map(|hash| history.summary_for_hash(hash))
                .transpose()?
                .flatten()
        } else {
            None
        };
        let captured = history.capture_branch(&bytes, &path, &peer, base.as_deref())?;
        self.audit.record(
            "save-branch-preserved",
            Some(&path),
            Some(&hash),
            Some(&peer),
            &captured.id,
        )?;
        self.remote_preview = Some(PeerPreview {
            hash,
            base,
            summary,
            base_summary,
            bytes,
        });
        self.history_cursor = None;
        self.refresh_history()?;
        Ok(())
    }

    fn save_conflict(&self) -> Option<SaveConflict> {
        let local = self.local_hash.as_ref()?;
        let remote = self
            .pending_save
            .as_ref()
            .map(|pending| &pending.hash)
            .or_else(|| self.remote.as_ref().and_then(|remote| remote.hash.as_ref()))?;
        if local == remote
            || (self.pending_save.is_none()
                && (Some(local) == self.settings.sync_base.as_ref()
                    || Some(remote) == self.settings.sync_base.as_ref()))
        {
            return None;
        }
        let preview = self
            .remote_preview
            .as_ref()
            .filter(|preview| &preview.hash == remote);
        let common_base = preview.is_none_or(|preview| preview.base == self.settings.sync_base);
        Some(SaveConflict {
            local_hash: local.clone(),
            remote_hash: remote.clone(),
            base_hash: common_base
                .then(|| self.settings.sync_base.clone())
                .flatten(),
            base: preview
                .filter(|_| common_base)
                .and_then(|preview| preview.base_summary.clone()),
            remote: preview.map(|preview| preview.summary.clone()),
        })
    }

    fn resolve_conflict(
        &mut self,
        local_hash: &str,
        remote_hash: &str,
        use_remote: bool,
    ) -> Result<()> {
        self.game_running = self.process.game_running();
        self.require_idle()?;
        ensure!(
            self.ready(),
            "Reconnect to reconcile saves; your local branch is preserved"
        );
        ensure!(
            self.current_hash()?.as_deref() == Some(local_hash),
            "Your save changed; review the new comparison"
        );
        ensure!(
            self.remote
                .as_ref()
                .is_some_and(|remote| remote.hash.as_deref() == Some(remote_hash)),
            "Partner's save changed; review the new comparison"
        );
        ensure!(
            self.sent_save.is_none()
                && self.incoming_iso.is_none()
                && self.outgoing_iso.is_none()
                && self.iso_offer.is_none()
                && !self.iso_preparing
                && self.iso_verifying.is_none(),
            "Finish other transfers before reconciling saves"
        );
        if use_remote {
            if let Some(pending) = self.pending_save.take() {
                ensure!(
                    pending.hash == remote_hash && pending.deadline > Instant::now(),
                    "Incoming save changed or expired"
                );
                return self.receive_save(pending, true);
            }
            let preview = self
                .remote_preview
                .as_ref()
                .filter(|preview| preview.hash == remote_hash)
                .context("Wait for the verified partner preview")?;
            let pending = PendingSave {
                hash: preview.hash.clone(),
                expected: Some(local_hash.into()),
                bytes: preview.bytes.clone(),
                lease: None,
                deadline: Instant::now() + Duration::from_secs(120),
            };
            self.commit_save(pending, true, false)
        } else {
            ensure!(
                self.remote_preview
                    .as_ref()
                    .is_some_and(|preview| preview.hash == remote_hash),
                "Wait for both branches to be preserved before choosing"
            );
            if let Some(pending) = self.pending_save.take() {
                self.send(&Event::SaveDeclined { hash: pending.hash })?;
            }
            self.offer_save(true)
        }
    }

    fn request_launch(&mut self) -> Result<()> {
        self.request_launch_mode(false)
    }

    fn request_launch_mode(&mut self, local_only: bool) -> Result<()> {
        self.ensure_history()?;
        if !self.textures_loaded && !self.texture_busy {
            self.reload_textures()?;
        }
        ensure!(
            !self.texture_state.recovery_needed,
            "Recover the interrupted texture switch before launching"
        );
        self.require_idle()?;
        ensure!(
            self.no_transfer()
                && !self.install_busy
                && !self.texture_busy
                && !self.self_updates.preparing,
            "Finish all transfers and installation before playing"
        );
        ensure!(
            self.settings.game_exe.as_ref().is_some_and(|p| p.is_file()),
            "Select or install Dusklight first"
        );
        self.game_running = self.process.game_running();
        ensure!(
            !self.game_running,
            "Dusklight was already launched outside this manager"
        );
        self.refresh_save()?;
        ensure!(
            self.save_valid && self.stable >= 2,
            "Wait for a stable, valid local save profile before playing"
        );
        let path = self
            .settings
            .save_file
            .clone()
            .context("Missing save location")?;
        let current = files::read_optional_save(&path)?;
        ensure!(
            current.as_deref().map(files::sha256).as_ref() == self.local_hash.as_ref(),
            "Local save changed before playing; retry after it stabilizes"
        );
        if let Some(bytes) = current {
            self.history
                .as_mut()
                .context("History is unavailable")?
                .capture(&bytes, &path, history::Reason::Observed, None)?;
            self.history_last_hash = self.local_hash.clone();
            self.history_candidate = None;
            self.history_cursor = None;
            self.refresh_history()?;
        }
        if local_only || !self.ready() {
            return self.start_game(None);
        }
        let remote = self
            .remote
            .as_ref()
            .context("Wait for the peer's save status")?;
        ensure!(
            self.save_valid
                && self.stable >= 2
                && remote.save_ready
                && self.local_hash == remote.hash,
            "Sync matching, valid saves before launching"
        );
        let token = token();
        let lock = play::acquire(
            self.settings.play_lock.as_ref(),
            self.game_running,
            &self.identity.public,
            &token,
        )?;
        self.audit.record(
            "play-lock-requested",
            None,
            self.local_hash.as_deref(),
            Some(&self.peer_key()?),
            &token,
        )?;
        self.store(|s| s.play_lock = Some(lock))?;
        self.launch_pending = Some(token.clone());
        self.send(&Event::LeaseRequest {
            token,
            hash: self.local_hash.clone(),
        })?;
        self.send_state()?;
        self.message = "Waiting for the other device's durable play-lock approval...".into();
        Ok(())
    }

    fn start_game(&mut self, lease: Option<String>) -> Result<()> {
        let executable = self
            .settings
            .game_exe
            .as_ref()
            .context("Select or install Dusklight")?;
        ensure!(
            self.save_profile.as_ref()
                == Some(&save_location::profile(
                    &self.default_profile,
                    Some(executable),
                )?),
            "Dusklight's profile changed while waiting for play approval; retry after automatic detection completes"
        );
        ensure!(
            save_location::binding_is_current(
                self.save_profile
                    .as_deref()
                    .context("Missing save profile")?,
                self.settings
                    .save_file
                    .as_deref()
                    .context("Missing save location")?,
            )?,
            "Dusklight's memory-card format changed while waiting for play approval"
        );
        let child = play::launch(executable)?;
        let protected = lease.is_some();
        self.managed_token = lease;
        self.child = Some(child);
        self.game_running = true;
        self.audit.record(
            "game-started",
            Some(executable),
            self.local_hash.as_deref(),
            self.settings
                .peer
                .as_ref()
                .map(|peer| peer.public_key.as_str()),
            if protected {
                "Both devices durably reserved the same exclusive-play lock"
            } else {
                "Independent local play; progress retained for later reconciliation"
            },
        )?;
        self.send_state()?;
        self.message = if protected {
            "Dusklight is running with online play protection."
        } else if self.ready() {
            "Playing this PC's save branch. Other progress remains preserved for reconciliation."
        } else {
            "Playing offline. Progress stays on this PC until your partner reconnects."
        }
        .into();
        Ok(())
    }

    fn send_state(&mut self) -> Result<()> {
        if self.ready() {
            self.send(&Event::State {
                hash: if self.save_valid {
                    self.local_hash.clone()
                } else {
                    None
                },
                save_ready: self.save_valid,
                game: self.game_running,
                lock: self.settings.play_lock.clone(),
            })?;
        }
        Ok(())
    }

    fn refresh_default_save(&mut self) -> Result<()> {
        let result = self
            .resolve_default_save()
            .context("Automatic Dusklight save detection failed; no save was moved or overwritten");
        if let Err(error) = &result {
            self.save_location_error = Some(format!("{error:#}"));
            self.save_profile = None;
            if self.settings.save_file.is_some() {
                self.store(|s| {
                    s.save_file = None;
                    s.sync_base = None;
                })?;
            }
            self.local_hash = None;
            self.local_save = None;
            self.previous_hash = None;
            self.save_valid = false;
            self.stable = 0;
            self.history_candidate = None;
            self.send_state()?;
        } else if let Some(detail) = self.save_location_error.take()
            && self.error.as_deref() == Some(detail.as_str())
        {
            self.error = None;
        }
        result
    }

    fn resolve_default_save(&mut self) -> Result<()> {
        let pinned = self.save_profile.is_some()
            && (self.game_running
                || self.child.is_some()
                || self.settings.play_lock.is_some()
                || self.launch_pending.is_some()
                || !self.no_transfer());
        let profile = if pinned {
            if !self.no_transfer() {
                return Ok(());
            }
            if let Some(path) = &self.settings.save_file {
                if files::is_raw(path) {
                    if self.game_running || self.current_hash()?.is_some() {
                        return Ok(());
                    }
                } else if path.try_exists()? {
                    return Ok(());
                }
            }
            self.save_profile
                .clone()
                .context("Pinned save profile is missing")?
        } else {
            save_location::profile(&self.default_profile, self.settings.game_exe.as_deref())?
        };
        let path = if pinned {
            save_location::discover_with_storage(
                &profile,
                save_location::storage_for_path(
                    self.settings
                        .save_file
                        .as_deref()
                        .context("Missing pinned save location")?,
                ),
            )?
        } else {
            save_location::discover(&profile)?
        };
        self.apply_automatic_save_location(profile, path)
    }

    fn apply_automatic_save_location(&mut self, profile: PathBuf, path: PathBuf) -> Result<()> {
        let changed = self.settings.save_file.as_ref() != Some(&path);
        if changed || self.save_profile.is_none() {
            self.audit.record(
                "automatic-save-location",
                Some(&path),
                None,
                None,
                "Automatically resolved from Dusklight's active profile",
            )?;
        }
        if changed {
            self.store(|s| {
                s.save_file = Some(path);
                s.sync_base = None;
            })?;
            self.local_hash = None;
            self.local_save = None;
            self.previous_hash = None;
            self.save_valid = false;
            self.stable = 0;
            self.stable_since = Instant::now();
            self.history_candidate = None;
            self.history_last_hash = None;
            self.history_cursor = None;
        }
        if self.save_profile.as_ref() != Some(&profile) {
            self.textures_loaded = false;
        }
        self.save_profile = Some(profile);
        if changed {
            self.send_state()?;
        }
        Ok(())
    }

    fn refresh_save(&mut self) -> Result<()> {
        self.refresh_default_save()?;
        if self.game_running {
            self.stable = 0;
            self.stable_since = Instant::now();
            return Ok(());
        }
        if self.settings.save_file.is_none() {
            self.local_hash = None;
            self.save_valid = false;
            return Ok(());
        }
        match self.current_hash() {
            Ok(hash) => {
                let was_valid = self.save_valid;
                self.save_valid = true;
                if hash == self.previous_hash {
                    self.stable = if self.stable_since.elapsed() >= Duration::from_secs(2) {
                        2
                    } else {
                        0
                    };
                } else {
                    self.stable = 0;
                    self.stable_since = Instant::now();
                }
                self.previous_hash = hash.clone();
                let changed = !was_valid || self.local_hash != hash;
                if changed {
                    self.local_save = match &hash {
                        Some(expected) => {
                            let bytes = files::read_save(
                                self.settings
                                    .save_file
                                    .as_deref()
                                    .context("Missing save location")?,
                            )?;
                            ensure!(
                                files::sha256(&bytes) == *expected,
                                "Save changed during inspection; retrying shortly"
                            );
                            Some(save_data::inspect(&bytes)?)
                        }
                        None => None,
                    };
                }
                self.local_hash = hash;
                if changed {
                    self.send_state()?;
                }
                Ok(())
            }
            Err(error) => {
                self.save_valid = false;
                self.stable = 0;
                self.stable_since = Instant::now();
                self.local_hash = None;
                self.local_save = None;
                self.send_state()?;
                Err(error).context("Save validation failed; no data was transferred or overwritten")
            }
        }
    }

    fn initialize_self_updates(&mut self) -> Result<()> {
        match self_update::load_enabled(&self.root) {
            Ok(enabled) => self.self_updates.enabled = enabled,
            Err(error) => {
                self.self_updates.enabled = false;
                self.self_updates.error =
                    Some(format!("Update preferences need review: {error:#}"));
                return Err(error);
            }
        }
        self.clean_completed_updates()
    }

    fn clean_completed_updates(&mut self) -> Result<()> {
        match self_update::cleanup(&self.root) {
            Ok(count) => {
                if count > 0 {
                    self.audit.record("app-update-staging-cleaned", None, None, None, &format!("Removed {count} completed/cancelled updater folders; failed backups were retained"))?;
                }
            }
            Err(error) => {
                let detail = format!("Completed update cleanup needs review: {error:#}");
                self.audit
                    .record("app-update-cleanup-failed", None, None, None, &detail)?;
                self.self_updates.error = Some(detail);
            }
        }
        Ok(())
    }

    fn begin_self_update_check(&mut self) -> Result<()> {
        ensure!(
            !self.self_updates.checking && !self.self_updates.preparing,
            "A Twilight Sync update operation is already running"
        );
        ensure!(
            self.last_self_update_check
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(10)),
            "Update checks are rate limited; retry shortly"
        );
        self.audit.record(
            "app-update-check-requested",
            None,
            None,
            None,
            "Official GitHub stable-release metadata only; no automatic download or installation",
        )?;
        self.last_self_update_check = Some(Instant::now());
        self.self_updates.checking = true;
        self.self_updates.error = None;
        self.self_updates.message = "Checking official Twilight Sync releases...".into();
        let sender = self.sender.clone();
        thread::spawn(move || {
            if sender
                .send(Input::SelfUpdateChecked(self_update::check()))
                .is_err()
            {
                eprintln!("Twilight Sync stopped during its app-update check");
            }
        });
        Ok(())
    }

    fn reload_textures(&mut self) -> Result<()> {
        let profile =
            save_location::profile(&self.default_profile, self.settings.game_exe.as_deref())?;
        if self.settings.texture_profile.as_ref() != Some(&profile) {
            self.store(|settings| settings.texture_profile = Some(profile))?;
        }
        self.texture_state =
            textures::inventory(&self.root, self.settings.texture_profile.as_deref())?;
        self.textures_loaded = true;
        Ok(())
    }

    fn ensure_history(&mut self) -> Result<()> {
        if self.history.is_none() {
            self.history = Some(History::open(
                &self.root,
                self.audit.username(),
                &self.identity.public,
            )?);
        }
        if !self.migrated_backups {
            let migrated = self
                .history
                .as_mut()
                .context("History is unavailable")?
                .migrate_backups(&self.root.join("backups"))?;
            if migrated > 0 {
                self.audit.record("legacy-backups-migrated", None, None, None, &format!("{migrated} verified backups migrated into compressed history before deleting originals"))?;
            }
            self.migrated_backups = true;
        }
        self.history
            .as_mut()
            .context("History is unavailable")?
            .ensure_ready()?;
        Ok(())
    }

    fn refresh_history(&mut self) -> Result<()> {
        self.ensure_history()?;
        self.history_page = self
            .history
            .as_ref()
            .context("History is unavailable")?
            .page(self.history_cursor.as_deref())?;
        Ok(())
    }

    fn snapshot_tick(&mut self) -> Result<()> {
        let Some(path) = self.settings.save_file.clone() else {
            return Ok(());
        };
        ensure_safe_path(&path)?;
        if !path.try_exists()? {
            self.history_candidate = None;
            return Ok(());
        }
        let bytes = match files::read_optional_save(&path).and_then(|bytes| {
            if let Some(bytes) = &bytes {
                save_location::validate_at_path(
                    self.save_profile
                        .as_deref()
                        .context("Automatic save profile is unavailable")?,
                    &path,
                    bytes,
                )?;
            }
            Ok(bytes)
        }) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                self.history_candidate = None;
                return Ok(());
            }
            Err(error) if self.game_running => {
                let notice = format!("Waiting for a complete valid in-game save: {error:#}");
                if self.history_page.notice.as_deref() != Some(&notice) {
                    self.audit.record(
                        "snapshot-waiting-for-stable-save",
                        Some(&path),
                        None,
                        None,
                        &notice,
                    )?;
                }
                self.history_page.notice = Some(notice);
                self.history_candidate = None;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let hash = files::sha256(&bytes);
        if self
            .history_candidate
            .as_ref()
            .is_none_or(|(candidate, _)| *candidate != hash)
        {
            self.history_candidate = Some((hash, Instant::now()));
            return Ok(());
        }
        if self
            .history_candidate
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() < Duration::from_secs(2))
        {
            return Ok(());
        }
        self.history_page.notice = None;
        if self.history_last_hash.as_ref() == Some(&hash) {
            return Ok(());
        }
        self.ensure_history()?;
        let captured = self
            .history
            .as_mut()
            .context("History is unavailable")?
            .capture(&bytes, &path, history::Reason::Observed, None)?;
        if captured.added {
            self.audit.record(
                "save-snapshot-created",
                Some(&path),
                Some(&hash),
                None,
                &format!(
                    "commit {}; {} old commits pruned",
                    captured.id, captured.pruned
                ),
            )?;
        }
        self.local_hash = Some(hash.clone());
        self.local_save = Some(save_data::inspect(&bytes)?);
        self.save_valid = true;
        self.history_last_hash = Some(hash);
        self.history_cursor = None;
        self.refresh_history()?;
        self.send_state()?;
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        if self.self_updates.enabled
            && !self.self_updates.checking
            && !self.self_updates.preparing
            && self.last_self_update_check.map_or_else(
                || Instant::now() >= self.first_self_update_check,
                |last| last.elapsed() >= self_update::CHECK_INTERVAL,
            )
        {
            self.begin_self_update_check()?;
        }
        if !self.self_updates.preparing
            && self.last_self_update_cleanup.elapsed() >= Duration::from_secs(60)
        {
            self.last_self_update_cleanup = Instant::now();
            self.clean_completed_updates()?;
        }
        if self.network.is_some() && self.last_verified.elapsed() > Duration::from_secs(12) {
            self.stop_network("Router verification is stale; all network activity stopped");
            self.network = None;
            bail!("Router verification exceeded its freshness limit");
        }
        if self.last_network.elapsed() >= Duration::from_secs(5) && !self.network_job {
            self.last_network = Instant::now();
            self.network_job = true;
            let sender = self.sender.clone();
            thread::spawn(move || {
                if sender.send(Input::Network(lan::probe())).is_err() {
                    eprintln!("Twilight Sync stopped during the LAN check");
                }
            });
        }
        if self.trusted_network() {
            self.network_tick()?;
        }
        if self.last_ping.elapsed() >= Duration::from_secs(5) {
            self.last_ping = Instant::now();
            if self.link.is_some() {
                self.send(&Event::Ping)?;
                self.send_state()?;
            }
        }
        if self.link.as_ref().is_some_and(|l| {
            (!l.ready && l.deadline < Instant::now())
                || l.last_seen.elapsed() > Duration::from_secs(20)
        }) {
            self.disconnect("Pairing or heartbeat timed out; play locks were preserved");
        }
        if self
            .iso_verifying
            .as_ref()
            .is_some_and(|job| job.deadline < Instant::now())
        {
            self.disconnect("Disc verification timed out; staged data was discarded");
        }
        if self
            .pending_save
            .as_ref()
            .is_some_and(|p| p.deadline < Instant::now())
            || self
                .iso_offer
                .as_ref()
                .is_some_and(|p| p.deadline < Instant::now())
            || self
                .outgoing_iso
                .as_ref()
                .is_some_and(|p| (!p.started || p.sent) && p.deadline < Instant::now())
            || self
                .sent_save
                .as_ref()
                .is_some_and(|(_, deadline)| *deadline < Instant::now())
            || self
                .incoming_iso
                .as_ref()
                .is_some_and(|p| p.last_chunk.elapsed() > Duration::from_secs(30))
        {
            self.disconnect("Transfer approval or progress timed out; staged data discarded");
        }
        if self.last_process.elapsed() >= Duration::from_secs(1) {
            self.last_process = Instant::now();
            self.process_tick()?;
            if !self.textures_loaded && !self.texture_busy {
                self.reload_textures()?;
            }
            self.refresh_save()?;
            self.snapshot_tick()?;
            self.sync_tick()?;
        }
        Ok(())
    }

    fn network_tick(&mut self) -> Result<()> {
        let network = self.network.as_ref().context("No LAN")?.clone();
        self.devices
            .retain(|_, d| d.seen.elapsed() < Duration::from_secs(8));
        if self.last_discovery.elapsed() >= Duration::from_secs(2) {
            self.last_discovery = Instant::now();
            let bytes = serde_json::to_vec(&Advertisement {
                magic: "twilightProto/1".into(),
                network: network.fingerprint.clone(),
                key: self.identity.public.clone(),
                name: self.settings.name.clone(),
                pairing: self.pairing_open(),
            })?;
            if let Some(socket) = &self.discovery {
                socket.send_to(&bytes, (network.broadcast(), protocol::DISCOVERY_PORT))?;
            }
        }
        if let Some(socket) = &self.discovery {
            let mut buffer = [0; 513];
            for _ in 0..16 {
                match socket.recv_from(&mut buffer) {
                    Ok((count, source)) => match lan::decode_advertisement(
                        &buffer[..count],
                        source,
                        &network,
                        &self.identity.public,
                    ) {
                        Ok(device) => {
                            if self.devices.contains_key(&device.key) || self.devices.len() < 32 {
                                self.devices.insert(device.key.clone(), device);
                            }
                        }
                        Err(_) => self.discovery_rejections += 1,
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error).context("Receiving local discovery"),
                }
            }
        }
        if self.discovery_rejections > 0
            && self.last_rejection_log.elapsed() >= Duration::from_secs(30)
        {
            self.audit.record(
                "discovery-rejected",
                None,
                None,
                None,
                &format!(
                    "{} invalid/untrusted announcements; contents not logged",
                    self.discovery_rejections
                ),
            )?;
            self.discovery_rejections = 0;
            self.last_rejection_log = Instant::now();
        }
        if self.link.is_none()
            && !self.connecting.load(Ordering::Acquire)
            && self.last_connect.elapsed() >= Duration::from_secs(2)
        {
            if let Some(listener) = &self.listener {
                match listener.accept() {
                    Ok((stream, source)) => {
                        let known_source = self.devices.values().any(|d| {
                            d.address.ip() == source.ip()
                                && (self
                                    .settings
                                    .peer
                                    .as_ref()
                                    .is_some_and(|p| p.public_key == d.key)
                                    || (self.pairing_open() && d.pairing))
                        });
                        if known_source {
                            self.connect(source, None, Some(stream))?;
                        } else {
                            self.discovery_rejections += 1;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error).context("Accepting local channel"),
                }
            }
            if self.link.is_none()
                && !self.connecting.load(Ordering::Acquire)
                && self.last_connect.elapsed() >= Duration::from_secs(5)
                && let Some(peer) = &self.settings.peer
                && self.identity.public > peer.public_key
                && let Some(device) = self.devices.get(&peer.public_key)
            {
                self.connect(device.address, Some(peer.public_key.clone()), None)?;
            }
        }
        Ok(())
    }

    fn process_tick(&mut self) -> Result<()> {
        let was_running = self.game_running;
        self.game_running = self.process.game_running();
        if let Some(child) = &mut self.child
            && let Some(status) = child.try_wait()?
        {
            self.audit.record(
                "game-stopped",
                self.settings.game_exe.as_deref(),
                None,
                None,
                &status.to_string(),
            )?;
            self.child = None;
            self.stable = 0;
            self.stable_since = Instant::now();
            self.release_sent = false;
            if self.managed_token.is_none() {
                self.message = "Game stopped. Local progress will sync or be compared when your partner is available.".into();
            }
        }
        if self.game_running && self.child.is_none() {
            self.message = "Dusklight was launched outside Twilight Sync. Save writes are blocked; exclusive play cannot be enforced for this launch.".into();
            if !was_running {
                self.audit.record(
                    "external-game-detected",
                    None,
                    None,
                    None,
                    "No automatic termination; sync and managed launches blocked",
                )?;
            }
        }
        if was_running != self.game_running {
            self.send_state()?;
        }
        Ok(())
    }

    fn sync_tick(&mut self) -> Result<()> {
        if (!self.ready() || self.save_conflict().is_some())
            && !self.game_running
            && self.child.is_none()
            && self.save_valid
            && self.stable >= 2
            && self.managed_token.as_deref().is_some_and(|token| {
                play::matches(
                    self.settings.play_lock.as_ref(),
                    &self.identity.public,
                    token,
                )
            })
        {
            ensure!(
                self.current_hash()? == self.local_hash,
                "Final local save changed"
            );
            self.audit.record(
                "play-session-completed-offline",
                self.settings.save_file.as_deref(),
                self.local_hash.as_deref(),
                None,
                "Final save retained locally; reconnect will reconcile before any replacement",
            )?;
            self.store(|settings| settings.play_lock = None)?;
            self.managed_token = None;
            self.release_sent = false;
            self.message = "Game stopped. Progress is saved on this PC and will sync when your partner returns.".into();
            self.send_state()?;
        }
        if !self.ready() || self.game_running || !self.save_valid || self.stable < 2 {
            return Ok(());
        }
        let Some(remote) = &self.remote else {
            return Ok(());
        };
        if remote.game || !remote.save_ready {
            return Ok(());
        }
        if let Some(conflict) = self.save_conflict() {
            if self.no_transfer()
                && self.remote_preview.is_none()
                && self.preview_requested.as_ref().is_none_or(|(hash, sent)| {
                    hash != &conflict.remote_hash || sent.elapsed() >= Duration::from_secs(10)
                })
            {
                self.send(&Event::SavePreviewRequest {
                    hash: conflict.remote_hash.clone(),
                })?;
                self.preview_requested = Some((conflict.remote_hash, Instant::now()));
            }
            self.message = "Save fork: both PCs have different progress. Compare the branches or continue your local save.".into();
            return Ok(());
        }
        if self.local_hash == remote.hash {
            let hash = self.local_hash.clone();
            if self.settings.sync_base != hash {
                self.store(|s| s.sync_base = hash)?;
            }
            if self.managed_token.is_some()
                && self.child.is_none()
                && !self.release_sent
                && self.no_transfer()
            {
                let token = self
                    .managed_token
                    .clone()
                    .context("Missing finalization token")?;
                self.send(&Event::LeaseRelease {
                    token,
                    hash: self.local_hash.clone(),
                })?;
                self.release_sent = true;
            }
            return Ok(());
        }
        let remote = self.remote.as_ref().context("Missing peer status")?;
        if self.local_hash.is_none() {
            return Ok(());
        }
        let allowed_lock = self.settings.play_lock.as_ref().is_none_or(|l| {
            l.owner == self.identity.public
                && self.managed_token.as_deref() == Some(&l.token)
                && self.child.is_none()
        });
        if self.no_transfer()
            && allowed_lock
            && (remote.hash.is_none() || remote.hash == self.settings.sync_base)
        {
            self.offer_save(false)?;
        }
        Ok(())
    }

    fn receive_disc_offer(
        &mut self,
        format: DiscFormat,
        id: String,
        hash: String,
        size: u64,
    ) -> Result<()> {
        ensure!(
            self.settings.iso_enabled,
            "Disc-image transfer is disabled locally"
        );
        self.require_idle()?;
        ensure!(self.no_transfer(), "A transfer is already active");
        ensure!(
            valid_hash(&id) && valid_hash(&hash),
            "Invalid disc offer identity or hash"
        );
        format.validate_size(size)?;
        self.iso_offer = Some(IsoOffer {
            id,
            hash,
            size,
            format,
            deadline: Instant::now() + Duration::from_secs(120),
        });
        self.message = format!(
            "Your partner offers a verified {} image. Accept only if you own this game.",
            format.label()
        );
        Ok(())
    }

    fn accept_iso(&mut self, destination: Option<PathBuf>) -> Result<()> {
        let offer = self
            .iso_offer
            .take()
            .context("No incoming disc-image offer")?;
        if let Some(destination) = destination {
            self.require_idle()?;
            ensure!(
                self.settings.iso_enabled && offer.deadline > Instant::now(),
                "Disc-image permission or offer expired"
            );
            ensure_safe_path(&destination)?;
            ensure!(
                !destination.try_exists()?,
                "Choose a new destination; existing disc images are never overwritten"
            );
            ensure!(
                DiscFormat::from_path(&destination)? == offer.format,
                "The destination extension must match the offered {} image",
                offer.format.label()
            );
            let parent = destination
                .parent()
                .context("Destination has no directory")?;
            ensure!(parent.is_dir(), "Disc-image directory does not exist");
            ensure!(
                fs2::available_space(parent)? > offer.size + 64 * 1024 * 1024,
                "Insufficient disk space for a staged disc image"
            );
            let temporary = NamedTempFile::new_in(parent)?;
            let id = offer.id.clone();
            self.audit.record(
                "iso-receive-approved",
                Some(&destination),
                Some(&offer.hash),
                Some(&self.peer_key()?),
                "User explicitly approved this disc image and local destination",
            )?;
            self.incoming_iso = Some(IncomingIso {
                id: offer.id,
                hash: offer.hash,
                size: offer.size,
                format: offer.format,
                received: 0,
                sha: Sha256::new(),
                official: Xxh3::new(),
                header: Vec::with_capacity(0x440),
                temporary,
                destination,
                last_chunk: Instant::now(),
            });
            self.send(&Event::IsoAccept { id })?;
        } else {
            self.send(&Event::Error {
                message: "Disc-image transfer declined by the other user".into(),
            })?;
        }
        Ok(())
    }

    fn stream_iso(&mut self, id: &str) -> Result<()> {
        self.require_idle()?;
        ensure!(
            self.settings.iso_enabled,
            "Disc-image permission was revoked"
        );
        let outgoing = self
            .outgoing_iso
            .as_mut()
            .context("No outgoing disc-image offer")?;
        ensure!(
            outgoing.id == id && !outgoing.started && outgoing.deadline > Instant::now(),
            "Unexpected disc-image acceptance"
        );
        outgoing.started = true;
        let path = outgoing.path.clone();
        let expected = outgoing.hash.clone();
        let size = outgoing.size;
        let transfer_id = outgoing.id.clone();
        let link = self.link.as_ref().context("No channel")?;
        let id = link.id;
        let writer = link.writer.clone();
        let sender = self.sender.clone();
        let cancel = self.cancel_transfer.clone();
        self.transfer = "Streaming approved disc image in bounded 32 KiB chunks...".into();
        thread::spawn(move || {
            let result = (|| -> Result<()> {
                ensure_safe_path(&path)?;
                let mut file = File::open(&path)?;
                ensure!(
                    file.metadata()?.len() == size,
                    "Disc image changed since verification"
                );
                let mut buffer = [0; CHUNK_SIZE];
                let mut sha = Sha256::new();
                let mut total = 0_u64;
                loop {
                    ensure!(
                        !cancel.load(Ordering::Acquire),
                        "Disc-image transfer was cancelled"
                    );
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    total += count as u64;
                    ensure!(total <= size, "Disc image grew during transfer");
                    sha.update(&buffer[..count]);
                    writer
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Writer lock poisoned"))?
                        .send(&Event::IsoChunk {
                            id: transfer_id.clone(),
                            bytes: buffer[..count].to_vec(),
                        })?;
                }
                ensure!(
                    total == size && hex::encode(sha.finalize()) == expected,
                    "Disc image changed during transfer"
                );
                writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Writer lock poisoned"))?
                    .send(&Event::IsoDone { id: transfer_id })?;
                Ok(())
            })();
            if sender.send(Input::IsoSent { id, result }).is_err() {
                eprintln!("Twilight Sync stopped while the disc sender was finishing");
            }
        });
        Ok(())
    }

    fn finish_iso(&mut self, id: &str) -> Result<()> {
        let incoming = self.incoming_iso.take().context("No incoming disc image")?;
        ensure!(
            incoming.id == id && incoming.received == incoming.size,
            "Disc image is incomplete"
        );
        ensure!(
            !self.process.game_running(),
            "A game started while receiving the disc image"
        );
        ensure!(
            hex::encode(incoming.sha.clone().finalize()) == incoming.hash,
            "Received disc SHA-256 mismatch"
        );
        if incoming.format == DiscFormat::Iso {
            let expected = files::validate_iso_header(&incoming.header, incoming.size)?;
            ensure!(
                format!("{:032x}", incoming.official.digest128()) == expected,
                "Received ISO is not an unmodified official disc"
            );
        }
        incoming.temporary.as_file().sync_all()?;
        let pending = PendingDisc {
            id: incoming.id,
            hash: incoming.hash,
            size: incoming.size,
            format: incoming.format,
            destination: incoming.destination,
            temporary: incoming.temporary.into_temp_path(),
        };
        if pending.format == DiscFormat::Iso {
            return self.commit_disc(pending);
        }
        let channel = self
            .link
            .as_ref()
            .context("Missing RVZ verification channel")?
            .id;
        self.iso_verifying = Some(DiscVerification {
            channel,
            id: pending.id.clone(),
            deadline: Instant::now() + disc::VERIFY_TIMEOUT,
        });
        self.transfer = "RVZ received; verifying the reconstructed original disc...".into();
        let sender = self.sender.clone();
        let cancel = self.cancel_transfer.clone();
        std::thread::spawn(move || {
            let result = disc::verify_rvz(&pending.temporary, cancel);
            if sender
                .send(Input::IsoVerified {
                    channel,
                    pending: Box::new(pending),
                    result,
                })
                .is_err()
            {
                eprintln!("Twilight Sync stopped during RVZ verification; staging was discarded");
            }
        });
        Ok(())
    }

    fn commit_disc(&mut self, incoming: PendingDisc) -> Result<()> {
        self.game_running = self.process.game_running();
        self.require_idle()?;
        ensure!(
            self.ready() && self.trusted_network() && self.settings.iso_enabled,
            "Disc permission or the trusted channel changed before commit"
        );
        ensure_safe_path(&incoming.destination)?;
        incoming
            .temporary
            .persist_noclobber(&incoming.destination)
            .map_err(|e| e.error)?;
        self.audit.record(
            "iso-committed",
            Some(&incoming.destination),
            Some(&incoming.hash),
            Some(&self.peer_key()?),
            &format!(
                "{}: reconstructed official XXH3-128 and transfer SHA-256 verified; never executed",
                incoming.format.label()
            ),
        )?;
        self.store(|s| s.iso_file = Some(incoming.destination))?;
        self.send(&Event::IsoReceived { id: incoming.id })?;
        self.transfer = format!(
            "{} received, verified, and committed.",
            incoming.format.label()
        );
        Ok(())
    }

    fn publish(&mut self) {
        let link = self.link.as_ref();
        let snapshot = Snapshot {
            settings: self.settings.clone(),
            public_key: self.identity.public.clone(),
            network: self.network.clone(),
            devices: self.devices.values().cloned().collect(),
            peer_name: link.and_then(|l| l.name.clone()),
            comparison_code: link
                .filter(|l| !l.committed)
                .map(|l| l.comparison_code.clone()),
            local_approved: link.is_some_and(|l| l.local_approved),
            remote_approved: link.is_some_and(|l| l.remote_approved),
            ready: self.ready(),
            connecting: self.connecting.load(Ordering::Acquire),
            pairing_open: self.pairing_open(),
            local_hash: self.local_hash.clone(),
            local_save: self.local_save.clone(),
            conflict: self.save_conflict(),
            save_profile: self.save_profile.clone(),
            save_ready: self.save_valid,
            remote_hash: self.remote.as_ref().and_then(|r| r.hash.clone()),
            game_running: self.game_running,
            remote_game: self.remote.as_ref().is_some_and(|r| r.game),
            pending_save: self.pending_save.is_some(),
            pending_iso: self.iso_offer.is_some(),
            pending_disc_format: self.iso_offer.as_ref().map(|offer| offer.format),
            pending_recovery: self.pending_recovery.is_some(),
            transfer: self.transfer.clone(),
            install_busy: self.install_busy,
            latest_release: self.latest_release.clone(),
            selected_release: self.selected_release.clone(),
            texture_busy: self.texture_busy,
            textures: self.texture_state.clone(),
            history: self.history_page.clone(),
            self_updates: self.self_updates.clone(),
            message: self.message.clone(),
            error: self.error.clone(),
            shutdown: self.shutdown,
        };
        match self.view.lock() {
            Ok(mut view) => *view = snapshot,
            Err(_) => {
                self.fatal = true;
                self.shutdown = true;
                eprintln!(
                    "Twilight Sync dashboard state poisoned; shutting down without clearing play locks"
                );
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::files::ISO_SIZE;
    use std::fs;

    struct Harness {
        left: Engine,
        right: Engine,
        left_rx: Receiver<Input>,
        right_rx: Receiver<Input>,
        _directory: tempfile::TempDir,
    }

    fn variant(marker: u8) -> Vec<u8> {
        let mut bytes = save_location::tests::stock_save("GZ2E");
        bytes[0x100] = marker;
        bytes
    }

    fn fork_harness() -> Result<Harness> {
        let mut left = save_data::tests::populated(true)?;
        left[8..40].fill(0);
        left[8..16].copy_from_slice(b"gczelda2");
        let mut right = left.clone();
        for offset in [0x4048, 0x6048] {
            right[offset + 4..offset + 6].copy_from_slice(&456_u16.to_be_bytes());
        }
        files::tests::reseal(&mut right)?;
        let mut harness = Harness::new(Some(left), Some(right))?;
        harness.pair()?;
        harness.left.sync_tick()?;
        harness.pump_until(|harness| harness.left.remote_preview.is_some())?;
        Ok(harness)
    }

    #[test]
    fn fork_preview_preserves_both_branches_without_overwriting() -> Result<()> {
        let harness = fork_harness()?;
        let conflict = harness.left.save_conflict().context("Missing conflict")?;
        assert_eq!(
            harness
                .left
                .local_save
                .as_ref()
                .context("Missing local summary")?
                .slots[0]
                .rupees,
            321
        );
        assert_eq!(
            conflict.remote.context("Missing partner summary")?.slots[0].rupees,
            456
        );
        assert!(harness.left.pending_save.is_none());
        let history = harness.left.history.as_ref().context("Missing history")?;
        for hash in [&conflict.local_hash, &conflict.remote_hash] {
            let id = history
                .find_hash(hash)?
                .context("Branch was not preserved")?;
            assert_eq!(files::sha256(&history.read(&id)?), *hash);
        }
        assert_eq!(
            harness.left.current_hash()?.as_ref(),
            Some(&conflict.local_hash)
        );
        Ok(())
    }

    #[test]
    fn fork_selecting_partner_reconciles_locally_and_retains_original() -> Result<()> {
        let mut harness = fork_harness()?;
        let conflict = harness.left.save_conflict().context("Missing conflict")?;
        harness
            .left
            .resolve_conflict(&conflict.local_hash, &conflict.remote_hash, true)?;
        harness.pump_until(|harness| {
            harness
                .right
                .remote
                .as_ref()
                .is_some_and(|remote| remote.hash == harness.right.local_hash)
        })?;
        harness.right.sync_tick()?;
        assert_eq!(harness.left.current_hash()?, harness.right.current_hash()?);
        assert!(harness.left.save_conflict().is_none());
        let history = harness.left.history.as_ref().context("Missing history")?;
        assert!(history.find_hash(&conflict.local_hash)?.is_some());
        assert!(history.page(None)?.entries[0].merge_parent.is_some());
        Ok(())
    }

    #[test]
    fn fork_selecting_local_still_requires_partner_consent() -> Result<()> {
        let mut harness = fork_harness()?;
        let conflict = harness.left.save_conflict().context("Missing conflict")?;
        harness
            .left
            .resolve_conflict(&conflict.local_hash, &conflict.remote_hash, false)?;
        harness.pump_until(|harness| harness.right.pending_save.is_some())?;
        assert_eq!(
            harness.right.current_hash()?.as_ref(),
            Some(&conflict.remote_hash)
        );
        harness.right.command(Command::AcceptSave(true))?;
        harness.pump_until(|harness| harness.left.sent_save.is_none())?;
        assert_eq!(
            harness.right.current_hash()?.as_ref(),
            Some(&conflict.local_hash)
        );
        assert!(harness.left.history_page.entries[0].merge_parent.is_some());
        Ok(())
    }

    #[test]
    fn fork_can_continue_local_play_without_matching_or_replacing_partner() -> Result<()> {
        let mut harness = fork_harness()?;
        let before = harness.right.current_hash()?;
        let executable = harness.left.root.join("dusklight.exe");
        fs::write(&executable, b"Not an executable")?;
        harness.left.settings.game_exe = Some(executable);
        let expected = harness.left.local_hash.clone();
        let error = harness
            .left
            .command(Command::ContinueLocal { expected })
            .err()
            .context("Invalid executable accepted")?;
        assert!(
            format!("{error:#}").contains("Launching Dusklight"),
            "{error:#}"
        );
        assert!(harness.left.settings.play_lock.is_none());
        assert!(harness.left.sent_save.is_none());
        assert_eq!(harness.right.current_hash()?, before);
        Ok(())
    }

    #[test]
    fn fork_with_completed_online_lease_does_not_deadlock_reconciliation() -> Result<()> {
        let mut harness = fork_harness()?;
        let lease = PlayLock {
            owner: harness.left.identity.public.clone(),
            token: token(),
        };
        harness.left.managed_token = Some(lease.token.clone());
        harness.left.settings.play_lock = Some(lease.clone());
        harness.right.settings.play_lock = Some(lease);
        harness.left.sync_tick()?;
        harness.pump_until(|harness| harness.right.settings.play_lock.is_none())?;
        assert!(harness.left.settings.play_lock.is_none());
        assert!(harness.left.save_conflict().is_some());
        Ok(())
    }

    #[test]
    fn fork_progress_refreshes_only_after_a_stable_valid_autosave() -> Result<()> {
        let mut harness = fork_harness()?;
        harness.left.game_running = true;
        let path = harness
            .left
            .settings
            .save_file
            .clone()
            .context("Missing save")?;
        let mut bytes = files::read_save(&path)?;
        for offset in [0x4048, 0x6048] {
            bytes[offset + 4..offset + 6].copy_from_slice(&777_u16.to_be_bytes());
        }
        files::tests::reseal(&mut bytes)?;
        files::atomic_write(&path, &bytes)?;
        harness.left.snapshot_tick()?;
        assert_eq!(
            harness
                .left
                .local_save
                .as_ref()
                .context("Missing progress")?
                .slots[0]
                .rupees,
            321
        );
        harness
            .left
            .history_candidate
            .as_mut()
            .context("Missing candidate")?
            .1 = Instant::now() - Duration::from_secs(2);
        harness.left.snapshot_tick()?;
        assert_eq!(
            harness
                .left
                .local_save
                .as_ref()
                .context("Missing progress")?
                .slots[0]
                .rupees,
            777
        );
        assert!(harness.left.sent_save.is_none());
        Ok(())
    }

    #[test]
    fn fork_automatic_offer_with_disagreeing_bases_requires_consent() -> Result<()> {
        let mut harness = Harness::new(Some(variant(2)), Some(variant(3)))?;
        harness.pair()?;
        let original = harness.right.current_hash()?;
        harness.left.settings.sync_base = original.clone();
        harness.right.settings.sync_base = Some(files::sha256(&variant(1)));
        harness.left.sync_tick()?;
        harness.pump_until(|harness| harness.right.pending_save.is_some())?;
        assert_eq!(harness.right.current_hash()?, original);
        assert!(harness.right.save_conflict().is_some());
        Ok(())
    }

    #[test]
    fn fork_stale_comparison_or_running_game_never_replaces_a_save() -> Result<()> {
        let mut harness = fork_harness()?;
        let conflict = harness.left.save_conflict().context("Missing conflict")?;
        harness.left.process = ProcessMonitor::for_test(true);
        assert!(
            harness
                .left
                .resolve_conflict(&conflict.local_hash, &conflict.remote_hash, true)
                .is_err()
        );
        assert_eq!(
            harness.left.current_hash()?.as_ref(),
            Some(&conflict.local_hash)
        );
        harness.left.process = ProcessMonitor::for_test(false);
        let path = harness
            .left
            .settings
            .save_file
            .clone()
            .context("Missing save")?;
        let mut changed = files::read_save(&path)?;
        changed[0x100] ^= 1;
        files::atomic_write(&path, &changed)?;
        assert!(
            harness
                .left
                .resolve_conflict(&conflict.local_hash, &conflict.remote_hash, true)
                .is_err()
        );
        assert_eq!(files::read_save(&path)?, changed);
        Ok(())
    }

    #[test]
    fn offline_launch_needs_no_peer_or_network_and_failure_leaves_no_lock() -> Result<()> {
        let mut harness = Harness::new(Some(variant(1)), None)?;
        let engine = &mut harness.left;
        engine.disconnect("Offline launch test");
        engine.network = None;
        engine.settings.peer = None;
        let executable = engine.root.join("dusklight.exe");
        fs::write(&executable, b"Not an executable")?;
        engine.settings.game_exe = Some(executable);
        let error = engine
            .request_launch()
            .err()
            .context("Invalid executable was accepted")?;
        assert!(
            format!("{error:#}").contains("Launching Dusklight"),
            "{error:#}"
        );
        assert!(engine.settings.play_lock.is_none());
        assert!(engine.managed_token.is_none());
        assert!(engine.launch_pending.is_none());
        assert!(engine.child.is_none());
        Ok(())
    }

    #[test]
    fn offline_completion_releases_only_the_finished_local_session() -> Result<()> {
        let mut harness = Harness::new(Some(variant(1)), Some(variant(1)))?;
        let lease = token();
        let owner = harness.left.identity.public.clone();
        harness.left.settings.play_lock = Some(PlayLock {
            owner: owner.clone(),
            token: lease.clone(),
        });
        harness.left.managed_token = Some(lease.clone());
        harness.right.settings.play_lock = Some(PlayLock {
            owner,
            token: lease,
        });
        harness.left.disconnect("Connection lost during play");
        harness.left.game_running = true;
        harness.left.sync_tick()?;
        assert!(harness.left.settings.play_lock.is_some());
        harness.left.game_running = false;
        harness.left.stable = 0;
        harness.left.sync_tick()?;
        assert!(harness.left.settings.play_lock.is_some());
        harness.left.stable = 2;
        harness.left.sync_tick()?;
        assert!(harness.left.settings.play_lock.is_none());
        assert!(harness.left.managed_token.is_none());
        harness.right.authenticated_event(Event::State {
            hash: harness.left.local_hash.clone(),
            save_ready: true,
            game: false,
            lock: None,
        })?;
        assert!(harness.right.settings.play_lock.is_none());
        Ok(())
    }

    fn native_variant(marker: u8) -> Vec<u8> {
        let mut bytes = files::fixture_native_save();
        bytes[8..40].fill(0);
        bytes[8..16].copy_from_slice(b"gczelda2");
        bytes[0x100] = marker;
        bytes
    }

    fn use_raw_card(engine: &mut Engine, save: Option<&[u8]>) -> Result<PathBuf> {
        fs::create_dir_all(&engine.default_profile)?;
        let path = engine.default_profile.join("MemoryCardA.USA.raw");
        fs::write(&path, crate::raw_card::tests::empty())?;
        if let Some(bytes) = save {
            files::write_save(&path, bytes, None)?;
        }
        files::atomic_write(
            &engine.default_profile.join("config.json"),
            &serde_json::to_vec(
                &serde_json::json!({"backend.cardFileType":0,"video.enableVsync":false}),
            )?,
        )?;
        engine.refresh_save()?;
        engine.stable_since = Instant::now() - Duration::from_secs(2);
        engine.refresh_save()?;
        Ok(path)
    }

    impl Harness {
        fn new(left_save: Option<Vec<u8>>, right_save: Option<Vec<u8>>) -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let mut engines = Vec::new();
            let mut receivers = Vec::new();
            for (name, address, save) in [
                ("Alice", "192.168.10.2", left_save),
                ("Bob", "192.168.10.3", right_save),
            ] {
                let root = directory.path().join(name);
                fs::create_dir(&root)?;
                let network = Network::from_parts(
                    address.parse()?,
                    24,
                    "192.168.10.1".parse()?,
                    "02-AA-BB-CC-DD-EE",
                    "Test adapter".into(),
                )?;
                let profile = root.join("Dusklight");
                let path = match &save {
                    Some(bytes) => {
                        save_location::path_for_identity(&profile, &files::validate_save(bytes)?)?
                    }
                    None => save_location::discover(&profile)?,
                };
                if let Some(bytes) = save {
                    save_location::prepare(&path)?;
                    files::atomic_write(&path, &bytes)?;
                }
                let settings = Settings {
                    name: name.into(),
                    startup: false,
                    network: Some(network.fingerprint.clone()),
                    save_file: Some(path),
                    ..Settings::default()
                };
                settings.persist(&root)?;
                let identity = Arc::new(Identity::load(&root)?);
                let audit = Audit::new(&root)?;
                let (sender, receiver) = bounded(128);
                let view = Arc::new(Mutex::new(initial_snapshot(&settings, &identity.public)));
                let mut engine =
                    Engine::new(root, settings, profile, identity, audit, sender, view);
                engine.network = Some(network);
                engine.last_verified = Instant::now();
                engine.process = ProcessMonitor::for_test(false);
                engine.pairing_until = Some(Instant::now() + Duration::from_secs(120));
                engine.refresh_save()?;
                engine.stable_since = Instant::now() - Duration::from_secs(2);
                engine.refresh_save()?;
                engines.push(engine);
                receivers.push(receiver);
            }
            let right = engines.pop().context("Missing right test engine")?;
            let left = engines.pop().context("Missing left test engine")?;
            let right_rx = receivers.pop().context("Missing right test inbox")?;
            let left_rx = receivers.pop().context("Missing left test inbox")?;
            let mut harness = Self {
                left,
                right,
                left_rx,
                right_rx,
                _directory: directory,
            };
            harness.attach()?;
            harness.pump_until(|h| {
                h.left.link.as_ref().is_some_and(|l| l.name.is_some())
                    && h.right.link.as_ref().is_some_and(|l| l.name.is_some())
            })?;
            Ok(harness)
        }

        fn attach(&mut self) -> Result<()> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let right_private = self.right.identity.private_key().to_vec();
            let network = self
                .left
                .network
                .as_ref()
                .context("Missing test LAN")?
                .fingerprint
                .clone();
            let remote_network = network.clone();
            let worker = thread::spawn(move || -> Result<Handshake> {
                let (stream, _) = listener.accept()?;
                // Accepted Winsock sockets inherit the listener's nonblocking mode.
                stream.set_nonblocking(true)?;
                protocol::handshake(stream, &right_private, &remote_network, false)
            });
            let left_session = protocol::handshake(
                TcpStream::connect(address)?,
                self.left.identity.private_key(),
                &network,
                true,
            )?;
            let right_session = worker
                .join()
                .map_err(|_| anyhow::anyhow!("Test handshake panicked"))??;
            ensure!(
                left_session.comparison_code == right_session.comparison_code,
                "Comparison codes differ"
            );
            self.left.last_verified = Instant::now();
            self.right.last_verified = Instant::now();
            self.left.establish(
                rand::random(),
                left_session,
                Some(self.right.identity.public.clone()),
            )?;
            self.right.establish(
                rand::random(),
                right_session,
                Some(self.left.identity.public.clone()),
            )?;
            Ok(())
        }

        fn pump_until(&mut self, predicate: impl Fn(&Self) -> bool) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                for _ in 0..128 {
                    let Ok(input) = self.left_rx.try_recv() else {
                        break;
                    };
                    self.left.input(input)?;
                }
                for _ in 0..128 {
                    let Ok(input) = self.right_rx.try_recv() else {
                        break;
                    };
                    self.right.input(input)?;
                }
                if predicate(self) {
                    return Ok(());
                }
                ensure!(
                    Instant::now() < deadline,
                    "Timed out waiting for two-device protocol transition"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }

        fn pair(&mut self) -> Result<()> {
            self.left.command(Command::ApprovePairing)?;
            self.pump_until(|h| h.right.link.as_ref().is_some_and(|l| l.remote_approved))?;
            ensure!(
                self.left.settings.peer.is_none() && self.right.settings.peer.is_none(),
                "One-sided approval established trust"
            );
            self.right.command(Command::ApprovePairing)?;
            self.pump_until(|h| {
                h.left.ready()
                    && h.right.ready()
                    && h.left.remote.is_some()
                    && h.right.remote.is_some()
            })
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.left.disconnect("Test completed");
            self.right.disconnect("Test completed");
        }
    }

    #[test]
    fn app_update_opt_out_and_network_failures_do_not_change_save_settings() -> Result<()> {
        let mut h = Harness::new(Some(variant(1)), None)?;
        let original = fs::read(h.left.root.join("settings.json"))?;
        h.left.command(Command::SetAutoUpdateChecks(false))?;
        assert!(!self_update::load_enabled(&h.left.root)?);
        assert_eq!(fs::read(h.left.root.join("settings.json"))?, original);
        h.left.input(Input::SelfUpdateChecked(Err(anyhow::anyhow!(
            "simulated offline connection"
        ))))?;
        assert!(
            h.left
                .self_updates
                .error
                .as_ref()
                .is_some_and(|error| error.contains("offline"))
        );
        assert!(
            h.left.error.is_none(),
            "App update metadata failures must not fail LAN syncing"
        );
        assert!(!h.left.self_updates.available());
        Ok(())
    }

    #[test]
    fn app_updates_require_verified_availability_and_an_idle_manager() -> Result<()> {
        let mut h = Harness::new(Some(variant(1)), None)?;
        h.left.self_updates.preparing = true;
        assert!(h.left.require_idle().is_err());
        h.left.self_updates.preparing = false;
        h.left.process = ProcessMonitor::for_test(true);
        assert!(h.left.command(Command::UpdateSelf).is_err());
        h.left.process = ProcessMonitor::for_test(false);
        assert!(h.left.command(Command::UpdateSelf).is_err());
        assert!(!h.left.self_updates.preparing);
        assert!(
            !h.left.root.join("updates").try_exists()?,
            "No available release means no download or staging"
        );
        Ok(())
    }

    #[test]
    fn valid_stable_autosaves_are_snapshotted_while_the_game_runs() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), None)?;
        h.left.game_running = true;
        h.left.snapshot_tick()?;
        let candidate = h
            .left
            .history_candidate
            .as_mut()
            .context("Missing history candidate")?;
        candidate.1 = Instant::now() - Duration::from_secs(2);
        h.left.snapshot_tick()?;
        assert_eq!(h.left.history_page.stats.commits, 1);
        let path = h.left.settings.save_file.clone().context("Missing path")?;
        files::atomic_write(&path, &variant(1))?;
        h.left.snapshot_tick()?;
        h.left
            .history_candidate
            .as_mut()
            .context("Missing candidate")?
            .1 = Instant::now() - Duration::from_secs(2);
        h.left.snapshot_tick()?;
        assert_eq!(h.left.history_page.stats.commits, 2);
        assert_eq!(
            h.left.history_page.entries[0].hash,
            files::sha256(&variant(1))
        );
        assert!(
            h.left.sent_save.is_none(),
            "In-game snapshots must not send saves"
        );
        Ok(())
    }

    #[test]
    fn restoring_from_the_dashboard_creates_a_new_history_commit() -> Result<()> {
        let mut h = Harness::new(Some(variant(1)), Some(variant(1)))?;
        h.pair()?;
        h.left.ensure_history()?;
        let path = h.left.settings.save_file.clone().context("Missing path")?;
        let captured = h
            .left
            .history
            .as_mut()
            .context("Missing history")?
            .capture(&variant(1), &path, history::Reason::Observed, None)?;
        files::atomic_write(&path, &variant(2))?;
        h.left.refresh_save()?;
        h.left.command(Command::RestoreSnapshot {
            id: captured.id,
            expected: Some(files::sha256(&variant(2))),
        })?;
        assert_eq!(files::read_save(&path)?, variant(1));
        assert_eq!(
            h.left.history_page.entries[0].reason,
            history::Reason::Restored
        );
        assert!(
            h.left
                .history_page
                .entries
                .iter()
                .any(|entry| entry.hash == files::sha256(&variant(2)))
        );
        Ok(())
    }

    #[test]
    fn texture_operations_and_restore_are_blocked_during_gameplay_or_management() -> Result<()> {
        let mut h = Harness::new(Some(variant(1)), None)?;
        h.left.process = ProcessMonitor::for_test(true);
        assert!(
            h.left
                .command(Command::ActivateTextures {
                    id: Some("a".repeat(64)),
                    preserve_unmanaged: false
                })
                .is_err()
        );
        assert!(
            h.left
                .command(Command::RestoreSnapshot {
                    id: "b".repeat(64),
                    expected: h.left.local_hash.clone()
                })
                .is_err()
        );
        h.left.game_running = false;
        h.left.texture_busy = true;
        assert!(h.left.require_idle().is_err());
        Ok(())
    }

    #[test]
    fn consent_is_required_on_both_devices_and_persists_identity() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), None)?;
        assert!(!h.left.ready() && !h.right.ready());
        h.pair()?;
        assert_eq!(
            Settings::load(&h.left.root)?
                .peer
                .context("Missing peer")?
                .public_key,
            h.right.identity.public
        );
        assert_eq!(
            Settings::load(&h.right.root)?
                .peer
                .context("Missing peer")?
                .public_key,
            h.left.identity.public
        );
        assert_eq!(Identity::load(&h.left.root)?.public, h.left.identity.public);
        Ok(())
    }

    #[test]
    fn unpaired_channel_cannot_send_save_data() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), None)?;
        let bytes = variant(0);
        assert!(
            h.right
                .event(Event::Save {
                    hash: files::sha256(&bytes),
                    base: None,
                    expected: None,
                    manual: false,
                    lease: None,
                    bytes,
                })
                .is_err()
        );
        assert!(
            !h.right
                .settings
                .save_file
                .as_ref()
                .context("No test path")?
                .try_exists()?
        );
        Ok(())
    }

    #[test]
    fn encrypted_sync_commits_only_to_the_detected_local_default() -> Result<()> {
        let mut h = Harness::new(Some(variant(1)), None)?;
        h.pair()?;
        h.left.offer_save(false)?;
        h.pump_until(|h| h.left.sent_save.is_none() && h.right.local_hash == h.left.local_hash)?;
        assert_eq!(
            files::read_save(
                h.right
                    .settings
                    .save_file
                    .as_ref()
                    .context("No test path")?
            )?,
            variant(1)
        );
        assert_eq!(h.left.settings.sync_base, h.right.settings.sync_base);
        Ok(())
    }

    #[test]
    fn conflict_requires_receiver_consent_and_preserves_the_old_copy() -> Result<()> {
        let mut h = Harness::new(Some(variant(1)), Some(variant(2)))?;
        let base = files::sha256(&variant(0));
        h.left.store(|s| s.sync_base = Some(base.clone()))?;
        h.right.store(|s| s.sync_base = Some(base))?;
        h.pair()?;
        assert!(h.left.offer_save(false).is_err());
        h.left.offer_save(true)?;
        h.pump_until(|h| h.right.pending_save.is_some())?;
        let path = h.right.settings.save_file.clone().context("No test path")?;
        assert_eq!(files::read_save(&path)?, variant(2));
        h.right.command(Command::AcceptSave(true))?;
        h.pump_until(|h| h.left.sent_save.is_none())?;
        assert_eq!(files::read_save(&path)?, variant(1));
        let history = h
            .right
            .history
            .as_ref()
            .context("Missing receiver history")?;
        let page = history.page(None)?;
        let old = page
            .entries
            .iter()
            .find(|entry| entry.hash == files::sha256(&variant(2)))
            .context("Replaced save was not snapshotted")?;
        assert_eq!(history.read(&old.id)?, variant(2));
        assert!(!h.right.root.join("backups").try_exists()?);
        Ok(())
    }

    #[test]
    fn declining_a_conflict_never_changes_the_destination() -> Result<()> {
        let mut h = Harness::new(Some(variant(1)), Some(variant(2)))?;
        h.pair()?;
        h.left.offer_save(true)?;
        h.pump_until(|h| h.right.pending_save.is_some())?;
        h.right.command(Command::AcceptSave(false))?;
        h.pump_until(|h| h.left.sent_save.is_none())?;
        assert_eq!(
            files::read_save(h.right.settings.save_file.as_ref().context("No path")?)?,
            variant(2)
        );
        Ok(())
    }

    #[test]
    fn detects_a_game_started_after_the_offer_before_commit() -> Result<()> {
        let mut h = Harness::new(Some(variant(1)), Some(variant(0)))?;
        h.pair()?;
        h.right.process = ProcessMonitor::for_test(true);
        let pending = PendingSave {
            hash: files::sha256(&variant(1)),
            expected: h.right.local_hash.clone(),
            bytes: variant(1),
            lease: None,
            deadline: Instant::now() + Duration::from_secs(120),
        };
        assert!(h.right.receive_save(pending, false).is_err());
        assert_eq!(
            files::read_save(h.right.settings.save_file.as_ref().context("No path")?)?,
            variant(0)
        );
        Ok(())
    }

    #[test]
    fn actual_elapsed_stability_is_required_not_repeated_polling() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), None)?;
        let path = h.left.settings.save_file.clone().context("No path")?;
        files::atomic_write(&path, &variant(1))?;
        for _ in 0..20 {
            h.left.refresh_save()?;
            if h.left.stable_since.elapsed() < Duration::from_secs(2) {
                assert_eq!(h.left.stable, 0);
            }
        }
        h.left.stable_since = Instant::now() - Duration::from_secs(2);
        h.left.refresh_save()?;
        assert_eq!(h.left.stable, 2);
        Ok(())
    }

    #[test]
    fn crash_lock_survives_disconnect_and_needs_bilateral_recovery() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
        h.pair()?;
        let lock = PlayLock {
            owner: h.left.identity.public.clone(),
            token: token(),
        };
        h.left.store(|s| s.play_lock = Some(lock.clone()))?;
        h.right.store(|s| s.play_lock = Some(lock))?;
        h.left.launch_pending = Some(token());
        h.left.disconnect("Simulated process/network crash");
        h.right.disconnect("Simulated process/network crash");
        assert!(h.left.launch_pending.is_none());
        assert!(Settings::load(&h.left.root)?.play_lock.is_some());
        assert!(Settings::load(&h.right.root)?.play_lock.is_some());
        h.attach()?;
        h.pump_until(|h| {
            h.left.ready() && h.right.ready() && h.left.remote.is_some() && h.right.remote.is_some()
        })?;
        h.left.command(Command::Recover)?;
        h.pump_until(|h| h.right.pending_recovery.is_some())?;
        assert!(h.left.settings.play_lock.is_some() && h.right.settings.play_lock.is_some());
        h.right.command(Command::AcceptRecovery(true))?;
        h.pump_until(|h| {
            h.left.settings.play_lock.is_none() && h.right.settings.play_lock.is_none()
        })?;
        assert!(Settings::load(&h.left.root)?.play_lock.is_none());
        assert!(Settings::load(&h.right.root)?.play_lock.is_none());
        Ok(())
    }

    #[test]
    fn a_second_owner_or_wrong_final_save_cannot_release_a_lock() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
        h.pair()?;
        let held = PlayLock {
            owner: h.left.identity.public.clone(),
            token: token(),
        };
        h.right.store(|s| s.play_lock = Some(held.clone()))?;
        assert!(
            h.right
                .event(Event::LeaseRelease {
                    token: held.token.clone(),
                    hash: Some("0".repeat(64))
                })
                .is_err()
        );
        assert!(h.right.settings.play_lock.is_some());
        assert!(
            h.right
                .event(Event::LeaseRelease {
                    token: token(),
                    hash: h.right.local_hash.clone()
                })
                .is_err()
        );
        assert!(h.right.settings.play_lock.is_some());
        Ok(())
    }

    #[test]
    fn changing_routers_disconnects_without_clearing_a_play_lock() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
        h.pair()?;
        h.left.store(|s| {
            s.play_lock = Some(PlayLock {
                owner: "a".repeat(64),
                token: token(),
            })
        })?;
        let network = Network::from_parts(
            "192.168.10.2".parse()?,
            24,
            "192.168.10.1".parse()?,
            "02-AA-BB-CC-DD-EF",
            "Different router".into(),
        )?;
        h.left.input(Input::Network(Ok(network)))?;
        assert!(h.left.link.is_none());
        assert!(h.left.listener.is_none() && h.left.discovery.is_none());
        assert!(!h.left.trusted_network());
        assert!(h.left.settings.play_lock.is_some());
        Ok(())
    }

    #[test]
    fn stale_router_checks_stop_network_activity() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), None)?;
        h.left.last_verified = Instant::now() - Duration::from_secs(13);
        assert!(h.left.tick().is_err());
        assert!(h.left.network.is_none() && h.left.link.is_none());
        Ok(())
    }

    #[test]
    fn iso_transfer_is_a_separate_disabled_by_default_permission() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
        h.pair()?;
        assert!(Settings::default().startup);
        assert!(!h.right.settings.iso_enabled);
        assert!(
            h.right
                .event(Event::IsoOffer {
                    id: token(),
                    hash: "a".repeat(64),
                    size: ISO_SIZE
                })
                .is_err()
        );
        assert!(h.right.iso_offer.is_none() && h.right.incoming_iso.is_none());
        Ok(())
    }

    #[test]
    fn rvz_requires_its_own_valid_offer_consent_and_matching_destination() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
        h.pair()?;
        assert!(
            h.right
                .event(Event::RvzOffer {
                    id: token(),
                    hash: "a".repeat(64),
                    size: 512
                })
                .is_err()
        );
        h.right.command(Command::SetIsoEnabled(true))?;
        for size in [0, 0x147, disc::MAX_IMAGE_SIZE + 1, u64::MAX] {
            assert!(
                h.right
                    .event(Event::RvzOffer {
                        id: token(),
                        hash: "a".repeat(64),
                        size
                    })
                    .is_err()
            );
            assert!(h.right.iso_offer.is_none());
        }
        h.right.event(Event::RvzOffer {
            id: token(),
            hash: "a".repeat(64),
            size: 512,
        })?;
        h.right.publish();
        assert_eq!(
            h.right
                .view
                .lock()
                .map_err(|_| anyhow::anyhow!("Snapshot lock poisoned"))?
                .pending_disc_format,
            Some(DiscFormat::Rvz)
        );
        assert!(h.right.incoming_iso.is_none());
        let wrong = h.right.root.join("wrong.iso");
        assert!(h.right.accept_iso(Some(wrong.clone())).is_err());
        assert!(!wrong.exists() && h.right.incoming_iso.is_none());
        h.right.event(Event::RvzOffer {
            id: token(),
            hash: "a".repeat(64),
            size: 512,
        })?;
        let destination = h.right.root.join("received.RVZ");
        h.right.accept_iso(Some(destination.clone()))?;
        let staged = h
            .right
            .incoming_iso
            .as_ref()
            .context("Missing staged RVZ")?
            .temporary
            .path()
            .to_path_buf();
        assert!(staged.is_file() && !destination.exists() && !h.right.no_transfer());
        assert_eq!(
            h.right.incoming_iso.as_ref().map(|image| image.format),
            Some(DiscFormat::Rvz)
        );
        h.right.disconnect("Testing interrupted RVZ");
        assert!(!staged.exists() && !destination.exists());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn encrypted_rvz_receiver_remains_responsive_and_independently_rejects_modified_discs()
    -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
        h.pair()?;
        h.left.command(Command::SetIsoEnabled(true))?;
        h.right.command(Command::SetIsoEnabled(true))?;
        let (bytes, _) = disc::tests::fixture(ISO_SIZE, true)?;
        let source = h.left.root.join("modified.rvz");
        fs::write(&source, &bytes)?;
        let verified = VerifiedDisc {
            format: DiscFormat::Rvz,
            sha256: files::sha256(&bytes),
            size: bytes.len() as u64,
        };
        // A paired malicious sender can lie about validation; the receiver must verify independently.
        h.left.input(Input::IsoPrepared {
            id: h.left.link.as_ref().context("Missing source channel")?.id,
            path: source,
            result: Ok(verified),
        })?;
        h.pump_until(|h| h.right.iso_offer.is_some())?;
        let destination = h.right.root.join("must-not-commit.rvz");
        h.right.accept_iso(Some(destination.clone()))?;
        h.pump_until(|h| {
            h.right.iso_verifying.is_some()
                && h.left.outgoing_iso.as_ref().is_some_and(|image| image.sent)
        })?;
        assert!(!destination.exists());
        let last_seen = h
            .right
            .link
            .as_ref()
            .context("Missing receiver channel")?
            .last_seen;
        h.left.send(&Event::Ping)?;
        h.pump_until(|h| {
            h.right
                .link
                .as_ref()
                .is_some_and(|link| link.last_seen > last_seen)
        })?;
        assert!(
            h.right.iso_verifying.is_some(),
            "Verifier finished before the responsiveness check"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let input = h
                .right_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            if matches!(&input, Input::IsoVerified { .. }) {
                let error = h
                    .right
                    .input(input)
                    .err()
                    .context("Modified disc passed receiver verification")?;
                assert!(
                    format!("{error:#}").contains("official unmodified disc"),
                    "{error:#}"
                );
                break;
            }
            h.right.input(input)?;
        }
        h.pump_until(|h| h.left.outgoing_iso.is_none())?;
        assert!(!destination.exists() && h.right.incoming_iso.is_none());
        assert!(h.right.iso_verifying.is_none());
        Ok(())
    }

    // Inject worker results here to isolate actor commit gates; disc::tests exercises the real decoder.
    fn staged_rvz(
        engine: &mut Engine,
        name: &str,
    ) -> Result<(u64, Box<PendingDisc>, VerifiedDisc)> {
        let mut bytes = vec![0_u8; 512];
        bytes[..4].copy_from_slice(b"RVZ\x01");
        let destination = engine.root.join(name);
        let mut temporary = NamedTempFile::new_in(&engine.root)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        let verified = VerifiedDisc {
            format: DiscFormat::Rvz,
            sha256: files::sha256(&bytes),
            size: bytes.len() as u64,
        };
        let pending = Box::new(PendingDisc {
            id: token(),
            hash: verified.sha256.clone(),
            size: verified.size,
            format: verified.format,
            temporary: temporary.into_temp_path(),
            destination,
        });
        let channel = engine.link.as_ref().context("Missing test channel")?.id;
        engine.iso_verifying = Some(DiscVerification {
            channel,
            id: pending.id.clone(),
            deadline: Instant::now() + disc::VERIFY_TIMEOUT,
        });
        engine.transfer = "Verifying RVZ".into();
        assert!(!engine.no_transfer());
        Ok((channel, pending, verified))
    }

    #[test]
    fn verified_rvz_commits_compressed_bytes_and_never_clobbers_a_destination() -> Result<()> {
        for existing in [false, true] {
            let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
            h.pair()?;
            h.right.command(Command::SetIsoEnabled(true))?;
            let (channel, pending, verified) = staged_rvz(&mut h.right, "received.rvz")?;
            let destination = pending.destination.clone();
            let staged = pending.temporary.to_path_buf();
            let hash = verified.sha256.clone();
            if existing {
                fs::write(&destination, b"keep my original image")?;
            }
            let result = h.right.input(Input::IsoVerified {
                channel,
                pending,
                result: Ok(verified),
            });
            assert!(!staged.exists() && h.right.iso_verifying.is_none());
            if existing {
                assert!(result.is_err());
                assert_eq!(fs::read(&destination)?, b"keep my original image");
                assert!(h.right.settings.iso_file.is_none());
            } else {
                result?;
                assert_eq!(files::sha256(&fs::read(&destination)?), hash);
                assert_eq!(h.right.settings.iso_file.as_ref(), Some(&destination));
                assert!(
                    h.right
                        .transfer
                        .contains("RVZ received, verified, and committed")
                );
            }
        }
        Ok(())
    }

    #[test]
    fn rvz_worker_failure_or_result_mismatch_discards_staging() -> Result<()> {
        for mismatch in 0..4 {
            let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
            h.pair()?;
            h.right.command(Command::SetIsoEnabled(true))?;
            let (channel, pending, mut verified) = staged_rvz(&mut h.right, "rejected.rvz")?;
            let destination = pending.destination.clone();
            let staged = pending.temporary.to_path_buf();
            let result = match mismatch {
                0 => Err(anyhow::anyhow!("Synthetic decoder rejection")),
                1 => {
                    verified.sha256 = "f".repeat(64);
                    Ok(verified)
                }
                2 => {
                    verified.size += 1;
                    Ok(verified)
                }
                _ => {
                    verified.format = DiscFormat::Iso;
                    Ok(verified)
                }
            };
            assert!(
                h.right
                    .input(Input::IsoVerified {
                        channel,
                        pending,
                        result
                    })
                    .is_err()
            );
            assert!(!staged.exists() && !destination.exists());
            assert!(h.right.iso_verifying.is_none() && h.right.transfer.is_empty());
        }
        Ok(())
    }

    #[test]
    fn rvz_commit_rechecks_game_locks_router_permission_and_management() -> Result<()> {
        for change in 0..6 {
            let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
            h.pair()?;
            h.right.command(Command::SetIsoEnabled(true))?;
            let (channel, pending, verified) = staged_rvz(&mut h.right, "blocked.rvz")?;
            let destination = pending.destination.clone();
            let staged = pending.temporary.to_path_buf();
            match change {
                0 => h.right.process = ProcessMonitor::for_test(true),
                1 => {
                    h.right.settings.play_lock = Some(PlayLock {
                        owner: h.right.identity.public.clone(),
                        token: token(),
                    })
                }
                2 => {
                    h.right
                        .remote
                        .as_mut()
                        .context("Missing remote state")?
                        .game = true
                }
                3 => h.right.last_verified = Instant::now() - Duration::from_secs(13),
                4 => h.right.settings.iso_enabled = false,
                _ => h.right.install_busy = true,
            }
            assert!(
                h.right
                    .input(Input::IsoVerified {
                        channel,
                        pending,
                        result: Ok(verified)
                    })
                    .is_err()
            );
            assert!(!staged.exists() && !destination.exists());
            assert!(h.right.iso_verifying.is_none() && h.right.link.is_none());
        }
        Ok(())
    }

    #[test]
    fn peer_reported_error_cancels_rvz_verification_without_dropping_the_pairing() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
        h.pair()?;
        h.right.command(Command::SetIsoEnabled(true))?;
        h.right.event(Event::RvzOffer {
            id: token(),
            hash: "a".repeat(64),
            size: 512,
        })?;
        h.right.event(Event::Error {
            message: "Offer withdrawn".into(),
        })?;
        assert!(h.right.iso_offer.is_none() && h.right.ready());
        h.right.event(Event::RvzOffer {
            id: token(),
            hash: "a".repeat(64),
            size: 512,
        })?;
        let interrupted = h.right.root.join("interrupted.rvz");
        h.right.accept_iso(Some(interrupted.clone()))?;
        let partial = h
            .right
            .incoming_iso
            .as_ref()
            .context("Missing RVZ staging")?
            .temporary
            .path()
            .to_path_buf();
        h.right.event(Event::Error {
            message: "Sender stopped".into(),
        })?;
        assert!(h.right.incoming_iso.is_none() && !partial.exists() && !interrupted.exists());
        assert!(h.right.no_transfer() && h.right.ready());
        let (channel, pending, verified) = staged_rvz(&mut h.right, "cancelled-by-partner.rvz")?;
        let destination = pending.destination.clone();
        let staged = pending.temporary.to_path_buf();
        let cancelled = h.right.cancel_transfer.clone();
        h.right.event(Event::Error {
            message: "Disc transfer was cancelled".into(),
        })?;
        assert!(cancelled.load(Ordering::Acquire));
        assert!(!h.right.cancel_transfer.load(Ordering::Acquire));
        assert!(h.right.iso_verifying.is_none() && h.right.no_transfer() && h.right.ready());
        h.right.input(Input::IsoVerified {
            channel,
            pending,
            result: Ok(verified),
        })?;
        assert!(!staged.exists() && !destination.exists());
        assert!(h.right.ready());
        Ok(())
    }

    #[test]
    fn stale_rvz_results_do_not_commit_or_clear_a_newer_job() -> Result<()> {
        for disconnected in [false, true] {
            let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
            h.pair()?;
            h.right.command(Command::SetIsoEnabled(true))?;
            let (channel, pending, verified) = staged_rvz(&mut h.right, "stale.rvz")?;
            let destination = pending.destination.clone();
            let staged = pending.temporary.to_path_buf();
            let newer_id = token();
            if disconnected {
                h.right.disconnect("Cancelling verification");
                assert!(h.right.cancel_transfer.load(Ordering::Acquire));
            } else {
                h.right
                    .iso_verifying
                    .as_mut()
                    .context("Missing verification")?
                    .id = newer_id.clone();
                h.right.transfer = "Newer verification".into();
            }
            h.right.input(Input::IsoVerified {
                channel,
                pending,
                result: Ok(verified),
            })?;
            assert!(!staged.exists() && !destination.exists());
            if disconnected {
                assert!(h.right.iso_verifying.is_none());
            } else {
                assert_eq!(
                    h.right.iso_verifying.as_ref().map(|job| &job.id),
                    Some(&newer_id)
                );
                assert_eq!(h.right.transfer, "Newer verification");
            }
        }
        Ok(())
    }

    #[test]
    fn pinned_identity_is_not_silently_replaced() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), None)?;
        h.pair()?;
        h.left.disconnect("Testing a changed identity");
        h.right.disconnect("Testing a changed identity");
        h.left.store(|s| {
            if let Some(peer) = &mut s.peer {
                peer.public_key = "f".repeat(64);
            }
        })?;
        assert!(h.attach().is_err());
        assert_eq!(
            h.left
                .settings
                .peer
                .as_ref()
                .context("Missing pin")?
                .public_key,
            "f".repeat(64)
        );
        assert!(h.left.link.is_none());
        Ok(())
    }

    #[test]
    fn final_save_is_committed_before_both_play_locks_are_released() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), Some(variant(0)))?;
        h.pair()?;
        let old_hash = files::sha256(&variant(0));
        let held = PlayLock {
            owner: h.left.identity.public.clone(),
            token: token(),
        };
        h.left.store(|s| {
            s.play_lock = Some(held.clone());
            s.sync_base = Some(old_hash.clone());
        })?;
        h.right.store(|s| {
            s.play_lock = Some(held.clone());
            s.sync_base = Some(old_hash);
        })?;
        h.left.managed_token = Some(held.token);
        h.left.send_state()?;
        h.right.send_state()?;
        h.pump_until(|h| {
            h.left.remote.as_ref().is_some_and(|r| r.lock.is_some())
                && h.right.remote.as_ref().is_some_and(|r| r.lock.is_some())
        })?;
        let path = h.left.settings.save_file.clone().context("Missing path")?;
        files::atomic_write(&path, &variant(1))?;
        h.left.refresh_save()?;
        h.left.stable_since = Instant::now() - Duration::from_secs(2);
        h.left.refresh_save()?;
        h.left.sync_tick()?;
        assert!(h.left.settings.play_lock.is_some() && h.right.settings.play_lock.is_some());
        h.pump_until(|h| h.left.sent_save.is_none() && h.right.local_hash == h.left.local_hash)?;
        assert!(h.left.settings.play_lock.is_some() && h.right.settings.play_lock.is_some());
        h.left.sync_tick()?;
        h.pump_until(|h| {
            h.left.settings.play_lock.is_none() && h.right.settings.play_lock.is_none()
        })?;
        assert_eq!(
            files::read_save(
                h.right
                    .settings
                    .save_file
                    .as_ref()
                    .context("Missing path")?
            )?,
            variant(1)
        );
        Ok(())
    }

    #[test]
    fn native_saves_sync_between_raw_and_gci_storage_in_both_directions() -> Result<()> {
        for (left_raw, right_raw) in [(true, false), (false, true), (true, true)] {
            let first = native_variant(1);
            let old = native_variant(0);
            let mut h = Harness::new(Some(first.clone()), Some(old.clone()))?;
            if left_raw {
                use_raw_card(&mut h.left, Some(&first))?;
            }
            let raw_path = if right_raw {
                Some(use_raw_card(&mut h.right, Some(&old))?)
            } else {
                None
            };
            h.pair()?;
            h.left.command(Command::SendSave)?;
            h.pump_until(|h| h.right.pending_save.is_some())?;
            h.right.command(Command::AcceptSave(true))?;
            let expected = files::sha256(&first);
            h.pump_until(|h| {
                h.left.sent_save.is_none() && h.right.local_hash.as_ref() == Some(&expected)
            })?;
            assert_eq!(
                files::read_save(
                    h.right
                        .settings
                        .save_file
                        .as_ref()
                        .context("Missing save")?
                )?,
                first
            );
            let previous = h
                .right
                .history
                .as_ref()
                .context("Missing history")?
                .page(None)?
                .entries
                .into_iter()
                .find(|entry| entry.hash == files::sha256(&old))
                .context("Previous save was not retained")?;
            h.right.command(Command::RestoreSnapshot {
                id: previous.id,
                expected: Some(expected),
            })?;
            assert_eq!(
                files::read_save(
                    h.right
                        .settings
                        .save_file
                        .as_ref()
                        .context("Missing save")?
                )?,
                old
            );
            if let Some(path) = raw_path {
                assert_eq!(
                    fs::metadata(path)?.len(),
                    crate::raw_card::tests::empty().len() as u64
                );
            }
        }
        Ok(())
    }

    #[test]
    fn an_empty_formatted_raw_card_receives_and_snapshots_only_the_game_save() -> Result<()> {
        let bytes = native_variant(1);
        let mut h = Harness::new(Some(bytes.clone()), None)?;
        let path = use_raw_card(&mut h.right, None)?;
        let original = fs::read(&path)?;
        assert!(h.right.save_valid && h.right.local_hash.is_none());
        h.right.snapshot_tick()?;
        h.pair()?;
        h.left.command(Command::SendSave)?;
        let expected = files::sha256(&bytes);
        h.pump_until(|h| {
            h.left.sent_save.is_none() && h.right.local_hash.as_ref() == Some(&expected)
        })?;
        assert_eq!(files::read_save(&path)?, bytes);
        assert_eq!(
            &fs::read(&path)?[..crate::raw_card::BLOCK],
            &original[..crate::raw_card::BLOCK]
        );
        let stats = h
            .right
            .history
            .as_ref()
            .context("Missing history")?
            .stats()?;
        assert_eq!(stats.logical_bytes, files::SAVE_SIZE as u64);
        assert!(stats.stored_bytes < 64 * 1024);
        Ok(())
    }

    #[test]
    fn storage_format_changes_cancel_a_pending_save_without_converting_or_touching_files()
    -> Result<()> {
        let mut h = Harness::new(Some(native_variant(1)), Some(native_variant(0)))?;
        h.pair()?;
        h.left.command(Command::SendSave)?;
        h.pump_until(|h| h.right.pending_save.is_some())?;
        let original = h
            .right
            .settings
            .save_file
            .clone()
            .context("Missing GCI path")?;
        files::atomic_write(
            &h.right.default_profile.join("config.json"),
            br#"{"backend.cardFileType":0}"#,
        )?;
        let error = h
            .right
            .command(Command::AcceptSave(true))
            .err()
            .context("Changed storage accepted the save")?;
        assert!(format!("{error:#}").contains("memory-card format changed"));
        assert_eq!(files::read_save(&original)?, native_variant(0));
        assert!(!h.right.default_profile.join("MemoryCardA.USA.raw").exists());
        Ok(())
    }

    #[test]
    fn local_texture_install_detects_and_prepares_the_profile_without_a_config_picker() -> Result<()>
    {
        let mut h = Harness::new(Some(variant(0)), None)?;
        let executable = h.left.root.join("dusklight.exe");
        fs::write(&executable, b"not executed")?;
        h.left.command(Command::SetExecutable(executable))?;
        let wrong = h.left.root.join("not-the-game-profile");
        h.left.settings.texture_profile = Some(wrong.clone());
        let source = textures::tests::source(&h.left.root)?;
        h.left.command(Command::ImportTextures {
            source: "local-hd".into(),
            version: "test".into(),
            path: source,
            preserve_unmanaged: false,
        })?;
        h.pump_until(|h| !h.left.texture_busy)?;
        assert_eq!(
            h.left.settings.texture_profile.as_ref(),
            Some(&h.left.default_profile)
        );
        assert!(h.left.texture_state.active.is_some());
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(h.left.default_profile.join("config.json"))?)?;
        assert_eq!(
            config.get("game.enableTextureReplacements"),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(!wrong.exists());
        Ok(())
    }

    #[test]
    fn old_custom_selection_is_ignored_without_opening_or_modifying_its_file() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), None)?;
        let default = h
            .left
            .settings
            .save_file
            .clone()
            .context("Missing automatic path")?;
        let old = h.left.root.join("my-directory-experiment.gci");
        fs::write(&old, b"not a GCI; never inspect, rename, or overwrite this")?;
        let network = h.left.settings.network.clone();
        h.left.store(|settings| {
            settings.save_file = Some(old.clone());
            settings.sync_base = Some("a".repeat(64));
        })?;
        h.left.save_profile = None;
        h.left.refresh_save()?;
        assert_eq!(h.left.settings.save_file.as_ref(), Some(&default));
        assert_eq!(h.left.local_hash, Some(files::sha256(&variant(0))));
        assert!(h.left.settings.sync_base.is_none());
        assert_eq!(h.left.settings.network, network);
        assert_eq!(
            fs::read(&old)?,
            b"not a GCI; never inspect, rename, or overwrite this"
        );
        assert_eq!(fs::read(default)?, variant(0));
        assert_eq!(
            Settings::load(&h.left.root)?.save_file,
            h.left.settings.save_file
        );
        Ok(())
    }

    #[test]
    fn active_profile_changes_are_detected_without_moving_saves_or_resetting_game_settings()
    -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), None)?;
        let old = h
            .left
            .settings
            .save_file
            .clone()
            .context("Missing automatic path")?;
        let custom = h.left.root.join("existing-dusklight-profile");
        let path = save_location::path_for_identity(&custom, &files::validate_save(&variant(1))?)?;
        save_location::prepare(&path)?;
        files::atomic_write(&path, &variant(1))?;
        let config = b"{\"backend.cardFileType\":1,\"video.enableVsync\":false}";
        fs::write(custom.join("config.json"), config)?;
        files::atomic_write(
            &h.left.default_profile.join("data_location.json"),
            &serde_json::to_vec(
                &serde_json::json!({"version":1,"mode":"custom","customPath":custom}),
            )?,
        )?;
        h.left
            .store(|settings| settings.sync_base = Some(files::sha256(&variant(0))))?;
        h.left.refresh_save()?;
        assert_eq!(h.left.settings.save_file.as_ref(), Some(&path));
        assert_eq!(h.left.local_hash, Some(files::sha256(&variant(1))));
        assert!(h.left.settings.sync_base.is_none());
        assert_eq!(fs::read(old)?, variant(0));
        assert_eq!(fs::read(&path)?, variant(1));
        assert_eq!(fs::read(custom.join("config.json"))?, config);
        Ok(())
    }

    #[test]
    fn first_received_save_selects_its_default_region_without_a_picker_or_fake_file() -> Result<()>
    {
        let bytes = save_location::tests::stock_save("GZ2P");
        let mut h = Harness::new(Some(bytes.clone()), None)?;
        let empty = h
            .right
            .settings
            .save_file
            .clone()
            .context("Missing automatic path")?;
        assert!(!empty.exists());
        assert!(!empty.parent().context("Missing Card A")?.exists());
        assert!(h.right.save_valid);
        h.pair()?;
        h.left.command(Command::SendSave)?;
        let hash = files::sha256(&bytes);
        h.pump_until(|h| h.right.local_hash.as_ref() == Some(&hash) && h.left.sent_save.is_none())?;
        let expected = h
            .right
            .default_profile
            .join("EUR")
            .join("Card A")
            .join("01-GZ2P-gczelda2.gci");
        assert_eq!(h.right.settings.save_file.as_ref(), Some(&expected));
        assert_eq!(fs::read(expected)?, bytes);
        assert!(!empty.exists());
        Ok(())
    }

    #[test]
    fn running_game_keeps_its_profile_and_detects_its_first_regional_save() -> Result<()> {
        let mut h = Harness::new(None, None)?;
        let original = h
            .left
            .save_profile
            .clone()
            .context("Missing original profile")?;
        let lock = PlayLock {
            owner: h.left.identity.public.clone(),
            token: token(),
        };
        h.left
            .store(|settings| settings.play_lock = Some(lock.clone()))?;
        h.left.managed_token = Some(lock.token);
        h.left.game_running = true;
        let custom = h.left.root.join("next-profile");
        fs::create_dir_all(&h.left.default_profile)?;
        files::atomic_write(
            &h.left.default_profile.join("data_location.json"),
            &serde_json::to_vec(
                &serde_json::json!({"version":1,"mode":"custom","customPath":custom,"previousPath":original}),
            )?,
        )?;
        let bytes = save_location::tests::stock_save("GZ2J");
        let path = save_location::path_for_identity(&original, &files::validate_save(&bytes)?)?;
        save_location::prepare(&path)?;
        files::atomic_write(&path, &bytes)?;
        h.left.refresh_save()?;
        assert_eq!(h.left.save_profile.as_ref(), Some(&original));
        assert_eq!(h.left.settings.save_file.as_ref(), Some(&path));
        h.left.game_running = false;
        h.left.refresh_save()?;
        assert_eq!(h.left.local_hash, Some(files::sha256(&bytes)));
        assert!(h.left.settings.play_lock.is_some());
        h.left.store(|settings| settings.play_lock = None)?;
        h.left.managed_token = None;
        let error = h
            .left
            .refresh_save()
            .err()
            .context("Pending game migration was ignored")?;
        assert!(format!("{error:#}").contains("migration"));
        assert!(h.left.settings.save_file.is_none() && !h.left.save_valid);
        assert_eq!(fs::read(path)?, bytes);
        assert!(!custom.exists());
        Ok(())
    }

    #[test]
    fn snapshot_restore_to_an_empty_pc_uses_the_snapshots_default_region() -> Result<()> {
        let mut h = Harness::new(None, None)?;
        let bytes = save_location::tests::stock_save("RZDP");
        h.right.ensure_history()?;
        let source = h.right.root.join("old-snapshot-source.gci");
        let capture = h.right.history.as_mut().context("No history")?.capture(
            &bytes,
            &source,
            history::Reason::Observed,
            None,
        )?;
        h.right.command(Command::RestoreSnapshot {
            id: capture.id,
            expected: None,
        })?;
        let path = h
            .right
            .default_profile
            .join("EUR")
            .join("Card A")
            .join("01-RZDP-gczelda2.gci");
        assert_eq!(h.right.settings.save_file.as_ref(), Some(&path));
        assert_eq!(fs::read(path)?, bytes);
        assert!(!source.exists());
        assert!(
            h.right
                .history_page
                .entries
                .iter()
                .any(|entry| entry.reason == history::Reason::Restored)
        );
        Ok(())
    }

    #[test]
    fn ambiguous_defaults_pause_sync_instead_of_reusing_an_old_manual_path() -> Result<()> {
        let mut h = Harness::new(Some(variant(0)), None)?;
        let bytes = save_location::tests::stock_save("GZ2P");
        let other = save_location::path_for_identity(
            &h.left.default_profile,
            &files::validate_save(&bytes)?,
        )?;
        save_location::prepare(&other)?;
        files::atomic_write(&other, &bytes)?;
        let old = h.left.root.join("manual-path.gci");
        fs::write(&old, b"untouched")?;
        h.left.settings.save_file = Some(old.clone());
        h.left.save_profile = None;
        let error = h
            .left
            .refresh_save()
            .err()
            .context("Ambiguous saves were accepted")?;
        assert!(format!("{error:#}").contains("Multiple"));
        h.left.report(error);
        assert!(
            h.left.error.is_some() && !h.left.save_valid && h.left.settings.save_file.is_none()
        );
        assert_eq!(fs::read(old)?, b"untouched");
        assert_eq!(fs::read(other)?, bytes);
        Ok(())
    }

    #[test]
    fn desktop_recovery_clears_only_the_reported_desktop_error() {
        let detail = format!(
            "Background desktop error: {}",
            std::io::Error::from_raw_os_error(1460)
        );
        let mut error = Some(detail.clone());
        clear_desktop_error(&mut error, &detail);
        assert!(error.is_none());

        error = Some("Unrelated network problem".into());
        clear_desktop_error(&mut error, &detail);
        assert_eq!(error.as_deref(), Some("Unrelated network problem"));

        error = Some(format!("{detail}; AUDIT FAILURE: storage unavailable"));
        clear_desktop_error(&mut error, &detail);
        assert!(error.is_some());

        error = None;
        clear_desktop_error(&mut error, &detail);
        assert!(error.is_none());
    }

    #[test]
    fn save_detection_recovers_automatically_without_dismissing_unrelated_errors() -> Result<()> {
        for unrelated in [false, true] {
            let mut h = Harness::new(Some(variant(0)), None)?;
            let bytes = save_location::tests::stock_save("GZ2P");
            let other = save_location::path_for_identity(
                &h.left.default_profile,
                &files::validate_save(&bytes)?,
            )?;
            save_location::prepare(&other)?;
            files::atomic_write(&other, &bytes)?;
            let error = h
                .left
                .refresh_save()
                .err()
                .context("Expected an ambiguous location")?;
            h.left.report(error);
            assert!(h.left.error.is_some());
            if unrelated {
                h.left.error = Some("Unrelated network problem".into());
            }
            fs::remove_file(other)?;
            h.left.refresh_save()?;
            assert!(h.left.save_valid && h.left.settings.save_file.is_some());
            assert_eq!(
                h.left.error.as_deref(),
                if unrelated {
                    Some("Unrelated network problem")
                } else {
                    None
                }
            );
        }
        Ok(())
    }

    #[test]
    fn receiving_checks_profile_changes_before_the_next_poll_and_never_writes_the_old_profile()
    -> Result<()> {
        for conflict in [false, true] {
            let mut h = Harness::new(Some(variant(1)), Some(variant(0)))?;
            h.pair()?;
            let old = h
                .right
                .settings
                .save_file
                .clone()
                .context("Missing default save")?;
            let old_hash = files::sha256(&variant(0));
            if conflict {
                h.left.command(Command::SendSave)?;
                h.pump_until(|h| h.right.pending_save.is_some())?;
            }
            let custom = h.right.root.join("new-active-profile");
            let new =
                save_location::path_for_identity(&custom, &files::validate_save(&variant(2))?)?;
            save_location::prepare(&new)?;
            files::atomic_write(&new, &variant(2))?;
            files::atomic_write(
                &h.right.default_profile.join("data_location.json"),
                &serde_json::to_vec(
                    &serde_json::json!({"version":1,"mode":"custom","customPath":custom}),
                )?,
            )?;
            let result = if conflict {
                h.right.command(Command::AcceptSave(true))
            } else {
                h.right.event(Event::Save {
                    hash: files::sha256(&variant(1)),
                    base: Some(old_hash.clone()),
                    expected: Some(old_hash),
                    manual: false,
                    lease: None,
                    bytes: variant(1),
                })
            };
            let error = result
                .err()
                .context("Changed profile accepted a stale offer")?;
            assert!(
                format!("{error:#}").contains("active profile changed"),
                "{error:#}"
            );
            assert_eq!(fs::read(old)?, variant(0));
            assert_eq!(fs::read(new)?, variant(2));
        }
        Ok(())
    }

    #[test]
    fn changed_profile_during_play_approval_is_rejected_before_launching() -> Result<()> {
        let mut h = Harness::new(None, None)?;
        h.pair()?;
        let executable = h.left.root.join("dusklight.exe");
        fs::write(&executable, b"fixture that must never execute")?;
        h.left.command(Command::SetExecutable(executable))?;
        h.left.request_launch()?;
        let token = h
            .left
            .launch_pending
            .clone()
            .context("Missing play request")?;
        let custom = h.left.root.join("changed-profile");
        fs::create_dir_all(&h.left.default_profile)?;
        files::atomic_write(
            &h.left.default_profile.join("data_location.json"),
            &serde_json::to_vec(
                &serde_json::json!({"version":1,"mode":"custom","customPath":custom}),
            )?,
        )?;
        let error = h
            .left
            .event(Event::LeaseGranted { token })
            .err()
            .context("Changed profile was launched")?;
        assert!(
            format!("{error:#}").contains("profile changed"),
            "{error:#}"
        );
        assert!(h.left.child.is_none() && h.left.managed_token.is_none());
        assert!(h.left.settings.play_lock.is_some());
        Ok(())
    }

    #[test]
    fn first_play_can_reserve_two_explicit_empty_save_destinations() -> Result<()> {
        let mut h = Harness::new(None, None)?;
        h.pair()?;
        assert!(h.left.save_valid && h.right.save_valid);
        assert!(h.left.local_hash.is_none() && h.right.local_hash.is_none());
        let token = token();
        h.right.event(Event::LeaseRequest {
            token: token.clone(),
            hash: None,
        })?;
        assert!(play::matches(
            h.right.settings.play_lock.as_ref(),
            &h.left.identity.public,
            &token
        ));
        assert!(Settings::load(&h.right.root)?.play_lock.is_some());
        Ok(())
    }
}

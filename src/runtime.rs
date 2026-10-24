use crate::{
    audit::Audit,
    config::{self, Identity, Peer, PlayLock, Settings},
    files::{self, CHUNK_SIZE, ISO_SIZE, ensure_safe_path, valid_hash},
    history::{self, History},
    install::{self, Installed, ReleaseInfo},
    lan::{self, Advertisement, Discovered, Network},
    play::{self, ProcessMonitor},
    protocol::{self, Event, Handshake, SecureWriter},
    self_update,
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
    path::PathBuf,
    process::Child,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;
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
    pub remote_hash: Option<String>,
    pub game_running: bool,
    pub remote_game: bool,
    pub pending_save: bool,
    pub pending_iso: bool,
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

pub enum Command {
    DesktopError(String),
    TrustNetwork,
    OpenPairing,
    Connect(String),
    ApprovePairing,
    RejectPairing,
    SetSave(PathBuf),
    SetExecutable(PathBuf),
    SetIso(PathBuf),
    SetIsoEnabled(bool),
    SetStartup(bool),
    SendSave,
    AcceptSave(bool),
    Launch,
    Recover,
    AcceptRecovery(bool),
    SendIso,
    AcceptIso(Option<PathBuf>),
    Install,
    CheckRelease,
    CheckSelfUpdate,
    SetAutoUpdateChecks(bool),
    UpdateSelf,
    SetTextureProfile(PathBuf),
    OpenTextureCreator(String),
    ImportTextures {
        source: String,
        version: String,
        path: PathBuf,
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
    TextureDone(Result<TextureOutcome>),
    IsoPrepared {
        id: u64,
        path: PathBuf,
        result: Result<(String, u64)>,
    },
    IsoSent {
        id: u64,
        result: Result<()>,
    },
}

enum TextureOutcome {
    Imported(Pack),
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

struct OutgoingIso {
    id: String,
    path: PathBuf,
    hash: String,
    size: u64,
    started: bool,
    sent: bool,
    deadline: Instant,
}

struct IsoOffer {
    id: String,
    hash: String,
    size: u64,
    deadline: Instant,
}

struct IncomingIso {
    id: String,
    hash: String,
    size: u64,
    received: u64,
    sha: Sha256,
    official: Xxh3,
    header: Vec<u8>,
    temporary: NamedTempFile,
    destination: PathBuf,
    last_chunk: Instant,
}

struct Engine {
    root: PathBuf,
    settings: Settings,
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
    cancel_transfer: Arc<AtomicBool>,
    install_busy: bool,
    latest_release: Option<ReleaseInfo>,
    selected_release: Option<String>,
    texture_busy: bool,
    texture_state: textures::State,
    textures_loaded: bool,
    history: Option<History>,
    history_page: history::Page,
    history_candidate: Option<(String, Instant)>,
    history_last_hash: Option<String>,
    history_cursor: Option<String>,
    migrated_backups: bool,
    self_updates: self_update::State,
    last_self_update_check: Instant,
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

pub fn start() -> Result<Backend> {
    let root = config::root_directory()?;
    let instance = config::acquire_instance(&root)?;
    let settings = Settings::load(&root)?;
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
    let view = Arc::new(Mutex::new(initial_snapshot(&settings, &identity.public)));
    let mut engine = Engine::new(
        root,
        settings,
        identity,
        audit,
        sender.clone(),
        view.clone(),
    );
    thread::Builder::new()
        .name("twilight-manager".into())
        .spawn(move || {
            let _instance = instance;
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
        snapshot.settings.save_file = Some(PathBuf::from(
            r"C:\Games\Dusklight\Card A\01-GZ2E-zeldaTp.dat.gci",
        ));
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
                game: "GZ2E".into(),
                name: "Adventure".into(),
                hash: if sequence == 12 {
                    "a".repeat(64)
                } else {
                    format!("{sequence:064x}")
                },
                timestamp: format!("2026-10-20T14:{sequence:02}:00Z"),
                username: "Player".into(),
                reason: history::Reason::Observed,
                restored_from: None,
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
        remote_hash: None,
        game_running: false,
        remote_game: false,
        pending_save: false,
        pending_iso: false,
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
        identity: Arc<Identity>,
        audit: Audit,
        sender: Sender<Input>,
        view: Arc<Mutex<Snapshot>>,
    ) -> Self {
        let past = Instant::now() - Duration::from_secs(60);
        Self {
            root,
            settings,
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
            cancel_transfer: Arc::new(AtomicBool::new(false)),
            install_busy: false,
            latest_release: None,
            selected_release: None,
            texture_busy: false,
            texture_state: textures::State::default(),
            textures_loaded: false,
            history: None,
            history_page: history::Page::default(),
            history_candidate: None,
            history_last_hash: None,
            history_cursor: None,
            migrated_backups: false,
            self_updates: self_update::State::default(),
            last_self_update_check: Instant::now() - self_update::CHECK_INTERVAL
                + Duration::from_secs(15),
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
                    self.disconnect(&reason);
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
                    TextureOutcome::Imported(pack) => {
                        self.audit.record(
                            "texture-pack-imported",
                            None,
                            Some(&pack.id),
                            None,
                            &format!(
                                "{} {}; {} images; {} unrelated files skipped",
                                pack.source, pack.version, pack.files, pack.skipped
                            ),
                        )?;
                        self.texture_state.notice = format!(
                            "Installed {} textures; {} bundled/non-texture files were not installed. Select Activate to switch.",
                            pack.files, pack.skipped
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
                    "Peer changed during ISO verification"
                );
                let (hash, size) = result?;
                self.require_idle()?;
                let transfer_id = token();
                self.send(&Event::IsoOffer {
                    id: transfer_id.clone(),
                    hash: hash.clone(),
                    size,
                })?;
                self.outgoing_iso = Some(OutgoingIso {
                    id: transfer_id,
                    path,
                    hash,
                    size,
                    started: false,
                    sent: false,
                    deadline: Instant::now() + Duration::from_secs(120),
                });
                self.transfer = "Verified ISO; waiting for approval on the other device".into();
                Ok(())
            }
            Input::IsoSent { id, result } => {
                if self.link.as_ref().is_some_and(|l| l.id == id) {
                    if let Err(error) = result {
                        self.disconnect(
                            "ISO sender failed; receiver must discard its staged image",
                        );
                        return Err(error);
                    }
                    if let Some(outgoing) = &mut self.outgoing_iso {
                        outgoing.sent = true;
                        outgoing.deadline = Instant::now() + Duration::from_secs(60);
                        self.transfer =
                            "ISO sent; waiting for the receiver's verified commit".into();
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
    }

    fn command(&mut self, command: Command) -> Result<()> {
        self.error = None;
        match command {
            Command::DesktopError(detail) => bail!("{detail}"),
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
            Command::SetSave(path) => {
                self.require_idle()?;
                ensure!(
                    self.no_transfer(),
                    "Finish or reject the pending transfer first"
                );
                ensure_safe_path(&path)?;
                ensure!(
                    path.extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("gci")),
                    "Select a .gci save, not a raw card or archive"
                );
                ensure!(
                    path.parent().is_some_and(|p| p.is_dir()),
                    "Save directory does not exist"
                );
                if path.try_exists()? {
                    files::read_save(&path)?;
                }
                self.audit.record(
                    "save-selected",
                    Some(&path),
                    None,
                    None,
                    "Destination chosen locally; peer paths are never used",
                )?;
                self.store(|s| {
                    s.save_file = Some(path);
                    s.sync_base = None;
                })?;
                self.stable = 0;
                self.history_candidate = None;
                self.history_last_hash = None;
                self.history_cursor = None;
                self.refresh_save()?;
                self.refresh_history()?;
                self.send_state()?;
            }
            Command::SetExecutable(path) => {
                self.require_idle()?;
                ensure_safe_path(&path)?;
                ensure!(
                    path.is_file()
                        && path
                            .file_name()
                            .is_some_and(|name| name.eq_ignore_ascii_case("dusklight.exe")),
                    "Select dusklight.exe"
                );
                self.store(|s| s.game_exe = Some(path))?;
                self.selected_release =
                    install::selected_release(&self.root, self.settings.game_exe.as_deref())?;
            }
            Command::SetIso(path) => {
                self.require_idle()?;
                ensure!(self.no_transfer(), "Another transfer is active");
                ensure_safe_path(&path)?;
                ensure!(
                    path.is_file()
                        && path
                            .extension()
                            .is_some_and(|e| e.eq_ignore_ascii_case("iso")),
                    "Select your own uncompressed .iso dump"
                );
                self.store(|s| s.iso_file = Some(path))?;
                self.message =
                    "ISO selected. Full official-disc verification happens before any offer."
                        .into();
            }
            Command::SetIsoEnabled(enabled) => {
                self.require_idle()?;
                ensure!(
                    self.no_transfer(),
                    "Finish the current transfer before changing ISO permissions"
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
                    ensure!(
                        self.pending_save.take().is_some(),
                        "No incoming save to reject"
                    );
                    self.send(&Event::Error {
                        message: "The other user declined the save replacement".into(),
                    })?;
                    self.message =
                        "Incoming save declined. Your local save was not changed.".into();
                }
            }
            Command::Launch => self.request_launch()?,
            Command::Recover => {
                self.game_running = self.process.game_running();
                ensure!(
                    self.ready(),
                    "Both devices must be online for lock recovery"
                );
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
                    "Connect your partner and explicitly enable ISO transfer"
                );
                self.require_idle()?;
                ensure!(self.no_transfer(), "Another transfer is active");
                let path = self
                    .settings
                    .iso_file
                    .clone()
                    .context("Select your own ISO first")?;
                let id = self.link.as_ref().context("No peer")?.id;
                let sender = self.sender.clone();
                self.iso_preparing = true;
                self.transfer = "Verifying the entire ISO against the official disc hash...".into();
                thread::spawn(move || {
                    let result = files::verify_iso(&path);
                    if sender
                        .send(Input::IsoPrepared { id, path, result })
                        .is_err()
                    {
                        eprintln!("Twilight Sync stopped before ISO verification completed");
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
                    self.last_self_update_check = Instant::now() - self_update::CHECK_INTERVAL;
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
            Command::SetTextureProfile(path) => {
                self.game_running = self.process.game_running();
                self.require_idle()?;
                textures::validate_profile(&path)?;
                self.audit.record(
                    "texture-profile-selected",
                    Some(&path),
                    None,
                    None,
                    "User selected Dusklight's actual config.json directory",
                )?;
                self.store(|s| s.texture_profile = Some(path))?;
                self.reload_textures()?;
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
            } => {
                self.game_running = self.process.game_running();
                self.require_idle()?;
                textures::catalog(&source)?;
                self.audit.record(
                    "texture-import-requested",
                    Some(&path),
                    None,
                    None,
                    &format!("{source} {version}; user confirms permission to use these assets"),
                )?;
                self.texture_busy = true;
                self.texture_state.notice = "Validating and importing texture data only...".into();
                let root = self.root.clone();
                let sender = self.sender.clone();
                thread::spawn(move || {
                    let mut reported = 0;
                    let result =
                        textures::import(&root, &source, &version, &path, |files, bytes| {
                            if files >= reported + 25 {
                                reported = files;
                                if let Err(error) =
                                    sender.send(Input::TextureProgress { files, bytes })
                                {
                                    eprintln!("Cannot deliver texture import progress: {error}");
                                }
                            }
                        })
                        .map(TextureOutcome::Imported);
                    if sender.send(Input::TextureDone(result)).is_err() {
                        eprintln!("Twilight Sync stopped during texture import");
                    }
                });
            }
            Command::ActivateTextures {
                id,
                preserve_unmanaged,
            } => {
                self.game_running = self.process.game_running();
                self.require_idle()?;
                let profile = self
                    .settings
                    .texture_profile
                    .clone()
                    .context("Select Dusklight's config.json first")?;
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
                thread::spawn(move || {
                    let result = textures::activate(
                        &root,
                        &profile,
                        id.as_deref(),
                        preserve_unmanaged,
                        texture_game_stopped,
                    )
                    .map(TextureOutcome::Activated);
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
                let path = self
                    .settings
                    .save_file
                    .clone()
                    .context("Choose the local GCI destination first")?;
                self.ensure_history()?;
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
                    "Wait for installation, textures, or ISO verification to finish"
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
        self.pending_save = None;
        self.sent_save = None;
        self.pending_recovery = None;
        self.requested_recovery = None;
        self.iso_offer = None;
        self.incoming_iso = None;
        self.outgoing_iso = None;
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
                    "Unpaired devices cannot exchange saves, ISO data, or play locks"
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
                manual,
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
                files::validate_save(&bytes)?;
                self.ensure_save_receive_allowed(lease.as_deref())?;
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
                let automatic =
                    current.is_none() || current.as_ref() == Some(&hash) || current == base;
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
                    ensure!(
                        manual,
                        "Both saves changed; automatic sync refuses to choose a winner"
                    );
                    self.pending_save = Some(pending);
                    self.message = "Save conflict: review and approve replacement locally. A backup will be kept.".into();
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
                self.store(|s| s.sync_base = Some(hash.clone()))?;
                if let Some(remote) = &mut self.remote {
                    remote.hash = Some(hash);
                }
                self.sent_save = None;
                self.message = "Save synced, verified, and backed up.".into();
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
                let executable = self
                    .settings
                    .game_exe
                    .as_ref()
                    .context("Select or install Dusklight")?;
                self.managed_token = Some(token);
                let child = play::launch(executable)?;
                self.child = Some(child);
                self.game_running = true;
                self.audit.record(
                    "game-started",
                    Some(executable),
                    self.local_hash.as_deref(),
                    Some(&peer),
                    "Both devices durably reserved the same exclusive-play lock",
                )?;
                self.send_state()?;
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
                ensure!(
                    self.settings.iso_enabled,
                    "ISO transfer is disabled locally"
                );
                self.require_idle()?;
                ensure!(self.no_transfer(), "A transfer is already active");
                ensure!(
                    valid_hash(&id) && valid_hash(&hash) && size == ISO_SIZE,
                    "Invalid or unsupported ISO offer"
                );
                self.iso_offer = Some(IsoOffer {
                    id,
                    hash,
                    size,
                    deadline: Instant::now() + Duration::from_secs(120),
                });
                self.message =
                    "Your partner offers a verified disc image. Accept only if you own this game."
                        .into();
            }
            Event::IsoAccept { id } => self.stream_iso(&id)?,
            Event::IsoChunk { id, bytes } => {
                let incoming = self
                    .incoming_iso
                    .as_mut()
                    .context("No approved ISO transfer")?;
                ensure!(
                    incoming.id == id && !bytes.is_empty() && bytes.len() <= CHUNK_SIZE,
                    "Unexpected ISO chunk"
                );
                ensure!(
                    !self.game_running,
                    "An external game started during ISO transfer"
                );
                incoming.received += bytes.len() as u64;
                ensure!(
                    incoming.received <= incoming.size,
                    "ISO exceeds approved size"
                );
                if incoming.header.len() < 0x440 {
                    let take = (0x440 - incoming.header.len()).min(bytes.len());
                    incoming.header.extend_from_slice(&bytes[..take]);
                }
                incoming.temporary.write_all(&bytes)?;
                incoming.sha.update(&bytes);
                incoming.official.update(&bytes);
                incoming.last_chunk = Instant::now();
                self.transfer = format!(
                    "Receiving ISO: {:.1}%",
                    incoming.received as f64 * 100.0 / incoming.size as f64
                );
            }
            Event::IsoDone { id } => self.finish_iso(&id)?,
            Event::IsoReceived { id } => {
                let outgoing = self.outgoing_iso.as_ref().context("No outgoing ISO")?;
                ensure!(
                    outgoing.id == id && outgoing.started,
                    "Unexpected ISO receipt"
                );
                self.audit.record(
                    "iso-acknowledged",
                    Some(&outgoing.path),
                    Some(&outgoing.hash),
                    Some(&peer),
                    "Peer verified official disc hash and SHA-256 before committing",
                )?;
                self.outgoing_iso = None;
                self.transfer = "ISO synced and verified on the other device.".into();
            }
            _ => bail!("Unexpected twilightProto event in the paired state"),
        }
        Ok(())
    }

    fn current_hash(&self) -> Result<Option<String>> {
        files::save_hash(
            self.settings
                .save_file
                .as_ref()
                .context("Select the local GCI save destination")?,
        )
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

    fn receive_save(&mut self, pending: PendingSave, manual: bool) -> Result<()> {
        self.game_running = self.process.game_running();
        self.ensure_save_receive_allowed(pending.lease.as_deref())?;
        let path = self
            .settings
            .save_file
            .as_ref()
            .context("Select a save destination")?
            .clone();
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
        self.send(&Event::Saved { hash })?;
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
            "Select a valid save or receive destination on the other device"
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
            .context("Select a save file")?;
        let bytes = files::read_save(path)?;
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
        self.message = "Save sent; waiting for validated commit or conflict approval.".into();
        Ok(())
    }

    fn request_launch(&mut self) -> Result<()> {
        self.ensure_history()?;
        if !self.textures_loaded {
            self.reload_textures()?;
        }
        ensure!(
            !self.texture_state.recovery_needed,
            "Recover the interrupted texture switch before launching"
        );
        ensure!(
            self.ready(),
            "Strict play protection requires your paired device online"
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

    fn refresh_save(&mut self) -> Result<()> {
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
            self.last_self_update_check.elapsed() >= Duration::from_secs(10),
            "Update checks are rate limited; retry shortly"
        );
        self.audit.record(
            "app-update-check-requested",
            None,
            None,
            None,
            "Official GitHub stable-release metadata only; no automatic download or installation",
        )?;
        self.last_self_update_check = Instant::now();
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
        let bytes = match files::read_save(&path) {
            Ok(bytes) => bytes,
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
        self.history_last_hash = Some(hash);
        self.history_cursor = None;
        self.refresh_history()?;
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        if self.self_updates.enabled
            && !self.self_updates.checking
            && !self.self_updates.preparing
            && self.last_self_update_check.elapsed() >= self_update::CHECK_INTERVAL
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
            if !self.textures_loaded {
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
        if !self.ready() || self.game_running || !self.save_valid || self.stable < 2 {
            return Ok(());
        }
        let Some(remote) = &self.remote else {
            return Ok(());
        };
        if remote.game || !remote.save_ready {
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

    fn accept_iso(&mut self, destination: Option<PathBuf>) -> Result<()> {
        let offer = self.iso_offer.take().context("No incoming ISO offer")?;
        if let Some(destination) = destination {
            self.require_idle()?;
            ensure!(
                self.settings.iso_enabled && offer.deadline > Instant::now(),
                "ISO permission or offer expired"
            );
            ensure_safe_path(&destination)?;
            ensure!(
                !destination.try_exists()?
                    && destination
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("iso")),
                "Choose a new .iso destination; existing images are never overwritten"
            );
            let parent = destination
                .parent()
                .context("Destination has no directory")?;
            ensure!(parent.is_dir(), "ISO directory does not exist");
            ensure!(
                fs2::available_space(parent)? > offer.size + 64 * 1024 * 1024,
                "Insufficient disk space for a staged ISO"
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
                message: "ISO transfer declined by the other user".into(),
            })?;
        }
        Ok(())
    }

    fn stream_iso(&mut self, id: &str) -> Result<()> {
        self.require_idle()?;
        ensure!(self.settings.iso_enabled, "ISO permission was revoked");
        let outgoing = self
            .outgoing_iso
            .as_mut()
            .context("No outgoing ISO offer")?;
        ensure!(
            outgoing.id == id && !outgoing.started && outgoing.deadline > Instant::now(),
            "Unexpected ISO acceptance"
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
        self.transfer = "Streaming approved ISO in bounded 32 KiB chunks...".into();
        thread::spawn(move || {
            let result = (|| -> Result<()> {
                ensure_safe_path(&path)?;
                let mut file = File::open(&path)?;
                ensure!(
                    file.metadata()?.len() == size,
                    "ISO changed since verification"
                );
                let mut buffer = [0; CHUNK_SIZE];
                let mut sha = Sha256::new();
                let mut total = 0_u64;
                loop {
                    ensure!(
                        !cancel.load(Ordering::Acquire),
                        "ISO transfer was cancelled"
                    );
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    total += count as u64;
                    ensure!(total <= size, "ISO grew during transfer");
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
                    "ISO changed during transfer"
                );
                writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Writer lock poisoned"))?
                    .send(&Event::IsoDone { id: transfer_id })?;
                Ok(())
            })();
            if sender.send(Input::IsoSent { id, result }).is_err() {
                eprintln!("Twilight Sync stopped while the ISO sender was finishing");
            }
        });
        Ok(())
    }

    fn finish_iso(&mut self, id: &str) -> Result<()> {
        let incoming = self.incoming_iso.take().context("No incoming ISO")?;
        ensure!(
            incoming.id == id && incoming.received == incoming.size,
            "ISO is incomplete"
        );
        ensure!(
            !self.process.game_running(),
            "A game started while receiving the ISO"
        );
        ensure!(
            hex::encode(incoming.sha.finalize()) == incoming.hash,
            "Received ISO SHA-256 mismatch"
        );
        let expected = files::validate_iso_header(&incoming.header, incoming.size)?;
        ensure!(
            format!("{:032x}", incoming.official.digest128()) == expected,
            "Received ISO is not an unmodified official disc"
        );
        ensure_safe_path(&incoming.destination)?;
        incoming.temporary.as_file().sync_all()?;
        incoming
            .temporary
            .persist_noclobber(&incoming.destination)
            .map_err(|e| e.error)?;
        self.audit.record(
            "iso-committed",
            Some(&incoming.destination),
            Some(&incoming.hash),
            Some(&self.peer_key()?),
            "Official XXH3-128 and transfer SHA-256 both verified; never executed",
        )?;
        self.store(|s| s.iso_file = Some(incoming.destination))?;
        self.send(&Event::IsoReceived { id: id.into() })?;
        self.transfer = "ISO received, verified, and committed.".into();
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
            remote_hash: self.remote.as_ref().and_then(|r| r.hash.clone()),
            game_running: self.game_running,
            remote_game: self.remote.as_ref().is_some_and(|r| r.game),
            pending_save: self.pending_save.is_some(),
            pending_iso: self.iso_offer.is_some(),
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
    use crate::files::fixture_save;
    use std::fs;

    struct Harness {
        left: Engine,
        right: Engine,
        left_rx: Receiver<Input>,
        right_rx: Receiver<Input>,
        _directory: tempfile::TempDir,
    }

    fn variant(marker: u8) -> Vec<u8> {
        let mut bytes = fixture_save();
        bytes[0x100] = marker;
        bytes
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
                let path = root.join("selected-locally.gci");
                if let Some(bytes) = save {
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
                let mut engine = Engine::new(root, settings, identity, audit, sender, view);
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
    fn encrypted_sync_commits_only_to_the_local_selected_path() -> Result<()> {
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
        }
        assert_eq!(h.left.stable, 0);
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

use crate::{
    game_file::{Format as TransferFormat, Kind as GameFileKind},
    install,
    protocol::GameId,
    runtime::{Backend, Command, HistoryPreviewData, NativeCommand, Snapshot},
    source_build::{self, Method, Preference, SdkSetup},
    textures,
};
use anyhow::{Context, Result, bail, ensure};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::{WinitWindowAccessor, winit};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    future::poll_fn,
    path::{Path, PathBuf},
    rc::Rc,
    task::{Poll, Waker},
    time::{Duration, Instant},
};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

mod appearance;
mod background;
mod icons;
mod model;
#[cfg(test)]
mod tests;
#[allow(clippy::unwrap_used)]
mod widgets {
    slint::include_modules!();
}
use widgets::{Action, MainWindow, Toggle};

const WINDOW_TITLE: &str = "Twilight Sync";
const NOTICES: &str = include_str!(r"..\THIRD_PARTY_NOTICES.txt");

#[derive(Clone)]
struct Tray {
    icon: TrayIcon,
    status: MenuItem,
    startup: CheckMenuItem,
    automatic_updates: CheckMenuItem,
    check_updates: MenuItem,
    update_app: MenuItem,
    show: MenuId,
    sync: MenuId,
    launch: MenuId,
    launch_item: MenuItem,
    sync_item: MenuItem,
    quit: MenuId,
}

fn make_tray(snapshot: &Snapshot) -> Result<Tray> {
    let pixels = icons::tray()?;
    let menu = Menu::new();
    let status = MenuItem::new("Checking local state...", false, None);
    let show = MenuItem::new("Open Twilight Sync", true, None);
    let sync = MenuItem::new("Sync selected game's save", true, None);
    let launch = MenuItem::new("Play selected game", true, None);
    let startup = CheckMenuItem::new("Start with Windows", true, snapshot.settings.startup, None);
    let automatic_updates = CheckMenuItem::new(
        "Automatically check app updates",
        true,
        snapshot.self_updates.enabled,
        None,
    );
    let check_updates = MenuItem::new("Check Twilight Sync updates", true, None);
    let update_app = MenuItem::new("Update Twilight Sync (check first)", false, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append_items(&[
        &status,
        &PredefinedMenuItem::separator(),
        &show,
        &sync,
        &launch,
        &PredefinedMenuItem::separator(),
        &check_updates,
        &update_app,
        &automatic_updates,
        &PredefinedMenuItem::separator(),
        &startup,
        &quit,
    ])?;
    let icon = TrayIconBuilder::new()
        .with_tooltip("Twilight Sync")
        .with_icon(Icon::from_rgba(pixels.rgba, pixels.width, pixels.height)?)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?;
    Ok(Tray {
        icon,
        status,
        startup,
        automatic_updates,
        check_updates,
        update_app,
        show: show.id().clone(),
        sync: sync.id().clone(),
        launch: launch.id().clone(),
        launch_item: launch,
        sync_item: sync,
        quit: quit.id().clone(),
    })
}

fn rows<T: Clone + 'static>(values: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(values)).into()
}

fn replace_rows<T: Clone + PartialEq + 'static>(
    current: ModelRc<T>,
    values: Vec<T>,
    set: impl FnOnce(ModelRc<T>),
) {
    if let Some(model) = current.as_any().downcast_ref::<VecModel<T>>()
        && model.row_count() == values.len()
    {
        for (index, value) in values.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&value) {
                model.set_row_data(index, value);
            }
        }
        return;
    }
    if current.row_count() != values.len()
        || values
            .iter()
            .enumerate()
            .any(|(index, value)| current.row_data(index).as_ref() != Some(value))
    {
        set(rows(values));
    }
}

fn open_path(path: &Path) -> Result<()> {
    crate::files::ensure_safe_path(path)?;
    ensure!(
        path.try_exists()?,
        "The selected file or folder is not available"
    );
    let system =
        PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot is not available")?);
    std::process::Command::new(if path.is_dir() {
        system.join("explorer.exe")
    } else {
        system.join("System32").join("notepad.exe")
    })
    .arg(path)
    .spawn()
    .context("Opening the local file or folder")?;
    Ok(())
}

fn open_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)?;
    ensure!(
        parsed.scheme() == "https" && parsed.username().is_empty() && parsed.password().is_none(),
        "Only HTTPS help links are supported"
    );
    let operation: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    let target: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    ensure!(result as isize > 32, "Windows could not open the help link");
    Ok(())
}

fn copy_commands(setup: SdkSetup) -> Result<()> {
    arboard::Clipboard::new()
        .context("Opening the clipboard")?
        .set_text(setup.script())
        .context("Copying SDK setup commands")
}

struct Approval {
    game: GameId,
    commit: String,
    preference: Preference,
}

struct Controller {
    ui: slint::Weak<MainWindow>,
    backend: Backend,
    snapshot: Snapshot,
    tray: Tray,
    background: Option<background::BackgroundGuard>,
    appearance: appearance::Store,
    native_appearance: Option<appearance::Appearance>,
    game: GameId,
    game_path: Option<PathBuf>,
    approval: Option<Approval>,
    texture_consent: bool,
    local_error: Option<String>,
    demo: bool,
    started: Instant,
    start_hidden: bool,
    preview_directory: Option<tempfile::TempDir>,
}

enum Interaction {
    Action(Action, i32),
    Toggle(Toggle, bool),
    SelectGame(i32),
    ChooseInput(i32),
    ClearInput(i32),
    Appearance(widgets::AppearanceMode),
}

impl Interaction {
    async fn apply(self, controller: &mut Controller) -> Result<()> {
        match self {
            Self::Action(action, index) => controller.handle(action, index).await,
            Self::Toggle(toggle, value) => controller.toggle(toggle, value).await,
            Self::SelectGame(index) => controller.select_game(index),
            Self::ChooseInput(kind) => controller.choose_input(kind),
            Self::ClearInput(kind) => controller.clear_input(kind),
            Self::Appearance(mode) => {
                controller.appearance.set(match mode {
                    widgets::AppearanceMode::System => appearance::Appearance::System,
                    widgets::AppearanceMode::Light => appearance::Appearance::Light,
                    widgets::AppearanceMode::Dark => appearance::Appearance::Dark,
                })?;
                controller.refresh_view()
            }
        }
    }
}

struct Dispatcher {
    controller: RefCell<Option<Controller>>,
    ui: slint::Weak<MainWindow>,
}

impl Dispatcher {
    fn report(&self, error: anyhow::Error) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_error(format!("{error:#}").into());
        } else {
            eprintln!("Twilight Sync interface: {error:#}");
        }
    }

    fn run(self: &Rc<Self>, interaction: Interaction) {
        let dispatcher = self.clone();
        if let Err(error) = slint::spawn_local(async move {
            let controller = dispatcher.controller.borrow_mut().take();
            let Some(mut controller) = controller else {
                dispatcher.report(anyhow::anyhow!(
                    "Close the active dialog before starting another action"
                ));
                return;
            };
            if let Err(error) = interaction.apply(&mut controller).await {
                controller.report(error);
            }
            *dispatcher.controller.borrow_mut() = Some(controller);
        }) {
            self.report(error.into());
        }
    }

    fn tick(self: &Rc<Self>) {
        let interaction = {
            let mut slot = self.controller.borrow_mut();
            let Some(controller) = slot.as_mut() else {
                return;
            };
            match controller.tick() {
                Ok(interaction) => interaction,
                Err(error) => {
                    controller.report(error);
                    None
                }
            }
        };
        if let Some(interaction) = interaction {
            self.run(interaction);
        }
    }
}

impl Controller {
    async fn confirm(&self, title: &str, message: impl Into<String>) -> Result<bool> {
        let ui = self.ui.upgrade().context("The window has closed")?;
        let message = message.into();
        ensure!(
            !title.contains('\0') && !message.contains('\0'),
            "Confirmation text contains invalid null characters"
        );
        let response = Rc::new(RefCell::new((None, None::<Waker>)));
        let callback = response.clone();
        let weak = ui.as_weak();
        ui.on_confirmation_response(move |approved| {
            if let Some(ui) = weak.upgrade() {
                ui.set_confirmation_open(false);
            }
            let waker = {
                let mut state = callback.borrow_mut();
                state.0 = Some(approved);
                state.1.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        });
        ui.show()?;
        ui.window().set_minimized(false);
        ui.set_confirmation_title(title.into());
        ui.set_confirmation_message(message.into());
        ui.set_confirmation_open(true);
        let approved = poll_fn(|cx| {
            let mut state = response.borrow_mut();
            if let Some(approved) = state.0.take() {
                Poll::Ready(approved)
            } else {
                state.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
        ui.on_confirmation_response(|_| {});
        Ok(approved)
    }

    fn data_directory(&self) -> Result<PathBuf> {
        match &self.preview_directory {
            Some(directory) => Ok(directory.path().to_path_buf()),
            None => crate::config::root_path(),
        }
    }

    fn report(&mut self, error: anyhow::Error) {
        self.local_error = Some(format!("{error:#}"));
        if let Some(ui) = self.ui.upgrade() {
            ui.set_error(self.local_error.as_deref().unwrap_or_default().into());
        }
    }

    fn dispatch(&self, command: Command) -> Result<()> {
        if self.demo {
            return Ok(());
        }
        self.backend.command(command)
    }

    fn native(&self, command: NativeCommand) -> Result<()> {
        ensure!(
            self.game != GameId::Dusklight,
            "Dusklight uses its own save workflow"
        );
        self.dispatch(Command::Native {
            game: self.game,
            command,
        })
    }

    fn approved(&self) -> bool {
        self.approval.as_ref().is_some_and(|approval| {
            approval.game == self.game
                && self
                    .snapshot
                    .source
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.game == self.game && plan.commit == approval.commit)
                && self.snapshot.source.preferences.get(self.game) == &approval.preference
        })
    }

    fn select_game(&mut self, index: i32) -> Result<()> {
        let game = model::game(index)?;
        self.game = game;
        self.approval = None;
        if let Some(path) = &self.game_path {
            crate::files::atomic_write(path, &serde_json::to_vec(&game)?)?;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_selected_game(index);
        }
        self.refresh_game()?;
        self.refresh_view()
    }

    fn refresh_game(&self) -> Result<()> {
        if self.game == GameId::Dusklight {
            self.dispatch(Command::RefreshSave)
        } else {
            self.native(NativeCommand::Refresh)
        }
    }

    fn set_preference(&mut self, preference: Preference) -> Result<()> {
        self.approval = None;
        if self.demo {
            self.snapshot
                .source
                .preferences
                .set(self.game, preference)?;
            self.refresh_view()
        } else {
            self.backend.command(Command::SetSourcePreference {
                game: self.game,
                preference,
            })
        }
    }

    async fn toggle(&mut self, toggle: Toggle, value: bool) -> Result<()> {
        self.local_error = None;
        match toggle {
            Toggle::SourceConsent => {
                ensure!(
                    !self.snapshot.install_busy && !self.snapshot.source.busy,
                    "Finish or cancel the current build before changing its approval"
                );
                if value {
                    let plan = self
                        .snapshot
                        .source
                        .plan
                        .as_ref()
                        .filter(|plan| plan.game == self.game)
                        .context("Check and review the source revision first")?;
                    self.approval = Some(Approval {
                        game: self.game,
                        commit: plan.commit.clone(),
                        preference: self.snapshot.source.preferences.get(self.game).clone(),
                    });
                } else {
                    self.approval = None;
                }
                self.refresh_view()
            }
            Toggle::Startup => self.dispatch(Command::SetStartup(value)),
            Toggle::Updates => self.dispatch(Command::SetAutoUpdateChecks(value)),
            Toggle::DiscSharing => self.dispatch(Command::SetIsoEnabled(value)),
            Toggle::GameFileSharing => {
                let kind = match self.game {
                    GameId::Triforce => GameFileKind::OotNup,
                    GameId::Totk => GameFileKind::TotkBase,
                    GameId::Dusklight => bail!("Use Dusklight's separate disc-transfer permission"),
                };
                self.dispatch(Command::SetGameFileSharing {
                    kind,
                    enabled: value,
                })
            }
            Toggle::KeySharing => {
                ensure!(
                    self.game == GameId::Totk,
                    "Select Tears of the Kingdom first"
                );
                if value && !self.confirm(
                    "Allow console-key transfer",
                    "Console keys are sensitive. Allow transferring your own prod.keys only to your pinned, paired PC over the encrypted LAN connection? The other PC must enable this separately and approve each incoming file. This does not share the app's private pairing identity.",
                ).await? {
                    return Ok(());
                }
                self.dispatch(Command::SetGameFileSharing {
                    kind: GameFileKind::TotkKeys,
                    enabled: value,
                })
            }
            Toggle::TextureConsent => {
                self.texture_consent = value;
                self.refresh_view()
            }
            Toggle::SourceMode
            | Toggle::ManagedTools
            | Toggle::Renodx
            | Toggle::Fsr
            | Toggle::Nvidia
            | Toggle::Shaders => {
                ensure!(
                    !self.snapshot.install_busy && !self.snapshot.source.busy,
                    "Finish the current operation before changing build preferences"
                );
                let mut preference = self.snapshot.source.preferences.get(self.game).clone();
                match toggle {
                    Toggle::SourceMode => {
                        preference.method = if value {
                            Method::Source
                        } else {
                            Method::Official
                        }
                    }
                    Toggle::ManagedTools => preference.managed_tools = value,
                    Toggle::Renodx => preference.renodx_hdr = value,
                    Toggle::Fsr => preference.amd_fsr4 = value,
                    Toggle::Shaders => preference.prepare_shaders = value,
                    Toggle::Nvidia => {
                        if value
                            && !self.confirm(
                                "NVIDIA SDK terms",
                                "Enable NVIDIA DLSS/frame generation and accept NVIDIA's SDK license for this build? Review https://developer.nvidia.com/rtx/sdk-license before accepting. Its SDK/runtime files will be downloaded.",
                            ).await?
                        {
                            return Ok(());
                        }
                        preference.nvidia_dlss = value;
                    }
                    _ => bail!("Unsupported source preference"),
                }
                self.set_preference(preference)
            }
        }
    }

    fn choose_input(&mut self, kind: i32) -> Result<()> {
        ensure!(
            !self.snapshot.install_busy,
            "Wait for the current installation"
        );
        let dialog = rfd::FileDialog::new();
        let selected = match kind {
            0 if self.game == GameId::Triforce => dialog
                .set_title("Choose your Ocarina of Time game (.nup)")
                .add_filter("Ocarina of Time game (.nup)", &["nup"])
                .pick_file(),
            0 if self.game == GameId::Totk => dialog
                .set_title("Choose your Tears of the Kingdom base game")
                .add_filter(
                    "TotK base game (.nsp, .xci, .nsz, .xcz)",
                    &["nsp", "xci", "nsz", "xcz"],
                )
                .pick_file(),
            1 if self.game == GameId::Totk => dialog
                .set_title("Choose your Tears of the Kingdom update")
                .add_filter("TotK update (.nsp, .nsz)", &["nsp", "nsz"])
                .pick_file(),
            2 if self.game == GameId::Totk => dialog
                .set_title("Your console's prod.keys")
                .add_filter("Console keys", &["keys"])
                .pick_file(),
            3 if self.game == GameId::Totk => dialog
                .set_title("Optional firmware font folder")
                .pick_folder(),
            4 if self.game == GameId::Triforce => dialog
                .set_title("Choose extracted Ocarina of Time files (romfs and exefs)")
                .pick_folder(),
            _ => bail!("Unknown source input"),
        };
        if let Some(path) = selected {
            let mut preference = self.snapshot.source.preferences.get(self.game).clone();
            match kind {
                0 | 4 => preference.game_input = Some(path),
                1 => preference.update = Some(path),
                2 => preference.keys = Some(path),
                3 => preference.firmware = Some(path),
                _ => bail!("Unknown source input"),
            }
            self.set_preference(preference)?;
        }
        Ok(())
    }

    fn clear_input(&mut self, kind: i32) -> Result<()> {
        ensure!(
            !self.snapshot.install_busy,
            "Wait for the current installation"
        );
        let mut preference = self.snapshot.source.preferences.get(self.game).clone();
        match kind {
            0 => preference.game_input = None,
            1 => preference.update = None,
            2 => preference.keys = None,
            3 => preference.firmware = None,
            _ => bail!("Unknown source input"),
        }
        self.set_preference(preference)
    }

    fn history(&self) -> &crate::history::Page {
        model::native(&self.snapshot, self.game)
            .map_or(&self.snapshot.history, |state| &state.history)
    }

    async fn preserve_textures(&self) -> Result<bool> {
        if self.snapshot.textures.unmanaged {
            self.confirm(
                "Preserve existing textures",
                "Keep existing unmanaged texture files in a backup before applying this change? Disabling the managed pack restores them.",
            ).await
        } else {
            Ok(true)
        }
    }

    async fn handle(&mut self, action: Action, index: i32) -> Result<()> {
        self.local_error = None;
        let ui = self.ui.upgrade().context("The window has closed")?;
        match action {
            Action::Play => {
                ensure!(
                    model::play_blocker(&self.snapshot, self.game).is_none(),
                    "{}",
                    model::play_blocker(&self.snapshot, self.game).unwrap_or("Play is not ready")
                );
                if self.game == GameId::Dusklight {
                    self.dispatch(Command::Launch)
                } else {
                    self.native(NativeCommand::Launch { local_only: false })
                }
            }
            Action::SyncSave => {
                if self.game == GameId::Dusklight {
                    self.dispatch(Command::SendSave)
                } else {
                    self.native(NativeCommand::Send)
                }
            }
            Action::Setup => {
                ui.invoke_navigate(1);
                Ok(())
            }
            Action::LocateGame => {
                let executable = match self.game {
                    GameId::Dusklight => "dusklight.exe",
                    GameId::Totk => "totk.exe",
                    GameId::Triforce => "Triforce.exe",
                };
                if let Some(path) = rfd::FileDialog::new()
                    .set_title(format!("Locate {executable}"))
                    .add_filter(executable, &["exe"])
                    .pick_file()
                {
                    if self.game == GameId::Dusklight {
                        self.dispatch(Command::SetExecutable(path))?;
                    } else {
                        self.native(NativeCommand::SetExecutable(path))?;
                    }
                }
                Ok(())
            }
            Action::Refresh => self.refresh_game(),
            Action::Compare => {
                ui.invoke_navigate(0);
                ui.set_comparison_open(true);
                self.refresh_game()
            }
            Action::ViewGame => {
                ui.invoke_navigate(0);
                self.select_game(index)
            }
            Action::ContinueLocal => {
                ensure!(
                    model::can_continue(&self.snapshot, self.game),
                    "Finish the active session or transfer before continuing"
                );
                if self.confirm(
                    "Keep both branches",
                    "Keep both complete save branches and play this PC's progress? No other-PC save is overwritten. Reconcile when both PCs reconnect.",
                ).await? {
                    let expected = model::local_hash(&self.snapshot, self.game).map(str::to_owned);
                    if self.game == GameId::Dusklight {
                        self.dispatch(Command::ContinueLocal { expected })?;
                    } else {
                        self.native(NativeCommand::Continue { expected })?;
                    }
                }
                Ok(())
            }
            Action::UseLocal | Action::UseRemote => {
                ensure!(
                    model::can_resolve(&self.snapshot, self.game),
                    "Wait for a complete verified comparison before choosing a branch"
                );
                let (local_hash, remote_hash) =
                    if let Some(state) = model::native(&self.snapshot, self.game) {
                        (
                            state.local_hash.clone().context("No local branch")?,
                            state.remote_hash.clone().context("No other-PC branch")?,
                        )
                    } else {
                        let conflict = self
                            .snapshot
                            .conflict
                            .as_ref()
                            .context("No branch choice is pending")?;
                        (conflict.local_hash.clone(), conflict.remote_hash.clone())
                    };
                let use_remote = action == Action::UseRemote;
                if self.confirm(
                    "Choose a complete save branch",
                    format!(
                        "Use {} complete save? Both branches are retained. The other PC still approves replacement of its own save.",
                        if use_remote {
                            "your other PC's"
                        } else {
                            "this PC's"
                        }
                    ),
                ).await? {
                    if self.game == GameId::Dusklight {
                        self.dispatch(Command::ResolveConflict {
                            local_hash,
                            remote_hash,
                            use_remote,
                        })?;
                    } else {
                        self.native(NativeCommand::Resolve {
                            local_hash,
                            remote_hash,
                            use_remote,
                        })?;
                    }
                }
                Ok(())
            }
            Action::AcceptSave | Action::DeclineSave => {
                let approved = action == Action::AcceptSave;
                ensure!(
                    !approved || model::can_accept(&self.snapshot, self.game),
                    "Close this game's sessions and finish conflicting operations before receiving a save"
                );
                if approved
                    && !self.confirm(
                        "Receive the complete save",
                        "Receive the offered save? Existing committed progress is retained in history and a rollback copy before replacement.",
                    ).await?
                {
                    return Ok(());
                }
                if self.game == GameId::Dusklight {
                    self.dispatch(Command::AcceptSave(approved))
                } else {
                    self.native(NativeCommand::Accept(approved))
                }
            }
            Action::Recover => {
                ensure!(
                    model::can_recover(&self.snapshot, self.game),
                    "Close this game on both PCs and finish transfers before recovering a session"
                );
                if self.confirm(
                    "Recover a completed play session",
                    "Have you stopped playing on both PCs? Recover only a completed session. Saves and both branches are retained; another PC's offline reservation is never silently removed.",
                ).await? {
                    if self.game == GameId::Dusklight {
                        self.dispatch(Command::Recover)?;
                    } else {
                        self.native(NativeCommand::Recover)?;
                    }
                }
                Ok(())
            }
            Action::ApproveRecovery | Action::DeclineRecovery => {
                let approved = action == Action::ApproveRecovery;
                ensure!(
                    !approved || model::can_recover(&self.snapshot, self.game),
                    "Close this game on both PCs before approving recovery"
                );
                if approved
                    && !self.confirm(
                        "Approve completed-session recovery",
                        "Confirm the game has stopped on both PCs before releasing this completed reservation.",
                    ).await?
                {
                    return Ok(());
                }
                if self.game == GameId::Dusklight {
                    self.dispatch(Command::AcceptRecovery(approved))
                } else {
                    self.native(NativeCommand::AcceptRecovery(approved))
                }
            }
            Action::ResetSource => {
                if self.confirm(
                    "Reset build preferences",
                    "Reset build methods, optional settings and local input selections? Installed games, saves, pairing and play reservations are unchanged.",
                ).await? {
                    self.approval = None;
                    if self.demo {
                        self.snapshot.source = source_build::State {
                            preferences_valid: true,
                            ..Default::default()
                        };
                        self.refresh_view()?;
                    } else {
                        self.dispatch(Command::ResetSourcePreferences)?;
                    }
                }
                Ok(())
            }
            Action::CheckSource => {
                self.approval = None;
                if self.demo {
                    self.snapshot.source.game = Some(self.game);
                    self.snapshot.source.plan = Some(source_build::Plan {
                        game: self.game,
                        tag: "v1.0.0-example".into(),
                        commit: "a".repeat(40),
                        review_url: source_build::source_url(self.game),
                    });
                    self.snapshot.source.prerequisites = source_build::prerequisites(self.game)?;
                    return self.refresh_view();
                }
                self.dispatch(Command::CheckSource(self.game))
            }
            Action::ReviewSource => {
                let plan = self
                    .snapshot
                    .source
                    .plan
                    .as_ref()
                    .filter(|plan| plan.game == self.game)
                    .context("Check the source first")?;
                open_url(&plan.review_url)
            }
            Action::CompareSource => {
                let plan = self
                    .snapshot
                    .source
                    .plan
                    .as_ref()
                    .filter(|plan| plan.game == self.game)
                    .context("Check the source first")?;
                let url = self.snapshot.installed_versions
                    [usize::try_from(model::index(self.game))?]
                .installed
                .as_ref()
                .and_then(|version| version.compare_url(self.game, &plan.commit))
                .context(
                    "A recorded, different installed source revision is required for a diff",
                )?;
                open_url(&url)
            }
            Action::ToolHelp => {
                let index = usize::try_from(index).context("Invalid prerequisite selection")?;
                let prerequisite = self
                    .snapshot
                    .source
                    .prerequisites
                    .get(index)
                    .context("The prerequisite list changed; check again")?;
                open_url(prerequisite.url)
            }
            Action::NvidiaTerms => open_url("https://developer.nvidia.com/rtx/sdk-license"),
            Action::PrepareTools => {
                if self.confirm(
                    "Prepare verified build tools",
                    "Download portable Git, Python tooling and this game's compiler/SDK archives from the named publishers? Archives are checksum-verified, not proven safe. This can use several GiB. System SDK installers are separate approved steps.",
                ).await? {
                    self.dispatch(Command::PrepareSourceTools {
                        game: self.game,
                        approved: true,
                    })?;
                }
                Ok(())
            }
            Action::BuildSource => {
                ensure!(
                    self.approved(),
                    "Review and approve the current source revision and settings first"
                );
                ensure!(
                    model::source_blocker(&self.snapshot).is_none(),
                    "{}",
                    model::source_blocker(&self.snapshot).unwrap_or("Source build is not ready")
                );
                let plan = self
                    .snapshot
                    .source
                    .plan
                    .as_ref()
                    .context("Check the source revision first")?;
                let commit = plan.commit.clone();
                self.approval = None;
                self.dispatch(Command::BuildSource {
                    game: self.game,
                    commit,
                    preference: self.snapshot.source.preferences.get(self.game).clone(),
                    approved: true,
                })
            }
            Action::CancelSource => self.dispatch(Command::CancelSource),
            Action::UseBuilt => self.dispatch(Command::ActivateSourceBuild(self.game)),
            Action::OpenLog => {
                let log = self
                    .snapshot
                    .source
                    .log
                    .as_deref()
                    .context("No build log is available")?;
                open_path(log)
            }
            Action::SetupDotnet | Action::SetupMsys | Action::SetupVisualStudio => {
                let setup = match action {
                    Action::SetupDotnet => SdkSetup::DotNet10,
                    Action::SetupVisualStudio => SdkSetup::VisualStudio,
                    _ => SdkSetup::Msys2Ucrt64,
                };
                if self
                    .confirm("Set up source-build SDK", setup.notice())
                    .await?
                {
                    self.approval = None;
                    self.dispatch(Command::InstallSourceSdk {
                        game: self.game,
                        setup,
                        approved: true,
                    })?;
                }
                Ok(())
            }
            Action::CopyDotnet => copy_commands(SdkSetup::DotNet10),
            Action::CopyMsys => copy_commands(SdkSetup::Msys2Ucrt64),
            Action::CopyVisualStudio => copy_commands(SdkSetup::VisualStudio),
            Action::CheckRelease => match self.game {
                GameId::Dusklight => self.dispatch(Command::CheckRelease),
                GameId::Totk => self.dispatch(Command::CheckTotkRelease),
                GameId::Triforce => bail!("Triforce is installed from source"),
            },
            Action::InstallRelease => match self.game {
                GameId::Dusklight => self.dispatch(Command::Install),
                GameId::Totk => self.dispatch(Command::UpdateTotk),
                GameId::Triforce => bail!("Triforce is installed from source"),
            },
            Action::TrustNetwork => {
                if self
                    .confirm(
                        "Approve this private LAN",
                        format!(
                            "Trust the displayed home network and router identity?\n\n{}",
                            ui.get_network_detail()
                        ),
                    )
                    .await?
                {
                    self.dispatch(Command::TrustNetwork)?;
                }
                Ok(())
            }
            Action::OpenPairing => {
                ensure!(
                    model::can_pair(&self.snapshot),
                    "Approve a private network and finish active operations before pairing; unpair an existing partner first"
                );
                self.dispatch(Command::OpenPairing)
            }
            Action::Reconnect => {
                ensure!(
                    model::can_reconnect(&self.snapshot),
                    "Your pinned partner must be discovered on the approved LAN before reconnecting"
                );
                let peer = self
                    .snapshot
                    .settings
                    .peer
                    .as_ref()
                    .context("No paired device")?;
                self.dispatch(Command::Connect(peer.public_key.clone()))
            }
            Action::ConnectDevice => {
                ensure!(
                    model::can_pair(&self.snapshot) && self.snapshot.pairing_open,
                    "Open pairing on both PCs before comparing codes"
                );
                let device = self
                    .snapshot
                    .devices
                    .get(usize::try_from(index)?)
                    .context("The device list changed; try again")?;
                ensure!(device.pairing, "Open pairing on the other PC too");
                self.dispatch(Command::Connect(device.key.clone()))
            }
            Action::ApprovePairing => {
                ensure!(
                    self.snapshot.comparison_code.is_some(),
                    "There is no code to compare"
                );
                if self.confirm(
                    "Compare every part of the code",
                    format!(
                        "Does this full code match the other PC?\n\n{}\n\nApprove only after checking both screens.",
                        self.snapshot
                            .comparison_code
                            .as_deref()
                            .context("The code changed")?
                    ),
                ).await? {
                    self.dispatch(Command::ApprovePairing)?;
                }
                Ok(())
            }
            Action::RejectPairing => self.dispatch(Command::RejectPairing),
            Action::Unpair => {
                if self.confirm(
                    "Unpair this device",
                    "Remove trust in the paired PC? Saves and stored branches stay local. Finish any protected play sessions first.",
                ).await? {
                    self.dispatch(Command::Unpair)?;
                }
                Ok(())
            }
            Action::HistoryRefresh | Action::HistoryOlder => {
                let before = if action == Action::HistoryOlder {
                    self.history().next.clone()
                } else {
                    None
                };
                if self.game == GameId::Dusklight {
                    self.dispatch(Command::HistoryPage(before))
                } else {
                    self.native(NativeCommand::HistoryPage(before))
                }
            }
            Action::HistoryPreview => {
                let entry = self
                    .history()
                    .entries
                    .get(usize::try_from(index)?)
                    .context("History changed; refresh the list")?;
                self.dispatch(Command::PreviewHistory {
                    game: self.game,
                    id: entry.id.clone(),
                })
            }
            Action::HistoryRestore => {
                ensure!(
                    model::can_restore(&self.snapshot, self.game),
                    "Finish play and transfers before restoring"
                );
                let entry = self
                    .history()
                    .entries
                    .get(usize::try_from(index)?)
                    .context("History changed; refresh the list")?;
                if self.confirm(
                    "Restore a complete snapshot",
                    format!(
                        "Restore snapshot #{} from {}? Current progress is retained before replacement. Graphics settings and other accounts stay local.",
                        entry.sequence, entry.timestamp
                    ),
                ).await? {
                    let expected = model::local_hash(&self.snapshot, self.game).map(str::to_owned);
                    let id = entry.id.clone();
                    if self.game == GameId::Dusklight {
                        self.dispatch(Command::RestoreSnapshot { id, expected })?;
                    } else {
                        self.native(NativeCommand::Restore { id, expected })?;
                    }
                }
                Ok(())
            }
            Action::ClosePreview => self.dispatch(Command::ClearHistoryPreview),
            Action::CheckAppUpdate => self.dispatch(Command::CheckSelfUpdate),
            Action::AppReleaseNotes => open_url(
                &self
                    .snapshot
                    .self_updates
                    .latest
                    .as_ref()
                    .context("Check for an app release first")?
                    .notes_url,
            ),
            Action::OfficialHelp => {
                ensure!(self.game == GameId::Totk, "Select TotK Recomp first");
                open_url(crate::totk_update::SETUP_DOWNLOAD_URL)
            }
            Action::UpdateApp => {
                ensure!(
                    model::app_update_blocker(&self.snapshot).is_none(),
                    "{}",
                    model::app_update_blocker(&self.snapshot).unwrap_or("Update is not ready")
                );
                if self.confirm(
                    "Update Twilight Sync",
                    "Download and verify the new app, wait for a clean exit, replace the managed executable and restart? Saves and reservations are preserved.",
                ).await? {
                    self.dispatch(Command::UpdateSelf)?;
                }
                Ok(())
            }
            Action::ChooseIso => {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Your Twilight Princess GameCube disc dump")
                    .add_filter("GameCube disc", &["iso", "rvz"])
                    .pick_file()
                {
                    self.dispatch(Command::SetIso(path))?;
                }
                Ok(())
            }
            Action::SendIso => {
                ensure!(
                    model::can_send_disc(&self.snapshot),
                    "Enable disc sharing, choose a verified disc, and finish active work on the connected PCs first"
                );
                self.dispatch(Command::SendIso)
            }
            Action::OfferGameFile => {
                let kind = match (self.game, index) {
                    (GameId::Triforce, 0) => GameFileKind::OotNup,
                    (GameId::Totk, 0) => GameFileKind::TotkBase,
                    (GameId::Totk, 1) => GameFileKind::TotkUpdate,
                    (GameId::Totk, 2) => GameFileKind::TotkKeys,
                    _ => bail!("Choose a supported game input to transfer"),
                };
                ensure!(
                    model::can_send_game_file(&self.snapshot, kind),
                    "Select this file, enable its transfer permission and finish active work on the connected PCs first"
                );
                let message = if kind == GameFileKind::TotkKeys {
                    "Send your selected prod.keys to the pinned paired PC? Console keys are sensitive. Continue only if this is your own trusted PC. Key contents are never written to transfer logs."
                } else {
                    "Offer the selected local game file to your paired PC? Continue only if you own the game. The other user must accept and choose a destination. This will not install or launch anything."
                };
                if self.confirm(kind.label(), message).await? {
                    self.dispatch(Command::SendGameFile(kind))?;
                }
                Ok(())
            }
            Action::AcceptIso => {
                ensure!(
                    model::can_accept_disc(&self.snapshot),
                    "Enable this file's sharing permission and finish active work before accepting it"
                );
                let format = self
                    .snapshot
                    .pending_disc_format
                    .context("Image format missing; decline and request a new offer")?;
                if format == TransferFormat::Game(GameFileKind::TotkKeys)
                    && !self.confirm("Receive console keys",
                        "Receive prod.keys from your pinned paired PC? Console keys are sensitive and will be saved unencrypted at the local destination you choose. This never replaces the app's private pairing identity.").await?
                {
                    return Ok(());
                }
                let file_name = match format {
                    TransferFormat::Game(kind) => kind.file_name().to_owned(),
                    _ => format!("Twilight Princess.{}", format.extension()),
                };
                if let Some(path) = rfd::FileDialog::new()
                    .set_title(format!("Save offered {}", format.label()))
                    .add_filter(format.label(), &[format.extension()])
                    .set_file_name(file_name)
                    .save_file()
                {
                    self.dispatch(Command::AcceptIso(Some(path)))?;
                }
                Ok(())
            }
            Action::DeclineIso => self.dispatch(Command::AcceptIso(None)),
            Action::OpenTextureSite => {
                let entry = textures::CATALOG
                    .get(usize::try_from(index)?)
                    .context("Unknown texture creator")?;
                self.dispatch(Command::OpenTextureCreator(entry.id.into()))
            }
            Action::ImportTextureZip
            | Action::ImportTextureFolder
            | Action::ImportExistingTextures => {
                ensure!(
                    self.texture_consent,
                    "Confirm that you have permission to use these textures"
                );
                ensure!(
                    model::can_import_textures(&self.snapshot),
                    "Set up Dusklight and finish active work or texture recovery before importing"
                );
                let creator = textures::CATALOG
                    .get(usize::try_from(ui.get_texture_creator())?)
                    .context("Choose a texture creator")?;
                let dialog = rfd::FileDialog::new().set_title("Authorized texture pack");
                let selected = if action == Action::ImportExistingTextures {
                    ensure!(
                        self.snapshot.textures.unmanaged,
                        "Existing textures changed; refresh the library before importing"
                    );
                    Some(
                        self.snapshot
                            .settings
                            .texture_profile
                            .as_ref()
                            .context("The Dusklight profile is not available")?
                            .join("texture_replacements"),
                    )
                } else if action == Action::ImportTextureFolder {
                    dialog.pick_folder()
                } else {
                    dialog.add_filter("Texture archive", &["zip"]).pick_file()
                };
                if let Some(path) = selected
                    && self.preserve_textures().await?
                {
                    self.dispatch(Command::ImportTextures {
                        source: creator.id.into(),
                        version: ui.get_texture_version().to_string(),
                        path,
                        preserve_unmanaged: true,
                    })?;
                }
                Ok(())
            }
            Action::ActivateTexture => {
                ensure!(
                    model::can_change_textures(&self.snapshot),
                    "Finish active work or texture recovery and verify the Dusklight profile first"
                );
                let pack = self
                    .snapshot
                    .textures
                    .packs
                    .get(usize::try_from(index)?)
                    .context("Texture list changed; refresh")?;
                if self.preserve_textures().await? {
                    self.dispatch(Command::ActivateTextures {
                        id: Some(pack.id.clone()),
                        preserve_unmanaged: true,
                    })?;
                }
                Ok(())
            }
            Action::DisableTextures => {
                ensure!(
                    model::can_change_textures(&self.snapshot),
                    "Finish active work or texture recovery and verify the Dusklight profile first"
                );
                if self.preserve_textures().await? {
                    self.dispatch(Command::ActivateTextures {
                        id: None,
                        preserve_unmanaged: true,
                    })?;
                }
                Ok(())
            }
            Action::RecoverTextures => self.dispatch(Command::RecoverTextures),
            Action::RefreshTextures => self.dispatch(Command::RefreshTextures),
            Action::CancelTextures => self.dispatch(Command::CancelTextures),
            Action::OpenData => open_path(&self.data_directory()?),
            Action::CreateDesktopShortcut => self.dispatch(Command::CreateDesktopShortcut),
            Action::ProjectLink => match index {
                0 => open_url("https://github.com/false00"),
                1 => open_url(env!("CARGO_PKG_REPOSITORY")),
                2 => open_url(concat!(env!("CARGO_PKG_REPOSITORY"), "/releases")),
                _ => bail!("Unknown project link"),
            },
            Action::OpenLicenses => {
                let path = self.data_directory()?.join("THIRD_PARTY_NOTICES.txt");
                crate::files::atomic_write(&path, NOTICES.as_bytes())?;
                open_path(&path)
            }
            Action::PreviewScenario => {
                ensure!(
                    self.demo,
                    "Example scenarios are only available in the isolated preview"
                );
                #[cfg(debug_assertions)]
                {
                    let scenario = match index {
                        0 => "ready",
                        1 => "attention",
                        2 => "pairing",
                        3 => "offline",
                        4 => "connecting",
                        _ => bail!("Unknown preview scenario"),
                    };
                    let backend = crate::runtime::ui_smoke_backend(scenario)?;
                    self.snapshot = backend.snapshot()?;
                    backend.command(Command::Shutdown)?;
                    self.approval = None;
                    ui.set_about_open(false);
                    ui.set_comparison_open(false);
                    self.refresh_view()
                }
                #[cfg(not(debug_assertions))]
                bail!("Example scenarios require a debug build")
            }
            Action::ExportPreview => self.export_preview(),
            Action::Quit => self.backend.command(Command::Shutdown),
        }
    }

    fn export_preview(&self) -> Result<()> {
        ensure!(
            self.demo,
            "Screenshot export is limited to the example-data preview"
        );
        let ui = self.ui.upgrade().context("The preview has closed")?;
        ui.set_about_open(false);
        let image = ui
            .window()
            .take_snapshot()
            .context("Rendering the example screenshot")?;
        let Some(path) = rfd::FileDialog::new()
            .set_title("Save example interface image")
            .add_filter("PNG image", &["png"])
            .save_file()
        else {
            return Ok(());
        };
        crate::files::ensure_safe_path(&path)?;
        ensure!(!path.try_exists()?, "Choose a new screenshot filename");
        let file = std::fs::File::create_new(&path)?;
        image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(image.width(), image.height(), image.as_bytes().to_vec())
                .context("Invalid screenshot dimensions")?,
        )
        .write_to(&mut std::io::BufWriter::new(file), image::ImageFormat::Png)?;
        Ok(())
    }

    fn refresh_view(&mut self) -> Result<()> {
        let ui = self.ui.upgrade().context("The window has closed")?;
        let snapshot = &self.snapshot;
        let game = self.game;
        if self.approval.is_some() && !self.approved() {
            self.approval = None;
        }
        ui.set_dark_mode(self.appearance.dark()?);
        if self.native_appearance != Some(self.appearance.appearance) {
            let theme = match self.appearance.appearance {
                appearance::Appearance::System => None,
                appearance::Appearance::Light => Some(winit::window::Theme::Light),
                appearance::Appearance::Dark => Some(winit::window::Theme::Dark),
            };
            if ui
                .window()
                .with_winit_window(|window| window.set_theme(theme))
                .is_some()
            {
                self.native_appearance = Some(self.appearance.appearance);
            }
        }
        ui.set_appearance_mode(match self.appearance.appearance {
            appearance::Appearance::System => widgets::AppearanceMode::System,
            appearance::Appearance::Light => widgets::AppearanceMode::Light,
            appearance::Appearance::Dark => widgets::AppearanceMode::Dark,
        });
        ui.set_version(env!("CARGO_PKG_VERSION").into());
        ui.set_build_target(
            format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH).into(),
        );
        ui.set_build_profile(
            if cfg!(debug_assertions) {
                "Debug"
            } else {
                "Release"
            }
            .into(),
        );
        ui.set_connected(snapshot.ready);
        ui.set_connecting(snapshot.connecting);
        ui.set_connection(
            if snapshot.ready {
                format!(
                    "Connected · {}",
                    snapshot.peer_name.as_deref().unwrap_or("other PC")
                )
            } else if snapshot.comparison_code.is_some() {
                "Pairing · compare both screens".into()
            } else if snapshot.connecting {
                "Connecting…".into()
            } else {
                "Offline · your saves stay here".into()
            }
            .into(),
        );
        ui.set_status(snapshot.message.clone().into());
        let desktop_error = self
            .background
            .as_ref()
            .and_then(|background| background.error());
        ui.set_error(
            self.local_error
                .as_deref()
                .or(desktop_error.as_deref())
                .or(snapshot.error.as_deref())
                .unwrap_or_default()
                .into(),
        );
        let games = model::GAMES
            .iter()
            .map(|&game| widgets::GameCard {
                title: model::title(game).into(),
                subtitle: model::subtitle(game).into(),
                installed: model::installed(snapshot, game),
                status: model::game_status(snapshot, game).into(),
            })
            .collect();
        replace_rows(ui.get_games(), games, |rows| ui.set_games(rows));
        ui.set_game_title(model::title(game).into());
        ui.set_game_subtitle(model::subtitle(game).into());
        ui.set_game_status(model::game_status(snapshot, game).into());
        ui.set_game_message(
            model::native(snapshot, game)
                .map_or(snapshot.message.as_str(), |state| state.message.as_str())
                .into(),
        );
        ui.set_verified_at(
            model::native(snapshot, game)
                .and_then(|state| state.verified_at.as_deref())
                .unwrap_or_default()
                .into(),
        );
        ui.set_saves_match(model::saves_match(snapshot, game));
        ui.set_local_revision(model::local_hash(snapshot, game).unwrap_or_default().into());
        ui.set_remote_revision(
            model::native(snapshot, game)
                .map_or(snapshot.remote_hash.as_deref(), |state| {
                    state.remote_hash.as_deref()
                })
                .unwrap_or_default()
                .into(),
        );
        ui.set_base_revision(
            model::native(snapshot, game)
                .map_or_else(
                    || {
                        snapshot
                            .conflict
                            .as_ref()
                            .and_then(|conflict| conflict.base_hash.as_deref())
                    },
                    |state| state.base.as_deref(),
                )
                .unwrap_or_default()
                .into(),
        );
        let changes = |summary: Option<&crate::save_data::Summary>| {
            if game == GameId::Dusklight {
                summary
                    .zip(
                        snapshot
                            .conflict
                            .as_ref()
                            .and_then(|conflict| conflict.base.as_ref()),
                    )
                    .map(|(summary, base)| summary.changes_from(base).join("\n"))
                    .unwrap_or_default()
            } else {
                String::new()
            }
        };
        ui.set_local_changes(changes(snapshot.local_save.as_ref()).into());
        ui.set_remote_changes(
            changes(
                snapshot
                    .conflict
                    .as_ref()
                    .and_then(|conflict| conflict.remote.as_ref()),
            )
            .into(),
        );
        ui.set_connection_setup(
            if snapshot.ready {
                ""
            } else if !model::network_trusted(snapshot) {
                "Approve your private home network to connect another PC."
            } else if snapshot.settings.peer.is_none() {
                "Pair your other PC to bring your progress along."
            } else {
                ""
            }
            .into(),
        );
        ui.set_transfer(snapshot.transfer.clone().into());
        ui.set_management_available(model::management_available(snapshot));
        let attention = model::GAMES
            .iter()
            .filter_map(|&game| {
                if game == self.game && ui.get_section() == 0 {
                    return None;
                }
                let (pending, recovery) = model::native(snapshot, game).map_or(
                    (snapshot.pending_save, snapshot.pending_recovery),
                    |state| (state.pending, state.recovery_pending),
                );
                (pending || recovery).then(|| widgets::AttentionCard {
                    game_index: model::index(game),
                    title: model::title(game).into(),
                    detail: if recovery {
                        "A completed-session recovery needs your decision."
                    } else {
                        "An incoming complete save needs your decision."
                    }
                    .into(),
                })
            })
            .collect();
        replace_rows(ui.get_attention(), attention, |rows| ui.set_attention(rows));
        ui.set_installed(model::installed(snapshot, game));
        let path = model::native(snapshot, game)
            .and_then(|state| state.install_path.as_ref())
            .cloned()
            .or_else(|| {
                if game == GameId::Dusklight {
                    snapshot
                        .settings
                        .game_exe
                        .as_ref()
                        .and_then(|path| path.parent())
                        .map(Path::to_path_buf)
                } else {
                    None
                }
            });
        ui.set_installed_path(
            path.as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        let selected_executable = model::executable(snapshot, game);
        ui.set_installed_executable(
            selected_executable
                .map(|path| path.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        let version_state = &snapshot.installed_versions[usize::try_from(model::index(game))?];
        let installed_version = version_state.installed.as_ref();
        ui.set_installed_release(
            version_state
                .error
                .clone()
                .unwrap_or_else(|| {
                    installed_version.map_or_else(
                        || {
                            if selected_executable.is_some() {
                                "Version not recorded for this installation".into()
                            } else {
                                String::new()
                            }
                        },
                        install::Version::label,
                    )
                })
                .into(),
        );
        ui.set_can_play(model::play_blocker(snapshot, game).is_none());
        ui.set_can_sync(model::sync_blocker(snapshot, game).is_none());
        ui.set_can_continue(model::can_continue(snapshot, game));
        ui.set_can_restore(model::can_restore(snapshot, game));
        ui.set_can_resolve(model::can_resolve(snapshot, game));
        ui.set_comparison_reason(
            model::comparison_blocker(snapshot, game)
                .unwrap_or_default()
                .into(),
        );
        ui.set_can_accept_save(model::can_accept(snapshot, game));
        ui.set_can_recover(model::can_recover(snapshot, game));
        ui.set_play_reason(
            model::play_blocker(snapshot, game)
                .unwrap_or_default()
                .into(),
        );
        ui.set_sync_reason(
            model::sync_blocker(snapshot, game)
                .unwrap_or_default()
                .into(),
        );
        ui.set_can_manage(model::configuration_available(snapshot));
        ui.set_can_locate(model::can_locate(snapshot, game));
        ui.set_operation_busy(snapshot.install_busy);
        ui.set_desktop_shortcut_busy(snapshot.desktop_shortcut_busy);
        ui.set_preferences_valid(snapshot.source.preferences_valid);
        if !snapshot.source.preferences_valid
            && let Some(error) = &snapshot.source.error
        {
            ui.set_error(error.clone().into());
        }
        let (local, remote, conflict, pending, recovery, save_state) = if let Some(state) =
            model::native(snapshot, game)
        {
            (
                state
                    .local_summary
                    .as_ref()
                    .map(model::native_cards)
                    .unwrap_or_default(),
                if state.comparison_verified {
                    state
                        .remote_summary
                        .as_ref()
                        .map(model::native_cards)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                },
                state.conflict,
                state.pending,
                state.recovery_pending
                    || state.lock.is_some()
                    || state.remote_lock.is_some()
                    || state.release_pending,
                if state.comparison_verified {
                    "Complete snapshots verified for comparison."
                } else if state.local_hash.is_some() {
                    "Committed local save verified. Compare before choosing a different branch."
                } else {
                    "No committed save yet. Play and save, or receive your other PC's progress."
                }
                .to_string(),
            )
        } else {
            (snapshot.local_save.as_ref().map(model::dusklight_cards).unwrap_or_default(),
             snapshot.conflict.as_ref().and_then(|conflict| conflict.remote.as_ref()).map(model::dusklight_cards).unwrap_or_default(),
             snapshot.conflict.is_some(), snapshot.pending_save, snapshot.pending_recovery || snapshot.settings.play_lock.is_some(),
             if snapshot.save_ready { "Your active profile is detected and verified automatically." } else { "Waiting for a valid Dusklight profile. No manual save-folder selection is needed." }.to_string())
        };
        replace_rows(ui.get_local_saves(), local, |rows| ui.set_local_saves(rows));
        replace_rows(ui.get_remote_saves(), remote, |rows| {
            ui.set_remote_saves(rows)
        });
        ui.set_has_conflict(conflict);
        ui.set_pending_save(pending);
        ui.set_recovery_needed(recovery);
        ui.set_incoming_recovery(
            model::native(snapshot, game)
                .map_or(snapshot.pending_recovery, |state| state.recovery_pending),
        );
        ui.set_save_state(save_state.into());
        let preference = snapshot.source.preferences.get(game);
        ui.set_source_mode(preference.method == Method::Source);
        ui.set_managed_tools(preference.managed_tools);
        ui.set_renodx(preference.renodx_hdr);
        ui.set_fsr(preference.amd_fsr4);
        ui.set_nvidia(preference.nvidia_dlss);
        ui.set_shaders(preference.prepare_shaders);
        ui.set_source_consent(self.approved());
        for (path, setter) in [
            (
                &preference.game_input,
                MainWindow::set_source_input as fn(&MainWindow, SharedString),
            ),
            (&preference.update, MainWindow::set_source_update),
            (&preference.keys, MainWindow::set_source_keys),
            (&preference.firmware, MainWindow::set_source_firmware),
        ] {
            setter(
                &ui,
                path.as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
                    .into(),
            );
        }
        let plan = snapshot
            .source
            .plan
            .as_ref()
            .filter(|plan| plan.game == game);
        ui.set_source_tag(
            plan.map(|plan| plan.tag.as_str())
                .unwrap_or_default()
                .into(),
        );
        ui.set_source_commit(
            plan.map(|plan| plan.commit.as_str())
                .unwrap_or_default()
                .into(),
        );
        ui.set_source_comparison(plan.map_or_else(String::new, |plan| {
            installed_version.map_or_else(
                || "Installed version is unrecorded; cannot determine whether this is an upgrade.".into(),
                |version| version.comparison(&plan.tag, Some(&plan.commit)).into(),
            )
        }).into());
        ui.set_can_compare_source(plan.is_some_and(|plan| {
            installed_version
                .and_then(|version| version.compare_url(game, &plan.commit))
                .is_some()
        }));
        let same_game = snapshot.source.game == Some(game);
        ui.set_source_building(snapshot.source.building);
        let completion_current = match snapshot.source.completion {
            Some(source_build::Completion::Ready(completed_game)) => {
                snapshot.source.built.as_ref().is_some_and(|built| {
                    built.game == completed_game
                        && model::executable(snapshot, completed_game) == Some(&built.executable)
                })
            }
            Some(source_build::Completion::NeedsActivation(_)) => true,
            None => false,
        };
        ui.set_build_complete(!snapshot.source.busy && completion_current);
        ui.set_build_needs_activation(matches!(
            snapshot.source.completion,
            Some(source_build::Completion::NeedsActivation(_))
        ));
        if let Some(completion) = snapshot.source.completion {
            let (completed_game, ready) = match completion {
                source_build::Completion::Ready(game) => (game, true),
                source_build::Completion::NeedsActivation(game) => (game, false),
            };
            ui.set_build_result_game(model::index(completed_game));
            ui.set_build_result_detail(format!("{} — {}", model::title(completed_game),
                if ready {
                    "Your built copy is installed and selected. Open Play when you're ready; the game was not started automatically."
                } else {
                    "Game files were built, but activation needs attention before this copy can be used. View the build to resolve it safely."
                }).into());
        }
        let progress = &snapshot.source.progress;
        let report = &progress.report;
        ui.set_source_progress_visible(
            progress.started.is_some()
                && (same_game || snapshot.source.busy)
                && (!snapshot.install_busy || snapshot.source.busy),
        );
        ui.set_source_progress_known(report.fraction.is_some());
        ui.set_source_progress(report.fraction.unwrap_or_default() as f32);
        ui.set_source_elapsed(source_build::progress::duration_text(progress.elapsed()).into());
        ui.set_source_work(report.work_text().into());
        ui.set_source_eta(
            report
                .eta
                .map(|eta| format!("About {}", source_build::progress::duration_text(eta)))
                .unwrap_or_else(|| {
                    if report.stalled {
                        "Re-estimating".into()
                    } else {
                        "Estimating…".into()
                    }
                })
                .into(),
        );
        ui.set_source_rate(
            report
                .rate
                .map(|rate| {
                    if report.unit == "bytes" {
                        format!("{:.1} MiB/s", rate / (1024.0 * 1024.0))
                    } else {
                        format!("{rate:.1} {}/s", report.unit)
                    }
                })
                .unwrap_or_default()
                .into(),
        );
        ui.set_source_activity(report.activity.clone().into());
        ui.set_source_activity_status(
            if report.quiet_for >= Duration::from_secs(30) {
                format!("No new tool output for {}. Some steps work silently; this does not mean they have stopped.",
                    source_build::progress::duration_text(report.quiet_for))
            } else if report.stalled {
                "The tool is active, but its progress counter has not advanced recently. The estimate will return when it does.".into()
            } else {
                String::new()
            }.into(),
        );
        ui.set_tools_missing(
            same_game && snapshot.source.prerequisites.iter().any(|tool| !tool.ready),
        );
        ui.set_source_busy(snapshot.source.busy);
        ui.set_has_built(snapshot.source.built.as_ref().is_some_and(|built| {
            built.game == game
                && (snapshot.source.completion
                    == Some(source_build::Completion::NeedsActivation(game))
                    || selected_executable != Some(&built.executable))
        }));
        ui.set_sdk_busy(snapshot.source.sdk_setup.is_some());
        ui.set_source_message(
            if same_game || snapshot.source.busy {
                match snapshot.source.game {
                    Some(active) if active != game => {
                        format!("{}: {}", model::title(active), snapshot.source.message)
                    }
                    _ => snapshot.source.message.clone(),
                }
            } else {
                String::new()
            }
            .into(),
        );
        ui.set_source_error(
            if same_game {
                snapshot.source.error.as_deref().unwrap_or_default()
            } else {
                ""
            }
            .into(),
        );
        ui.set_source_log(
            if same_game || snapshot.source.busy {
                snapshot
                    .source
                    .log
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            }
            .into(),
        );
        let tools = if same_game {
            snapshot
                .source
                .prerequisites
                .iter()
                .map(|tool| widgets::ToolRow {
                    name: tool.name.into(),
                    detail: tool.detail.as_deref().unwrap_or_default().into(),
                    ready: tool.ready,
                    busy: tool.setup.is_some() && snapshot.source.sdk_setup == tool.setup,
                    setup: match tool.setup {
                        Some(SdkSetup::DotNet10) => 1,
                        Some(SdkSetup::Msys2Ucrt64) => 2,
                        Some(SdkSetup::VisualStudio) => 3,
                        None => 0,
                    },
                })
                .collect()
        } else {
            Vec::new()
        };
        replace_rows(ui.get_tools(), tools, |rows| ui.set_tools(rows));
        let input_problem = source_build::validate_inputs(game, preference, path.as_deref())
            .err()
            .map(|error| format!("{error:#}"));
        let blocker = model::source_blocker(snapshot)
            .map(str::to_owned)
            .or(input_problem);
        ui.set_source_blocker(blocker.as_deref().unwrap_or_default().into());
        ui.set_source_ready(
            self.approved()
                && plan.is_some()
                && same_game
                && blocker.is_none()
                && snapshot.source.preferences_valid
                && snapshot.source.prerequisites.iter().all(|tool| tool.ready),
        );
        let release = match game {
            GameId::Dusklight => snapshot.latest_release.as_ref(),
            GameId::Totk => snapshot.latest_totk_release.as_ref(),
            GameId::Triforce => None,
        };
        ui.set_release_comparison(release.map_or_else(String::new, |release| {
            installed_version.map_or_else(
                || "Installed version is unrecorded; cannot determine whether this is an upgrade.".into(),
                |version| version.comparison(&release.tag, None).into(),
            )
        }).into());
        ui.set_release_tag(
            release
                .map(|release| release.tag.as_str())
                .unwrap_or_default()
                .into(),
        );
        ui.set_release_detail(
            release
                .map(|release| {
                    format!(
                        "{} · {:.1} MiB\nSHA-256: {}",
                        release.asset,
                        release.size as f64 / 1_048_576.0,
                        release.sha256
                    )
                })
                .unwrap_or_default()
                .into(),
        );
        let official_inputs = (|| -> Result<()> {
            if game == GameId::Totk {
                let directory = snapshot
                    .totk
                    .install_path
                    .as_deref()
                    .context("Waiting for the managed TotK installation location")?;
                if crate::totk_update::needs_setup(directory)? {
                    crate::totk_update::validate_setup_inputs(
                        directory,
                        &crate::totk_update::SetupInputs::from(preference),
                    )?;
                }
            }
            Ok(())
        })()
        .err()
        .map(|error| format!("{error:#}"));
        ui.set_can_install(model::can_install(snapshot, game) && official_inputs.is_none());
        ui.set_install_reason(
            if !model::can_install(snapshot, game) {
                model::source_blocker(snapshot).unwrap_or(
                    "Finish verification, active sessions or transfers before installing.",
                )
            } else {
                official_inputs.as_deref().unwrap_or_default()
            }
            .into(),
        );
        ui.set_network_name(
            snapshot
                .network
                .as_ref()
                .map(|network| network.adapter.as_str())
                .unwrap_or("Checking your private LAN…")
                .into(),
        );
        ui.set_network_detail(snapshot.network.as_ref().map(|network| format!("This PC: {}/{}\nRouter: {} · MAC {}\nOnly approve your own private home network.", network.address, network.prefix, network.gateway, network.router_mac)).unwrap_or_else(|| "Connect to your home network. Public networks are never trusted automatically.".into()).into());
        ui.set_network_trusted(model::network_trusted(snapshot));
        ui.set_network_available(snapshot.network.is_some());
        ui.set_network_fingerprint(
            snapshot
                .network
                .as_ref()
                .map(|network| network.fingerprint.as_str())
                .unwrap_or("No eligible private physical network. Public and VPN-only interfaces are not accepted.")
                .into(),
        );
        ui.set_can_pair(model::can_pair(snapshot));
        ui.set_can_reconnect(model::can_reconnect(snapshot));
        ui.set_pairing_open(snapshot.pairing_open);
        ui.set_pairing_code(
            snapshot
                .comparison_code
                .as_deref()
                .unwrap_or_default()
                .into(),
        );
        ui.set_pairing_approved(snapshot.local_approved);
        ui.set_remote_approved(snapshot.remote_approved);
        ui.set_peer_name(
            snapshot
                .settings
                .peer
                .as_ref()
                .map(|peer| peer.name.as_str())
                .or(snapshot.peer_name.as_deref())
                .unwrap_or_default()
                .into(),
        );
        ui.set_peer_key(
            snapshot
                .settings
                .peer
                .as_ref()
                .map(|peer| peer.public_key.as_str())
                .unwrap_or_default()
                .into(),
        );
        ui.set_public_key(snapshot.public_key.clone().into());
        let devices = snapshot
            .devices
            .iter()
            .map(|device| widgets::DeviceCard {
                name: device.name.clone().into(),
                detail: format!(
                    "{} · unverified discovery announcement\n{}",
                    device.address,
                    if device.pairing {
                        "Ready to compare codes"
                    } else {
                        "Open pairing on this PC too"
                    }
                )
                .into(),
                pairing: device.pairing && snapshot.pairing_open && model::can_pair(snapshot),
            })
            .collect();
        replace_rows(ui.get_devices(), devices, |rows| ui.set_devices(rows));
        let history = self.history();
        let entries = history
            .entries
            .iter()
            .map(|entry| widgets::HistoryCard {
                id: entry.id.clone().into(),
                title: format!("Snapshot #{} · {}", entry.sequence, entry.timestamp).into(),
                detail: format!(
                    "{} · {}\n{}",
                    entry.reason.label(),
                    entry.username,
                    entry.changes.join("\n")
                )
                .into(),
                metadata: format!(
                    "Game: {} / {}\nCommit: {}\nSave SHA-256: {}\nDevice: {}\nParent: {}\nPartner branch: {}\nRestored from: {}",
                    entry.game,
                    entry.name,
                    entry.id,
                    entry.hash,
                    entry.device,
                    entry.parent.as_deref().unwrap_or("None"),
                    entry.merge_parent.as_deref().unwrap_or("None"),
                    entry.restored_from.as_deref().unwrap_or("None"),
                )
                .into(),
                ancestry: if entry.merge_parent.is_some() {
                    "Reconciled save · both parent branches retained"
                } else if entry.reason == crate::history::Reason::PeerBranch {
                    "Preserved partner branch"
                } else {
                    "Local save branch"
                }
                .into(),
                branch: if entry.merge_parent.is_some() {
                    2
                } else if entry.reason == crate::history::Reason::PeerBranch {
                    1
                } else {
                    0
                },
                current: model::local_hash(snapshot, game) == Some(entry.hash.as_str()),
            })
            .collect();
        replace_rows(ui.get_history(), entries, |rows| ui.set_history(rows));
        ui.set_history_more(history.next.is_some());
        ui.set_history_info(
            format!(
                "{} snapshots · {:.1} MiB stored · {:.0} MiB managed budget. {}",
                history.stats.commits,
                history.stats.stored_bytes as f64 / 1_048_576.0,
                history.stats.budget as f64 / 1_048_576.0,
                history.notice.as_deref().unwrap_or_default()
            )
            .into(),
        );
        ui.set_history_usage(
            (history.stats.stored_bytes as f32 / history.stats.budget.max(1) as f32)
                .clamp(0.0, 1.0),
        );
        ui.set_history_storage(
            format!(
                "{} shared blocks · {:.2} MiB as full copies · {} oldest snapshots pruned.",
                history.stats.blocks,
                history.stats.logical_bytes as f64 / 1_048_576.0,
                history.stats.pruned
            )
            .into(),
        );
        let preview = snapshot
            .history_preview
            .as_ref()
            .filter(|preview| preview.game == game);
        let preview_cards = preview
            .map(|preview| match &preview.data {
                HistoryPreviewData::Dusklight(summary) => model::dusklight_cards(summary),
                HistoryPreviewData::Totk(summary) | HistoryPreviewData::Triforce(summary) => {
                    model::native_cards(summary)
                }
            })
            .unwrap_or_default();
        replace_rows(ui.get_preview_saves(), preview_cards, |rows| {
            ui.set_preview_saves(rows)
        });
        ui.set_preview_id(
            preview
                .map(|preview| preview.id.as_str())
                .unwrap_or_default()
                .into(),
        );
        ui.set_preview_title(
            preview
                .map(|_| "Snapshot preview · read only")
                .unwrap_or_default()
                .into(),
        );
        ui.set_startup(snapshot.settings.startup);
        ui.set_auto_updates(snapshot.self_updates.enabled);
        ui.set_app_update(
            snapshot
                .self_updates
                .error
                .as_deref()
                .unwrap_or(&snapshot.self_updates.message)
                .into(),
        );
        ui.set_can_update_app(
            snapshot.self_updates.available() && model::app_update_blocker(snapshot).is_none(),
        );
        ui.set_can_check_app(!snapshot.self_updates.checking && !snapshot.self_updates.preparing);
        ui.set_has_app_release(snapshot.self_updates.latest.is_some());
        ui.set_app_release_detail(
            snapshot
                .self_updates
                .latest
                .as_ref()
                .map(|release| {
                    format!(
                        "{} · {:.1} MiB\nSHA-256: {}\nLast checked: {}",
                        release.tag,
                        release.size as f64 / 1_048_576.0,
                        release.sha256,
                        snapshot
                            .self_updates
                            .checked_at
                            .as_deref()
                            .unwrap_or("Not yet checked")
                    )
                })
                .unwrap_or_default()
                .into(),
        );
        ui.set_app_update_reason(
            model::app_update_blocker(snapshot)
                .unwrap_or_default()
                .into(),
        );
        ui.set_disc_sharing(snapshot.settings.iso_enabled);
        let input_kind = if game == GameId::Triforce {
            GameFileKind::OotNup
        } else {
            GameFileKind::TotkBase
        };
        ui.set_game_file_sharing(snapshot.game_file_permissions.allows(input_kind));
        ui.set_key_sharing(snapshot.game_file_permissions.keys);
        ui.set_can_offer_game_file(model::can_send_game_file(snapshot, input_kind));
        ui.set_can_offer_update(model::can_send_game_file(
            snapshot,
            GameFileKind::TotkUpdate,
        ));
        ui.set_can_offer_keys(model::can_send_game_file(snapshot, GameFileKind::TotkKeys));
        ui.set_game_file_transfer_reason(
            if !snapshot.ready {
                "Connect your paired PC in Devices first."
            } else if !snapshot.game_files_supported {
                "Update Twilight Sync on both PCs for game-file and console-key transfers."
            } else if !model::management_available(snapshot)
                || model::native(snapshot, game).is_some_and(|state| {
                    state.running
                        || state.remote_running
                        || state.busy
                        || state.updating
                        || state.launch_pending
                        || state.recovery_pending
                })
                || snapshot.pending_save
                || snapshot.pending_iso
                || snapshot.pending_recovery
                || !snapshot.transfer.is_empty()
            {
                "Finish active play, transfers or recovery before offering another input."
            } else {
                ""
            }
            .into(),
        );
        ui.set_pending_iso(snapshot.pending_iso);
        ui.set_can_send_iso(model::can_send_disc(snapshot));
        ui.set_can_accept_iso(model::can_accept_disc(snapshot));
        ui.set_disc_offer(
            snapshot
                .pending_disc_format
                .map(|format| match format {
                    TransferFormat::Game(GameFileKind::TotkKeys) => "Incoming console keys (prod.keys). Sensitive: accept only from your own trusted paired PC. Choose a new destination; existing files are never overwritten.".into(),
                    _ => format!("Incoming {}. Confirm you own this game, then choose a new destination. Existing files are never overwritten.", format.label()),
                })
                .unwrap_or_else(|| "Image format missing; decline and request a new offer.".into())
                .into(),
        );
        ui.set_iso_path(
            snapshot
                .settings
                .iso_file
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "No disc image selected".into())
                .into(),
        );
        ui.set_texture_status(snapshot.textures.notice.clone().into());
        ui.set_texture_reason(model::texture_blocker(snapshot).unwrap_or_default().into());
        ui.set_texture_busy(snapshot.texture_busy);
        ui.set_texture_cancellable(snapshot.textures.cancellable);
        ui.set_texture_recovery(snapshot.textures.recovery_needed);
        ui.set_texture_consent(self.texture_consent);
        ui.set_can_import_textures(model::can_import_textures(snapshot));
        ui.set_can_change_textures(model::can_change_textures(snapshot));
        ui.set_textures_active(snapshot.textures.active.is_some());
        ui.set_textures_unmanaged(snapshot.textures.unmanaged);
        ui.set_texture_profile(
            snapshot
                .settings
                .texture_profile
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        let packs = snapshot
            .textures
            .packs
            .iter()
            .map(|pack| widgets::PackCard {
                title: format!(
                    "{} · {}",
                    textures::CATALOG
                        .iter()
                        .find(|creator| creator.id == pack.source)
                        .map_or(pack.source.as_str(), |creator| creator.name),
                    pack.version
                )
                .into(),
                detail: format!(
                    "{} textures · {:.1} MiB",
                    pack.files,
                    pack.bytes as f64 / 1_048_576.0
                )
                .into(),
                metadata: format!(
                    "Imported: {}\nContent ID: {}\nSkipped entries: {}",
                    pack.imported, pack.id, pack.skipped
                )
                .into(),
                active: snapshot.textures.active.as_deref() == Some(pack.id.as_str()),
            })
            .collect();
        replace_rows(ui.get_packs(), packs, |rows| ui.set_packs(rows));
        self.tray
            .launch_item
            .set_text(format!("Play {}", model::title(game)));
        self.tray
            .sync_item
            .set_text(format!("Sync {} progress", model::title(game)));
        self.tray
            .launch_item
            .set_enabled(model::play_blocker(snapshot, game).is_none());
        self.tray
            .sync_item
            .set_enabled(model::sync_blocker(snapshot, game).is_none());
        Ok(())
    }

    fn tick(&mut self) -> Result<Option<Interaction>> {
        let ui = self.ui.upgrade().context("The window has closed")?;
        let snapshot = self.backend.snapshot()?;
        if !self.demo {
            self.snapshot = snapshot;
        } else if snapshot.shutdown {
            self.snapshot.shutdown = true;
        }
        if self.background.is_none() {
            let handle = ui.window().window_handle();
            match handle.window_handle() {
                Ok(handle) => {
                    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                        bail!("The dashboard requires a Win32 window");
                    };
                    self.background = Some(background::BackgroundGuard::install(
                        handle.hwnd,
                        ui.as_weak(),
                        self.backend.clone(),
                        self.tray.clone(),
                    )?);
                    if self.start_hidden {
                        ui.hide()?;
                        self.start_hidden = false;
                    }
                }
                Err(error) if self.started.elapsed() > Duration::from_secs(5) => {
                    return Err(error).context("Creating the native dashboard window");
                }
                Err(_) => {}
            }
        }
        if let Some(background) = &self.background
            && let Some(game) = background.take_open_game_controls()
        {
            self.select_game(model::index(game))?;
            ui.invoke_navigate(0);
        }
        let mut interaction = None;
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.tray.show {
                if let Some(background) = &self.background {
                    background.reveal();
                }
            } else if event.id == self.tray.launch {
                interaction = Some(Interaction::Action(Action::Play, 0));
            } else if event.id == self.tray.sync {
                interaction = Some(Interaction::Action(Action::SyncSave, 0));
            } else if event.id == self.tray.startup.id().clone() {
                self.dispatch(Command::SetStartup(self.tray.startup.is_checked()))?;
            } else if event.id == self.tray.automatic_updates.id().clone() {
                self.dispatch(Command::SetAutoUpdateChecks(
                    self.tray.automatic_updates.is_checked(),
                ))?;
            } else if event.id == self.tray.check_updates.id().clone() {
                interaction = Some(Interaction::Action(Action::CheckAppUpdate, 0));
            } else if event.id == self.tray.update_app.id().clone() {
                interaction = Some(Interaction::Action(Action::UpdateApp, 0));
            } else if event.id == self.tray.quit {
                interaction = Some(Interaction::Action(Action::Quit, 0));
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(event, TrayIconEvent::DoubleClick { .. })
                && let Some(background) = &self.background
            {
                background.reveal();
            }
        }
        if self.snapshot.shutdown {
            slint::quit_event_loop()?;
            return Ok(None);
        }
        self.refresh_view()?;
        Ok(interaction)
    }
}

pub fn open_dashboard() -> Result<bool> {
    background::open_dashboard(WINDOW_TITLE)
}

pub fn open_dusklight_controls() -> Result<bool> {
    background::open_game_controls(WINDOW_TITLE)
}
pub fn open_totk_controls() -> Result<bool> {
    background::open_selected_game(WINDOW_TITLE, GameId::Totk)
}
pub fn open_triforce_controls() -> Result<bool> {
    background::open_selected_game(WINDOW_TITLE, GameId::Triforce)
}
pub fn run(backend: Backend, background: bool) -> Result<()> {
    run_window(backend, background, None, 0, None, 1.0)
}
pub fn run_totk(backend: Backend) -> Result<()> {
    run_window(backend, false, Some(GameId::Totk), 0, None, 1.0)
}
pub fn run_triforce(backend: Backend) -> Result<()> {
    run_window(backend, false, Some(GameId::Triforce), 0, None, 1.0)
}

#[cfg(debug_assertions)]
pub fn run_smoke(
    backend: Backend,
    background: bool,
    page: &str,
    theme: &str,
    scale: f32,
) -> Result<()> {
    let section = match page {
        "overview" | "totk" | "triforce" => 0,
        "library" => 1,
        "devices" => 2,
        "history" => 3,
        "textures" => 4,
        "security" => 5,
        _ => bail!("Unknown preview page"),
    };
    ensure!(
        scale.is_finite() && (0.75..=2.0).contains(&scale),
        "Preview scale must be between 0.75 and 2"
    );
    let game = match page {
        "totk" => Some(GameId::Totk),
        "triforce" => Some(GameId::Triforce),
        _ => Some(GameId::Dusklight),
    };
    run_window(
        backend,
        background,
        game,
        section,
        Some(appearance::Store::preview(theme)?),
        scale,
    )
}

fn run_window(
    backend: Backend,
    hidden: bool,
    initial_game: Option<GameId>,
    section: i32,
    preview: Option<appearance::Store>,
    scale: f32,
) -> Result<()> {
    let demo = preview.is_some();
    let root = crate::config::root_path()?;
    let appearance = match preview {
        Some(preview) => preview,
        None => appearance::Store::load(&root)?,
    };
    let game_path = (!demo).then(|| root.join("game-view.json"));
    let game = match initial_game {
        Some(game) => game,
        None => match game_path.as_ref() {
            Some(path) if path.try_exists()? => {
                crate::files::ensure_safe_path(path)?;
                ensure!(
                    path.metadata()?.len() <= 4096,
                    "Game-view preferences exceed their size limit"
                );
                serde_json::from_slice(&std::fs::read(path)?)
                    .context("Reading the selected game")?
            }
            _ => GameId::Dusklight,
        },
    };
    if demo {
        icons::set_preview_identity()?;
    } else {
        icons::set_application_identity()?;
    }
    let snapshot = backend.snapshot()?;
    let tray = make_tray(&snapshot)?;
    let ui = MainWindow::new()?;
    if demo {
        ui.set_window_title("Twilight Sync - UI smoke".into());
    }
    ui.set_demo_mode(demo);
    ui.set_section(section);
    ui.set_selected_game(model::index(game));
    ui.set_texture_creators(rows(
        textures::CATALOG
            .iter()
            .map(|entry| widgets::TextureCreator {
                name: entry.name.into(),
                description: entry.description.into(),
                version: entry
                    .suggested_version
                    .map_or_else(
                        || format!("Imported {}", time::OffsetDateTime::now_utc().date()),
                        str::to_owned,
                    )
                    .into(),
                has_site: entry.creator_url.is_some(),
            })
            .collect(),
    ));
    if let Some(creator) = ui.get_texture_creators().row_data(0) {
        ui.set_texture_version(creator.version);
    }
    ui.window()
        .set_size(slint::LogicalSize::new(1200.0 / scale, 820.0 / scale));
    let mut controller = Controller {
        ui: ui.as_weak(),
        backend,
        snapshot,
        tray,
        background: None,
        appearance,
        native_appearance: None,
        game,
        game_path,
        approval: None,
        texture_consent: false,
        local_error: None,
        demo,
        started: Instant::now(),
        start_hidden: hidden,
        preview_directory: if demo {
            Some(tempfile::tempdir()?)
        } else {
            None
        },
    };
    controller.refresh_view()?;
    let controller = Rc::new(Dispatcher {
        controller: RefCell::new(Some(controller)),
        ui: ui.as_weak(),
    });
    let weak = Rc::downgrade(&controller);
    ui.on_action(move |action, index| {
        if let Some(controller) = weak.upgrade() {
            controller.run(Interaction::Action(action, index));
        }
    });
    let weak = Rc::downgrade(&controller);
    ui.on_toggle(move |toggle, value| {
        if let Some(controller) = weak.upgrade() {
            controller.run(Interaction::Toggle(toggle, value));
        }
    });
    let weak = Rc::downgrade(&controller);
    ui.on_game_selected(move |index| {
        if let Some(controller) = weak.upgrade() {
            controller.run(Interaction::SelectGame(index));
        }
    });
    let weak = Rc::downgrade(&controller);
    ui.on_choose_input(move |kind| {
        if let Some(controller) = weak.upgrade() {
            controller.run(Interaction::ChooseInput(kind));
        }
    });
    let weak = Rc::downgrade(&controller);
    ui.on_clear_input(move |kind| {
        if let Some(controller) = weak.upgrade() {
            controller.run(Interaction::ClearInput(kind));
        }
    });
    let weak = Rc::downgrade(&controller);
    ui.on_appearance_selected(move |mode| {
        if let Some(controller) = weak.upgrade() {
            controller.run(Interaction::Appearance(mode));
        }
    });
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_confirmation_open()
        {
            ui.invoke_confirmation_response(false);
            return slint::CloseRequestResponse::KeepWindowShown;
        }
        if demo && let Err(error) = slint::quit_event_loop() {
            eprintln!("Cannot close the UI preview: {error}");
        }
        slint::CloseRequestResponse::HideWindow
    });
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            controller.tick();
        },
    );
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    timer.stop();
    Ok(())
}

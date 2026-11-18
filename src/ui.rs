use crate::protocol::GameId;
use crate::runtime::{Backend, Command, NativeCommand, Snapshot};
use anyhow::{Context, Result};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

const WINDOW_TITLE: &str = "Twilight Sync";
const TOTK_SETUP_LABEL: &str = "Download official TotK Recomp setup";

mod background;
mod design;
mod game_view;
mod icons;
mod saves;

use design::{Appearance, AppearanceStore, Palette, semibold, symbol};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Page {
    Overview,
    Devices,
    Library,
    Totk,
    Triforce,
    Textures,
    History,
    Security,
}

impl Page {
    const ALL: [Self; 6] = [
        Self::Overview,
        Self::Devices,
        Self::Library,
        Self::Textures,
        Self::History,
        Self::Security,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Home",
            Self::Devices => "Devices",
            Self::Library => "Games",
            Self::Totk => "Tears of the Kingdom",
            Self::Triforce => "Ocarina of Time",
            Self::Textures => "Texture packs",
            Self::History => "Save history",
            Self::Security => "Settings",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Overview => "Your save progress and game sessions.",
            Self::Devices => "Approve your network and pair one trusted device.",
            Self::Library => "",
            Self::Totk => "",
            Self::Triforce => "",
            Self::Textures => "Import authorized texture packs and choose the active version.",
            Self::History => "Save branches, progress changes, and recovery.",
            Self::Security => "Appearance, startup, application updates, and privacy.",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Overview => "\u{e80f}",
            Self::Devices => "\u{e772}",
            Self::Library => "\u{e7fc}",
            Self::Totk => "\u{e7fc}",
            Self::Triforce => "\u{e7fc}",
            Self::Textures => "\u{e91b}",
            Self::History => "\u{e81c}",
            Self::Security => "\u{e713}",
        }
    }
}

#[derive(Clone, Copy)]
enum Tone {
    Info,
    Warning,
    Error,
}

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

struct Dashboard {
    backend: Backend,
    tray: Tray,
    page: Page,
    game: GameId,
    game_view_path: Option<PathBuf>,
    local_error: Option<String>,
    events: crossbeam_channel::Receiver<MenuEvent>,
    background: background::BackgroundGuard,
    start_hidden: bool,
    texture_source: String,
    texture_version: String,
    texture_permission: bool,
    appearance: AppearanceStore,
    shown_pairing_code: Option<String>,
    shown_fork: Option<(String, String, bool)>,
    fork_dialog_open: bool,
    fork_choice: Option<bool>,
    source_approved: bool,
    source_reviewed: Option<(GameId, String)>,
}

fn make_tray(startup: bool, automatic_updates: bool) -> Result<Tray> {
    let pixels = icons::tray()?;
    let menu = Menu::new();
    let status = MenuItem::new("Checking private LAN...", false, None);
    let show = MenuItem::new("Open Twilight Sync", true, None);
    let sync = MenuItem::new("Send save to partner", true, None);
    let launch = MenuItem::new("Play Dusklight", true, None);
    let startup = CheckMenuItem::new("Start with Windows", true, startup, None);
    let automatic_updates = CheckMenuItem::new(
        "Automatically check app updates",
        true,
        automatic_updates,
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
        .with_tooltip("Twilight Sync - checking private LAN")
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

pub fn run(backend: Backend, background: bool) -> Result<()> {
    let appearance = AppearanceStore::load(&crate::config::root_directory()?)?;
    run_page(
        backend,
        background,
        Page::Overview,
        WINDOW_TITLE,
        appearance,
        1.0,
    )
}

pub fn open_dusklight_controls() -> Result<bool> {
    background::open_game_controls(WINDOW_TITLE)
}

pub fn open_totk_controls() -> Result<bool> {
    background::open_selected_game(WINDOW_TITLE, GameId::Totk)
}

pub fn run_totk(backend: Backend) -> Result<()> {
    let appearance = AppearanceStore::load(&crate::config::root_directory()?)?;
    run_page(backend, false, Page::Totk, WINDOW_TITLE, appearance, 1.0)
}

pub fn open_triforce_controls() -> Result<bool> {
    background::open_selected_game(WINDOW_TITLE, GameId::Triforce)
}

pub fn run_triforce(backend: Backend) -> Result<()> {
    let appearance = AppearanceStore::load(&crate::config::root_directory()?)?;
    run_page(
        backend,
        false,
        Page::Triforce,
        WINDOW_TITLE,
        appearance,
        1.0,
    )
}

#[cfg(debug_assertions)]
pub fn run_smoke(
    backend: Backend,
    background: bool,
    page: &str,
    theme: &str,
    scale: f32,
) -> Result<()> {
    let page = match page {
        "overview" => Page::Overview,
        "devices" => Page::Devices,
        "library" => Page::Library,
        "totk" => Page::Totk,
        "triforce" => Page::Triforce,
        "textures" => Page::Textures,
        "history" => Page::History,
        "security" => Page::Security,
        _ => anyhow::bail!("Unknown UI smoke page"),
    };
    anyhow::ensure!(
        scale.is_finite() && (0.75..=2.0).contains(&scale),
        "UI smoke scale must be between 0.75 and 2"
    );
    run_page(
        backend,
        background,
        page,
        "Twilight Sync - UI smoke",
        AppearanceStore::preview(Appearance::parse(theme)?),
        scale,
    )
}

fn run_page(
    backend: Backend,
    background: bool,
    initial_page: Page,
    title: &str,
    appearance: AppearanceStore,
    scale: f32,
) -> Result<()> {
    let game_view_path = if title == WINDOW_TITLE {
        Some(crate::config::root_path()?.join("game-view.json"))
    } else {
        None
    };
    let initial_game = if initial_page == Page::Triforce {
        GameId::Triforce
    } else if initial_page == Page::Totk {
        GameId::Totk
    } else {
        game_view_path
            .as_ref()
            .and_then(|path| {
                crate::files::ensure_safe_path(path).ok()?;
                let metadata = std::fs::metadata(path).ok()?;
                if metadata.len() > 4096 {
                    return None;
                }
                serde_json::from_slice(&std::fs::read(path).ok()?).ok()
            })
            .unwrap_or(GameId::Dusklight)
    };
    let snapshot = backend.snapshot()?;
    icons::set_application_identity()?;
    let tray = make_tray(snapshot.settings.startup, snapshot.self_updates.enabled)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 760.0])
            .with_min_inner_size([640.0, 460.0])
            .with_visible(!background)
            .with_icon(Arc::new(icons::window()?)),
        ..Default::default()
    };
    eframe::run_native(
        title,
        options,
        Box::new(move |context| {
            let RawWindowHandle::Win32(window) = context.window_handle()?.as_raw() else {
                return Err("The dashboard requires a Win32 window".into());
            };
            let window = window.hwnd;
            design::install(&context.egui_ctx)?;
            appearance.appearance.apply(&context.egui_ctx);
            context.egui_ctx.set_zoom_factor(scale);
            let (events_sender, events) = crossbeam_channel::bounded(32);
            let menu_context = context.egui_ctx.clone();
            MenuEvent::set_event_handler(Some(move |event| {
                if let Err(error) = events_sender.try_send(event) {
                    eprintln!("Twilight Sync tray event queue: {error}");
                }
                background::reveal(window, &menu_context);
                menu_context.request_repaint();
            }));
            let tray_context = context.egui_ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |event| {
                if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                    background::reveal(window, &tray_context);
                    tray_context.request_repaint();
                }
            }));
            let background_guard = background::BackgroundGuard::install(
                window,
                context.egui_ctx.clone(),
                backend.clone(),
                tray.clone(),
            )?;
            Ok(Box::new(Dashboard {
                backend,
                tray,
                page: if matches!(initial_page, Page::Totk | Page::Triforce) {
                    Page::Overview
                } else {
                    initial_page
                },
                game: initial_game,
                game_view_path,
                local_error: None,
                events,
                background: background_guard,
                start_hidden: background,
                texture_source: "henriko-4k".into(),
                texture_version: format!("Imported {}", time::OffsetDateTime::now_utc().date()),
                texture_permission: false,
                appearance,
                shown_pairing_code: None,
                shown_fork: None,
                fork_dialog_open: false,
                fork_choice: None,
                source_approved: false,
                source_reviewed: None,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("Native dashboard failed: {e}"))
}

impl Dashboard {
    fn dispatch(&mut self, command: Command) {
        self.local_error = self
            .backend
            .command(command)
            .err()
            .map(|e| format!("{e:#}"));
    }

    fn tray_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if event.id == self.tray.show {
                self.background.reveal();
            } else if event.id == self.tray.sync {
                self.dispatch(match self.game {
                    GameId::Dusklight => Command::SendSave,
                    game => Command::Native {
                        game,
                        command: NativeCommand::Send,
                    },
                });
            } else if event.id == self.tray.launch {
                self.dispatch(match self.game {
                    GameId::Dusklight => Command::Launch,
                    game => Command::Native {
                        game,
                        command: NativeCommand::Launch { local_only: false },
                    },
                });
            } else if event.id == *self.tray.startup.id() {
                self.dispatch(Command::SetStartup(self.tray.startup.is_checked()));
            } else if event.id == *self.tray.automatic_updates.id() {
                self.dispatch(Command::SetAutoUpdateChecks(
                    self.tray.automatic_updates.is_checked(),
                ));
            } else if event.id == *self.tray.check_updates.id() {
                self.dispatch(Command::CheckSelfUpdate);
            } else if event.id == *self.tray.update_app.id() {
                self.confirm_app_update();
            } else if event.id == self.tray.quit {
                self.dispatch(Command::Shutdown);
            }
        }
    }

    fn confirm_app_update(&mut self) {
        let snapshot = match self.backend.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.local_error = Some(format!("{error:#}"));
                return;
            }
        };
        let Some(release) = snapshot
            .self_updates
            .latest
            .filter(|release| release.update_available)
        else {
            self.local_error = Some("Check for a newer Twilight Sync release first.".into());
            return;
        };
        let approved = rfd::MessageDialog::new().set_title("Update Twilight Sync")
            .set_description(format!("Download and install Twilight Sync {} ({:.1} MiB)?\n\nBoth games and all transfers must be idle. The app will verify SHA-256, shut down cleanly, update its executable, and restart in the tray. Saves, pairing, snapshots, textures, and preferences are preserved.\n\nNo update is installed without your approval.", release.tag, release.size as f64 / 1_048_576.0))
            .set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes;
        if approved {
            self.dispatch(Command::UpdateSelf);
        }
    }

    fn app_updates(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let p = Palette::of(ui);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Twilight Sync updates");
            Self::status(
                ui,
                "Installed",
                concat!("v", env!("CARGO_PKG_VERSION")),
                p.text,
            );
            Self::secondary(ui, &snapshot.self_updates.message);
            if let Some(error) = &snapshot.self_updates.error {
                Self::notice(ui, Tone::Warning, "Update check needs attention", error);
            }
            if let Some(release) = &snapshot.self_updates.latest {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("Latest release: {}", release.tag)).color(
                            if release.update_available {
                                p.accent
                            } else {
                                p.success
                            },
                        ),
                    );
                    ui.hyperlink_to("Release notes", &release.notes_url);
                });
            }
            if let Some(checked) = &snapshot.self_updates.checked_at {
                ui.label(
                    RichText::new(format!("Last check: {checked}"))
                        .small()
                        .color(p.secondary),
                );
            }
            let mut enabled = snapshot.self_updates.enabled;
            if ui
                .add_enabled(
                    !snapshot.self_updates.preparing,
                    egui::Checkbox::new(
                        &mut enabled,
                        "Check automatically at startup and every six hours",
                    ),
                )
                .changed()
            {
                self.dispatch(Command::SetAutoUpdateChecks(enabled));
            }
            ui.horizontal_wrapped(|ui| {
                if snapshot.self_updates.checking || snapshot.self_updates.preparing {
                    ui.spinner();
                }
                if ui
                    .add_enabled(
                        !snapshot.self_updates.checking && !snapshot.self_updates.preparing,
                        egui::Button::new("Check for updates"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::CheckSelfUpdate);
                }
                let safe = Self::update_blocker(snapshot).is_none();
                if Self::primary(
                    ui,
                    safe && snapshot.self_updates.available(),
                    "Update Twilight Sync...",
                )
                .clicked()
                {
                    self.confirm_app_update();
                }
            });
            if snapshot.self_updates.available()
                && let Some(reason) = Self::update_blocker(snapshot)
            {
                Self::notice(ui, Tone::Info, "Update waiting", reason);
            }
            Self::secondary(
                ui,
                "Automatic checks only read version metadata. Downloading and restarting always require confirmation.",
            );
        });
    }

    fn header(ui: &mut egui::Ui, page: Page) {
        ui.heading(page.title());
        ui.label(RichText::new(page.description()).color(Palette::of(ui).secondary));
        ui.add_space(12.0);
    }

    fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
        let colors = Palette::of(ui);
        egui::Frame::new()
            .fill(colors.surface)
            .corner_radius(6.0)
            .inner_margin(16.0)
            .stroke(Stroke::new(1.0_f32, colors.border))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                content(ui);
            });
    }

    fn status(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
        ui.scope(|ui| {
            ui.spacing_mut().interact_size.y = 18.0;
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(label).color(Palette::of(ui).secondary));
                ui.label(RichText::new(value).color(color).strong());
            });
        });
    }

    fn section_title(ui: &mut egui::Ui, title: &str) {
        ui.label(RichText::new(title).font(semibold(17.0)));
    }

    fn secondary(ui: &mut egui::Ui, text: impl Into<String>) {
        ui.label(RichText::new(text).color(Palette::of(ui).secondary));
    }

    fn notice(ui: &mut egui::Ui, tone: Tone, title: &str, body: &str) {
        Self::notice_with_link(ui, tone, title, body, None);
    }

    fn notice_with_link(
        ui: &mut egui::Ui,
        tone: Tone,
        title: &str,
        body: &str,
        link: Option<(&str, &str)>,
    ) {
        let p = Palette::of(ui);
        let (foreground, background, glyph) = match tone {
            Tone::Info => (p.accent, p.info_background, "\u{e946}"),
            Tone::Warning => (p.warning, p.warning_background, "\u{e7ba}"),
            Tone::Error => (p.error, p.error_background, "\u{ea39}"),
        };
        egui::Frame::new()
            .fill(background)
            .corner_radius(4)
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_top(|ui| {
                    ui.label(symbol(glyph, 18.0).color(foreground));
                    ui.vertical(|ui| {
                        ui.label(RichText::new(title).font(semibold(14.0)).color(foreground));
                        if !body.is_empty() {
                            ui.label(RichText::new(body).color(p.text));
                        }
                        if let Some((label, url)) = link {
                            ui.hyperlink_to(label, url);
                        }
                    });
                });
            });
    }

    fn action_needed(ui: &mut egui::Ui, game: GameId, snapshot: &Snapshot, error: &str) -> bool {
        Self::notice(ui, Tone::Error, "Action needed", error);
        if game == GameId::Totk && !snapshot.totk.installed {
            Self::secondary(
                ui,
                "Select your local base game, update and keys in Games; Twilight Sync handles the official installation.",
            );
            ui.button("Set up TotK automatically").clicked()
        } else {
            false
        }
    }

    fn primary(ui: &mut egui::Ui, enabled: bool, title: &str) -> egui::Response {
        let p = Palette::of(ui);
        ui.scope(|ui| {
            ui.visuals_mut().disabled_alpha = 1.0;
            ui.add_enabled(
                enabled,
                egui::Button::new(RichText::new(title).color(if enabled {
                    p.on_accent
                } else {
                    p.secondary
                }))
                .fill(if enabled { p.accent } else { p.disabled })
                .stroke(Stroke::new(
                    1.0_f32,
                    if enabled { p.accent } else { p.border },
                )),
            )
        })
        .inner
    }

    fn path(ui: &mut egui::Ui, path: &std::path::Path) {
        let text = path.display().to_string();
        let width = ui.available_width();
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new((width - 42.0).max(40.0), 30.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&text)
                                .monospace()
                                .color(Palette::of(ui).secondary),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&text);
                },
            );
            ui.scope(|ui| {
                ui.spacing_mut().button_padding = Vec2::splat(5.0);
                if ui
                    .add_sized([30.0, 30.0], egui::Button::new(symbol("\u{e8c8}", 14.0)))
                    .on_hover_text("Copy path")
                    .clicked()
                {
                    ui.ctx().copy_text(text.clone());
                }
            });
        });
    }

    fn update_blocker(snapshot: &Snapshot) -> Option<&'static str> {
        if snapshot.game_running
            || snapshot.remote_game
            || snapshot.totk.running
            || snapshot.totk.remote_running
        {
            Some("Close the running game on both PCs before updating Twilight Sync.")
        } else if snapshot.self_updates.checking {
            Some("The release check is still running.")
        } else if snapshot.self_updates.preparing || snapshot.install_busy || snapshot.texture_busy
        {
            Some("Finish the current installation or update first.")
        } else if snapshot.pending_save
            || snapshot.pending_iso
            || snapshot.pending_recovery
            || !snapshot.transfer.is_empty()
            || snapshot.totk.busy
            || snapshot.totk.launch_pending
            || snapshot.totk.recovery_pending
        {
            Some("Finish or decline the pending save, transfer, launch, or recovery first.")
        } else {
            None
        }
    }

    fn management_available(snapshot: &Snapshot) -> bool {
        !snapshot.game_running
            && !snapshot.remote_game
            && snapshot.settings.play_lock.is_none()
            && !snapshot.install_busy
            && !snapshot.texture_busy
            && !snapshot.self_updates.preparing
    }

    fn can_launch(snapshot: &Snapshot) -> bool {
        Self::local_play_available(snapshot, false)
    }

    fn local_play_available(snapshot: &Snapshot, allow_pending_save: bool) -> bool {
        Self::management_available(snapshot)
            && !snapshot.textures.recovery_needed
            && snapshot.settings.save_file.is_some()
            && snapshot.save_profile.is_some()
            && snapshot.save_ready
            && snapshot.settings.game_exe.is_some()
            && (allow_pending_save || !snapshot.pending_save)
            && !snapshot.pending_iso
            && !snapshot.pending_recovery
            && snapshot.transfer.is_empty()
    }

    fn start_play(&mut self, snapshot: &Snapshot) {
        if snapshot.conflict.is_some() {
            self.fork_dialog_open = true;
        } else {
            self.dispatch(Command::Launch);
        }
    }

    fn can_reconcile(snapshot: &Snapshot) -> bool {
        snapshot.ready
            && Self::management_available(snapshot)
            && snapshot.save_ready
            && snapshot.save_profile.is_some()
            && snapshot.settings.save_file.is_some()
            && !snapshot.pending_iso
            && !snapshot.pending_recovery
            && snapshot.transfer.is_empty()
            && snapshot
                .conflict
                .as_ref()
                .and_then(|conflict| conflict.remote.as_ref())
                .zip(snapshot.local_save.as_ref())
                .is_some_and(|(remote, local)| {
                    remote.game == local.game && remote.file_name == local.file_name
                })
    }

    fn reconcile_prompt(&mut self, context: &egui::Context, snapshot: &Snapshot) {
        let model = game_view::view(self.game, snapshot);
        if !model.conflict {
            self.shown_fork = None;
            self.fork_dialog_open = false;
            self.fork_choice = None;
            return;
        }
        let (Some(local_hash), Some(remote_hash)) = (&model.local_hash, &model.remote_hash) else {
            return;
        };
        let fingerprint = (local_hash.clone(), remote_hash.clone(), model.pending);
        if self.shown_fork.as_ref() != Some(&fingerprint) && !model.running && !model.remote_running
        {
            self.shown_fork = Some(fingerprint);
            self.fork_dialog_open = true;
            self.fork_choice = None;
        }
        if !self.fork_dialog_open {
            return;
        }
        let (close, action) = game_view::prompt(context, &model, &mut self.fork_choice);
        if close {
            self.fork_dialog_open = false;
        }
        if let Some(action) = action {
            let command = match (self.game, action) {
                (GameId::Dusklight, game_view::CompareAction::Use(use_remote)) => {
                    Command::ResolveConflict {
                        local_hash: local_hash.clone(),
                        remote_hash: remote_hash.clone(),
                        use_remote,
                    }
                }
                (GameId::Totk, game_view::CompareAction::Use(use_remote)) => Command::ResolveTotk {
                    local_hash: local_hash.clone(),
                    remote_hash: remote_hash.clone(),
                    use_remote,
                },
                (GameId::Triforce, game_view::CompareAction::Use(use_remote)) => Command::Native {
                    game: GameId::Triforce,
                    command: NativeCommand::Resolve {
                        local_hash: local_hash.clone(),
                        remote_hash: remote_hash.clone(),
                        use_remote,
                    },
                },
                (GameId::Dusklight, game_view::CompareAction::Continue) => Command::ContinueLocal {
                    expected: Some(local_hash.clone()),
                },
                (GameId::Totk, game_view::CompareAction::Continue) => Command::ContinueTotk {
                    expected: Some(local_hash.clone()),
                },
                (GameId::Triforce, game_view::CompareAction::Continue) => Command::Native {
                    game: GameId::Triforce,
                    command: NativeCommand::Continue {
                        expected: Some(local_hash.clone()),
                    },
                },
            };
            self.dispatch(command);
        }
    }

    fn setting_row(
        ui: &mut egui::Ui,
        title: &str,
        description: &str,
        controls: impl FnOnce(&mut egui::Ui),
    ) {
        let width = ui.available_width();
        if width >= 520.0 {
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new((width - 240.0).max(260.0), 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.label(RichText::new(title).font(semibold(14.0)));
                        Self::secondary(ui, description);
                    },
                );
                ui.allocate_ui_with_layout(
                    Vec2::new(220.0, 34.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    controls,
                );
            });
        } else {
            ui.label(RichText::new(title).font(semibold(14.0)));
            Self::secondary(ui, description);
            ui.horizontal_wrapped(controls);
        }
    }

    fn game_picker(ui: &mut egui::Ui, game: &mut GameId) -> bool {
        let before = *game;
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(game, GameId::Dusklight, "Dusklight");
            ui.selectable_value(game, GameId::Totk, "Tears of the Kingdom");
            ui.selectable_value(game, GameId::Triforce, "Ocarina of Time");
        });
        *game != before
    }

    fn game_action(&mut self, snapshot: &Snapshot, game: GameId, action: game_view::Action) {
        match action {
            game_view::Action::Play => match game {
                GameId::Dusklight => self.start_play(snapshot),
                GameId::Totk => self.dispatch(Command::LaunchTotk { local_only: false }),
                GameId::Triforce => self.dispatch(Command::Native {
                    game,
                    command: NativeCommand::Launch { local_only: false },
                }),
            },
            game_view::Action::Sync => self.dispatch(match game {
                GameId::Dusklight => Command::SendSave,
                GameId::Totk => Command::SendTotk,
                GameId::Triforce => Command::Native {
                    game,
                    command: NativeCommand::Send,
                },
            }),
            game_view::Action::Compare => {
                self.fork_choice = None;
                self.fork_dialog_open = true;
            }
            game_view::Action::History => {
                self.page = Page::History;
                self.dispatch(match game {
                    GameId::Dusklight => Command::HistoryPage(None),
                    GameId::Totk => Command::TotkHistoryPage(None),
                    GameId::Triforce => Command::Native {
                        game,
                        command: NativeCommand::HistoryPage(None),
                    },
                });
            }
            game_view::Action::Refresh => self.dispatch(match game {
                GameId::Dusklight => Command::RefreshSave,
                GameId::Totk => Command::RefreshTotk,
                GameId::Triforce => Command::Native {
                    game,
                    command: NativeCommand::Refresh,
                },
            }),
            game_view::Action::Setup => self.page = Page::Library,
            game_view::Action::Devices => self.page = Page::Devices,
            game_view::Action::Incoming(approved) => {
                let confirmed = !approved || rfd::MessageDialog::new().set_title(format!("Accept {} save", game_view::title(game)))
                    .set_description("Replace this PC's complete active save with the incoming snapshot? Existing progress is retained in history or rollback storage; the game's safety checks still apply.")
                    .set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes;
                if confirmed {
                    self.dispatch(game_view::incoming_command(game, approved));
                }
            }
            game_view::Action::Recovery(approved) => self.dispatch(match game {
                GameId::Dusklight => Command::AcceptRecovery(approved),
                GameId::Totk => Command::AcceptTotkRecovery(approved),
                GameId::Triforce => Command::Native {
                    game,
                    command: NativeCommand::AcceptRecovery(approved),
                },
            }),
            game_view::Action::Home => self.page = Page::Overview,
            game_view::Action::Locate => {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title(format!("Locate {}", game_view::title(game)))
                    .add_filter("Windows game executable", &["exe"])
                    .pick_file()
                {
                    self.dispatch(match game {
                        GameId::Dusklight => Command::SetExecutable(path),
                        GameId::Totk => Command::SetTotkExecutable(path),
                        GameId::Triforce => Command::Native {
                            game,
                            command: NativeCommand::SetExecutable(path),
                        },
                    });
                }
            }
            game_view::Action::Recover => {
                let approved = rfd::MessageDialog::new().set_title("Recover completed game session")
                    .set_description(if snapshot.ready {
                        "Confirm this game is closed on both PCs. The paired manager must approve recovery. No save will be replaced."
                    } else {
                        "Confirm this PC's game session has ended. Partner activity is unknown; offline recovery does not replace progress and may require branch reconciliation after reconnection."
                    }).set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes;
                if approved {
                    self.dispatch(match game {
                        GameId::Dusklight => Command::Recover,
                        GameId::Totk => Command::RecoverTotk,
                        GameId::Triforce => Command::Native {
                            game,
                            command: NativeCommand::Recover,
                        },
                    });
                }
            }
        }
    }

    fn game_home(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let model = game_view::view(self.game, snapshot);
        if let Some(action) = game_view::home(ui, &model) {
            self.game_action(snapshot, model.game, action);
        }
    }

    fn game_library(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let model = game_view::view(self.game, snapshot);
        if let Some(action) = game_view::installation(ui, &model) {
            self.game_action(snapshot, model.game, action);
        }
        self.library(ui, snapshot);
    }

    fn native_session_blocker<'a>(
        snapshot: &Snapshot,
        state: &'a crate::totk::Status,
    ) -> Option<&'a str> {
        if state.updating {
            Some("Game files are being updated or rebuilt. Play and save sync are paused.")
        } else if let Some(error) = &state.verification_error {
            Some(error.as_str())
        } else if !state.checked {
            Some("The game installation and save are still being checked.")
        } else if !state.installed {
            Some("The native game installation was not found.")
        } else if state.running {
            Some("The game is running on this PC. Save replacement is paused.")
        } else if state.remote_running {
            Some("The paired PC is playing. Save replacement is paused.")
        } else if state.launch_pending {
            Some("Waiting for the paired PC to reserve the play session.")
        } else if state.release_pending && snapshot.ready {
            Some("The saved session is waiting for the paired PC's release acknowledgement.")
        } else if state.pending {
            Some("An incoming save is waiting for your Accept or Keep decision.")
        } else if state.busy {
            Some("A save transfer or comparison is in progress.")
        } else if state.recovery_pending {
            Some("A completed-session recovery is waiting for your decision.")
        } else if state.lock.is_some() || state.remote_lock.is_some() {
            if !snapshot.ready
                && state
                    .lock
                    .as_ref()
                    .is_some_and(|lock| lock.owner != snapshot.public_key)
            {
                Some(
                    "The stored reservation belongs to the paired PC. Reconnect it for completed-session recovery.",
                )
            } else {
                Some("A stored play reservation needs completed-session recovery.")
            }
        } else if snapshot.self_updates.preparing {
            Some("Twilight Sync is preparing an app update.")
        } else {
            None
        }
    }

    fn native_play_blocker<'a>(
        snapshot: &Snapshot,
        state: &'a crate::totk::Status,
    ) -> Option<&'a str> {
        Self::native_session_blocker(snapshot, state).or_else(|| {
            if snapshot.ready && state.peer_checked && state.remote_installed && !state.remote_save_ready {
                Some("The paired PC's save is not verified yet. Protected play is waiting for its valid state.")
            } else { state.conflict.then_some(
                "Both PCs have different progress. A branch choice or Keep both is required before play."
            ) }
        })
    }

    fn native_sync_blocker<'a>(
        snapshot: &Snapshot,
        state: &'a crate::totk::Status,
    ) -> Option<&'a str> {
        Self::native_session_blocker(snapshot, state).or_else(|| {
            if !snapshot.ready {
                Some("The paired PC is offline. This PC's progress is retained locally.")
            } else if !state.peer_checked {
                Some("Waiting for the paired PC's game save status.")
            } else if !state.remote_installed {
                Some("The paired PC has no available native installation.")
            } else if !state.remote_save_ready {
                Some("The paired PC's committed save is not verified and ready yet.")
            } else if state.local_hash.is_none() {
                Some("This PC has no committed save to send.")
            } else if state.local_hash == state.remote_hash {
                Some("Both save snapshots already match. There is nothing to send.")
            } else {
                None
            }
        })
    }

    fn devices(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let p = Palette::of(ui);
        let trusted = snapshot.network.as_ref().is_some_and(|network| {
            snapshot.settings.network.as_deref() == Some(network.fingerprint.as_str())
        });
        if snapshot.comparison_code.is_none() {
            Self::card(ui, |ui| {
                Self::section_title(ui, "Current network");
                if let Some(network) = &snapshot.network {
                    Self::status(
                        ui,
                        "Adapter",
                        &format!("{} ({})", network.adapter, network.address),
                        p.text,
                    );
                    Self::status(ui, "Router", &network.gateway.to_string(), p.text);
                    Self::status(ui, "Router MAC", &network.router_mac, p.text);
                    if trusted {
                        ui.label(RichText::new("Approved for local discovery").color(p.success));
                    } else {
                        Self::secondary(
                            ui,
                            "Check that this is your private home router on both PCs.",
                        );
                        if Self::primary(
                            ui,
                            Self::management_available(snapshot),
                            "Approve this network",
                        )
                        .clicked()
                        {
                            self.dispatch(Command::TrustNetwork);
                        }
                    }
                    ui.collapsing("Network fingerprint", |ui| {
                        ui.label(RichText::new(&network.fingerprint).monospace());
                        Self::secondary(
                            ui,
                            format!("Direct subnet: {} / {}", network.address, network.prefix),
                        );
                        Self::secondary(
                            ui,
                            "A router fingerprint is a network check, not device authentication.",
                        );
                    });
                } else {
                    Self::notice(
                        ui,
                        Tone::Warning,
                        "No supported private network",
                        "Connect through a physical network adapter and set the Windows network profile to Private. VPN-only and public interfaces are not accepted.",
                    );
                }
            });
            ui.add_space(12.0);
        }
        if let Some(code) = &snapshot.comparison_code {
            Self::card(ui, |ui| {
                Self::section_title(
                    ui,
                    snapshot
                        .peer_name
                        .as_deref()
                        .unwrap_or("Authenticating device"),
                );
                let code_size = (ui.available_width() / 5.8).clamp(24.0, 36.0);
                ui.label(
                    RichText::new(code)
                        .monospace()
                        .size(code_size)
                        .color(p.accent),
                );
                ui.label("Both screens must show this exact code.");
                ui.scope(|ui| {
                    ui.spacing_mut().interact_size.y = 18.0;
                    ui.horizontal_wrapped(|ui| {
                        Self::status(
                            ui,
                            "This PC",
                            if snapshot.local_approved {
                                "Approved"
                            } else {
                                "Waiting"
                            },
                            if snapshot.local_approved {
                                p.success
                            } else {
                                p.secondary
                            },
                        );
                        Self::status(
                            ui,
                            "Other PC",
                            if snapshot.remote_approved {
                                "Approved"
                            } else {
                                "Waiting"
                            },
                            if snapshot.remote_approved {
                                p.success
                            } else {
                                p.secondary
                            },
                        );
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    if Self::primary(ui, !snapshot.local_approved, "Codes match - approve")
                        .clicked()
                    {
                        self.dispatch(Command::ApprovePairing);
                    }
                    if ui.button("Reject").clicked() {
                        self.dispatch(Command::RejectPairing);
                    }
                });
                ui.collapsing("Connection details", |ui| {
                    ui.label("Reject this request if any digit differs. The code changes for each handshake.");
                    if let Some(network) = &snapshot.network {
                        Self::secondary(ui, format!("Router: {} / {}", network.gateway, network.router_mac));
                        ui.label(RichText::new(&network.fingerprint).monospace());
                    }
                });
            });
        } else if let Some(peer) = &snapshot.settings.peer {
            Self::card(ui, |ui| {
                Self::section_title(ui, "Paired device");
                ui.label(RichText::new(&peer.name).font(semibold(18.0)));
                Self::status(
                    ui,
                    "Status",
                    if snapshot.ready {
                        "Connected - identity verified"
                    } else {
                        "Offline - trust is retained"
                    },
                    if snapshot.ready {
                        p.success
                    } else {
                        p.secondary
                    },
                );
                ui.horizontal_wrapped(|ui| {
                    let discovered = snapshot.devices.iter().any(|device| device.key == peer.public_key);
                    if !snapshot.ready && ui.add_enabled(
                        trusted && discovered && !snapshot.connecting,
                        egui::Button::new(if snapshot.connecting { "Connecting..." } else { "Reconnect" }),
                    ).clicked() {
                        self.dispatch(Command::Connect(peer.public_key.clone()));
                    }
                    if ui.add_enabled(Self::management_available(snapshot), egui::Button::new("Unpair...")).clicked() {
                        let approved = rfd::MessageDialog::new().set_title("Unpair device")
                            .set_description(format!("Remove trust for {}?\n\nSave files and history are kept. Pair and approve both devices again before syncing.", peer.name))
                            .set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes;
                        if approved { self.dispatch(Command::Unpair); }
                    }
                });
                ui.collapsing("Verified device identity", |ui| {
                    ui.label(RichText::new(&peer.public_key).monospace());
                });
            });
        } else {
            Self::card(ui, |ui| {
                Self::section_title(ui, "Pair a device");
                Self::secondary(
                    ui,
                    "Open pairing on both PCs. Choose Compare codes on only ONE PC, then approve the same code on BOTH.",
                );
                ui.horizontal_wrapped(|ui| {
                    if Self::primary(
                        ui,
                        trusted && !snapshot.connecting && Self::management_available(snapshot),
                        if snapshot.pairing_open {
                            "Restart pairing window"
                        } else {
                            "Open pairing"
                        },
                    )
                    .clicked()
                    {
                        self.dispatch(Command::OpenPairing);
                    }
                    if snapshot.connecting {
                        ui.spinner();
                        ui.label("Establishing an encrypted connection...");
                    } else if snapshot.pairing_open {
                        ui.label(RichText::new("Pairing window is open").color(p.accent));
                    }
                });
                if !trusted {
                    Self::secondary(ui, "Approve the current network first.");
                }
                ui.add_space(8.0);
                if snapshot.devices.is_empty() {
                    Self::notice(
                        ui,
                        Tone::Info,
                        "No nearby devices yet",
                        "Open Twilight Sync on the other PC and approve the same router. Guest Wi-Fi isolation or a firewall may block local discovery.",
                    );
                }
                for device in &snapshot.devices {
                    ui.separator();
                    Self::setting_row(
                        ui,
                        &device.name,
                        &format!(
                            "{} - unverified discovery announcement",
                            device.address.ip()
                        ),
                        |ui| {
                            if ui
                                .add_enabled(
                                    snapshot.pairing_open && device.pairing && !snapshot.connecting,
                                    egui::Button::new("Compare codes"),
                                )
                                .clicked()
                            {
                                self.dispatch(Command::Connect(device.key.clone()));
                            }
                        },
                    );
                    if !device.pairing {
                        Self::secondary(ui, "Open the pairing window on this device too.");
                    }
                }
            });
        }
    }

    fn library(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let game = self.game;
        let mut preference = snapshot.source.preferences.get(game).clone();
        let original = preference.clone();
        Self::card(ui, |ui| {
            Self::section_title(ui, "Installation method");
            ui.add_enabled_ui(
                snapshot.source.preferences_valid
                    && !snapshot.source.busy
                    && !snapshot.install_busy,
                |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if game != GameId::Triforce {
                            ui.radio_value(
                                &mut preference.method,
                                crate::source_build::Method::Official,
                                "Official release / setup",
                            );
                        }
                        ui.radio_value(
                            &mut preference.method,
                            crate::source_build::Method::Source,
                            "Build from source",
                        );
                    });
                },
            );
            if game == GameId::Triforce {
                Self::secondary(
                    ui,
                    "Twilight Sync builds Triforce locally instead of running its publisher's Setup or downloading a prebuilt game.",
                );
            }
            if !snapshot.source.preferences_valid {
                Self::notice(
                    ui,
                    Tone::Error,
                    "Source-build preferences need attention",
                    snapshot
                        .source
                        .error
                        .as_deref()
                        .unwrap_or("Reset source-build preferences before using source builds."),
                );
                if ui
                    .add_enabled(
                        !snapshot.source.busy,
                        egui::Button::new("Reset source-build preferences"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::ResetSourcePreferences);
                }
            }
        });
        if preference.method == crate::source_build::Method::Source {
            self.source_tools(ui, snapshot, &mut preference, &original);
            if game == GameId::Dusklight {
                self.disc_tools(ui, snapshot);
            }
        } else {
            self.official_library(ui, snapshot, &mut preference);
        }
        if preference != original {
            self.source_approved = false;
            self.dispatch(Command::SetSourcePreference { game, preference });
        }
    }

    fn source_input(
        ui: &mut egui::Ui,
        label: &str,
        path: &mut Option<PathBuf>,
        extensions: &[&str],
        folder: bool,
    ) {
        ui.label(RichText::new(label).strong());
        if let Some(selected) = path.as_deref() {
            Self::path(ui, selected);
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(if folder {
                    "Choose folder..."
                } else {
                    "Choose file..."
                })
                .clicked()
            {
                let dialog = rfd::FileDialog::new().set_title(label);
                let selected = if folder {
                    dialog.pick_folder()
                } else {
                    dialog.add_filter(label, extensions).pick_file()
                };
                if let Some(selected) = selected {
                    *path = Some(selected);
                }
            }
            if path.is_some() && ui.button("Clear").clicked() {
                *path = None;
            }
        });
        ui.add_space(6.0);
    }

    fn source_tools(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &Snapshot,
        preference: &mut crate::source_build::Preference,
        original: &crate::source_build::Preference,
    ) {
        let game = self.game;
        let state = &snapshot.source;
        let plan = state.plan.as_ref().filter(|plan| plan.game == game);
        let reviewed = plan.map(|plan| (game, plan.commit.clone()));
        if self.source_reviewed != reviewed {
            self.source_approved = false;
            self.source_reviewed = reviewed;
        }
        let previous = match game {
            GameId::Dusklight => snapshot
                .settings
                .game_exe
                .as_ref()
                .and_then(|path| path.parent()),
            GameId::Totk => snapshot.totk.install_path.as_deref(),
            GameId::Triforce => snapshot.triforce.install_path.as_deref(),
        };
        ui.add_space(12.0);
        Self::notice(
            ui,
            Tone::Warning,
            "Source builds: slower, not a safety guarantee",
            crate::source_build::WARNING,
        );
        ui.hyperlink_to("Using a separate Windows Sandbox / VM", "https://learn.microsoft.com/windows/security/application-security/application-isolation/windows-sandbox/");
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "1. Source and build tools");
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!snapshot.install_busy && !snapshot.texture_busy && !snapshot.self_updates.preparing && state.preferences_valid,
                    egui::Button::new("Check source version / prerequisites")).clicked() {
                    self.source_approved = false;
                    self.dispatch(Command::CheckSource(game));
                }
                if ui.add_enabled(!snapshot.install_busy && !snapshot.texture_busy && !snapshot.self_updates.preparing && state.preferences_valid,
                    egui::Button::new("Prepare / verify official tools...")).clicked() {
                    let approved = rfd::MessageDialog::new().set_title("Download official build tools")
                        .set_description("Download and checksum-verify portable Git, uv/Python tooling, and this game's compiler/SDK archives from their official publisher repositories? This can use several GiB. Tools are kept per-user. Missing system SDKs are listed separately, with approved .NET/MSYS2 setup buttons where available. No game project scripts run during this preparation step.")
                        .set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes;
                    if approved { self.dispatch(Command::PrepareSourceTools { game, approved }); }
                }
            });
            if let Some(plan) = plan {
                Self::status(ui, "Stable source tag", &plan.tag, Palette::of(ui).text);
                ui.add(
                    egui::Label::new(RichText::new(format!("Commit: {}", plan.commit)).monospace())
                        .wrap(),
                );
                ui.hyperlink_to("Review this exact source commit", &plan.review_url);
            } else {
                ui.hyperlink_to(
                    "Upstream source repository",
                    crate::source_build::source_url(game),
                );
                Self::secondary(
                    ui,
                    "Check the stable source tag first. No project code runs during the metadata check.",
                );
            }
            if state.game == Some(game) {
                for prerequisite in &state.prerequisites {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(if prerequisite.ready {
                                "Found"
                            } else {
                                "Needs setup"
                            })
                            .color(if prerequisite.ready {
                                Palette::of(ui).success
                            } else {
                                Palette::of(ui).warning
                            }),
                        );
                        ui.label(prerequisite.name);
                        if !prerequisite.ready {
                            ui.hyperlink_to("Official setup instructions", prerequisite.url);
                        }
                    });
                    if let Some(detail) = &prerequisite.detail {
                        Self::secondary(ui, detail);
                    }
                    if !prerequisite.ready
                        && let Some(setup) = prerequisite.setup
                    {
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Copy setup commands").clicked() {
                                ui.ctx().copy_text(setup.script());
                            }
                            if ui
                                .add_enabled(
                                    !snapshot.install_busy
                                        && !snapshot.texture_busy
                                        && !snapshot.self_updates.preparing,
                                    egui::Button::new(setup.label()),
                                )
                                .clicked()
                            {
                                let approved = rfd::MessageDialog::new()
                                    .set_title("Set up source-build SDK")
                                    .set_description(setup.notice())
                                    .set_buttons(rfd::MessageButtons::YesNo)
                                    .show()
                                    == rfd::MessageDialogResult::Yes;
                                if approved {
                                    self.source_approved = false;
                                    self.dispatch(Command::InstallSourceSdk {
                                        game,
                                        setup,
                                        approved,
                                    });
                                }
                            }
                        });
                    }
                }
                if state
                    .prerequisites
                    .iter()
                    .any(|item| !item.ready && item.setup.is_some())
                {
                    Self::secondary(
                        ui,
                        "After running copied setup commands, click Check source version / prerequisites above to refresh these results. The automatic setup button rechecks them for you.",
                    );
                }
            }
            Self::secondary(
                ui,
                "Official archive checksums verify download integrity, not the safety of a publisher or source code. Some SDKs are installed separately; Python/project dependencies are set up during an approved build.",
            );
        });
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "2. Your local game files");
            ui.add_enabled_ui(!state.busy && state.preferences_valid, |ui| {
                match game {
                    GameId::Dusklight => Self::secondary(ui, "Compiling Dusklight does not require game assets. Configure it with your own legally obtained disc dump after installation, as with an official release."),
                    GameId::Totk => {
                        Self::source_input(ui, "Your TotK base game", &mut preference.game_input, &["nsp","xci","nsz","xcz"], false);
                        Self::source_input(ui, "Your supported TotK update", &mut preference.update, &["nsp","nsz"], false);
                        Self::source_input(ui, "Your console's prod.keys", &mut preference.keys, &["keys"], false);
                        ui.collapsing("Optional system fonts", |ui| {
                            Self::source_input(ui, "Firmware folder (Chinese/Korean fonts)", &mut preference.firmware, &[], true);
                        });
                        Self::secondary(ui, "Keys and game files stay local in Twilight Sync's workflow. The approved source scripts still have your user permissions. TotK's source installer may open a shader-preparation window; normal gameplay does not start.");
                    }
                    GameId::Triforce => {
                        Self::source_input(ui, "Your Triforce .nup", &mut preference.game_input, &["nup"], false);
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Choose existing game / extracted folder...").clicked()
                                && let Some(path) = rfd::FileDialog::new().set_title("Folder with romfs and exefs/.triforce_exefs").pick_folder()
                            { preference.game_input = Some(path); }
                        });
                        if preference.game_input.is_none() {
                            Self::secondary(ui, "When available, the current Triforce installation's retained game files are used automatically.");
                        }
                        Self::secondary(ui, "The game and its required Vulkan renderer are built locally. SDL and AV1/audio codec dependencies are included and checked; the separate WebGPU fallback is not required for Vulkan. Codec packages come from Takoroka/ffmpeg-av1, not FFmpeg.org; archive checksums do not prove source-to-binary safety.");
                        Self::secondary(ui, "The new build selects Vulkan even if the previous folder used WebGPU; the previous folder and its renderer settings are retained.");
                        ui.hyperlink_to("Codec supplier and source", "https://github.com/Takoroka/ffmpeg-av1");
                        ui.collapsing("Optional graphics features", |ui| {
                            ui.checkbox(&mut preference.renodx_hdr, "RenoDX HDR (upstream default; downloads its pinned shader/compiler dependencies)");
                            ui.checkbox(&mut preference.amd_fsr4, "AMD FSR 4 (optional; downloads AMD's runtime)");
                            ui.checkbox(&mut preference.nvidia_dlss, "NVIDIA DLSS / frame generation: I accept NVIDIA's SDK license and downloads");
                            ui.hyperlink_to("Review NVIDIA SDK terms", "https://developer.nvidia.com/rtx/sdk-license");
                            ui.checkbox(&mut preference.prepare_shaders, "Prepare shaders after compiling (runs the game's shader preparation, not normal gameplay)");
                        });
                    }
                }
                ui.horizontal_wrapped(|ui| {
                    ui.label("Compiler workers");
                    ui.add(egui::DragValue::new(&mut preference.jobs).range(1..=16));
                });
                ui.checkbox(&mut preference.managed_tools, "Download missing official tool archives after approval (Python/project packages are set up separately)");
            });
            Self::secondary(
                ui,
                "New builds are installed side-by-side under your Games folder. Existing installs, saves and mods are preserved. Native builds need at least 64 GiB free; build workspaces can use tens of GiB and are retained.",
            );
        });
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "3. Review and build");
            let session_blocker = Self::source_build_blocker(snapshot);
            let input_problem =
                crate::source_build::validate_inputs(game, preference, previous).err();
            let manual_ready = plan.is_some()
                && state.game == Some(game)
                && state.prerequisites.iter().all(|item| item.ready);
            ui.add_enabled_ui(!state.busy && state.preferences_valid && plan.is_some(), |ui| {
                ui.checkbox(&mut self.source_approved, "I own the required game files, reviewed the selected commit, and accept running its source/build scripts and dependencies.");
            });
            if let Some(error) = &input_problem {
                Self::secondary(ui, format!("{error:#}"));
            }
            if !manual_ready {
                Self::secondary(
                    ui,
                    "Check the source version and finish any missing SDK prerequisites above before building.",
                );
            }
            if let Some(reason) = session_blocker {
                Self::secondary(ui, reason);
            }
            let enabled = self.source_approved
                && manual_ready
                && input_problem.is_none()
                && !snapshot.install_busy
                && !snapshot.texture_busy
                && !snapshot.self_updates.preparing
                && session_blocker.is_none()
                && preference == original;
            ui.horizontal_wrapped(|ui| {
                if Self::primary(ui, enabled, "Build / update from source").clicked()
                    && let Some(plan) = plan
                {
                    self.source_approved = false;
                    self.dispatch(Command::BuildSource {
                        game,
                        commit: plan.commit.clone(),
                        approved: true,
                    });
                }
                if state.busy
                    && !state.sdk_setup
                    && state.game == Some(game)
                    && state.log.is_some()
                    && ui.button("Cancel build / download").clicked()
                {
                    self.dispatch(Command::CancelSource);
                }
            });
            if state.game == Some(game) {
                if state.busy {
                    ui.spinner();
                }
                if !state.message.is_empty() {
                    Self::secondary(ui, &state.message);
                }
                if let Some(error) = &state.error {
                    Self::notice(ui, Tone::Error, "Source operation needs attention", error);
                }
                if let Some(log) = &state.log {
                    Self::path(ui, log);
                    if log.is_file()
                        && ui.button("Open build log").clicked()
                        && let Err(error) = open_build_log(log)
                    {
                        self.local_error = Some(format!("{error:#}"));
                    }
                }
            }
            if let Some(built) = state.built.as_ref().filter(|built| built.game == game) {
                ui.collapsing("Last completed build", |ui| {
                    Self::path(ui, &built.executable);
                    Self::path(ui, &built.source);
                    if ui
                        .add_enabled(
                            !snapshot.install_busy && !state.busy,
                            egui::Button::new("Use completed build (verify progress first)"),
                        )
                        .clicked()
                    {
                        self.dispatch(Command::ActivateSourceBuild(game));
                    }
                });
            }
        });
    }

    fn source_build_blocker(snapshot: &Snapshot) -> Option<&'static str> {
        let native = [&snapshot.totk, &snapshot.triforce];
        if snapshot.game_running
            || snapshot.remote_game
            || native
                .iter()
                .any(|game| game.running || game.remote_running)
        {
            Some(
                "Close all games on both PCs before building. Source review and tool downloads remain available.",
            )
        } else if snapshot.settings.play_lock.is_some()
            || native.iter().any(|game| {
                game.lock.is_some()
                    || game.remote_lock.is_some()
                    || game.launch_pending
                    || game.release_pending
            })
        {
            Some("Finish or recover stored play reservations before building.")
        } else if snapshot.pending_save
            || snapshot.pending_iso
            || snapshot.pending_recovery
            || !snapshot.transfer.is_empty()
            || native
                .iter()
                .any(|game| game.pending || game.busy || game.recovery_pending)
        {
            Some("Finish pending transfers, save choices and recovery before building.")
        } else if snapshot.install_busy || snapshot.texture_busy || snapshot.self_updates.preparing
        {
            Some("Wait for the current installation, tool setup or update to finish.")
        } else {
            None
        }
    }

    fn official_library(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: &Snapshot,
        preference: &mut crate::source_build::Preference,
    ) {
        if self.game == GameId::Triforce {
            Self::secondary(
                ui,
                "Triforce is built from source using your own game files. Saves, branch choices, protected play and history are separate from the other games.",
            );
            ui.hyperlink_to(
                "Triforce source and build instructions",
                "https://git.triforceproject.com/triforce/source",
            );
            return;
        }
        let p = Palette::of(ui);
        let available = Self::management_available(snapshot);
        let totk = self.game == GameId::Totk;
        let setup_required = if totk {
            match snapshot.totk.install_path.as_deref() {
                Some(directory) => crate::totk_update::needs_setup(directory),
                None => Ok(!snapshot.totk.installed),
            }
        } else {
            Ok(false)
        };
        let fresh = matches!(&setup_required, Ok(true));
        let input_problem = if fresh {
            snapshot.totk.install_path.as_ref().and_then(|directory| {
                crate::totk_update::validate_setup_inputs(
                    directory,
                    &crate::totk_update::SetupInputs::from(&*preference),
                )
                .err()
            })
        } else {
            None
        };
        let update_reason = totk.then(|| Self::totk_update_blocker(snapshot)).flatten();
        let release = if totk {
            &snapshot.latest_totk_release
        } else {
            &snapshot.latest_release
        };
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(
                ui,
                if totk {
                    "TotK Recomp installation and updates"
                } else {
                    "Dusklight updates"
                },
            );
            if let Some(release) = release {
                Self::status(ui, "Latest official release", &release.tag, p.text);
                Self::secondary(
                    ui,
                    format!(
                        "Verified download: {:.1} MiB",
                        release.size as f64 / 1_048_576.0
                    ),
                );
                ui.collapsing("Download verification", |ui| {
                    ui.label(RichText::new(format!("SHA-256: {}", release.sha256)).monospace());
                });
            } else {
                Self::secondary(ui, "Check GitHub for the latest official game release.");
            }
            if fresh {
                Self::secondary(
                    ui,
                    "First install: select your own game files once. Twilight Sync downloads and verifies the official tools, then installs automatically; no .NET/MSYS2 source-build SDK setup is needed.",
                );
                ui.add_enabled_ui(
                    !snapshot.install_busy && snapshot.source.preferences_valid,
                    |ui| {
                        Self::source_input(
                            ui,
                            "Your TotK base NSP / XCI",
                            &mut preference.game_input,
                            &["nsp", "xci", "nsz", "xcz"],
                            false,
                        );
                        Self::source_input(
                            ui,
                            "Your supported TotK update",
                            &mut preference.update,
                            &["nsp", "nsz"],
                            false,
                        );
                        Self::source_input(
                            ui,
                            "Your console's prod.keys",
                            &mut preference.keys,
                            &["keys"],
                            false,
                        );
                        ui.collapsing("Optional system fonts", |ui| {
                            Self::source_input(
                                ui,
                                "Firmware folder",
                                &mut preference.firmware,
                                &[],
                                true,
                            );
                        });
                    },
                );
                if let Some(error) = &input_problem {
                    Self::secondary(ui, format!("{error:#}"));
                }
            }
            if let Err(error) = &setup_required {
                Self::notice(
                    ui,
                    Tone::Error,
                    "Cannot check TotK installation",
                    &format!("{error:#}"),
                );
            }
            ui.horizontal_wrapped(|ui| {
                if snapshot.install_busy {
                    ui.spinner();
                    ui.label(if snapshot.totk.updating {
                        "Installing/updating and preparing game files; this can take several minutes..."
                    } else {
                        "Checking, downloading, or verifying..."
                    });
                }
                if Self::primary(
                    ui,
                    available
                        && update_reason.is_none()
                        && input_problem.is_none()
                        && setup_required.is_ok()
                        && (!totk || &*preference == snapshot.source.preferences.get(GameId::Totk)),
                    if totk {
                        "Install / update TotK Recomp"
                    } else {
                        "Install / update Dusklight"
                    },
                )
                .clicked()
                {
                    self.dispatch(if totk {
                        Command::UpdateTotk
                    } else {
                        Command::Install
                    });
                }
                if ui
                    .add_enabled(available, egui::Button::new("Check latest release"))
                    .clicked()
                {
                    self.dispatch(if totk {
                        Command::CheckTotkRelease
                    } else {
                        Command::CheckRelease
                    });
                }
            });
            if let Some(reason) = update_reason {
                Self::secondary(ui, reason);
            }
            Self::secondary(
                ui,
                if totk {
                    "Downloads and verifies the official setup, then automatically installs or updates the game using your local inputs. Existing saves, settings and mods are preserved. Play and save sync pause until it finishes."
                } else {
                    "Installs side-by-side, verifies existing managed files, and preserves your profile, saves, and textures. Nothing launches automatically."
                },
            );
            Self::secondary(
                ui,
                if totk {
                    "First installs and updates use your own legally obtained game files. No game assets or console keys are downloaded. The setup download link below is an optional manual alternative, not a required step."
                } else {
                    "A Dusklight desktop shortcut opens this manager's protected Play controls."
                },
            );
            Self::secondary(
                ui,
                if totk {
                    "Managed install location (fresh installs use a versioned subfolder):"
                } else {
                    "New managed installations use:"
                },
            );
            match crate::install::default_directory(self.game) {
                Ok(directory) => Self::path(ui, &directory),
                Err(error) => Self::notice(
                    ui,
                    Tone::Error,
                    "Cannot locate your Games folder",
                    &format!("{error:#}"),
                ),
            }
            if totk {
                ui.hyperlink_to(TOTK_SETUP_LABEL, crate::totk_update::SETUP_DOWNLOAD_URL);
            }
        });
        if totk {
            return;
        }
        self.disc_tools(ui, snapshot);
    }

    fn disc_tools(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let available = Self::management_available(snapshot);
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Disc image (optional)");
            Self::secondary(
                ui,
                "Use your own Twilight Princess GameCube ISO or RVZ. RVZ must reconstruct a supported, full-size unmodified disc. Wii images and game downloads are not supported.",
            );
            let mut enabled = snapshot.settings.iso_enabled;
            if ui
                .checkbox(
                    &mut enabled,
                    "I own this game and explicitly allow disc-image transfers",
                )
                .changed()
            {
                self.dispatch(Command::SetIsoEnabled(enabled));
            }
            if let Some(path) = &snapshot.settings.iso_file {
                Self::path(ui, path);
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(available, egui::Button::new("Choose ISO / RVZ..."))
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Your GameCube disc dump", &["iso", "rvz"])
                        .pick_file()
                {
                    self.dispatch(Command::SetIso(path));
                }
                if ui
                    .add_enabled(
                        snapshot.ready && snapshot.settings.iso_enabled && available,
                        egui::Button::new("Offer verified disc image"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::SendIso);
                }
            });
            Self::secondary(
                ui,
                "The receiver must opt in and approve each transfer. Existing disc images are never overwritten.",
            );
            Self::secondary(
                ui,
                "RVZ stays compressed during transfer. Both managers need v0.5.0 or later; verification runs separately without blocking sync heartbeats.",
            );
        });
    }

    fn totk_update_blocker(snapshot: &Snapshot) -> Option<&'static str> {
        let state = &snapshot.totk;
        if state.updating {
            Some("Wait for the current TotK update to finish.")
        } else if !state.checked {
            Some("Checking your TotK installation folder...")
        } else if state.install_path.is_none() {
            Some(
                "The TotK installation folder could not be resolved. Check the folder error or locate a custom installation.",
            )
        } else if state.running || state.remote_running {
            Some("Close TotK on both PCs before updating.")
        } else if state.busy
            || state.pending
            || state.launch_pending
            || state.release_pending
            || state.recovery_pending
            || snapshot.pending_save
            || snapshot.pending_iso
        {
            Some("Finish pending transfers, play-session releases, or recovery before updating.")
        } else if state.lock.is_some() || state.remote_lock.is_some() {
            Some("Finish or recover the TotK play reservation before updating.")
        } else if !Self::management_available(snapshot) {
            Some("Finish the current game or management operation before updating.")
        } else {
            None
        }
    }

    fn preserve_texture_files(snapshot: &Snapshot) -> bool {
        !snapshot.textures.unmanaged || rfd::MessageDialog::new()
            .set_title("Preserve existing texture files")
            .set_description("Existing textures will be kept in a backup beside texture_replacements. Disabling the managed pack restores them. Continue?")
            .set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes
    }

    fn import_texture(&mut self, folder: bool, snapshot: &Snapshot) {
        if !self.texture_permission {
            self.local_error =
                Some("Confirm that you have permission to use the texture assets.".into());
            return;
        }
        let selected = if folder {
            rfd::FileDialog::new()
                .set_title("Choose the complete texture folder")
                .pick_folder()
        } else {
            rfd::FileDialog::new()
                .add_filter("Texture pack ZIP", &["zip"])
                .pick_file()
        };
        if let Some(path) = selected {
            if !Self::preserve_texture_files(snapshot) {
                return;
            }
            self.dispatch(Command::ImportTextures {
                source: self.texture_source.clone(),
                version: self.texture_version.clone(),
                path,
                preserve_unmanaged: snapshot.textures.unmanaged,
            });
        }
    }

    fn textures(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let p = Palette::of(ui);
        let available = Self::management_available(snapshot);
        if !snapshot.textures.notice.is_empty() {
            Self::notice(ui, Tone::Info, "Texture library", &snapshot.textures.notice);
            ui.add_space(12.0);
        }
        if snapshot.texture_busy
            && snapshot.textures.cancellable
            && ui.button("Cancel installation").clicked()
        {
            self.dispatch(Command::CancelTextures);
        }
        Self::card(ui, |ui| {
            Self::section_title(ui, "Dusklight profile");
            if let Some(path) = &snapshot.settings.texture_profile {
                Self::secondary(
                    ui,
                    "Detected automatically. The pack is applied to the profile Dusklight actually uses.",
                );
                ui.collapsing("Profile details", |ui| Self::path(ui, path));
            } else {
                Self::secondary(
                    ui,
                    "Locate or install Dusklight on the Dusklight page. Its profile will be detected automatically.",
                );
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(available, egui::Button::new("Refresh library"))
                    .clicked()
                {
                    self.dispatch(Command::RefreshTextures);
                }
            });
            Self::secondary(
                ui,
                "Pack changes apply to the next launch. Both games must be stopped; no game is forcibly restarted.",
            );
            if snapshot.textures.recovery_needed {
                Self::notice(
                    ui,
                    Tone::Error,
                    "Texture recovery required",
                    "An interrupted switch must be recovered before protected play.",
                );
                if ui
                    .add_enabled(
                        available,
                        egui::Button::new("Recover interrupted texture switch"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::RecoverTextures);
                }
            }
        });
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Installed packs");
            if snapshot.textures.packs.is_empty() {
                Self::secondary(
                    ui,
                    "No packs installed. Choose a pack and install its ZIP below.",
                );
            }
            for pack in &snapshot.textures.packs {
                ui.separator();
                let active = snapshot.textures.active.as_deref() == Some(&pack.id);
                let name = crate::textures::CATALOG
                    .iter()
                    .find(|project| project.id == pack.source)
                    .map_or(pack.source.as_str(), |project| project.name);
                let title = if name.ends_with(&format!(" v{}", pack.version)) {
                    name.to_owned()
                } else {
                    format!("{name} - {}", pack.version)
                };
                Self::setting_row(
                    ui,
                    &title,
                    &format!(
                        "{} images, {:.1} MiB",
                        pack.files,
                        pack.bytes as f64 / 1_048_576.0
                    ),
                    |ui| {
                        if active {
                            ui.label(RichText::new("Active").color(p.success));
                        } else if ui
                            .add_enabled(
                                available
                                    && snapshot.settings.texture_profile.is_some()
                                    && !snapshot.textures.recovery_needed,
                                egui::Button::new("Activate"),
                            )
                            .clicked()
                        {
                            let preserve = Self::preserve_texture_files(snapshot);
                            if preserve {
                                self.dispatch(Command::ActivateTextures {
                                    id: Some(pack.id.clone()),
                                    preserve_unmanaged: snapshot.textures.unmanaged,
                                });
                            }
                        }
                    },
                );
                egui::CollapsingHeader::new("Version details")
                    .id_salt(&pack.id)
                    .show(ui, |ui| {
                        Self::secondary(ui, format!("Imported: {}", pack.imported));
                        ui.label(RichText::new(format!("Content ID: {}", pack.id)).monospace());
                    });
            }
            if ui
                .add_enabled(
                    available
                        && snapshot.textures.active.is_some()
                        && !snapshot.textures.recovery_needed,
                    egui::Button::new("Disable pack and restore original textures"),
                )
                .clicked()
            {
                self.dispatch(Command::ActivateTextures {
                    id: None,
                    preserve_unmanaged: false,
                });
            }
        });
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Install a texture pack");
            Self::secondary(
                ui,
                "Install a complete ZIP or folder in one step. No manual extraction or config selection is needed.",
            );
            let before = self.texture_source.clone();
            let selected_name = crate::textures::CATALOG
                .iter()
                .find(|project| project.id == self.texture_source)
                .map_or(self.texture_source.as_str(), |project| project.name);
            egui::ComboBox::from_id_salt("texture-project")
                .selected_text(selected_name)
                .width(ui.available_width().min(520.0))
                .show_ui(ui, |ui| {
                    for project in crate::textures::CATALOG {
                        ui.selectable_value(
                            &mut self.texture_source,
                            project.id.into(),
                            project.name,
                        );
                    }
                });
            if before != self.texture_source
                && let Some(project) = crate::textures::CATALOG
                    .iter()
                    .find(|project| project.id == self.texture_source)
            {
                self.texture_version = project.suggested_version.map_or_else(
                    || format!("Imported {}", time::OffsetDateTime::now_utc().date()),
                    str::to_owned,
                );
            }
            if let Some(project) = crate::textures::CATALOG
                .iter()
                .find(|project| project.id == self.texture_source)
            {
                if project.creator_url.is_some() && ui.button("View creator page").clicked() {
                    self.dispatch(Command::OpenTextureCreator(project.id.into()));
                }
                ui.collapsing("About this pack", |ui| {
                    Self::secondary(ui, project.description);
                });
            }
            ui.checkbox(
                &mut self.texture_permission,
                "I have permission to use these texture assets",
            );
            ui.collapsing("Version details", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Version label");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.texture_version).desired_width(240.0),
                    );
                });
            });
            ui.horizontal_wrapped(|ui| {
                if Self::primary(
                    ui,
                    available && self.texture_permission && snapshot.settings.game_exe.is_some(),
                    "Install ZIP...",
                )
                .clicked()
                {
                    self.import_texture(false, snapshot);
                }
                if ui
                    .add_enabled(
                        available
                            && self.texture_permission
                            && snapshot.settings.game_exe.is_some(),
                        egui::Button::new("Install folder..."),
                    )
                    .clicked()
                {
                    self.import_texture(true, snapshot);
                }
            });
            Self::secondary(
                ui,
                "Only validated texture images are installed. Creator sign-in, paid access, quotas, and hosting restrictions are not bypassed.",
            );
        });
    }

    fn history(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let native = match self.game {
            GameId::Dusklight => None,
            GameId::Totk => Some(&snapshot.totk),
            GameId::Triforce => Some(&snapshot.triforce),
        };
        let history = if let Some(state) = native {
            &state.history
        } else {
            &snapshot.history
        };
        let current_hash = if let Some(state) = native {
            &state.local_hash
        } else {
            &snapshot.local_hash
        };
        let p = Palette::of(ui);
        Self::card(ui, |ui| {
            let stats = &history.stats;
            Self::section_title(ui, "Snapshot storage");
            ui.horizontal_wrapped(|ui| {
                for (value, label) in [
                    (stats.commits.to_string(), "Snapshots"),
                    (
                        format!("{:.2} MiB", stats.stored_bytes as f64 / 1_048_576.0),
                        "Stored",
                    ),
                    (
                        format!("{:.0} MiB", stats.budget as f64 / 1_048_576.0),
                        "Storage budget",
                    ),
                ] {
                    ui.vertical(|ui| {
                        ui.set_min_width(140.0);
                        ui.label(RichText::new(value).font(semibold(22.0)));
                        Self::secondary(ui, label);
                    });
                }
            });
            ui.add_space(8.0);
            ui.scope(|ui| {
                ui.visuals_mut().extreme_bg_color = p.disabled;
                ui.add(
                    egui::ProgressBar::new(
                        (stats.stored_bytes as f32 / stats.budget.max(1) as f32).clamp(0.0, 1.0),
                    )
                    .desired_width(ui.available_width())
                    .desired_height(6.0)
                    .corner_radius(2)
                    .fill(p.accent),
                );
            });
            Self::secondary(
                ui,
                "Unchanged blocks are shared. Oldest snapshots are pruned when the budget is reached; filesystem overhead is separate.",
            );
            if let Some(notice) = &history.notice {
                Self::notice(ui, Tone::Info, "Save history", notice);
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("Refresh / latest").clicked() {
                    self.dispatch(if native.is_some() {
                        Command::Native {
                            game: self.game,
                            command: NativeCommand::HistoryPage(None),
                        }
                    } else {
                        Command::HistoryPage(None)
                    });
                }
                if ui
                    .add_enabled(history.next.is_some(), egui::Button::new("Older snapshots"))
                    .clicked()
                {
                    self.dispatch(if native.is_some() {
                        Command::Native {
                            game: self.game,
                            command: NativeCommand::HistoryPage(history.next.clone()),
                        }
                    } else {
                        Command::HistoryPage(history.next.clone())
                    });
                }
            });
            ui.collapsing("How snapshots work", |ui| {
                Self::secondary(ui, format!("{} shared blocks; {:.2} MiB if stored as full copies; {} oldest snapshots pruned.", stats.blocks, stats.logical_bytes as f64 / 1_048_576.0, stats.pruned));
                Self::secondary(ui, "Valid saves are captured after two stable seconds, including in-game autosaves. Restore creates a new commit and retains normal sync conflict checks. A corrupt current file must be moved aside first.");
            });
        });
        ui.add_space(12.0);
        let can_restore = if let Some(state) = native {
            state.installed
                && !state.updating
                && !state.running
                && !state.remote_running
                && !state.busy
                && !state.launch_pending
                && state.lock.is_none()
                && state.remote_lock.is_none()
                && !snapshot.self_updates.preparing
        } else {
            snapshot.settings.save_file.is_some()
                && snapshot.save_profile.is_some()
                && snapshot.save_ready
                && Self::management_available(snapshot)
        };
        if history.entries.is_empty() {
            Self::notice(
                ui,
                Tone::Info,
                "No snapshots yet",
                if native.is_some() {
                    "No committed snapshots for this game yet."
                } else {
                    "Valid saves are recorded automatically from Dusklight's detected profile. Play and save in Dusklight, or receive your partner's save - no location setup is needed."
                },
            );
        }
        let mut nodes = std::collections::HashMap::new();
        for entry in &history.entries {
            ui.push_id(&entry.id, |ui| { ui.horizontal_top(|ui| {
                let (rail, response) = ui.allocate_exact_size(Vec2::new(60.0, 38.0), egui::Sense::hover());
                let partner_branch = entry.reason == crate::history::Reason::PeerBranch;
                let position = egui::pos2(rail.left() + if partner_branch { 44.0 } else { 14.0 }, rail.top() + 26.0);
                nodes.insert(entry.id.clone(), position);
                response.on_hover_text(if partner_branch { "Preserved partner branch" } else if entry.merge_parent.is_some() { "Reconciled save; both parent branches retained" } else { "Local save history" });
                ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 0.0), egui::Layout::top_down(egui::Align::Min), |ui| { Self::card(ui, |ui| {
                let current = current_hash.as_ref() == Some(&entry.hash);
                Self::setting_row(ui, &format!("#{}  {}", entry.sequence, if entry.merge_parent.is_some() { "Reconciled save" } else { entry.reason.label() }),
                    &format!("{}  /  {}", entry.timestamp, entry.username),
                    |ui| {
                        if ui.button("Preview").clicked() {
                            self.dispatch(Command::PreviewHistory { game: self.game, id: entry.id.clone() });
                        }
                        if current {
                            ui.label(RichText::new("Current save").color(p.success));
                        } else if ui.add_enabled(can_restore, egui::Button::new("Restore...")).clicked() {
                            if native.is_some() {
                                let approved = rfd::MessageDialog::new().set_title(format!("Restore {} snapshot", game_view::title(self.game)))
                                    .set_description(format!("Restore snapshot #{} from {}? The current active-account save is retained in history and rollback storage. Other local accounts and graphics settings are not changed.", entry.sequence, entry.timestamp))
                                    .set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes;
                                if approved { self.dispatch(Command::Native { game: self.game, command: NativeCommand::Restore { id: entry.id.clone(), expected: current_hash.clone() } }); }
                                return;
                            }
                            let Some(profile) = snapshot.save_profile.as_deref() else {
                                self.local_error = Some("Automatic Dusklight profile detection is not ready.".into());
                                return;
                            };
                            let destination = match crate::save_location::path_for_identity(profile, &crate::files::SaveIdentity {
                                game: entry.game.clone(), name: entry.name.clone(),
                            }) {
                                Ok(path) => path,
                                Err(error) => {
                                    self.local_error = Some(format!("{error:#}"));
                                    return;
                                }
                            };
                            let approved = rfd::MessageDialog::new().set_title("Restore save snapshot")
                                .set_description(format!("Restore {} / {} to {}?\n\nThe current valid save is snapshotted first. This creates a new commit; paired-device conflicts still need approval.", entry.game, entry.name, destination.display()))
                                .set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes;
                            if approved {
                                    self.dispatch(Command::RestoreSnapshot { id: entry.id.clone(), expected: current_hash.clone() });
                            }
                        }
                    },
                );
                if let Some(preview) = snapshot.history_preview.as_ref().filter(|preview| preview.game == self.game && preview.id == entry.id) {
                    ui.add_space(10.0);
                    game_view::history_preview(ui, preview);
                    if ui.button("Close preview").clicked() { self.dispatch(Command::ClearHistoryPreview); }
                }
                ui.collapsing("Snapshot details", |ui| {
                    Self::secondary(ui, format!("Game: {} / {}", entry.game, entry.name));
                    ui.label(RichText::new(format!("Commit: {}", entry.id)).monospace());
                    ui.label(RichText::new(format!("Save SHA-256: {}", entry.hash)).monospace());
                    if let Some(parent) = &entry.parent {
                        ui.label(RichText::new(format!("Parent: {parent}")).monospace());
                    }
                    if let Some(parent) = &entry.merge_parent {
                        ui.add(egui::Label::new(RichText::new(format!("Partner branch: {parent}")).monospace()).wrap());
                    }
                });
            }); }); }); });
            ui.add_space(8.0);
        }
        saves::history_graph(ui, &history.entries, &nodes, current_hash.as_deref());
    }

    fn security(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let p = Palette::of(ui);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Appearance");
            let mut appearance = self.appearance.appearance;
            Self::setting_row(
                ui,
                "App theme",
                "Use the Windows setting, or choose a light or dark appearance.",
                |ui| {
                    egui::ComboBox::from_id_salt("appearance")
                        .selected_text(appearance.label())
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            for choice in [Appearance::System, Appearance::Light, Appearance::Dark]
                            {
                                ui.selectable_value(&mut appearance, choice, choice.label());
                            }
                        });
                },
            );
            if appearance != self.appearance.appearance {
                match self.appearance.set(appearance) {
                    Ok(()) => appearance.apply(ui.ctx()),
                    Err(error) => self.local_error = Some(format!("{error:#}")),
                }
            }
        });
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Startup and background");
            let mut startup = snapshot.settings.startup;
            Self::setting_row(
                ui,
                "Start with Windows",
                "Start in the system tray when you sign in. Enabled by default on first launch.",
                |ui| {
                    if ui.checkbox(&mut startup, "Enabled").changed() {
                        self.dispatch(Command::SetStartup(startup));
                    }
                },
            );
            ui.separator();
            Self::secondary(
                ui,
                "Closing the window keeps sync running in the tray. Double-click the tray icon to reopen it; use its Quit action to exit.",
            );
        });
        ui.add_space(12.0);
        self.app_updates(ui, snapshot);
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Privacy and local data");
            Self::secondary(
                ui,
                "Logs contain your username, paths, hashes, peer identities, and security decisions. Keep them private.",
            );
            if ui.button("Open application data folder").clicked()
                && let Err(error) = open_data_directory()
            {
                self.local_error = Some(format!("{error:#}"));
            }
            ui.collapsing("Your device identity", |ui| {
                ui.label(RichText::new(&snapshot.public_key).monospace());
            });
        });
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Security");
            Self::status(ui, "Channel", "twilightProto 1 / Noise XX", p.success);
            ui.collapsing("Security details and limitations", |ui| {
            for (title, detail) in [
                (
                    "twilightProto / 1",
                    "Bounded binary events over TCP. Noise XX with X25519, ChaCha20-Poly1305, and SHA-256. Directional keys rotate every 4,096 events.",
                ),
                (
                    "Router-bound, identity-pinned",
                    "Private IPv4 + direct subnet + physical adapter + approved gateway fingerprint. Noise also binds the network tag. Router MACs are spoofable, so bilateral pairing remains essential.",
                ),
                (
                    "Data, never a peer executable",
                    "GCI folders and raw memory cards are supported. Only the validated Twilight Princess save crosses the LAN; other card contents stay local. Sector and quest checksums are verified. No mod scripts, ZIP saves, or executable attachments.",
                ),
                (
                    "Honest limitations",
                    "Validation cannot prove a save contains no exploit for every Dusklight build. Exclusive play protects managed launches; direct launches and a compromised OS can bypass it. Keep Dusklight updated.",
                ),
            ] {
                ui.label(RichText::new(title).font(semibold(14.0)));
                ui.label(detail);
                ui.add_space(8.0);
            }
            });
        });
    }

    fn approvals(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let p = Palette::of(ui);
        for game in [GameId::Dusklight, GameId::Totk, GameId::Triforce] {
            let model = game_view::view(game, snapshot);
            if let Some(action) = game_view::approval(ui, &model) {
                if action == game_view::Action::Compare {
                    self.game = game;
                }
                self.game_action(snapshot, game, action);
            }
        }
        if snapshot.pending_iso {
            Self::card(ui, |ui| {
                ui.label(
                    RichText::new("Incoming disc image - explicit permission required")
                        .color(p.warning)
                        .font(semibold(16.0)),
                );
                ui.horizontal_wrapped(|ui| {
                    if let Some(format) = snapshot.pending_disc_format {
                        if ui
                            .button(format!(
                                "I own this game - choose a new {} destination",
                                format.label()
                            ))
                            .clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter(
                                    format!("GameCube {}", format.label()),
                                    &[format.extension()],
                                )
                                .set_file_name(format!("Twilight Princess.{}", format.extension()))
                                .save_file()
                        {
                            self.dispatch(Command::AcceptIso(Some(path)));
                        }
                    } else {
                        ui.label(
                            RichText::new("Image format missing; decline and request a new offer.")
                                .color(p.error),
                        );
                    }
                    if ui.button("Decline image").clicked() {
                        self.dispatch(Command::AcceptIso(None));
                    }
                });
            });
            ui.add_space(12.0);
        }
    }

    fn navigation_button(&mut self, ui: &mut egui::Ui, page: Page, compact: bool) {
        let p = Palette::of(ui);
        let selected = self.page == page;
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), egui::Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::SelectableLabel,
                true,
                selected,
                page.title(),
            )
        });
        if response.gained_focus() {
            response.scroll_to_me(Some(egui::Align::Center));
        }
        if selected || response.hovered() {
            ui.painter()
                .rect_filled(rect, 4, if selected { p.selection } else { p.alternate });
        }
        if selected {
            let marker = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 2.0, rect.center().y),
                Vec2::new(3.0, 20.0),
            );
            ui.painter().rect_filled(marker, 2, p.accent);
        }
        if response.has_focus() {
            ui.painter().rect_stroke(
                rect.shrink(1.0),
                4,
                Stroke::new(1.5_f32, p.accent),
                egui::StrokeKind::Inside,
            );
        }
        let foreground = if selected { p.accent } else { p.text };
        ui.painter().text(
            egui::pos2(
                if compact {
                    rect.center().x
                } else {
                    rect.left() + 22.0
                },
                rect.center().y,
            ),
            egui::Align2::CENTER_CENTER,
            page.icon(),
            egui::FontId::new(18.0, egui::FontFamily::Name("Windows icons".into())),
            foreground,
        );
        if !compact {
            ui.painter().text(
                egui::pos2(rect.left() + 46.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                if page == Page::Totk {
                    "TotK saves"
                } else {
                    page.title()
                },
                egui::FontId::proportional(14.0),
                foreground,
            );
        }
        if response.clicked()
            || (response.has_focus()
                && ui.input(|input| {
                    input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Space)
                }))
        {
            response.request_focus();
            self.page = page;
            if page == Page::Totk {
                self.dispatch(Command::RefreshTotk);
            }
        }
        response.on_hover_text(page.title());
    }
}

impl eframe::App for Dashboard {
    fn update(&mut self, context: &egui::Context, _: &mut eframe::Frame) {
        if let Some(game) = self.background.take_open_game_controls() {
            self.start_hidden = false;
            self.page = Page::Overview;
            self.game = game;
            if game != GameId::Dusklight {
                self.dispatch(Command::Native {
                    game,
                    command: NativeCommand::Refresh,
                });
            }
        }
        self.tray_events();
        let snapshot = match self.backend.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                egui::CentralPanel::default().show(context, |ui| {
                    ui.label(format!("Manager unavailable: {error:#}"));
                });
                return;
            }
        };
        if snapshot.comparison_code.is_some() && snapshot.comparison_code != self.shown_pairing_code
        {
            self.page = Page::Devices;
        }
        self.shown_pairing_code = snapshot.comparison_code.clone();
        if snapshot.shutdown {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if std::mem::take(&mut self.start_hidden) {
            // eframe reveals its first frame even when the initial viewport is hidden.
            context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if context.input(|i| i.viewport().close_requested()) {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        let p = Palette::new(context.theme() == egui::Theme::Dark);
        let compact = context.content_rect().width() < 760.0;
        egui::SidePanel::left("navigation")
            .exact_width(if compact { 64.0 } else { 208.0 })
            .resizable(false)
            .frame(egui::Frame::new().fill(p.canvas).inner_margin(8))
            .show(context, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let show_brand = ui.available_height() >= 360.0;
                if show_brand && compact {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(symbol("\u{e708}", 22.0).color(p.accent))
                            .on_hover_text("Twilight Sync");
                    });
                } else if show_brand {
                    ui.add_space(4.0);
                    ui.label(RichText::new("Twilight Sync").font(semibold(18.0)));
                    Self::secondary(ui, "Local save manager");
                }
                if show_brand {
                    ui.add_space(20.0);
                }
                let footer_height = if compact { 48.0 } else { 72.0 };
                let menu_height = (ui.available_height() - footer_height).max(40.0);
                egui::ScrollArea::vertical()
                    .id_salt("navigation-menu")
                    .max_height(menu_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for page in Page::ALL.into_iter().filter(|page| *page != Page::Security) {
                            self.navigation_button(ui, page, compact);
                        }
                    });
                self.navigation_button(ui, Page::Security, compact);
                if !compact {
                    ui.label(
                        RichText::new(concat!("Version ", env!("CARGO_PKG_VERSION")))
                            .small()
                            .color(p.secondary),
                    );
                }
            });
        egui::TopBottomPanel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(p.canvas)
                    .inner_margin(egui::Margin::symmetric(16, 4)),
            )
            .show(context, |ui| {
                ui.spacing_mut().interact_size.y = 18.0;
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(if snapshot.ready {
                            "Connected securely"
                        } else if snapshot.connecting {
                            "Connecting..."
                        } else {
                            "Not connected"
                        })
                        .color(if snapshot.ready {
                            p.success
                        } else {
                            p.secondary
                        })
                        .size(12.0),
                    );
                    if !compact {
                        ui.separator();
                        ui.label(
                            RichText::new("Closing the window keeps sync running in the tray.")
                                .color(p.secondary)
                                .size(12.0),
                        );
                    }
                });
            });
        self.tray
            .launch_item
            .set_text(format!("Play {}", game_view::title(self.game)));
        self.tray.sync_item.set_text(format!(
            "Send {} save to partner",
            game_view::title(self.game)
        ));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.canvas)
                    .inner_margin(if compact { 16 } else { 24 }),
            )
            .show(context, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(("page-content", self.page))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let confirming_device =
                            self.page == Page::Devices && snapshot.comparison_code.is_some();
                        if confirming_device {
                            ui.heading("Confirm device");
                            ui.add_space(12.0);
                        } else {
                            Self::header(ui, self.page);
                        }
                        if matches!(
                            self.page,
                            Page::Overview
                                | Page::Library
                                | Page::History
                                | Page::Totk
                                | Page::Triforce
                        ) {
                            if Self::game_picker(ui, &mut self.game) {
                                self.shown_fork = None;
                                self.fork_dialog_open = false;
                                self.fork_choice = None;
                                if let Some(path) = &self.game_view_path {
                                    match serde_json::to_vec(&self.game)
                                        .map_err(anyhow::Error::from)
                                        .and_then(|bytes| crate::files::atomic_write(path, &bytes))
                                    {
                                        Ok(()) => {}
                                        Err(error) => {
                                            self.local_error = Some(format!(
                                                "Cannot remember selected game: {error:#}"
                                            ))
                                        }
                                    }
                                }
                                if self.game != GameId::Dusklight {
                                    self.dispatch(Command::Native {
                                        game: self.game,
                                        command: NativeCommand::Refresh,
                                    });
                                    self.dispatch(Command::Native {
                                        game: self.game,
                                        command: NativeCommand::HistoryPage(None),
                                    });
                                } else if self.page == Page::History {
                                    self.dispatch(Command::HistoryPage(None));
                                }
                            }
                            ui.add_space(16.0);
                        }
                        let background_error = self.background.error();
                        let error = self
                            .local_error
                            .as_ref()
                            .or(background_error.as_ref())
                            .or(snapshot.error.as_ref())
                            .cloned();
                        if !confirming_device && let Some(error) = &error {
                            if Self::action_needed(ui, self.game, &snapshot, error) {
                                self.page = Page::Library;
                                self.local_error = None;
                            }
                            ui.add_space(12.0);
                        }
                        if !snapshot.transfer.is_empty() {
                            Self::notice(
                                ui,
                                Tone::Info,
                                "Transfer in progress",
                                &snapshot.transfer,
                            );
                            ui.add_space(8.0);
                        }
                        self.approvals(ui, &snapshot);
                        match self.page {
                            Page::Overview => self.game_home(ui, &snapshot),
                            Page::Totk => self.game_home(ui, &snapshot),
                            Page::Triforce => self.game_home(ui, &snapshot),
                            Page::Devices => self.devices(ui, &snapshot),
                            Page::Library => self.game_library(ui, &snapshot),
                            Page::Textures => self.textures(ui, &snapshot),
                            Page::History => self.history(ui, &snapshot),
                            Page::Security => self.security(ui, &snapshot),
                        }
                        if confirming_device && let Some(error) = &error {
                            ui.add_space(12.0);
                            Self::notice(ui, Tone::Warning, "Previous action", error);
                        }
                        if self.page == Page::Overview
                            && (snapshot.self_updates.available()
                                || snapshot.self_updates.preparing
                                || snapshot.self_updates.error.is_some())
                        {
                            ui.add_space(12.0);
                            self.app_updates(ui, &snapshot);
                        }
                        ui.add_space(8.0);
                    });
            });
        self.reconcile_prompt(context, &snapshot);
        context.request_repaint_after(Duration::from_millis(200));
    }
}

fn open_build_log(path: &std::path::Path) -> Result<()> {
    crate::files::ensure_safe_path(path)?;
    anyhow::ensure!(path.is_file(), "The build log is not available yet");
    let windows = std::env::var_os("SystemRoot").context("SystemRoot is not available")?;
    std::process::Command::new(PathBuf::from(windows).join("System32").join("notepad.exe"))
        .arg(path)
        .spawn()
        .context("Opening the local build log")?;
    Ok(())
}

fn open_data_directory() -> Result<()> {
    let home = directories::UserDirs::new().context("Cannot find home directory")?;
    let directory = home.home_dir().join(".twilight-sync");
    crate::files::ensure_safe_path(&directory)?;
    let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    std::process::Command::new(PathBuf::from(windows).join("explorer.exe"))
        .arg(directory)
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_snapshot() -> Result<Snapshot> {
        let backend = crate::runtime::ui_smoke_backend("ready")?;
        let snapshot = backend.snapshot()?;
        backend.command(Command::Shutdown)?;
        Ok(snapshot)
    }

    #[test]
    fn disabled_totk_controls_explain_the_specific_state_without_bypassing_safety() -> Result<()> {
        let mut snapshot = ready_snapshot()?;
        snapshot.totk.checked = true;
        snapshot.totk.installed = true;
        snapshot.totk.peer_checked = true;
        snapshot.totk.remote_installed = true;
        snapshot.totk.remote_save_ready = true;
        snapshot.totk.local_hash = Some("a".repeat(64));
        snapshot.totk.remote_hash = Some("a".repeat(64));
        assert!(Dashboard::native_play_blocker(&snapshot, &snapshot.totk).is_none());
        assert!(
            Dashboard::native_sync_blocker(&snapshot, &snapshot.totk)
                .is_some_and(|reason| reason.contains("already match"))
        );
        snapshot.totk.remote_hash = Some("b".repeat(64));
        assert!(Dashboard::native_sync_blocker(&snapshot, &snapshot.totk).is_none());
        snapshot.totk.pending = true;
        assert!(
            Dashboard::native_play_blocker(&snapshot, &snapshot.totk)
                .is_some_and(|reason| reason.contains("decision"))
        );
        snapshot.totk.pending = false;
        snapshot.totk.lock = Some(crate::config::PlayLock {
            owner: snapshot.public_key.clone(),
            token: "c".repeat(64),
        });
        assert!(
            Dashboard::native_sync_blocker(&snapshot, &snapshot.totk)
                .is_some_and(|reason| reason.contains("recovery"))
        );
        snapshot.totk.lock = None;
        snapshot.totk.running = true;
        assert!(
            Dashboard::native_play_blocker(&snapshot, &snapshot.totk)
                .is_some_and(|reason| reason.contains("running on this PC"))
        );
        snapshot.totk.running = false;
        snapshot.totk.release_pending = true;
        assert!(
            Dashboard::native_play_blocker(&snapshot, &snapshot.totk)
                .is_some_and(|reason| reason.contains("acknowledgement"))
        );
        snapshot.ready = false;
        assert!(Dashboard::native_play_blocker(&snapshot, &snapshot.totk).is_none());
        assert!(
            Dashboard::native_sync_blocker(&snapshot, &snapshot.totk)
                .is_some_and(|reason| reason.contains("offline"))
        );
        Ok(())
    }

    #[test]
    fn game_selector_and_totk_comparisons_fit_light_dark_and_narrow_layouts() -> Result<()> {
        let summary = crate::totk::Summary {
            files: 32,
            bytes: 17_897_203,
            album_files: 6,
            slots: (0..6)
                .map(|number| crate::totk::SlotSummary {
                    number,
                    files: 4,
                    bytes: 2_945_388,
                    hash: "a".repeat(64),
                    details: Some(crate::totk_save::Details {
                        format: "1.4".into(),
                        max_hearts: Some(4.0),
                        rupees: Some(81),
                        stamina_wheels: Some(1.0),
                        play_seconds: Some(18_293),
                        map: Some("Dungeon136".into()),
                        saved_at: Some("2026-11-04 14:01".into()),
                        autosave: Some(number != 1),
                        ..crate::totk_save::Details::default()
                    }),
                    details_error: None,
                })
                .collect(),
        };
        let mut partner = summary.clone();
        partner.slots[2].hash = "b".repeat(64);
        for width in [260.0, 360.0, 640.0, 960.0] {
            for dark in [false, true] {
                let context = egui::Context::default();
                design::install(&context)?;
                context.set_visuals(if dark {
                    egui::Visuals::dark()
                } else {
                    egui::Visuals::light()
                });
                let mut game = GameId::Totk;
                let output = context.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(width, 1200.0),
                        )),
                        ..Default::default()
                    },
                    |context| {
                        egui::CentralPanel::default().show(context, |ui| {
                            Dashboard::game_picker(ui, &mut game);
                            game_view::progress(ui, &game_view::totk_cards(&summary));
                            game_view::progress(ui, &game_view::totk_cards(&partner));
                        });
                    },
                );
                let mut text_count = 0;
                for clipped in &output.shapes {
                    if let egui::Shape::Text(text) = &clipped.shape {
                        let bounds = text.galley.rect.translate(text.pos.to_vec2());
                        assert!(
                            bounds.left() >= -1.0 && bounds.right() <= width + 1.0,
                            "Game UI overflow at {width}px: {}",
                            text.galley.job.text
                        );
                        text_count += 1;
                    }
                }
                assert!(text_count > 10);
                assert_eq!(game, GameId::Totk);
            }
        }
        Ok(())
    }

    #[test]
    fn triforce_decoded_details_reach_progress_cards_in_latest_save_order() -> Result<()> {
        let temp = tempfile::tempdir()?;
        crate::triforce::tests::seed(temp.path(), 1)?;
        let account = temp
            .path()
            .join("save")
            .join(format!("{:032x}", crate::triforce::ACCOUNT));
        for number in [0, 1] {
            let (progress, mut caption) = crate::triforce_save::tests::fixture(number * 336);
            caption[0x4c..0x50].copy_from_slice(&(19 + number as u32).to_le_bytes());
            let slot = account.join(format!("slot_{number:02}"));
            std::fs::create_dir_all(&slot)?;
            std::fs::write(slot.join("progress.sav"), progress)?;
            std::fs::write(slot.join("caption.sav"), caption)?;
        }
        let save = crate::triforce::snapshot(temp.path())?.context("Missing fixture save")?;
        let summary = crate::triforce::summary(&save.bytes)?;
        let cards = game_view::totk_cards(&summary);
        assert_eq!(cards[0].number, 2);
        assert_eq!(cards[0].kind, "Manual save");
        assert_eq!(cards[0].hearts, Some(12.0));
        assert_eq!(cards[0].rupees, Some(888));
        assert_eq!(cards[0].played, Some(146_558));
        assert_eq!(
            cards[0].saved.as_deref(),
            Some("2026-11-17 01:20 (game clock)")
        );
        assert!(cards.iter().all(|card| card.warning.is_none()));
        Ok(())
    }

    #[test]
    fn missing_totk_installation_error_offers_automatic_setup_not_an_external_download()
    -> Result<()> {
        let mut snapshot = ready_snapshot()?;
        for (game, installed, expected_link) in [
            (GameId::Dusklight, false, false),
            (GameId::Totk, true, false),
            (GameId::Totk, false, true),
        ] {
            snapshot.totk.installed = installed;
            let context = egui::Context::default();
            design::install(&context)?;
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(640.0, 360.0));
            let selected = std::cell::Cell::new(false);
            let render = |context: &egui::Context| {
                egui::CentralPanel::default().show(context, |ui| {
                    selected.set(Dashboard::action_needed(
                        ui,
                        game,
                        &snapshot,
                        "Choose your TotK base game before the automatic first installation.",
                    ));
                });
            };
            let output = context.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                },
                render,
            );
            let link = output.shapes.iter().find_map(|clipped| {
                if let egui::Shape::Text(text) = &clipped.shape
                    && text.galley.job.text == "Set up TotK automatically"
                {
                    Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                } else {
                    None
                }
            });
            assert_eq!(link.is_some(), expected_link);
            if let Some(position) = link {
                let clicked = context.run(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        events: vec![
                            egui::Event::PointerMoved(position),
                            egui::Event::PointerButton {
                                pos: position,
                                button: egui::PointerButton::Primary,
                                pressed: true,
                                modifiers: egui::Modifiers::default(),
                            },
                            egui::Event::PointerButton {
                                pos: position,
                                button: egui::PointerButton::Primary,
                                pressed: false,
                                modifiers: egui::Modifiers::default(),
                            },
                        ],
                        ..Default::default()
                    },
                    render,
                );
                assert!(
                    selected.get(),
                    "The setup action must navigate to managed installation"
                );
                assert!(
                    !clicked
                        .platform_output
                        .commands
                        .iter()
                        .any(|command| { matches!(command, egui::OutputCommand::OpenUrl(_)) }),
                    "Automatic setup must not redirect to a manual installer download"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn totk_updates_block_game_actions_and_explain_when_updates_are_unsafe() -> Result<()> {
        let mut ready = ready_snapshot()?;
        ready.totk = crate::totk::Status {
            checked: true,
            installed: true,
            install_path: Some(PathBuf::from(r"C:\Games\TotKRecomp")),
            ..crate::totk::Status::default()
        };
        assert!(Dashboard::totk_update_blocker(&ready).is_none());
        ready.totk.installed = false;
        assert!(Dashboard::totk_update_blocker(&ready).is_none());
        ready.totk.installed = true;
        let available = game_view::view(GameId::Totk, &ready);
        assert!(available.can_locate && available.can_continue && available.can_accept);
        for condition in [
            "running",
            "remote-running",
            "updating",
            "transfer",
            "lock",
            "launch",
            "release",
            "recovery",
        ] {
            let mut snapshot = ready.clone();
            match condition {
                "running" => snapshot.totk.running = true,
                "remote-running" => snapshot.totk.remote_running = true,
                "updating" => snapshot.totk.updating = true,
                "transfer" => snapshot.totk.busy = true,
                "lock" => {
                    snapshot.totk.lock = Some(crate::config::PlayLock {
                        owner: snapshot.public_key.clone(),
                        token: "a".repeat(64),
                    })
                }
                "launch" => snapshot.totk.launch_pending = true,
                "release" => snapshot.totk.release_pending = true,
                "recovery" => snapshot.totk.recovery_pending = true,
                _ => unreachable!(),
            }
            assert!(
                Dashboard::totk_update_blocker(&snapshot).is_some(),
                "{condition}"
            );
        }
        ready.totk.updating = true;
        let updating = game_view::view(GameId::Totk, &ready);
        assert!(!updating.can_locate && !updating.can_continue && !updating.can_accept);
        assert!(
            updating
                .play_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("updated"))
        );
        assert!(
            updating
                .sync_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("updated"))
        );
        ready.totk.updating = false;
        ready.totk.verification_error = Some("An incomplete update needs retrying".into());
        assert!(Dashboard::totk_update_blocker(&ready).is_none());
        Ok(())
    }

    #[test]
    fn triforce_view_and_consent_do_not_follow_another_games_state() -> Result<()> {
        let mut snapshot = ready_snapshot()?;
        snapshot.totk.installed = true;
        snapshot.triforce = crate::totk::Status::default();
        assert!(!game_view::view(GameId::Triforce, &snapshot).installed);
        snapshot.triforce = crate::totk::Status {
            checked: true,
            installed: true,
            local_hash: Some("a".repeat(64)),
            remote_hash: Some("b".repeat(64)),
            peer_checked: true,
            remote_installed: true,
            remote_save_ready: true,
            conflict: true,
            comparison_verified: true,
            ..crate::totk::Status::default()
        };
        snapshot.triforce_supported = true;
        let model = game_view::view(GameId::Triforce, &snapshot);
        assert!(model.conflict && model.can_resolve);
        assert!(matches!(
            game_view::incoming_command(GameId::Triforce, false),
            Command::Native {
                game: GameId::Triforce,
                command: NativeCommand::Accept(false)
            }
        ));
        snapshot.triforce.updating = true;
        let paused = game_view::view(GameId::Triforce, &snapshot);
        assert!(!paused.can_continue && !paused.can_accept && !paused.can_resolve);
        assert!(
            paused
                .play_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("rebuilt"))
        );
        Ok(())
    }

    #[test]
    fn source_build_controls_explain_activity_and_reservations_in_every_game() -> Result<()> {
        let mut snapshot = ready_snapshot()?;
        assert!(Dashboard::source_build_blocker(&snapshot).is_none());
        snapshot.triforce.running = true;
        assert!(
            Dashboard::source_build_blocker(&snapshot)
                .is_some_and(|reason| reason.contains("Close all games"))
        );
        snapshot.triforce.running = false;
        snapshot.totk.lock = Some(crate::config::PlayLock {
            owner: snapshot.public_key.clone(),
            token: "a".repeat(64),
        });
        assert!(
            Dashboard::source_build_blocker(&snapshot)
                .is_some_and(|reason| reason.contains("reservations"))
        );
        snapshot.totk.lock = None;
        snapshot.triforce.busy = true;
        assert!(
            Dashboard::source_build_blocker(&snapshot)
                .is_some_and(|reason| reason.contains("transfers"))
        );
        Ok(())
    }

    #[test]
    fn app_update_button_ignores_stored_locks_but_explains_active_work() -> Result<()> {
        let mut snapshot = ready_snapshot()?;
        let lock = crate::config::PlayLock {
            owner: snapshot.public_key.clone(),
            token: "a".repeat(64),
        };
        snapshot.settings.play_lock = Some(lock.clone());
        snapshot.totk.lock = Some(lock.clone());
        snapshot.totk.remote_lock = Some(lock);
        assert!(Dashboard::update_blocker(&snapshot).is_none());
        assert!(!Dashboard::management_available(&snapshot));
        snapshot.totk.running = true;
        assert!(
            Dashboard::update_blocker(&snapshot)
                .is_some_and(|reason| reason.contains("running game"))
        );
        snapshot.totk.running = false;
        snapshot.totk.busy = true;
        assert!(
            Dashboard::update_blocker(&snapshot).is_some_and(|reason| reason.contains("pending"))
        );
        snapshot.totk.busy = false;
        assert!(Dashboard::update_blocker(&snapshot).is_none());
        assert!(snapshot.settings.play_lock.is_some() && snapshot.totk.lock.is_some());
        Ok(())
    }

    #[test]
    fn play_controls_allow_offline_but_never_busy_or_locked_devices() -> Result<()> {
        let ready = ready_snapshot()?;
        assert!(Dashboard::can_launch(&ready));
        let mut offline = ready.clone();
        offline.ready = false;
        assert!(Dashboard::can_launch(&offline));
        for condition in [
            "local-game",
            "remote-game",
            "lock",
            "install",
            "textures",
            "update",
        ] {
            let mut snapshot = ready.clone();
            match condition {
                "local-game" => snapshot.game_running = true,
                "remote-game" => snapshot.remote_game = true,
                "lock" => {
                    snapshot.settings.play_lock = Some(crate::config::PlayLock {
                        owner: snapshot.public_key.clone(),
                        token: "d".repeat(64),
                    })
                }
                "install" => snapshot.install_busy = true,
                "textures" => snapshot.texture_busy = true,
                "update" => snapshot.self_updates.preparing = true,
                _ => unreachable!(),
            }
            assert!(!Dashboard::can_launch(&snapshot), "{condition}");
        }
        Ok(())
    }

    #[test]
    fn play_controls_require_files_and_no_pending_consent_or_transfer() -> Result<()> {
        let ready = ready_snapshot()?;
        for condition in [
            "save",
            "profile",
            "save-not-ready",
            "game",
            "save-consent",
            "iso-consent",
            "recovery",
            "transfer",
            "texture-recovery",
        ] {
            let mut snapshot = ready.clone();
            match condition {
                "save" => snapshot.settings.save_file = None,
                "profile" => snapshot.save_profile = None,
                "save-not-ready" => snapshot.save_ready = false,
                "game" => snapshot.settings.game_exe = None,
                "save-consent" => snapshot.pending_save = true,
                "iso-consent" => snapshot.pending_iso = true,
                "recovery" => snapshot.pending_recovery = true,
                "transfer" => snapshot.transfer = "Receiving...".into(),
                "texture-recovery" => snapshot.textures.recovery_needed = true,
                _ => unreachable!(),
            }
            assert!(!Dashboard::can_launch(&snapshot), "{condition}");
        }
        Ok(())
    }

    #[test]
    fn reconciliation_needs_matching_save_identities_but_not_a_game_installation() -> Result<()> {
        let mut snapshot = ready_snapshot()?;
        let summary = crate::save_data::inspect(&crate::save_data::tests::populated(true)?)?;
        snapshot.local_save = Some(summary.clone());
        snapshot.conflict = Some(crate::runtime::SaveConflict {
            local_hash: "a".repeat(64),
            remote_hash: "b".repeat(64),
            base_hash: None,
            base: None,
            remote: Some(summary),
        });
        snapshot.settings.game_exe = None;
        assert!(Dashboard::can_reconcile(&snapshot));
        assert!(!Dashboard::can_launch(&snapshot));
        snapshot.ready = false;
        assert!(!Dashboard::can_reconcile(&snapshot));
        snapshot.ready = true;
        snapshot.pending_iso = true;
        assert!(!Dashboard::can_reconcile(&snapshot));
        snapshot.pending_iso = false;
        if let Some(remote) = snapshot
            .conflict
            .as_mut()
            .and_then(|conflict| conflict.remote.as_mut())
        {
            remote.game = "GZ2J".into();
        }
        assert!(!Dashboard::can_reconcile(&snapshot));
        Ok(())
    }
}

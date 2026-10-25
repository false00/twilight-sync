use crate::runtime::{Backend, Command, Snapshot};
use anyhow::{Context, Result};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

const WINDOW_TITLE: &str = "Twilight Sync";

mod background;
mod design;
mod icons;

use design::{Appearance, AppearanceStore, Palette, semibold, symbol};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Page {
    Overview,
    Devices,
    Library,
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
            Self::Library => "Dusklight",
            Self::Textures => "Texture packs",
            Self::History => "Save history",
            Self::Security => "Settings",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Overview => "Your connection, save files, and protected play.",
            Self::Devices => "Approve your network and pair one trusted device.",
            Self::Library => "Install the game, manage updates, and select your own disc image.",
            Self::Textures => "Import authorized texture packs and choose the active version.",
            Self::History => "Browse snapshots and safely restore an earlier save.",
            Self::Security => "Appearance, startup, application updates, and privacy.",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Overview => "\u{e80f}",
            Self::Devices => "\u{e772}",
            Self::Library => "\u{e7fc}",
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
    quit: MenuId,
}

struct Dashboard {
    backend: Backend,
    tray: Tray,
    page: Page,
    local_error: Option<String>,
    events: crossbeam_channel::Receiver<MenuEvent>,
    background: background::BackgroundGuard,
    start_hidden: bool,
    texture_source: String,
    texture_version: String,
    texture_permission: bool,
    appearance: AppearanceStore,
    shown_pairing_code: Option<String>,
}

fn make_tray(startup: bool, automatic_updates: bool) -> Result<Tray> {
    let pixels = icons::tray()?;
    let menu = Menu::new();
    let status = MenuItem::new("Checking private LAN...", false, None);
    let show = MenuItem::new("Open Twilight Sync", true, None);
    let sync = MenuItem::new("Send save to partner", true, None);
    let launch = MenuItem::new("Play Dusklight (protected)", true, None);
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
                page: initial_page,
                local_error: None,
                events,
                background: background_guard,
                start_hidden: background,
                texture_source: "henriko-4k".into(),
                texture_version: format!("Imported {}", time::OffsetDateTime::now_utc().date()),
                texture_permission: false,
                appearance,
                shown_pairing_code: None,
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
                self.dispatch(Command::SendSave);
            } else if event.id == self.tray.launch {
                self.dispatch(Command::Launch);
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
                let safe = Self::management_available(snapshot) && !snapshot.self_updates.checking;
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
                    });
                });
            });
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

    fn management_available(snapshot: &Snapshot) -> bool {
        !snapshot.game_running
            && !snapshot.remote_game
            && snapshot.settings.play_lock.is_none()
            && !snapshot.install_busy
            && !snapshot.texture_busy
            && !snapshot.self_updates.preparing
    }

    fn can_launch(snapshot: &Snapshot) -> bool {
        snapshot.ready
            && Self::management_available(snapshot)
            && !snapshot.textures.recovery_needed
            && snapshot.settings.save_file.is_some()
            && snapshot.save_profile.is_some()
            && snapshot.save_ready
            && snapshot.settings.game_exe.is_some()
            && !snapshot.pending_save
            && !snapshot.pending_iso
            && !snapshot.pending_recovery
            && snapshot.transfer.is_empty()
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

    fn overview(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let p = Palette::of(ui);
        Self::card(ui, |ui| {
            let (label, color) = if snapshot.game_running {
                ("Dusklight is running on this PC", p.warning)
            } else if snapshot.remote_game {
                ("Your partner is playing", p.warning)
            } else if snapshot.ready {
                ("Connected securely", p.success)
            } else if snapshot.settings.peer.is_some() {
                ("Waiting for your paired device", p.secondary)
            } else {
                ("Connect your other PC", p.text)
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(symbol("\u{e968}", 22.0).color(color));
                ui.label(RichText::new(label).font(semibold(20.0)).color(color));
            });
            if let Some(peer) = &snapshot.settings.peer {
                Self::secondary(ui, format!("Partner: {}", peer.name));
            }
            Self::secondary(ui, &snapshot.message);
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                if snapshot.ready {
                    if Self::primary(ui, Self::can_launch(snapshot), "Play Dusklight").clicked() {
                        self.dispatch(Command::Launch);
                    }
                    if ui
                        .add_enabled(
                            snapshot.save_ready
                                && snapshot.local_hash.is_some()
                                && Self::management_available(snapshot)
                                && snapshot.transfer.is_empty(),
                            egui::Button::new("Send this save"),
                        )
                        .clicked()
                    {
                        self.dispatch(Command::SendSave);
                    }
                    if ui.button("Connection details").clicked() {
                        self.page = Page::Devices;
                    }
                } else {
                    if Self::primary(ui, true, "Set up connection").clicked() {
                        self.page = Page::Devices;
                    }
                    if snapshot.settings.game_exe.is_none()
                        && ui.button("Set up Dusklight").clicked()
                    {
                        self.page = Page::Library;
                    }
                }
            });
            if !snapshot.ready {
                Self::secondary(ui, "Protected play needs both paired managers online.");
            } else if snapshot.settings.game_exe.is_none() {
                Self::secondary(
                    ui,
                    "Select or install Dusklight on the Dusklight page before playing.",
                );
            } else if snapshot.settings.save_file.is_none() {
                Self::secondary(
                    ui,
                    "Automatic save detection needs attention. Check the status message; no folder selection is needed.",
                );
            }
            if snapshot.ready && snapshot.settings.play_lock.is_none() {
                Self::secondary(ui, "Protected launch allows one game session at a time.");
            }
        });

        let network_approved = snapshot.network.as_ref().is_some_and(|network| {
            snapshot.settings.network.as_deref() == Some(network.fingerprint.as_str())
        });
        if !network_approved
            || snapshot.settings.peer.is_none()
            || snapshot.settings.game_exe.is_none()
        {
            ui.add_space(12.0);
            Self::card(ui, |ui| {
                Self::section_title(ui, "Finish setup");
                if !network_approved {
                    Self::setting_row(
                        ui,
                        "Approve your home network",
                        "Check the router on each PC before enabling discovery.",
                        |ui| {
                            if ui.button("Open devices").clicked() {
                                self.page = Page::Devices;
                            }
                        },
                    );
                }
                if snapshot.settings.peer.is_none() {
                    if !network_approved {
                        ui.separator();
                    }
                    Self::setting_row(
                        ui,
                        "Pair your other PC",
                        "Open pairing on both. Compare codes from only one device.",
                        |ui| {
                            if ui.button("Pair a device").clicked() {
                                self.page = Page::Devices;
                            }
                        },
                    );
                }
                if snapshot.settings.game_exe.is_none() {
                    ui.separator();
                    Self::setting_row(
                        ui,
                        "Locate or install Dusklight",
                        "Use your own game and disc image. Nothing launches automatically.",
                        |ui| {
                            if ui.button("Open Dusklight").clicked() {
                                self.page = Page::Library;
                            }
                        },
                    );
                }
            });
        }
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Automatic save sync");
            let matching =
                snapshot.local_hash.is_some() && snapshot.local_hash == snapshot.remote_hash;
            let (state, color) = if matching {
                ("Both copies match", p.success)
            } else if snapshot.local_hash.is_some() && snapshot.remote_hash.is_some() {
                ("Different copies - approval may be required", p.warning)
            } else if snapshot.local_hash.is_some() {
                ("Local save is ready", p.success)
            } else if snapshot.settings.save_file.is_some() && snapshot.save_ready {
                (
                    "No save yet - ready for Dusklight or your partner",
                    p.secondary,
                )
            } else {
                ("Automatic detection needs attention", p.warning)
            };
            Self::status(ui, "Status", state, color);
            Self::secondary(
                ui,
                "Uses the standard save inside Dusklight's active profile, including its own portable or custom profile. No folders to choose.",
            );
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("View history").clicked() {
                    self.page = Page::History;
                }
            });
            Self::secondary(ui, "A PC without a save is ready to receive automatically.");
            ui.collapsing("Save details", |ui| {
                if let Some(path) = &snapshot.settings.save_file {
                    Self::secondary(
                        ui,
                        if crate::files::is_raw(path) {
                            "Storage: Raw memory card"
                        } else {
                            "Storage: GCI folder"
                        },
                    );
                    Self::path(ui, path);
                }
                if let Some(hash) = &snapshot.local_hash {
                    ui.label(RichText::new(format!("Local SHA-256: {hash}")).monospace());
                }
                if let Some(hash) = &snapshot.remote_hash {
                    ui.label(RichText::new(format!("Partner SHA-256: {hash}")).monospace());
                }
            });
        });
        if snapshot.settings.play_lock.is_some() {
            ui.add_space(12.0);
            Self::card(ui, |ui| {
                Self::setting_row(
                    ui,
                    "Protected play lock is held",
                    "Disconnecting does not release it. Reconnect both PCs and stop both games before recovery.",
                    |ui| {
                        if ui
                            .add_enabled(
                                snapshot.ready && !snapshot.game_running && !snapshot.remote_game,
                                egui::Button::new("Request recovery"),
                            )
                            .clicked()
                        {
                            self.dispatch(Command::Recover);
                        }
                    },
                );
            });
        }
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
        let p = Palette::of(ui);
        let available = Self::management_available(snapshot);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Game installation");
            if let Some(path) = &snapshot.settings.game_exe {
                Self::path(ui, path);
            } else {
                Self::secondary(
                    ui,
                    "No game selected. Locate an existing copy or install the official Windows release.",
                );
            }
            if let Some(version) = &snapshot.selected_release {
                Self::status(ui, "Installed version", version, p.success);
            }
            ui.horizontal_wrapped(|ui| {
                if Self::primary(ui, Self::can_launch(snapshot), "Play Dusklight").clicked() {
                    self.dispatch(Command::Launch);
                }
                if ui
                    .add_enabled(available, egui::Button::new("Locate dusklight.exe..."))
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Dusklight executable", &["exe"])
                        .pick_file()
                {
                    self.dispatch(Command::SetExecutable(path));
                }
            });
            Self::secondary(
                ui,
                "Protected play requires both paired managers online and an automatically detected save profile.",
            );
        });
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::section_title(ui, "Dusklight updates");
            if let Some(release) = &snapshot.latest_release {
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
            ui.horizontal_wrapped(|ui| {
                if snapshot.install_busy {
                    ui.spinner();
                    ui.label("Checking, downloading, or verifying...");
                }
                if Self::primary(ui, available, "Install / update Dusklight").clicked() {
                    self.dispatch(Command::Install);
                }
                if ui
                    .add_enabled(available, egui::Button::new("Check latest release"))
                    .clicked()
                {
                    self.dispatch(Command::CheckRelease);
                }
            });
            Self::secondary(
                ui,
                "Installs side-by-side, verifies existing managed files, and preserves your profile, saves, and textures. Nothing launches automatically.",
            );
            Self::secondary(
                ui,
                "A Dusklight desktop shortcut opens this manager's protected Play controls.",
            );
        });
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
        let p = Palette::of(ui);
        Self::card(ui, |ui| {
            let stats = &snapshot.history.stats;
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
            if let Some(notice) = &snapshot.history.notice {
                Self::notice(ui, Tone::Info, "Save history", notice);
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("Refresh / latest").clicked() {
                    self.dispatch(Command::HistoryPage(None));
                }
                if ui
                    .add_enabled(
                        snapshot.history.next.is_some(),
                        egui::Button::new("Older snapshots"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::HistoryPage(snapshot.history.next.clone()));
                }
            });
            ui.collapsing("How snapshots work", |ui| {
                Self::secondary(ui, format!("{} shared blocks; {:.2} MiB if stored as full copies; {} oldest snapshots pruned.", stats.blocks, stats.logical_bytes as f64 / 1_048_576.0, stats.pruned));
                Self::secondary(ui, "Valid saves are captured after two stable seconds, including in-game autosaves. Restore creates a new commit and retains normal sync conflict checks. A corrupt current file must be moved aside first.");
            });
        });
        ui.add_space(12.0);
        let can_restore = snapshot.settings.save_file.is_some()
            && snapshot.save_profile.is_some()
            && snapshot.save_ready
            && Self::management_available(snapshot);
        if snapshot.history.entries.is_empty() {
            Self::notice(
                ui,
                Tone::Info,
                "No snapshots yet",
                "Valid saves are recorded automatically from Dusklight's detected profile. Play and save in Dusklight, or receive your partner's save - no location setup is needed.",
            );
        }
        for entry in &snapshot.history.entries {
            ui.push_id(&entry.id, |ui| { Self::card(ui, |ui| {
                let current = snapshot.local_hash.as_ref() == Some(&entry.hash);
                Self::setting_row(ui, &format!("#{}  {}", entry.sequence, entry.reason.label()),
                    &format!("{}  /  {}", entry.timestamp, entry.username),
                    |ui| {
                        if current {
                            ui.label(RichText::new("Current save").color(p.success));
                        } else if ui.add_enabled(can_restore, egui::Button::new("Restore...")).clicked() {
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
                                self.dispatch(Command::RestoreSnapshot { id: entry.id.clone(), expected: snapshot.local_hash.clone() });
                            }
                        }
                    },
                );
                ui.collapsing("Snapshot details", |ui| {
                    Self::secondary(ui, format!("Game: {} / {}", entry.game, entry.name));
                    ui.label(RichText::new(format!("Commit: {}", entry.id)).monospace());
                    ui.label(RichText::new(format!("Save SHA-256: {}", entry.hash)).monospace());
                    if let Some(parent) = &entry.parent {
                        ui.label(RichText::new(format!("Parent: {parent}")).monospace());
                    }
                });
            }); });
            ui.add_space(8.0);
        }
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
        if snapshot.pending_save {
            Self::card(ui, |ui| {
                ui.label(
                    RichText::new("A save replacement needs your consent")
                        .color(p.warning)
                        .font(semibold(16.0)),
                );
                ui.label("Your copy differs from the common version. Approving snapshots it into compact history before replacing it with your partner's copy.");
                ui.horizontal_wrapped(|ui| {
                    if Self::primary(ui, true, "Accept and keep snapshot").clicked() {
                        self.dispatch(Command::AcceptSave(true));
                    }
                    if ui.button("Keep my save").clicked() {
                        self.dispatch(Command::AcceptSave(false));
                    }
                });
            });
            ui.add_space(12.0);
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
        if snapshot.pending_recovery {
            Self::card(ui, |ui| {
                ui.label(
                    RichText::new("Crash recovery - check both devices")
                        .color(p.warning)
                        .font(semibold(16.0)),
                );
                ui.label("Confirm no Dusklight process is running on either device. Do not approve just because the other device disconnected.");
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("Both games are stopped - approve recovery")
                        .clicked()
                    {
                        self.dispatch(Command::AcceptRecovery(true));
                    }
                    if ui.button("Decline recovery").clicked() {
                        self.dispatch(Command::AcceptRecovery(false));
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
                page.title(),
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
        }
        response.on_hover_text(page.title());
    }
}

impl eframe::App for Dashboard {
    fn update(&mut self, context: &egui::Context, _: &mut eframe::Frame) {
        if self.background.take_open_game_controls() {
            self.start_hidden = false;
            self.page = Page::Overview;
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
                        let background_error = self.background.error();
                        let error = self
                            .local_error
                            .as_ref()
                            .or(background_error.as_ref())
                            .or(snapshot.error.as_ref())
                            .cloned();
                        if !confirming_device && let Some(error) = &error {
                            Self::notice(ui, Tone::Error, "Action needed", error);
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
                            Page::Overview => self.overview(ui, &snapshot),
                            Page::Devices => self.devices(ui, &snapshot),
                            Page::Library => self.library(ui, &snapshot),
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
        context.request_repaint_after(Duration::from_millis(200));
    }
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
    fn play_controls_never_enable_for_disconnected_busy_or_locked_devices() -> Result<()> {
        let ready = ready_snapshot()?;
        assert!(Dashboard::can_launch(&ready));
        for condition in [
            "offline",
            "local-game",
            "remote-game",
            "lock",
            "install",
            "textures",
            "update",
        ] {
            let mut snapshot = ready.clone();
            match condition {
                "offline" => snapshot.ready = false,
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
}

use crate::runtime::{Backend, Command, Snapshot};
use anyhow::{Context, Result};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

const BACKGROUND: Color32 = Color32::from_rgb(17, 18, 29);
const PANEL: Color32 = Color32::from_rgb(27, 28, 44);
const GOLD: Color32 = Color32::from_rgb(240, 199, 117);
const TEAL: Color32 = Color32::from_rgb(115, 219, 196);
const MUTED: Color32 = Color32::from_rgb(156, 159, 182);
const RED: Color32 = Color32::from_rgb(255, 145, 153);
const WINDOW_TITLE: &str = "Twilight Sync";

mod background;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Devices,
    Library,
    Textures,
    History,
    Security,
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
}

fn icon_pixels() -> Vec<u8> {
    let mut pixels = vec![0; 32 * 32 * 4];
    for y in 0_i32..32 {
        for x in 0_i32..32 {
            let outer = (x - 15).pow(2) + (y - 16).pow(2) < 13_i32.pow(2);
            let cutout = (x - 21).pow(2) + (y - 11).pow(2) < 11_i32.pow(2);
            if outer && !cutout {
                let offset = ((y * 32 + x) * 4) as usize;
                pixels[offset..offset + 4].copy_from_slice(&[240, 199, 117, 255]);
            }
        }
    }
    pixels
}

fn make_tray(startup: bool, automatic_updates: bool) -> Result<Tray> {
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
        .with_icon(Icon::from_rgba(icon_pixels(), 32, 32)?)
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
    run_page(backend, background, Page::Overview, WINDOW_TITLE)
}

pub fn open_dusklight_controls() -> Result<bool> {
    background::open_game_controls(WINDOW_TITLE)
}

#[cfg(debug_assertions)]
pub fn run_smoke(backend: Backend, background: bool, page: &str) -> Result<()> {
    let page = match page {
        "overview" => Page::Overview,
        "devices" => Page::Devices,
        "library" => Page::Library,
        "textures" => Page::Textures,
        "history" => Page::History,
        "security" => Page::Security,
        _ => anyhow::bail!("Unknown UI smoke page"),
    };
    run_page(backend, background, page, "Twilight Sync - UI smoke")
}

fn run_page(backend: Backend, background: bool, initial_page: Page, title: &str) -> Result<()> {
    let snapshot = backend.snapshot()?;
    let tray = make_tray(snapshot.settings.startup, snapshot.self_updates.enabled)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1060.0, 780.0])
            .with_min_inner_size([860.0, 640.0])
            .with_visible(!background)
            .with_icon(Arc::new(egui::IconData {
                rgba: icon_pixels(),
                width: 32,
                height: 32,
            })),
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
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = BACKGROUND;
            visuals.window_fill = PANEL;
            visuals.extreme_bg_color = Color32::from_rgb(12, 13, 22);
            visuals.override_text_color = Some(Color32::from_rgb(234, 235, 245));
            visuals.selection.bg_fill = Color32::from_rgb(68, 87, 91);
            visuals.widgets.inactive.bg_fill = Color32::from_rgb(46, 47, 68);
            visuals.widgets.hovered.bg_fill = Color32::from_rgb(68, 66, 88);
            context.egui_ctx.set_visuals(visuals);
            context.egui_ctx.style_mut(|style| {
                style.spacing.item_spacing = Vec2::new(12.0, 10.0);
                style.spacing.button_padding = Vec2::new(16.0, 10.0);
                style
                    .text_styles
                    .insert(egui::TextStyle::Heading, egui::FontId::proportional(26.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
            });
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
        Self::card(ui, |ui| {
            ui.label(RichText::new("Twilight Sync updates").strong().size(18.0));
            ui.label(format!("Current app: v{}", env!("CARGO_PKG_VERSION")));
            ui.label(&snapshot.self_updates.message);
            if let Some(error) = &snapshot.self_updates.error {
                ui.label(RichText::new(error).color(GOLD));
            }
            if let Some(release) = &snapshot.self_updates.latest {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Published release: {}", release.tag))
                            .color(if release.update_available { GOLD } else { TEAL }),
                    );
                    ui.hyperlink_to("Release notes", &release.notes_url);
                });
            }
            if let Some(checked) = &snapshot.self_updates.checked_at {
                ui.label(
                    RichText::new(format!("Last check: {checked}"))
                        .small()
                        .color(MUTED),
                );
            }
            let mut enabled = snapshot.self_updates.enabled;
            if ui
                .add_enabled(
                    !snapshot.self_updates.preparing,
                    egui::Checkbox::new(
                        &mut enabled,
                        "Automatically check GitHub at startup and every six hours",
                    ),
                )
                .changed()
            {
                self.dispatch(Command::SetAutoUpdateChecks(enabled));
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !snapshot.self_updates.checking && !snapshot.self_updates.preparing,
                        egui::Button::new("Check app updates now"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::CheckSelfUpdate);
                }
                let safe = !snapshot.game_running
                    && !snapshot.remote_game
                    && snapshot.settings.play_lock.is_none()
                    && !snapshot.texture_busy
                    && !snapshot.install_busy
                    && !snapshot.self_updates.preparing
                    && !snapshot.self_updates.checking;
                if ui
                    .add_enabled(
                        safe && snapshot.self_updates.available(),
                        egui::Button::new("Update Twilight Sync..."),
                    )
                    .clicked()
                {
                    self.confirm_app_update();
                }
            });
            ui.label(RichText::new("Only version metadata is checked automatically. Downloading and restarting require a confirmation. This is separate from the Dusklight game updater.").small().color(MUTED));
        });
    }

    fn header(ui: &mut egui::Ui, eyebrow: &str, title: &str, description: &str) {
        ui.label(RichText::new(eyebrow).color(GOLD).size(11.0).strong());
        ui.add_space(4.0);
        ui.heading(title);
        ui.label(RichText::new(description).color(MUTED));
        ui.add_space(16.0);
    }

    fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
        egui::Frame::new()
            .fill(PANEL)
            .corner_radius(14.0)
            .inner_margin(20.0)
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(47, 48, 67)))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                content(ui);
            });
    }

    fn status(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).color(MUTED));
            ui.label(RichText::new(value).color(color).strong());
        });
    }

    fn overview(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        Self::header(
            ui,
            "YOUR ADVENTURE, IN STEP",
            "A quiet guardian for your saves.",
            "One local network. Two trusted devices. One adventure at a time.",
        );
        Self::card(ui, |ui| {
            let (label, color) = if snapshot.game_running {
                ("Playing on this device", GOLD)
            } else if snapshot.remote_game {
                ("Playing on your partner", GOLD)
            } else if snapshot.ready {
                ("Secure partner connected", TEAL)
            } else {
                ("Waiting for your partner", MUTED)
            };
            ui.label(RichText::new(label).size(22.0).color(color).strong());
            ui.add_space(6.0);
            ui.label(&snapshot.message);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        snapshot.ready
                            && !snapshot.game_running
                            && !snapshot.remote_game
                            && !snapshot.install_busy
                            && !snapshot.texture_busy
                            && !snapshot.textures.recovery_needed
                            && !snapshot.self_updates.preparing,
                        egui::Button::new(RichText::new("Play Dusklight").color(BACKGROUND))
                            .fill(GOLD),
                    )
                    .clicked()
                {
                    self.dispatch(Command::Launch);
                }
                if ui
                    .add_enabled(snapshot.ready, egui::Button::new("Send this save"))
                    .clicked()
                {
                    self.dispatch(Command::SendSave);
                }
            });
        });
        ui.add_space(14.0);
        Self::card(ui, |ui| {
            ui.label(RichText::new("The latest chapter").strong().size(18.0));
            ui.add_space(6.0);
            let matching =
                snapshot.local_hash.is_some() && snapshot.local_hash == snapshot.remote_hash;
            Self::status(
                ui,
                "Save state",
                if matching {
                    "Matching, verified copies"
                } else if snapshot.local_hash.is_some() && snapshot.remote_hash.is_some() {
                    "Different copies - sync or resolve a conflict"
                } else {
                    "Select a GCI save on both devices"
                },
                if matching { TEAL } else { GOLD },
            );
            if let Some(path) = &snapshot.settings.save_file {
                ui.monospace(path.display().to_string());
            }
            if let Some(hash) = &snapshot.local_hash {
                ui.label(
                    RichText::new(format!("SHA-256  {hash}"))
                        .monospace()
                        .size(11.0)
                        .color(MUTED),
                );
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Choose existing save").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Dusklight GCI save", &["gci"])
                        .pick_file()
                {
                    self.dispatch(Command::SetSave(path));
                }
                if ui.button("Set receive destination").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Dusklight GCI save", &["gci"])
                        .set_file_name("01-GZ2E-zeldaTp.dat.gci")
                        .save_file()
                {
                    self.dispatch(Command::SetSave(path));
                }
            });
            ui.label(RichText::new("Select Dusklight's actual Card A GCI, not an export, raw memory card, or mod archive.").small().color(MUTED));
        });
        ui.add_space(14.0);
        Self::card(ui, |ui| {
            ui.label(RichText::new("Exclusive play").strong().size(18.0));
            let held = snapshot.settings.play_lock.is_some();
            Self::status(
                ui,
                "Durable lock",
                if held {
                    "Held - never expires on disconnect"
                } else {
                    "Available when both devices are online"
                },
                if held { GOLD } else { TEAL },
            );
            ui.label(RichText::new("Launch through this manager. Outside launches are detected, never forcibly terminated.").color(MUTED));
            if held
                && ui
                    .button("Request crash recovery on both devices")
                    .clicked()
            {
                self.dispatch(Command::Recover);
            }
        });
    }

    fn devices(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        Self::header(
            ui,
            "A HANDSHAKE, NOT A LEAP OF FAITH",
            "Meet your other device.",
            "Discovery is only a hint. Trust begins when both people confirm the same code.",
        );
        Self::card(ui, |ui| {
            ui.label(RichText::new("Your local network").strong().size(18.0));
            if let Some(network) = &snapshot.network {
                ui.label(format!(
                    "{}  |  {} / {}",
                    network.adapter, network.address, network.prefix
                ));
                ui.label(format!(
                    "Router {}  |  {}",
                    network.gateway, network.router_mac
                ));
                ui.label(
                    RichText::new(&network.fingerprint)
                        .monospace()
                        .size(11.0)
                        .color(MUTED),
                );
                let trusted = snapshot.settings.network.as_deref() == Some(&network.fingerprint);
                if trusted {
                    ui.label(RichText::new("Router fingerprint approved").color(TEAL));
                } else if ui
                    .button("I recognize this private router - approve network")
                    .clicked()
                {
                    self.dispatch(Command::TrustNetwork);
                }
            } else {
                ui.label("No verifiable private LAN. Use a physical adapter and set your Windows network profile to Private.");
            }
        });
        ui.add_space(14.0);
        if let Some(code) = &snapshot.comparison_code {
            Self::card(ui, |ui| {
                ui.label(
                    RichText::new("COMPARE ON BOTH SCREENS")
                        .color(GOLD)
                        .strong(),
                );
                ui.label(
                    snapshot
                        .peer_name
                        .as_deref()
                        .unwrap_or("Authenticating device..."),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(code)
                        .monospace()
                        .size(46.0)
                        .color(TEAL)
                        .strong(),
                );
                ui.label("Do not approve if even one digit differs. The code changes for every handshake.");
                ui.horizontal(|ui| {
                    Self::status(
                        ui,
                        "This device:",
                        if snapshot.local_approved {
                            "Approved"
                        } else {
                            "Waiting"
                        },
                        GOLD,
                    );
                    Self::status(
                        ui,
                        "Other device:",
                        if snapshot.remote_approved {
                            "Approved"
                        } else {
                            "Waiting"
                        },
                        GOLD,
                    );
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !snapshot.local_approved,
                            egui::Button::new("Codes match - approve this device"),
                        )
                        .clicked()
                    {
                        self.dispatch(Command::ApprovePairing);
                    }
                    if ui.button("Reject").clicked() {
                        self.dispatch(Command::RejectPairing);
                    }
                });
            });
        } else if let Some(peer) = &snapshot.settings.peer {
            Self::card(ui, |ui| {
                ui.label(RichText::new(&peer.name).size(20.0).strong());
                Self::status(
                    ui,
                    "Trust",
                    if snapshot.ready {
                        "Pinned identity, connected"
                    } else {
                        "Pinned identity, offline"
                    },
                    TEAL,
                );
                ui.label(
                    RichText::new(&peer.public_key)
                        .monospace()
                        .size(11.0)
                        .color(MUTED),
                );
                if !snapshot.ready && ui.button("Connect to discovered partner").clicked() {
                    self.dispatch(Command::Connect(peer.public_key.clone()));
                }
                if ui.button("Revoke this partner").clicked() {
                    self.dispatch(Command::Unpair);
                }
            });
        } else {
            Self::card(ui, |ui| {
                ui.label(RichText::new("Nearby devices").strong().size(18.0));
                if ui
                    .button(if snapshot.pairing_open {
                        "Pairing open for two minutes - reopen"
                    } else {
                        "Open a two-minute pairing window"
                    })
                    .clicked()
                {
                    self.dispatch(Command::OpenPairing);
                }
                ui.label("Open pairing on both devices. Choose Compare codes on ONE device, then approve matching codes on BOTH.");
                if snapshot.devices.is_empty() {
                    ui.label(
                        RichText::new(
                            "Open Twilight Sync on your other device and approve the same router.",
                        )
                        .color(MUTED),
                    );
                }
                for device in &snapshot.devices {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&device.name).strong());
                            ui.label(
                                RichText::new(format!(
                                    "{}  |  unverified announcement",
                                    device.address.ip()
                                ))
                                .small()
                                .color(MUTED),
                            );
                        });
                        if ui
                            .add_enabled(
                                snapshot.pairing_open && device.pairing,
                                egui::Button::new("Compare codes"),
                            )
                            .clicked()
                        {
                            self.dispatch(Command::Connect(device.key.clone()));
                        }
                    });
                }
            });
        }
    }

    fn library(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        Self::header(
            ui,
            "BRING YOUR OWN ADVENTURE",
            "Dusklight & your disc.",
            "Program installation and optional disc transfer are separate from the save-only channel.",
        );
        Self::card(ui, |ui| {
            ui.label(
                RichText::new("The official Dusklight release")
                    .strong()
                    .size(18.0),
            );
            if let Some(path) = &snapshot.settings.game_exe {
                ui.monospace(path.display().to_string());
            } else {
                ui.label("No executable selected. Locate your existing copy or install the official release.");
            }
            if let Some(version) = &snapshot.selected_release {
                ui.label(RichText::new(format!("Selected installation: {version}")).color(TEAL));
            }
            if let Some(release) = &snapshot.latest_release {
                ui.label(format!(
                    "Latest official release: {}  |  {:.1} MiB",
                    release.tag,
                    release.size as f64 / 1_048_576.0
                ));
                ui.label(
                    RichText::new(format!("Published SHA-256: {}", release.sha256))
                        .monospace()
                        .size(10.0)
                        .color(MUTED),
                );
            }
            ui.horizontal(|ui| {
                if ui.button("Locate dusklight.exe").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Dusklight executable", &["exe"])
                        .pick_file()
                {
                    self.dispatch(Command::SetExecutable(path));
                }
                if ui
                    .add_enabled(
                        !snapshot.install_busy
                            && !snapshot.texture_busy
                            && !snapshot.self_updates.preparing,
                        egui::Button::new(if snapshot.install_busy {
                            "Checking / verifying..."
                        } else {
                            "Install / update Dusklight"
                        }),
                    )
                    .clicked()
                {
                    self.dispatch(Command::Install);
                }
                if ui
                    .add_enabled(
                        !snapshot.install_busy
                            && !snapshot.texture_busy
                            && !snapshot.self_updates.preparing,
                        egui::Button::new("Check latest release"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::CheckRelease);
                }
            });
            ui.label(RichText::new("Updates install side-by-side and preserve your user profile, textures, saves, and old program. Existing managed files are checked before reuse. Nothing is launched automatically.").small().color(MUTED));
        });
        ui.add_space(14.0);
        Self::card(ui, |ui| {
            ui.label(
                RichText::new("Your own GameCube disc image")
                    .strong()
                    .size(18.0),
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
            ui.label("Only unmodified, full-size Twilight Princess GameCube .iso images are supported. No RVZ, Wii ISO, archives, or game downloads.");
            if let Some(path) = &snapshot.settings.iso_file {
                ui.monospace(path.display().to_string());
            }
            ui.horizontal(|ui| {
                if ui.button("Choose my ISO").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Your GameCube disc dump", &["iso"])
                        .pick_file()
                {
                    self.dispatch(Command::SetIso(path));
                }
                if ui
                    .add_enabled(
                        snapshot.ready && snapshot.settings.iso_enabled,
                        egui::Button::new("Offer verified ISO to partner"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::SendIso);
                }
            });
            ui.label(RichText::new("The receiver must separately opt in and approve every transfer. Existing ISOs are never overwritten.").small().color(MUTED));
        });
    }

    fn import_texture(&mut self, folder: bool) {
        if !self.texture_permission {
            self.local_error =
                Some("Confirm that you have permission to use the texture assets.".into());
            return;
        }
        let selected = if folder {
            rfd::FileDialog::new()
                .set_title("Choose one resolution's GZ2 texture folder")
                .pick_folder()
        } else {
            rfd::FileDialog::new()
                .add_filter("Texture pack ZIP", &["zip"])
                .pick_file()
        };
        if let Some(path) = selected {
            self.dispatch(Command::ImportTextures {
                source: self.texture_source.clone(),
                version: self.texture_version.clone(),
                path,
            });
        }
    }

    fn textures(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        Self::header(
            ui,
            "A FAMILIAR WORLD, A FRESH COAT OF LIGHT",
            "Texture library.",
            "Creator-supported downloads. Per-user installs. One complete pack at a time.",
        );
        let busy =
            snapshot.texture_busy || snapshot.install_busy || snapshot.self_updates.preparing;
        if !snapshot.textures.notice.is_empty() {
            ui.label(RichText::new(&snapshot.textures.notice).color(GOLD));
            ui.add_space(8.0);
        }
        Self::card(ui, |ui| {
            ui.label(RichText::new("Your Dusklight profile").strong().size(18.0));
            if let Some(path) = &snapshot.settings.texture_profile {
                ui.monospace(path.display().to_string());
            } else {
                ui.label("Select the actual Dusklight config.json, not the executable's folder. Start Dusklight once if its user profile does not exist yet.");
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("Choose Dusklight config.json"))
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Dusklight config.json", &["json"])
                        .pick_file()
                {
                    if path
                        .file_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("config.json"))
                    {
                        if let Some(parent) = path.parent() {
                            self.dispatch(Command::SetTextureProfile(parent.into()));
                        }
                    } else {
                        self.local_error =
                            Some("Choose Dusklight's file named config.json.".into());
                    }
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Refresh library"))
                    .clicked()
                {
                    self.dispatch(Command::RefreshTextures);
                }
            });
            ui.label(RichText::new("Switches update texture_replacements and the real game.enableTextureReplacements setting while both games are stopped. No live injection or restart is forced.").small().color(MUTED));
            if snapshot.textures.recovery_needed {
                ui.label(RichText::new("An interrupted switch needs recovery before this device can launch Dusklight.").color(RED));
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new("Recover interrupted texture switch"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::RecoverTextures);
                }
            }
        });
        ui.add_space(14.0);
        for project in crate::textures::CATALOG {
            Self::card(ui, |ui| {
                ui.label(RichText::new(project.name).strong().size(18.0));
                ui.label(project.description);
                ui.horizontal(|ui| {
                    if project.creator_url.is_some()
                        && ui.button("Get latest / updates from creator").clicked()
                    {
                        self.dispatch(Command::OpenTextureCreator(project.id.into()));
                    }
                    if ui
                        .selectable_label(self.texture_source == project.id, "Select for import")
                        .clicked()
                    {
                        self.texture_source = project.id.into();
                        self.texture_version = project.suggested_version.map_or_else(
                            || format!("Imported {}", time::OffsetDateTime::now_utc().date()),
                            str::to_owned,
                        );
                    }
                });
            });
            ui.add_space(10.0);
        }
        Self::card(ui, |ui| {
            ui.label(
                RichText::new("Install a downloaded pack or update")
                    .strong()
                    .size(18.0),
            );
            ui.label(format!("Project: {}", self.texture_source));
            ui.horizontal(|ui| {
                ui.label("Version label");
                ui.text_edit_singleline(&mut self.texture_version);
            });
            ui.checkbox(
                &mut self.texture_permission,
                "I have permission to use these assets",
            );
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !busy && self.texture_permission,
                        egui::Button::new("Install ZIP"),
                    )
                    .clicked()
                {
                    self.import_texture(false);
                }
                if ui
                    .add_enabled(
                        !busy && self.texture_permission,
                        egui::Button::new("Install extracted GZ2 folder"),
                    )
                    .clicked()
                {
                    self.import_texture(true);
                }
            });
            ui.label(RichText::new("For 7z/RAR, extract with your own archive utility first. Import only one resolution/variant. Only validated tex1_ PNG/DDS images are installed; bundled emulators, scripts, and installers are skipped.").small().color(MUTED));
            ui.label(RichText::new("Creator pages may require your own account or browser download. Twilight Sync does not scrape logins, bypass access controls, or mirror paid packs.").small().color(MUTED));
        });
        ui.add_space(14.0);
        Self::card(ui, |ui| {
            ui.label(RichText::new("Installed versions").strong().size(18.0));
            if snapshot.textures.packs.is_empty() {
                ui.label(RichText::new("No managed texture packs yet.").color(MUTED));
            }
            for pack in &snapshot.textures.packs {
                ui.separator();
                let active = snapshot.textures.active.as_deref() == Some(&pack.id);
                ui.label(
                    RichText::new(format!("{}  /  {}", pack.source, pack.version))
                        .color(if active { TEAL } else { Color32::WHITE })
                        .strong(),
                );
                ui.label(format!(
                    "{} images  |  {:.1} MiB  |  {}",
                    pack.files,
                    pack.bytes as f64 / 1_048_576.0,
                    pack.imported
                ));
                ui.label(
                    RichText::new(format!("Content ID: {}", pack.id))
                        .monospace()
                        .size(10.0)
                        .color(MUTED),
                );
                if active {
                    ui.label(RichText::new("ACTIVE FOR NEXT LAUNCH").color(TEAL).small());
                }
                if ui
                    .add_enabled(
                        !busy
                            && !active
                            && snapshot.settings.texture_profile.is_some()
                            && !snapshot.textures.recovery_needed,
                        egui::Button::new("Activate this version"),
                    )
                    .clicked()
                {
                    let preserve = if snapshot.textures.unmanaged {
                        rfd::MessageDialog::new().set_title("Preserve existing texture files")
                            .set_description("Existing unmanaged textures will be moved to a named backup beside texture_replacements, not deleted. Disabling the managed pack restores that folder. Continue?")
                            .set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes
                    } else {
                        true
                    };
                    if preserve {
                        self.dispatch(Command::ActivateTextures {
                            id: Some(pack.id.clone()),
                            preserve_unmanaged: snapshot.textures.unmanaged,
                        });
                    }
                }
            }
            if ui
                .add_enabled(
                    !busy
                        && snapshot.textures.active.is_some()
                        && !snapshot.textures.recovery_needed,
                    egui::Button::new("Disable managed pack / restore original textures"),
                )
                .clicked()
            {
                self.dispatch(Command::ActivateTextures {
                    id: None,
                    preserve_unmanaged: false,
                });
            }
        });
    }

    fn history(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        Self::header(
            ui,
            "EVERY CHAPTER HAS A WAY BACK",
            "Save time machine.",
            "Lossless copy-on-write blocks and a parent-linked commit trail. No Windows VSS service or administrator access.",
        );
        Self::card(ui, |ui| {
            let stats = &snapshot.history.stats;
            ui.label(
                RichText::new(format!(
                    "{} retained snapshots  /  {} shared blocks",
                    stats.commits, stats.blocks
                ))
                .strong()
                .size(18.0),
            );
            ui.label(format!(
                "{:.2} MiB stored, versus {:.2} MiB of full-file copies. Budget: {:.0} MiB.",
                stats.stored_bytes as f64 / 1_048_576.0,
                stats.logical_bytes as f64 / 1_048_576.0,
                stats.budget as f64 / 1_048_576.0
            ));
            ui.add(
                egui::ProgressBar::new(
                    (stats.stored_bytes as f32 / stats.budget as f32).clamp(0.0, 1.0),
                )
                .show_percentage(),
            );
            ui.label(RichText::new(format!("{} oldest commits pruned. Shared blocks survive as long as any retained commit needs them. Filesystem allocation overhead is separate.", stats.pruned)).small().color(MUTED));
            if let Some(notice) = &snapshot.history.notice {
                ui.label(RichText::new(notice).color(GOLD));
            }
            ui.horizontal(|ui| {
                if ui.button("Latest / refresh").clicked() {
                    self.dispatch(Command::HistoryPage(None));
                }
                if ui
                    .add_enabled(
                        snapshot.history.next.is_some(),
                        egui::Button::new("Older commits"),
                    )
                    .clicked()
                {
                    self.dispatch(Command::HistoryPage(snapshot.history.next.clone()));
                }
            });
            ui.label(RichText::new("Valid saves are captured after two stable seconds, including in-game autosaves. Restoring adds a new commit and obeys normal sync conflict rules. Corrupt current files must be moved aside first; they are never silently overwritten.").small().color(MUTED));
        });
        ui.add_space(14.0);
        let can_restore = snapshot.settings.save_file.is_some()
            && !snapshot.game_running
            && !snapshot.remote_game
            && snapshot.settings.play_lock.is_none()
            && !snapshot.texture_busy
            && !snapshot.install_busy
            && !snapshot.self_updates.preparing;
        if snapshot.history.entries.is_empty() {
            ui.label(RichText::new("Choose a GCI save and wait for a stable save, or refresh to migrate older backups.").color(MUTED));
        }
        for entry in &snapshot.history.entries {
            Self::card(ui, |ui| {
                ui.label(
                    RichText::new(format!("#{}  {}", entry.sequence, entry.reason.label()))
                        .strong()
                        .color(TEAL),
                );
                ui.label(format!(
                    "{}  |  {}  |  {} / {}",
                    entry.timestamp, entry.username, entry.game, entry.name
                ));
                ui.label(
                    RichText::new(format!("Commit {}", entry.id))
                        .monospace()
                        .size(10.0)
                        .color(MUTED),
                );
                ui.label(
                    RichText::new(format!("Save SHA-256 {}", entry.hash))
                        .monospace()
                        .size(10.0)
                        .color(MUTED),
                );
                if let Some(parent) = &entry.parent {
                    ui.label(
                        RichText::new(format!("Parent {parent}"))
                            .monospace()
                            .size(10.0)
                            .color(MUTED),
                    );
                }
                let current = snapshot.local_hash.as_ref() == Some(&entry.hash);
                if current {
                    ui.label(
                        RichText::new("MATCHES CURRENT LOCAL SAVE")
                            .color(GOLD)
                            .small(),
                    );
                }
                if ui
                    .add_enabled(
                        can_restore && !current,
                        egui::Button::new("Restore as a new commit"),
                    )
                    .clicked()
                {
                    let Some(destination) = snapshot.settings.save_file.as_ref() else {
                        self.local_error =
                            Some("Choose a local GCI destination before restoring.".into());
                        return;
                    };
                    let destination = destination.display();
                    let approved = rfd::MessageDialog::new().set_title("Restore save snapshot")
                        .set_description(format!("Restore {} / {} to {}?\n\nThe current valid save is snapshotted first. This restore creates a new commit; paired-device conflicts still need approval.", entry.game, entry.name, destination))
                        .set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes;
                    if approved {
                        self.dispatch(Command::RestoreSnapshot {
                            id: entry.id.clone(),
                            expected: snapshot.local_hash.clone(),
                        });
                    }
                }
            });
            ui.add_space(10.0);
        }
    }

    fn security(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        Self::header(
            ui,
            "SMALL PROTOCOL. EXPLICIT TRUST.",
            "Built to fail closed.",
            "No custom cryptography. No cloud account. No silent conflict winner.",
        );
        Self::card(ui, |ui| {
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
                    "Save traffic accepts only a strict GCI structure with both sector checksums, all six quest checksums, and bounded player/horse names. No mod scripts, raw cards, ZIP saves, or executable attachments.",
                ),
                (
                    "Honest limitations",
                    "Validation cannot prove a save contains no exploit for every Dusklight build. Exclusive play protects managed launches; direct launches and a compromised OS can bypass it. Keep Dusklight updated.",
                ),
            ] {
                ui.label(RichText::new(title).strong().color(TEAL));
                ui.label(detail);
                ui.add_space(8.0);
            }
        });
        ui.add_space(14.0);
        Self::card(ui, |ui| {
            let mut startup = snapshot.settings.startup;
            if ui
                .checkbox(&mut startup, "Start with Windows (enabled by default)")
                .changed()
            {
                self.dispatch(Command::SetStartup(startup));
            }
            ui.label("Audit logs in your home .twilight-sync folder contain your username, file paths, SHA-256 hashes, peer identities, and security decisions. Treat them as private.");
            ui.label(
                RichText::new(format!("Your public identity: {}", snapshot.public_key))
                    .monospace()
                    .size(11.0)
                    .color(MUTED),
            );
            if ui.button("Open private application folder").clicked() {
                match open_data_directory() {
                    Ok(()) => {}
                    Err(error) => self.local_error = Some(format!("{error:#}")),
                }
            }
        });
    }

    fn approvals(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        if snapshot.pending_save {
            Self::card(ui, |ui| {
                ui.label(
                    RichText::new("A save replacement needs your consent")
                        .color(GOLD)
                        .strong(),
                );
                ui.label("Your copy differs from the common version. Approving snapshots it into compact history before replacing it with your partner's copy.");
                ui.horizontal(|ui| {
                    if ui.button("Approve replacement and keep snapshot").clicked() {
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
                        .color(GOLD)
                        .strong(),
                );
                ui.horizontal(|ui| {
                    if ui
                        .button("I own this game - choose a new ISO destination")
                        .clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("GameCube ISO", &["iso"])
                            .set_file_name("Twilight Princess.iso")
                            .save_file()
                    {
                        self.dispatch(Command::AcceptIso(Some(path)));
                    }
                    if ui.button("Decline ISO").clicked() {
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
                        .color(GOLD)
                        .strong(),
                );
                ui.label("Confirm no Dusklight process is running on either device. Do not approve just because the other device disconnected.");
                ui.horizontal(|ui| {
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
}

impl eframe::App for Dashboard {
    fn update(&mut self, context: &egui::Context, _: &mut eframe::Frame) {
        if self.background.take_open_game_controls() {
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
        egui::SidePanel::left("navigation")
            .exact_width(190.0)
            .resizable(false)
            .show(context, |ui| {
                ui.add_space(24.0);
                ui.label(RichText::new("TWILIGHT").size(24.0).strong().color(GOLD));
                ui.label(RichText::new("S Y N C").size(14.0).color(MUTED));
                ui.add_space(28.0);
                for (page, title) in [
                    (Page::Overview, "Overview"),
                    (Page::Devices, "Your devices"),
                    (Page::Library, "Game & disc"),
                    (Page::Textures, "Texture packs"),
                    (Page::History, "Save history"),
                    (Page::Security, "Trust & settings"),
                ] {
                    if ui
                        .selectable_label(self.page == page, RichText::new(title).size(16.0))
                        .clicked()
                    {
                        self.page = page;
                    }
                }
                ui.add_space(28.0);
                ui.separator();
                ui.label(
                    RichText::new("LOCAL BY DESIGN")
                        .color(TEAL)
                        .size(11.0)
                        .strong(),
                );
                ui.label(
                    RichText::new("No cloud.\nNo relay.\nNo automatic trust.")
                        .color(MUTED)
                        .small(),
                );
                ui.add_space(20.0);
                ui.label(
                    RichText::new(concat!(
                        "v",
                        env!("CARGO_PKG_VERSION"),
                        "  /  twilightProto 1"
                    ))
                    .color(MUTED)
                    .size(10.0),
                );
            });
        egui::TopBottomPanel::bottom("footer").show(context, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if snapshot.ready {
                        "ENCRYPTED + PAIRED"
                    } else {
                        "NOT CONNECTED"
                    })
                    .color(if snapshot.ready { TEAL } else { MUTED })
                    .size(11.0)
                    .strong(),
                );
                ui.separator();
                ui.label(
                    RichText::new("Closing this window keeps Twilight Sync in the tray.")
                        .color(MUTED)
                        .size(11.0),
                );
            });
        });
        egui::CentralPanel::default().show(context, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(22.0);
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    ui.vertical(|ui| {
                        ui.set_max_width(ui.available_width() - 18.0);
                        let background_error = self.background.error();
                        if let Some(error) = self
                            .local_error
                            .as_ref()
                            .or(background_error.as_ref())
                            .or(snapshot.error.as_ref())
                        {
                            Self::card(ui, |ui| {
                                ui.label(RichText::new(error).color(RED));
                            });
                            ui.add_space(12.0);
                        }
                        if !snapshot.transfer.is_empty() {
                            ui.label(RichText::new(&snapshot.transfer).color(TEAL));
                            ui.add_space(8.0);
                        }
                        self.approvals(ui, &snapshot);
                        if self.page == Page::Security
                            || (self.page == Page::Overview
                                && (snapshot.self_updates.checking
                                    || snapshot.self_updates.preparing
                                    || snapshot.self_updates.checked_at.is_some()
                                    || snapshot.self_updates.error.is_some()))
                        {
                            self.app_updates(ui, &snapshot);
                            ui.add_space(14.0);
                        }
                        match self.page {
                            Page::Overview => self.overview(ui, &snapshot),
                            Page::Devices => self.devices(ui, &snapshot),
                            Page::Library => self.library(ui, &snapshot),
                            Page::Textures => self.textures(ui, &snapshot),
                            Page::History => self.history(ui, &snapshot),
                            Page::Security => self.security(ui, &snapshot),
                        }
                    });
                });
                ui.add_space(22.0);
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

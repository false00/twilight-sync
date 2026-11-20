use super::{Tray, widgets::MainWindow};
use crate::runtime::{Backend, Command};
use anyhow::{Result, ensure};
use slint::ComponentHandle;
use std::{
    cell::{Cell, RefCell},
    num::NonZeroIsize,
    rc::Rc,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            FindWindowW, IsWindow, KillTimer, RegisterWindowMessageW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
            SW_RESTORE, SendMessageTimeoutW, SetTimer, ShowWindow, WM_NCDESTROY, WM_SHOWWINDOW,
        },
    },
};

const TIMER_ID: usize = 0x54575359;

thread_local! {
    static PUMP: RefCell<Option<Rc<Pump>>> = const { RefCell::new(None) };
}

struct Pump {
    backend: Backend,
    ui: slint::Weak<MainWindow>,
    tray: Tray,
    window: NonZeroIsize,
    last_status: RefCell<String>,
    attention: Cell<bool>,
    error: RefCell<Option<String>>,
    dashboard_message: u32,
    open_message: u32,
    game_message: u32,
    open_game_controls: Cell<Option<crate::protocol::GameId>>,
    ui_timer: std::rc::Weak<slint::Timer>,
}

pub(super) struct BackgroundGuard {
    pump: Rc<Pump>,
    timer: usize,
}

pub(super) fn reveal(window: NonZeroIsize, ui: &slint::Weak<MainWindow>) {
    unsafe {
        ShowWindow(window.get() as HWND, SW_RESTORE);
    }
    if let Some(ui) = ui.upgrade() {
        if let Err(error) = ui.show() {
            eprintln!("Cannot reveal Twilight Sync: {error}");
        }
        ui.window().set_minimized(false);
        ui.window().request_redraw();
    }
}

fn open_message() -> std::io::Result<u32> {
    register_message("TwilightSync.OpenDusklightControls.v1")
}

fn dashboard_message() -> std::io::Result<u32> {
    register_message("TwilightSync.OpenDashboard.v1")
}

fn register_message(name: &str) -> std::io::Result<u32> {
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let message = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    if message == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(message)
}

pub(super) fn open_game_controls(title: &str) -> Result<bool> {
    open_selected_game(title, crate::protocol::GameId::Dusklight)
}

pub(super) fn open_selected_game(title: &str, game: crate::protocol::GameId) -> Result<bool> {
    let message = if game == crate::protocol::GameId::Dusklight {
        open_message()?
    } else {
        register_message("TwilightSync.OpenGameControls.v2")?
    };
    let argument = match game {
        crate::protocol::GameId::Dusklight => 0,
        crate::protocol::GameId::Totk => 1,
        crate::protocol::GameId::Triforce => 2,
    };
    request_open(title, message, argument)
}

pub(super) fn open_dashboard(title: &str) -> Result<bool> {
    request_open(title, dashboard_message()?, 0)
}

fn request_open(title: &str, message: u32, argument: WPARAM) -> Result<bool> {
    let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    let window = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if window.is_null() {
        return Ok(false);
    }
    let mut response = 0;
    ensure!(
        unsafe {
            SendMessageTimeoutW(
                window,
                message,
                argument,
                0,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                2000,
                &mut response,
            )
        } != 0,
        "The running manager did not respond; close any dialog and try again"
    );
    ensure!(
        response == 1,
        "Open Twilight Sync from the tray, or restart the running manager to use this shortcut"
    );
    Ok(true)
}

impl BackgroundGuard {
    pub(super) fn install(
        window: NonZeroIsize,
        ui: slint::Weak<MainWindow>,
        backend: Backend,
        tray: Tray,
        ui_timer: std::rc::Weak<slint::Timer>,
    ) -> std::io::Result<Self> {
        let pump = Rc::new(Pump {
            backend,
            ui,
            tray,
            window,
            last_status: RefCell::new(String::new()),
            attention: Cell::new(false),
            error: RefCell::new(None),
            dashboard_message: dashboard_message()?,
            open_message: open_message()?,
            game_message: register_message("TwilightSync.OpenGameControls.v2")?,
            open_game_controls: Cell::new(None),
            ui_timer,
        });
        PUMP.with(|cell| *cell.borrow_mut() = Some(pump.clone()));
        if unsafe { SetWindowSubclass(window.get() as HWND, Some(window_message), TIMER_ID, 0) }
            == 0
        {
            PUMP.with(|cell| *cell.borrow_mut() = None);
            return Err(std::io::Error::other(
                "Cannot install the desktop shortcut window handler",
            ));
        }
        // A GUI-thread timer keeps the tray alive without rendering the hidden dashboard.
        let timer = unsafe { SetTimer(window.get() as HWND, TIMER_ID, 1000, Some(tick)) };
        if timer == 0 {
            let error = std::io::Error::last_os_error();
            if unsafe { RemoveWindowSubclass(window.get() as HWND, Some(window_message), TIMER_ID) }
                == 0
            {
                eprintln!("Cannot remove the desktop shortcut handler after timer setup failed");
            }
            PUMP.with(|cell| *cell.borrow_mut() = None);
            return Err(error);
        }
        Ok(Self { pump, timer })
    }

    pub(super) fn reveal(&self) {
        self.pump.reveal_dashboard();
    }

    pub(super) fn error(&self) -> Option<String> {
        self.pump.error.borrow().clone()
    }

    pub(super) fn take_open_game_controls(&self) -> Option<crate::protocol::GameId> {
        self.pump.open_game_controls.take()
    }
}

impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        let window = self.pump.window.get() as HWND;
        unsafe {
            if IsWindow(window) != 0
                && RemoveWindowSubclass(window, Some(window_message), TIMER_ID) == 0
            {
                eprintln!("Cannot remove the desktop shortcut window handler");
            }
            if IsWindow(window) != 0 && KillTimer(window, self.timer) == 0 {
                eprintln!(
                    "Cannot remove Twilight Sync GUI timer: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        PUMP.with(|cell| *cell.borrow_mut() = None);
    }
}

unsafe extern "system" fn window_message(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    let pump = PUMP.with(|cell| cell.borrow().clone());
    if message == WM_SHOWWINDOW
        && let Some(timer) = pump.as_ref().and_then(|pump| pump.ui_timer.upgrade())
    {
        timer.set_interval(if wparam == 0 {
            std::time::Duration::from_secs(2)
        } else {
            std::time::Duration::from_millis(250)
        });
    }
    if let Some(pump) = &pump
        && message == pump.dashboard_message
    {
        pump.reveal_dashboard();
        return 1;
    }
    if let Some(pump) = pump
        && (message == pump.open_message || (message == pump.game_message && wparam <= 2))
    {
        pump.open_game_controls
            .set(Some(if message == pump.game_message && wparam == 2 {
                crate::protocol::GameId::Triforce
            } else if message == pump.game_message && wparam == 1 {
                crate::protocol::GameId::Totk
            } else {
                crate::protocol::GameId::Dusklight
            }));
        pump.reveal_dashboard();
        return 1;
    }
    if message == WM_NCDESTROY
        && unsafe { RemoveWindowSubclass(window, Some(window_message), TIMER_ID) } == 0
    {
        eprintln!("Cannot remove the desktop shortcut handler while the window is closing");
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

impl Pump {
    fn reveal_dashboard(&self) {
        reveal(self.window, &self.ui);
        if let Err(error) = self
            .backend
            .command(crate::runtime::Command::CheckSelfUpdateOnOpen)
        {
            *self.error.borrow_mut() =
                Some(format!("Could not request an update check: {error:#}"));
        }
    }

    fn update(&self) -> Result<()> {
        let snapshot = self.backend.snapshot()?;
        let status = if snapshot.error.is_some() || snapshot.textures.recovery_needed {
            "Action needed - open dashboard"
        } else if snapshot.triforce.pending
            || snapshot.triforce.recovery_pending
            || snapshot.triforce.conflict
        {
            "Triforce progress needs your choice - open dashboard"
        } else if snapshot.triforce.running {
            "Playing Triforce here - save writes paused"
        } else if snapshot.triforce.remote_running {
            "Partner playing Triforce - save writes paused"
        } else if snapshot.totk.pending || snapshot.totk.recovery_pending || snapshot.totk.conflict
        {
            "TotK progress needs your choice - open dashboard"
        } else if snapshot.totk.running {
            "Playing TotK here - save writes paused"
        } else if snapshot.totk.remote_running {
            "Partner playing TotK - save writes paused"
        } else if snapshot.game_running {
            "Playing here - save writes paused"
        } else if snapshot.remote_game {
            "Partner playing - save writes paused"
        } else if snapshot.self_updates.preparing {
            "Preparing verified Twilight Sync update"
        } else if snapshot.texture_busy {
            "Installing / switching texture data"
        } else if snapshot.install_busy {
            "Installing, updating or building a game"
        } else if snapshot.self_updates.available() {
            "Twilight Sync update available - open tray"
        } else if snapshot.connecting {
            "Connecting to your local device"
        } else if snapshot.ready {
            "Secure partner connected"
        } else {
            "Offline - locks preserved"
        };
        self.tray.startup.set_checked(snapshot.settings.startup);
        self.tray
            .automatic_updates
            .set_checked(snapshot.self_updates.enabled);
        self.tray
            .check_updates
            .set_enabled(!snapshot.self_updates.checking && !snapshot.self_updates.preparing);
        self.tray
            .check_updates
            .set_text(if snapshot.self_updates.checking {
                "Checking Twilight Sync updates..."
            } else {
                "Check Twilight Sync updates"
            });
        let can_update = snapshot.self_updates.available()
            && super::model::app_update_blocker(&snapshot).is_none();
        self.tray.update_app.set_enabled(can_update);
        let update_text = if snapshot.self_updates.preparing {
            "Preparing verified app update...".into()
        } else if snapshot.self_updates.error.is_some() {
            "App update check needs attention".into()
        } else if let Some(release) = &snapshot.self_updates.latest {
            if release.update_available {
                format!("Update Twilight Sync to {}...", release.tag)
            } else {
                "Twilight Sync is up to date".into()
            }
        } else {
            "Update Twilight Sync (check first)".into()
        };
        self.tray.update_app.set_text(update_text);
        if *self.last_status.borrow() != status {
            self.tray.status.set_text(status);
            self.tray
                .icon
                .set_tooltip(Some(format!("Twilight Sync - {status}")))?;
            *self.last_status.borrow_mut() = status.into();
        }
        let attention = snapshot.pending_save
            || snapshot.triforce.pending
            || snapshot.triforce.recovery_pending
            || snapshot.triforce.conflict
            || snapshot.totk.pending
            || snapshot.totk.recovery_pending
            || snapshot.totk.conflict
            || snapshot.pending_iso
            || snapshot.pending_recovery
            || snapshot.textures.recovery_needed
            || snapshot.comparison_code.is_some();
        let previous = self.attention.replace(attention);
        if snapshot.shutdown || (attention && !previous) {
            reveal(self.window, &self.ui);
        }
        let desktop_error = self.error.borrow().clone();
        if let Some(detail) = desktop_error {
            self.backend.command(Command::DesktopRecovered(detail))?;
            *self.error.borrow_mut() = None;
            if let Some(ui) = self.ui.upgrade() {
                ui.window().request_redraw();
            }
        }
        Ok(())
    }

    fn report(&self, error: anyhow::Error) {
        let detail = format!("Background desktop error: {error:#}");
        if self.error.borrow().as_ref() != Some(&detail) {
            *self.error.borrow_mut() = Some(detail.clone());
            if let Err(error) = self.backend.command(Command::DesktopError(detail)) {
                eprintln!("Cannot report the desktop failure to the manager: {error:#}");
            }
            reveal(self.window, &self.ui);
        }
    }
}

unsafe extern "system" fn tick(_: HWND, _: u32, _: usize, _: u32) {
    // Release the thread-local borrow before native calls can re-enter the message loop.
    let pump = PUMP.with(|cell| cell.borrow().clone());
    if let Some(pump) = pump
        && let Err(error) = pump.update()
    {
        pump.report(error);
    }
}

use super::Tray;
use crate::runtime::{Backend, Command};
use anyhow::{Result, ensure};
use eframe::egui;
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
            SW_RESTORE, SendMessageTimeoutW, SetTimer, ShowWindow, WM_NCDESTROY,
        },
    },
};

const TIMER_ID: usize = 0x54575359;

thread_local! {
    static PUMP: RefCell<Option<Rc<Pump>>> = const { RefCell::new(None) };
}

struct Pump {
    backend: Backend,
    context: egui::Context,
    tray: Tray,
    window: NonZeroIsize,
    last_status: RefCell<String>,
    attention: Cell<bool>,
    error: RefCell<Option<String>>,
    open_message: u32,
    open_game_controls: Cell<bool>,
}

pub(super) struct BackgroundGuard {
    pump: Rc<Pump>,
    timer: usize,
}

pub(super) fn reveal(window: NonZeroIsize, context: &egui::Context) {
    // Hidden Win32 windows do not receive redraws. Show natively before asking egui to repaint.
    unsafe {
        ShowWindow(window.get() as HWND, SW_RESTORE);
    }
    context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    context.send_viewport_cmd(egui::ViewportCommand::Focus);
    context.request_repaint();
}

fn open_message() -> std::io::Result<u32> {
    let name: Vec<u16> = "TwilightSync.OpenDusklightControls.v1"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let message = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    if message == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(message)
}

pub(super) fn open_game_controls(title: &str) -> Result<bool> {
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
                open_message()?,
                0,
                0,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                2000,
                &mut response,
            )
        } != 0,
        "The running manager did not respond; close any dialog and try the desktop shortcut again"
    );
    ensure!(
        response == 1,
        "The running manager must be restarted with the updated app before opening this shortcut"
    );
    Ok(true)
}

impl BackgroundGuard {
    pub(super) fn install(
        window: NonZeroIsize,
        context: egui::Context,
        backend: Backend,
        tray: Tray,
    ) -> std::io::Result<Self> {
        let pump = Rc::new(Pump {
            backend,
            context,
            tray,
            window,
            last_status: RefCell::new(String::new()),
            attention: Cell::new(false),
            error: RefCell::new(None),
            open_message: open_message()?,
            open_game_controls: Cell::new(false),
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
        reveal(self.pump.window, &self.pump.context);
    }

    pub(super) fn error(&self) -> Option<String> {
        self.pump.error.borrow().clone()
    }

    pub(super) fn take_open_game_controls(&self) -> bool {
        self.pump.open_game_controls.replace(false)
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
    if let Some(pump) = pump
        && message == pump.open_message
    {
        pump.open_game_controls.set(true);
        reveal(pump.window, &pump.context);
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
    fn update(&self) -> Result<()> {
        let snapshot = self.backend.snapshot()?;
        let status = if snapshot.error.is_some() || snapshot.textures.recovery_needed {
            "Action needed - open dashboard"
        } else if snapshot.game_running {
            "Playing here - save writes paused"
        } else if snapshot.remote_game {
            "Partner playing - save writes paused"
        } else if snapshot.self_updates.preparing {
            "Preparing verified Twilight Sync update"
        } else if snapshot.texture_busy {
            "Installing / switching texture data"
        } else if snapshot.install_busy {
            "Checking / updating official Dusklight"
        } else if snapshot.self_updates.available() {
            "Twilight Sync update available - open tray"
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
            && !snapshot.self_updates.checking
            && !snapshot.self_updates.preparing
            && !snapshot.game_running
            && !snapshot.remote_game
            && snapshot.settings.play_lock.is_none()
            && !snapshot.install_busy
            && !snapshot.texture_busy;
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
            || snapshot.pending_iso
            || snapshot.pending_recovery
            || snapshot.textures.recovery_needed
            || snapshot.comparison_code.is_some();
        let previous = self.attention.replace(attention);
        if snapshot.shutdown || (attention && !previous) {
            reveal(self.window, &self.context);
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
            reveal(self.window, &self.context);
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

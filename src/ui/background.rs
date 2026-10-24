use super::Tray;
use crate::runtime::{Backend, Command};
use anyhow::Result;
use eframe::egui;
use std::{
    cell::{Cell, RefCell},
    num::NonZeroIsize,
    rc::Rc,
};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{IsWindow, KillTimer, SW_RESTORE, SetTimer, ShowWindow},
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
        });
        PUMP.with(|cell| *cell.borrow_mut() = Some(pump.clone()));
        // A GUI-thread timer keeps the tray alive without rendering the hidden dashboard.
        let timer = unsafe { SetTimer(window.get() as HWND, TIMER_ID, 1000, Some(tick)) };
        if timer == 0 {
            PUMP.with(|cell| *cell.borrow_mut() = None);
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { pump, timer })
    }

    pub(super) fn reveal(&self) {
        reveal(self.pump.window, &self.pump.context);
    }

    pub(super) fn error(&self) -> Option<String> {
        self.pump.error.borrow().clone()
    }
}

impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        let window = self.pump.window.get() as HWND;
        unsafe {
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

impl Pump {
    fn update(&self) -> Result<()> {
        let snapshot = self.backend.snapshot()?;
        let status = if snapshot.error.is_some() || snapshot.textures.recovery_needed {
            "Action needed - open dashboard"
        } else if snapshot.game_running {
            "Playing here - save writes paused"
        } else if snapshot.remote_game {
            "Partner playing - save writes paused"
        } else if snapshot.texture_busy {
            "Installing / switching texture data"
        } else if snapshot.install_busy {
            "Checking / updating official Dusklight"
        } else if snapshot.ready {
            "Secure partner connected"
        } else {
            "Offline - locks preserved"
        };
        self.tray.startup.set_checked(snapshot.settings.startup);
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

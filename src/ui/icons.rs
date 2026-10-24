use anyhow::{Context, Result, ensure};
use eframe::egui;

const WINDOW_PNG: &[u8] = include_bytes!(r"..\..\assets\app-icon-256.png");
const TRAY_PNG: &[u8] = include_bytes!(r"..\..\assets\app-icon-32.png");

pub(super) fn window() -> Result<egui::IconData> {
    eframe::icon_data::from_png_bytes(WINDOW_PNG).context("Loading the embedded window icon")
}

pub(super) fn tray() -> Result<egui::IconData> {
    eframe::icon_data::from_png_bytes(TRAY_PNG).context("Loading the embedded tray icon")
}

pub(super) fn set_application_identity() -> Result<()> {
    let id: Vec<u16> = "false00.TwilightSync"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(id.as_ptr())
    };
    ensure!(
        result >= 0,
        "Cannot set Windows taskbar identity (HRESULT {result:#x})"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_taskbar_identity_is_registered_for_the_process() -> Result<()> {
        set_application_identity()?;
        let mut value = std::ptr::null_mut();
        let result = unsafe {
            windows_sys::Win32::UI::Shell::GetCurrentProcessExplicitAppUserModelID(&mut value)
        };
        ensure!(
            result >= 0 && !value.is_null(),
            "Windows did not retain the application identity"
        );
        let mut units = Vec::new();
        for index in 0..128 {
            let unit = unsafe { *value.add(index) };
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        unsafe { windows_sys::Win32::System::Com::CoTaskMemFree(value.cast()) };
        assert_eq!(String::from_utf16(&units)?, "false00.TwilightSync");
        Ok(())
    }

    #[test]
    fn icons_have_visible_pixels_at_native_and_high_dpi_sizes() -> Result<()> {
        for (icon, size) in [(tray()?, 32), (window()?, 256)] {
            assert_eq!((icon.width, icon.height), (size, size));
            assert_eq!(icon.rgba.len(), size as usize * size as usize * 4);
            let (pixels, remainder) = icon.rgba.as_chunks::<4>();
            assert!(remainder.is_empty());
            let visible = pixels.iter().filter(|pixel| pixel[3] > 200).count();
            assert!(visible > size as usize * size as usize / 2);
            assert!(
                pixels
                    .iter()
                    .any(|pixel| pixel[..3] == [255, 255, 255] && pixel[3] == 255)
            );
        }
        Ok(())
    }

    #[test]
    fn executable_icon_contains_all_windows_dpi_sizes() -> Result<()> {
        let bytes = include_bytes!(r"..\..\assets\twilight-sync.ico");
        assert_eq!(&bytes[..6], &[0, 0, 1, 0, 7, 0]);
        let (entries, remainder) = bytes[6..118].as_chunks::<16>();
        assert!(remainder.is_empty());
        for (entry, expected) in entries.iter().zip([16, 24, 32, 48, 64, 128, 256]) {
            let size = if entry[0] == 0 {
                256
            } else {
                u32::from(entry[0])
            };
            assert_eq!(size, expected);
            let length = u32::from_le_bytes(entry[8..12].try_into()?) as usize;
            let offset = u32::from_le_bytes(entry[12..16].try_into()?) as usize;
            let png = bytes
                .get(offset..offset + length)
                .context("Invalid ICO image range")?;
            let icon = eframe::icon_data::from_png_bytes(png)?;
            assert_eq!((icon.width, icon.height), (expected, expected));
        }
        Ok(())
    }
}

use crate::files::{atomic_write, ensure_safe_path};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Appearance {
    System,
    Light,
    #[default]
    Dark,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    version: u8,
    appearance: Appearance,
}

pub(super) struct Store {
    path: Option<PathBuf>,
    pub appearance: Appearance,
}

impl Store {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("interface.json");
        ensure_safe_path(&path)?;
        let appearance = if path.try_exists()? {
            ensure!(
                path.metadata()?.len() <= 4096,
                "Interface preferences exceed their size limit"
            );
            let preferences: Preferences = serde_json::from_slice(&fs::read(&path)?)
                .context("Reading interface preferences")?;
            ensure!(
                preferences.version == 1,
                "Unsupported interface preferences"
            );
            preferences.appearance
        } else {
            Appearance::Dark
        };
        Ok(Self {
            path: Some(path),
            appearance,
        })
    }

    #[cfg(debug_assertions)]
    pub fn preview(theme: &str) -> Result<Self> {
        let appearance = match theme {
            "dark" => Appearance::Dark,
            "light" => Appearance::Light,
            "system" => Appearance::System,
            _ => anyhow::bail!("Choose dark, light or system"),
        };
        Ok(Self {
            path: None,
            appearance,
        })
    }

    pub fn dark(&self) -> Result<bool> {
        match self.appearance {
            Appearance::Dark => Ok(true),
            Appearance::Light => Ok(false),
            Appearance::System => {
                let result = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
                    .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
                    .and_then(|key| key.get_value::<u32, _>("AppsUseLightTheme"));
                match result {
                    Ok(value) => Ok(value == 0),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(error).context("Reading the Windows application theme"),
                }
            }
        }
    }

    pub fn set_dark(&mut self, dark: bool) -> Result<()> {
        let appearance = if dark {
            Appearance::Dark
        } else {
            Appearance::Light
        };
        if let Some(path) = &self.path {
            atomic_write(
                path,
                &serde_json::to_vec_pretty(&Preferences {
                    version: 1,
                    appearance,
                })?,
            )?;
        }
        self.appearance = appearance;
        Ok(())
    }
}

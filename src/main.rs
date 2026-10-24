#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

#[cfg(windows)]
const PROJECT_LICENSE: &str = include_str!(r"..\LICENSE");
#[cfg(not(windows))]
const PROJECT_LICENSE: &str = include_str!("../LICENSE");
#[cfg(windows)]
const DEPENDENCY_NOTICES: &str = include_str!(r"..\THIRD_PARTY_NOTICES.txt");
#[cfg(not(windows))]
const DEPENDENCY_NOTICES: &str = include_str!("../THIRD_PARTY_NOTICES.txt");

fn main() {
    if let Err(error) = run() {
        eprintln!("Twilight Sync: {error:#}");
        #[cfg(windows)]
        if std::env::args_os().nth(1).is_none_or(|a| {
            a == "--background"
                || a == "--apply-update"
                || a == twilight_sync::desktop::OPEN_ARGUMENT
        }) {
            rfd::MessageDialog::new()
                .set_title("Twilight Sync - action needed")
                .set_description(format!("{error:#}"))
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    if first
        .as_deref()
        .is_some_and(|argument| argument == "--apply-update")
    {
        let plan = PathBuf::from(arguments.next().context("Missing private update plan")?);
        if arguments.next().is_some() {
            bail!("Unexpected update helper arguments");
        }
        return twilight_sync::self_update::apply(&twilight_sync::config::root_directory()?, &plan);
    }
    if first.as_deref().is_some_and(|a| a == "--help" || a == "-h") {
        println!(
            "Twilight Sync\n\n  twilight-sync                 Open the native dashboard and tray\n  twilight-sync --background    Start in the tray (Windows startup)\n  twilight-sync --open-dusklight Open protected Play controls, reusing the running manager\n  twilight-sync --check-save PATH\n                                Validate a GCI without changing anything\n  twilight-sync --check-iso PATH\n                                Verify your unmodified GameCube ISO\n  twilight-sync --check-network Check the current router without trusting it\n  twilight-sync --check-updates Check official Dusklight release metadata\n  twilight-sync --check-app-updates\n                                Check Twilight Sync's own release metadata\n  twilight-sync --licenses      Print embedded project/dependency licenses\n\nThe first desktop launch enables Windows startup by default. No LAN is trusted automatically. App update checks are optional; installation requires approval."
        );
        return Ok(());
    }
    #[cfg(windows)]
    if first
        .as_deref()
        .is_some_and(|argument| argument == twilight_sync::desktop::OPEN_ARGUMENT)
    {
        if arguments.next().is_some() {
            bail!("Unexpected desktop shortcut arguments");
        }
        if twilight_sync::ui::open_dusklight_controls()? {
            return Ok(());
        }
        return twilight_sync::ui::run(twilight_sync::runtime::start()?, false);
    }
    if first.as_deref().is_some_and(|a| a == "--licenses") {
        if arguments.next().is_some() {
            bail!("Unexpected extra arguments");
        }
        println!("{PROJECT_LICENSE}\n\n{DEPENDENCY_NOTICES}");
        return Ok(());
    }
    #[cfg(all(windows, debug_assertions))]
    if first.as_deref().is_some_and(|a| a == "--ui-smoke") {
        let mut background = false;
        let mut page = "overview".to_owned();
        let mut theme = "system".to_owned();
        let mut scenario = "offline".to_owned();
        let mut scale = 1.0_f32;
        while let Some(argument) = arguments.next() {
            if argument == "--background" {
                background = true;
            } else if argument == "--page" {
                page = arguments
                    .next()
                    .context("Missing UI smoke page")?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("UI smoke page must be UTF-8"))?;
            } else if argument == "--theme" {
                theme = arguments
                    .next()
                    .context("Missing UI smoke theme")?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("UI smoke theme must be UTF-8"))?;
            } else if argument == "--scenario" {
                scenario = arguments
                    .next()
                    .context("Missing UI smoke scenario")?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("UI smoke scenario must be UTF-8"))?;
            } else if argument == "--scale" {
                scale = arguments
                    .next()
                    .context("Missing UI smoke scale")?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("UI smoke scale must be UTF-8"))?
                    .parse()
                    .context("UI smoke scale must be numeric")?;
            } else {
                bail!(
                    "UI smoke accepts --background, --page PAGE, --theme system|light|dark, --scenario offline|ready|pairing|connecting|attention, and --scale NUMBER"
                );
            }
        }
        println!("UI_SMOKE_STARTED (no persistent changes; exits after 20 seconds)");
        twilight_sync::ui::run_smoke(
            twilight_sync::runtime::ui_smoke_backend(&scenario)?,
            background,
            &page,
            &theme,
            scale,
        )?;
        println!("UI_SMOKE_OK");
        return Ok(());
    }
    if first.as_deref().is_some_and(|a| a == "--check-updates") {
        if arguments.next().is_some() {
            bail!("Unexpected extra arguments");
        }
        let release = twilight_sync::install::latest_release()?;
        println!(
            "OFFICIAL DUSKLIGHT RELEASE\nVersion: {}\nAsset: {}\nSize: {}\nSHA-256: {}\nNo program or settings were changed.",
            release.tag, release.asset, release.size, release.sha256
        );
        return Ok(());
    }
    if first
        .as_deref()
        .is_some_and(|argument| argument == "--check-app-updates")
    {
        if arguments.next().is_some() {
            bail!("Unexpected extra arguments");
        }
        let release = twilight_sync::self_update::check()?;
        println!(
            "TWILIGHT SYNC RELEASE\nCurrent: {}\nPublished: {}\nUpdate available: {}\nSHA-256: {}\nNo program or settings were changed.",
            env!("CARGO_PKG_VERSION"),
            release.tag,
            release.update_available,
            release.sha256
        );
        return Ok(());
    }
    if first.as_deref().is_some_and(|a| a == "--check-network") {
        if arguments.next().is_some() {
            bail!("Unexpected extra arguments");
        }
        let network = twilight_sync::lan::probe()?;
        println!(
            "PRIVATE LAN\nAdapter: {}\nAddress: {}/{}\nRouter: {}\nRouter MAC: {}\nFingerprint: {}\nNo network trust was stored.",
            network.adapter,
            network.address,
            network.prefix,
            network.gateway,
            network.router_mac,
            network.fingerprint
        );
        return Ok(());
    }
    if first.as_deref().is_some_and(|a| a == "--check-iso") {
        let path = std::path::absolute(PathBuf::from(
            arguments.next().context("Missing ISO file path")?,
        ))?;
        if arguments.next().is_some() {
            bail!("Unexpected extra arguments");
        }
        let (hash, size) = twilight_sync::files::verify_iso(&path)?;
        println!("VERIFIED GAMECUBE ISO\nSize: {size}\nSHA-256: {hash}");
        return Ok(());
    }
    if first.as_deref().is_some_and(|a| a == "--check-save") {
        let path = PathBuf::from(arguments.next().context("Missing GCI file path")?);
        if arguments.next().is_some() {
            bail!("Unexpected extra arguments");
        }
        let path = std::path::absolute(path)?;
        let bytes = twilight_sync::files::read_save(&path)?;
        let identity = twilight_sync::files::validate_save(&bytes)?;
        println!(
            "VALID GCI\nGame: {}\nSave: {}\nSHA-256: {}",
            identity.game,
            identity.name,
            twilight_sync::files::sha256(&bytes)
        );
        return Ok(());
    }
    let background = first.as_deref().is_some_and(|a| a == "--background");
    if (first.is_some() && !background) || arguments.next().is_some() {
        bail!("Unknown arguments; use --help");
    }
    #[cfg(windows)]
    {
        twilight_sync::ui::run(twilight_sync::runtime::start()?, background)
    }
    #[cfg(not(windows))]
    {
        let _ = background;
        bail!(
            "The desktop app currently supports Windows; --check-save and the core tests are portable"
        );
    }
}

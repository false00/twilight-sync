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
                || a == twilight_sync::desktop::OPEN_TOTK_ARGUMENT
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
        .is_some_and(|argument| argument == "--verify-rvz-worker")
    {
        let path = PathBuf::from(arguments.next().context("Missing RVZ verifier path")?);
        if arguments.next().is_some() {
            bail!("Unexpected RVZ verifier arguments");
        }
        return twilight_sync::disc::worker_entry(&path);
    }
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
            "Twilight Sync\n\n  twilight-sync                 Open the native dashboard and tray\n  twilight-sync --background    Start in the tray (Windows startup)\n  twilight-sync --open-dusklight Open protected Play controls, reusing the running manager\n  twilight-sync --check-save-location\n                                Show the automatically detected Dusklight save\n  twilight-sync --check-save PATH\n                                Validate a GCI or raw-card save without changes\n  twilight-sync --check-iso PATH\n                                Verify your unmodified GameCube ISO or RVZ\n  twilight-sync --check-network Check the current router without trusting it\n  twilight-sync --check-updates Check official Dusklight release metadata\n  twilight-sync --check-app-updates\n                                Check Twilight Sync's own release metadata\n  twilight-sync --licenses      Print embedded project/dependency licenses\n\nThe first desktop launch enables Windows startup by default. No LAN is trusted automatically. App update checks are optional; installation requires approval."
        );
        println!(
            "\nTotK saves:\n  twilight-sync --open-totk       Open protected TotK Play controls\n  twilight-sync --check-totk-save\n  twilight-sync --export-totk-save ABSOLUTE_PATH\n  twilight-sync --import-totk-save ABSOLUTE_PATH --expected HASH|missing\n\nChoose a game on Home, Games, or Save history. Both updated PCs share the encrypted protocol, safe automatic sync, branch choices, compact history and game-scoped play protection."
        );
        return Ok(());
    }
    #[cfg(windows)]
    if first
        .as_deref()
        .is_some_and(|argument| argument == twilight_sync::desktop::OPEN_TOTK_ARGUMENT)
    {
        if arguments.next().is_some() {
            bail!("Unexpected desktop shortcut arguments");
        }
        if twilight_sync::ui::open_totk_controls()? {
            return Ok(());
        }
        return twilight_sync::ui::run_totk(twilight_sync::runtime::start()?);
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
    if first
        .as_deref()
        .is_some_and(|argument| argument == "--check-totk-save")
    {
        if arguments.next().is_some() {
            bail!("TotK save discovery does not accept a custom profile");
        }
        twilight_sync::totk::ensure_stopped()?;
        let profile = twilight_sync::totk::default_install()?.join("profile");
        let save = twilight_sync::totk::snapshot(&profile)?;
        let summary = save
            .as_ref()
            .map(|save| twilight_sync::totk::summary(&save.bytes))
            .transpose()?;
        let slots: Vec<_> = summary
            .as_ref()
            .into_iter()
            .flat_map(|summary| summary.slots.iter())
            .map(|slot| {
                serde_json::json!({"slot":slot.number+1,"files":slot.files,"bytes":slot.bytes,
                "sha256":slot.hash,"details":slot.details,"details_unavailable":slot.details_error})
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"game": "totk", "profile": profile, "hash": save.as_ref().map(|save| &save.hash), "files": save.as_ref().map_or(0, |save| save.files), "bytes": save.as_ref().map_or(0, |save| save.bytes.len()), "slots":slots, "changed": false})
        );
        return Ok(());
    }
    if first
        .as_deref()
        .is_some_and(|argument| argument == "--export-totk-save")
    {
        use std::io::Write;
        let destination = PathBuf::from(
            arguments
                .next()
                .context("Missing TotK export destination")?,
        );
        if arguments.next().is_some() {
            bail!("Unexpected TotK export arguments");
        }
        twilight_sync::files::ensure_safe_path(&destination)?;
        twilight_sync::totk::ensure_stopped()?;
        let profile = twilight_sync::totk::default_install()?.join("profile");
        let save = twilight_sync::totk::snapshot(&profile)?.context("No committed TotK save")?;
        let mut file = std::fs::File::create_new(&destination)?;
        file.write_all(&save.bytes)?;
        file.sync_all()?;
        twilight_sync::totk::ensure_stopped()?;
        anyhow::ensure!(
            twilight_sync::totk::snapshot(&profile)?
                .is_some_and(|current| current.hash == save.hash),
            "TotK progress changed during export; do not import this bundle"
        );
        println!(
            "{}",
            serde_json::json!({"game": "totk", "hash": save.hash, "files": save.files, "bytes": save.bytes.len(), "export": destination, "save_changed": false})
        );
        return Ok(());
    }
    if first
        .as_deref()
        .is_some_and(|argument| argument == "--import-totk-save")
    {
        use std::io::Read;
        let source = PathBuf::from(arguments.next().context("Missing TotK import bundle")?);
        anyhow::ensure!(
            arguments
                .next()
                .as_deref()
                .is_some_and(|argument| argument == "--expected"),
            "TotK import requires --expected HASH or --expected missing"
        );
        let expected = arguments
            .next()
            .context("Missing expected TotK save hash")?
            .into_string()
            .map_err(|_| anyhow::anyhow!("Expected hash must be UTF-8"))?;
        anyhow::ensure!(
            expected == "missing" || twilight_sync::files::valid_hash(&expected),
            "Invalid expected TotK hash"
        );
        if arguments.next().is_some() {
            bail!("Unexpected TotK import arguments");
        }
        twilight_sync::files::ensure_safe_path(&source)?;
        let mut bytes = Vec::new();
        std::fs::File::open(&source)?
            .take(twilight_sync::totk::MAX_BUNDLE_SIZE as u64 + 1)
            .read_to_end(&mut bytes)?;
        let profile = twilight_sync::totk::default_install()?.join("profile");
        let backup = twilight_sync::totk::apply(
            &profile,
            &bytes,
            (expected != "missing").then_some(expected.as_str()),
        )?;
        println!(
            "{}",
            serde_json::json!({"game": "totk", "hash": twilight_sync::files::sha256(&bytes), "backup": backup, "status": "imported_and_verified"})
        );
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
    if first
        .as_deref()
        .is_some_and(|a| a == "--check-save-location")
    {
        if arguments.next().is_some() {
            bail!("Automatic save detection does not accept a custom directory");
        }
        let settings = twilight_sync::config::Settings::load(&twilight_sync::config::root_path()?)?;
        let default = twilight_sync::save_location::default_profile()?;
        let profile =
            twilight_sync::save_location::profile(&default, settings.game_exe.as_deref())?;
        let path = twilight_sync::save_location::discover(&profile)?;
        let hash = twilight_sync::save_location::hash(&profile, &path)?;
        println!(
            "AUTOMATIC DUSKLIGHT SAVE\nProfile: {}\nSave: {}\nState: {}\nSHA-256: {}\nNo files or settings were changed.",
            profile.display(),
            path.display(),
            if hash.is_some() {
                "Valid Dusklight save"
            } else {
                "No save yet; automatic receive destination ready"
            },
            hash.as_deref().unwrap_or("none"),
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
        let verified = twilight_sync::disc::verify(&path)?;
        println!(
            "VERIFIED GAMECUBE {}\nSize: {}\nSHA-256: {}",
            verified.format.label(),
            verified.size,
            verified.sha256
        );
        return Ok(());
    }
    if first.as_deref().is_some_and(|a| a == "--check-save") {
        let path = PathBuf::from(
            arguments
                .next()
                .context("Missing GCI or raw-card file path")?,
        );
        if arguments.next().is_some() {
            bail!("Unexpected extra arguments");
        }
        let path = std::path::absolute(path)?;
        let bytes = twilight_sync::files::read_save(&path)?;
        let identity = twilight_sync::files::validate_save(&bytes)?;
        println!(
            "VALID DUSKLIGHT SAVE\nGame: {}\nSave: {}\nSHA-256: {}",
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

use crate::{
    files::{self, ensure_safe_path},
    install::{self, ReleaseInfo},
    totk,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

pub const SETUP_DOWNLOAD_URL: &str =
    "https://github.com/valentderah/totk-pc-recomp/releases/latest";

const PENDING: &str = "twilight-update-pending.json";
const INSTALL_PENDING: &str = "totk-install-pending.json";
const MAX_ENTRIES: usize = 16_384;
const MAX_EXPANDED: u64 = 2 * 1024 * 1024 * 1024;
const STDOUT: &str = "twilight-update.jsonl";
const STDERR: &str = "twilight-update-stderr.log";

#[derive(Deserialize)]
struct Installation {
    format: u8,
    release: String,
    steps: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct Completion {
    #[serde(rename = "type")]
    kind: String,
    result: String,
}

#[derive(PartialEq, Eq)]
struct Preserved {
    save: Option<String>,
    settings: Option<String>,
}

pub struct Updated {
    pub executable: PathBuf,
    pub tag: String,
    pub sha256: String,
    pub log: PathBuf,
    pub expected_save: Option<String>,
    pub fresh: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SetupInputs {
    pub base: Option<PathBuf>,
    pub update: Option<PathBuf>,
    pub keys: Option<PathBuf>,
    pub firmware: Option<PathBuf>,
}

impl From<&crate::source_build::Preference> for SetupInputs {
    fn from(preference: &crate::source_build::Preference) -> Self {
        Self {
            base: preference.game_input.clone(),
            update: preference.update.clone(),
            keys: preference.keys.clone(),
            firmware: preference.firmware.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingInstall {
    version: u8,
    requested: PathBuf,
    target: PathBuf,
}

pub fn needs_setup(destination: &Path) -> Result<bool> {
    let path = destination.join("totk-install.json");
    ensure_safe_path(&path)?;
    Ok(!path.try_exists()?)
}

pub fn validate_setup_inputs(destination: &Path, inputs: &SetupInputs) -> Result<()> {
    if !needs_setup(destination)? {
        return Ok(());
    }
    for (path, label) in [
        (&inputs.base, "your TotK base NSP/XCI"),
        (&inputs.update, "your supported TotK update NSP/NSZ"),
        (&inputs.keys, "your console's prod.keys"),
    ] {
        let path = path.as_ref().with_context(|| {
            format!("Choose {label} in Games before the automatic first installation")
        })?;
        ensure_safe_path(path)?;
        ensure!(path.is_file(), "{label} was not found");
    }
    if let Some(firmware) = &inputs.firmware {
        ensure_safe_path(firmware)?;
        ensure!(
            firmware.is_dir(),
            "The optional firmware folder was not found"
        );
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    ensure_safe_path(path)?;
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "TotK metadata exceeds its limit"
    );
    Ok(bytes)
}

fn version(value: &str) -> Result<semver::Version> {
    ensure!(value.len() <= 80, "TotK release version is too long");
    semver::Version::parse(value.strip_prefix('v').unwrap_or(value))
        .context("Invalid TotK release version")
}

pub(crate) fn installed_release(destination: &Path) -> Result<String> {
    let bytes = read_bounded(&destination.join("totk-install.json"), 256 * 1024)
        .context("The TotK installation receipt could not be read; existing program files were not overwritten")?;
    let installation: Installation =
        serde_json::from_slice(&bytes).context("Reading the TotK installation receipt")?;
    ensure!(
        installation.format == 1 && !installation.steps.is_empty(),
        "Unsupported or incomplete TotK installation receipt"
    );
    version(&installation.release)?;
    Ok(installation.release)
}

pub(crate) fn ensure_complete(destination: &Path) -> Result<()> {
    let pending = destination.join(PENDING);
    ensure_safe_path(&pending)?;
    ensure!(
        !pending.try_exists()?,
        "A TotK update is incomplete. Run Install / update TotK Recomp again before playing or syncing"
    );
    Ok(())
}

fn same_directory(left: &Path, right: &Path) -> Result<bool> {
    ensure_safe_path(left)?;
    ensure_safe_path(right)?;
    Ok(left == right
        || (left.try_exists()?
            && right.try_exists()?
            && left.canonicalize()? == right.canonicalize()?))
}

pub(crate) fn ensure_complete_at(root: &Path, destination: &Path) -> Result<()> {
    ensure_complete(destination)?;
    let path = root.join(INSTALL_PENDING);
    ensure_safe_path(&path)?;
    if path.try_exists()? {
        let pending: PendingInstall = serde_json::from_slice(&read_bounded(&path, 16 * 1024)?)?;
        ensure!(
            pending.version == 1,
            "Unsupported pending TotK installation state"
        );
        ensure!(
            !same_directory(&pending.requested, destination)?
                && !same_directory(&pending.target, destination)?,
            "A TotK install/update is incomplete. Run Install / update TotK Recomp again before playing or syncing"
        );
    }
    Ok(())
}

fn preserved(destination: &Path) -> Result<Preserved> {
    let ini = destination.join("totk.ini");
    ensure_safe_path(&ini)?;
    Ok(Preserved {
        save: totk::snapshot(&destination.join("profile"))?.map(|save| save.hash),
        settings: if ini.try_exists()? {
            Some(files::sha256(&read_bounded(&ini, 1024 * 1024)?))
        } else {
            None
        },
    })
}

fn setup_version(setup: &Path) -> Result<String> {
    let bytes = read_bounded(&setup.join("VERSION"), 256)?;
    let value = std::str::from_utf8(&bytes)?.trim();
    version(value)?;
    Ok(value.into())
}

fn unpack_setup(archive: &Path, destination: &Path, release: &ReleaseInfo) -> Result<PathBuf> {
    let executable = install::extract_package(
        archive,
        destination,
        "TotK-Setup.exe",
        MAX_ENTRIES,
        MAX_EXPANDED,
    )?;
    let setup = destination.join(executable.parent().context("TotK setup has no directory")?);
    for relative in [
        PathBuf::from("python").join("python.exe"),
        PathBuf::from("installer").join("__main__.py"),
    ] {
        ensure!(
            setup.join(relative).is_file(),
            "TotK setup is missing its bundled installer"
        );
    }
    ensure!(
        version(&setup_version(&setup)?)?.cmp_precedence(&version(&release.tag)?)
            == Ordering::Equal,
        "TotK setup version differs from the official release"
    );
    Ok(setup)
}

fn prepare(root: &Path, release: &ReleaseInfo) -> Result<PathBuf> {
    let packages = root.join("apps").join("totk");
    ensure_safe_path(&packages)?;
    fs::create_dir_all(&packages)?;
    let staging = tempfile::Builder::new()
        .prefix(&format!("{}-{}-", release.tag, &release.sha256[..12]))
        .tempdir_in(packages)?;
    let archive = staging.path().join("release.zip");
    install::download_archive(&install::official_client()?, release, &archive)?;
    let unpack = staging.path().join("unpacked");
    fs::create_dir(&unpack)?;
    let setup = unpack_setup(&archive, &unpack, release)?;
    fs::remove_file(archive)?;
    let relative = setup.strip_prefix(staging.path())?.to_path_buf();
    // The game's mod-rebuild command refers back to this setup and its build cache.
    Ok(staging.keep().join(relative))
}

fn setup_command(
    setup: &Path,
    destination: &Path,
    inputs: Option<&SetupInputs>,
) -> Result<Command> {
    let mut command = Command::new(setup.join("python").join("python.exe"));
    command
        .args(["-B", "-m", "installer", "--dest"])
        .arg(destination)
        .arg("--root")
        .arg(setup)
        .current_dir(setup)
        .stdin(Stdio::null())
        .stdout(File::create_new(setup.join(STDOUT))?)
        .stderr(File::create_new(setup.join(STDERR))?);
    if let Some(inputs) = inputs {
        command
            .args(["--lang", "en-US"])
            .arg("--base")
            .arg(inputs.base.as_ref().context("Missing TotK base game")?)
            .arg("--update")
            .arg(inputs.update.as_ref().context("Missing TotK update")?)
            .arg("--keys")
            .arg(inputs.keys.as_ref().context("Missing console keys file")?);
        if let Some(firmware) = &inputs.firmware {
            command.arg("--firmware").arg(firmware);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    Ok(command)
}

fn run_setup(command: &mut Command) -> Result<ExitStatus> {
    #[cfg(windows)]
    let job = crate::process_job::Job::new(None, None)?;
    let mut child = command
        .spawn()
        .context("Starting the verified TotK installer")?;
    #[cfg(windows)]
    if let Err(error) = job.assign(&child) {
        child
            .kill()
            .context("Stopping the unassigned TotK installer")?;
        child.wait()?;
        return Err(error);
    }
    let status = child.wait()?;
    #[cfg(windows)]
    drop(job);
    Ok(status)
}

fn verify_updated(
    destination: &Path,
    expected_version: &str,
    before: &Preserved,
    log: &Path,
) -> Result<PathBuf> {
    ensure_safe_path(log)?;
    let mut file = File::open(log)?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(64 * 1024)))?;
    let mut tail = Vec::new();
    file.take(64 * 1024).read_to_end(&mut tail)?;
    let last = tail
        .rsplit(|byte| *byte == b'\n')
        .find(|line| !line.iter().all(u8::is_ascii_whitespace))
        .context("TotK setup produced no completion event")?;
    let completion: Completion = serde_json::from_slice(last)
        .context("TotK setup did not report a valid completion event")?;
    ensure!(
        completion.kind == "end" && completion.result == "ok",
        "TotK setup did not complete successfully"
    );
    ensure!(
        installed_release(destination)? == expected_version,
        "The installed TotK version differs from the verified setup"
    );
    let executable = destination.join("totk.exe");
    ensure_safe_path(&executable)?;
    ensure!(executable.is_file(), "TotK setup did not produce totk.exe");
    let after = totk::snapshot(&destination.join("profile"))?.map(|save| save.hash);
    ensure!(
        before.save == after,
        "TotK saves changed during the update; automatic sync remains blocked"
    );
    Ok(executable)
}

pub fn update_official(root: &Path, destination: &Path) -> Result<Updated> {
    install_or_update(root, destination, &SetupInputs::default())
}

fn fresh_target(
    destination: &Path,
    release: &ReleaseInfo,
    pending: Option<PendingInstall>,
) -> Result<PathBuf> {
    fs::create_dir_all(destination)?;
    if let Some(pending) = pending {
        ensure!(
            pending.version == 1
                && same_directory(&pending.requested, destination)?
                && pending.target.starts_with(&pending.requested)
                && pending.target != pending.requested,
            "The pending TotK setup belongs to another folder; it was not overwritten"
        );
        ensure_safe_path(&pending.target)?;
        Ok(pending.target)
    } else {
        Ok(tempfile::Builder::new()
            .prefix(&format!(
                "release-{}-{}-",
                release.tag,
                &release.sha256[..12]
            ))
            .tempdir_in(destination)?
            .keep()
            .join("game"))
    }
}

pub fn install_or_update(root: &Path, destination: &Path, inputs: &SetupInputs) -> Result<Updated> {
    ensure_safe_path(destination)?;
    let fresh = needs_setup(destination)?;
    validate_setup_inputs(destination, inputs)?;
    let current = if fresh {
        None
    } else {
        Some(installed_release(destination)?)
    };
    let pending_path = root.join(INSTALL_PENDING);
    ensure_safe_path(&pending_path)?;
    let pending = if pending_path.try_exists()? {
        let pending: PendingInstall =
            serde_json::from_slice(&read_bounded(&pending_path, 16 * 1024)?)?;
        ensure!(
            pending.version == 1
                && (same_directory(&pending.requested, destination)?
                    || same_directory(&pending.target, destination)?),
            "Another TotK folder has an unfinished install/update; finish that operation first"
        );
        Some(pending)
    } else {
        None
    };
    ensure!(
        cfg!(windows),
        "Automatic TotK updates currently require Windows"
    );
    totk::ensure_stopped()?;
    let before = preserved(destination)?;
    let release = install::latest_totk_release()?;
    if let Some(current) = &current {
        ensure!(
            version(current)?.cmp_precedence(&version(&release.tag)?) != Ordering::Greater,
            "The installed TotK version is newer than the latest stable release; it was not downgraded"
        );
    }
    let setup = prepare(root, &release)?;
    totk::ensure_stopped()?;
    ensure!(
        preserved(destination)? == before,
        "TotK saves or settings changed while downloading; the installer was not run"
    );
    let expected_version = setup_version(&setup)?;
    let log = setup.join(STDOUT);
    let target = if fresh {
        fresh_target(destination, &release, pending)?
    } else {
        destination.to_path_buf()
    };
    let mut command = setup_command(&setup, &target, fresh.then_some(inputs))?;
    files::atomic_write(
        &pending_path,
        &serde_json::to_vec(&PendingInstall {
            version: 1,
            requested: destination.into(),
            target: target.clone(),
        })?,
    )?;
    let executable = (|| {
        let status = run_setup(&mut command)?;
        ensure!(status.success(), "TotK setup failed ({status})");
        totk::ensure_stopped()?;
        if fresh {
            for relative in ["profile", "mods", "shared-fonts", "totk.ini"] {
                let source = destination.join(relative);
                ensure_safe_path(&source)?;
                if source.try_exists()? { crate::source_build::copy_tree(&source, &target.join(relative))?; }
            }
        }
        verify_updated(&target, &expected_version, &before, &log)
    })()
    .with_context(|| {
        format!(
            "TotK install/update did not finish. Retry Install / update TotK Recomp to resume. Installer logs: {} and {}",
            log.display(),
            setup.join(STDERR).display()
        )
    })?;
    fs::remove_file(pending_path).context(
        "TotK was installed and verified, but its pending-install marker could not be cleared",
    )?;
    let legacy_marker = destination.join(PENDING);
    if legacy_marker.try_exists()? {
        fs::remove_file(legacy_marker).context("Clearing the previous TotK update marker")?;
    }
    Ok(Updated {
        executable,
        tag: release.tag,
        sha256: release.sha256,
        log,
        expected_save: before.save,
        fresh,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_arch = "x86_64")]
    use std::io::Write;
    #[cfg(target_arch = "x86_64")]
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    #[cfg(windows)]
    fn pending_install_guards_and_resume_recognize_windows_directory_case_aliases() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let requested = temp.path().join("TotKRecomp");
        let target = requested.join("release-staged").join("game");
        fs::create_dir_all(&target)?;
        let pending = PendingInstall {
            version: 1,
            requested: requested.clone(),
            target: target.clone(),
        };
        files::atomic_write(
            &temp.path().join(INSTALL_PENDING),
            &serde_json::to_vec(&pending)?,
        )?;
        let requested_alias = requested.with_file_name("TOTKRECOMP");
        let target_alias = target.with_file_name("GAME");
        assert!(ensure_complete_at(temp.path(), &requested_alias).is_err());
        assert!(ensure_complete_at(temp.path(), &target_alias).is_err());
        assert_eq!(
            fresh_target(
                &requested_alias,
                &install::tests::totk_release_fixture()?,
                Some(pending)
            )?,
            target
        );
        Ok(())
    }

    fn receipt(destination: &Path, release: &str) -> Result<()> {
        fs::create_dir_all(destination)?;
        files::atomic_write(
            &destination.join("totk-install.json"),
            &serde_json::to_vec(&serde_json::json!({
                "format": 1,
                "release": release,
                "steps": {"compile": {"key": "synthetic"}},
                "lang": "en-US",
            }))?,
        )
    }

    #[test]
    fn fresh_installs_require_local_inputs_before_network_or_staging() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let destination = temp.path().join("game");
        fs::create_dir(&destination)?;
        fs::write(destination.join("totk.exe"), b"never executed")?;
        let error = update_official(&temp.path().join("manager"), &destination)
            .err()
            .context("A fresh installation without local game files was accepted")?;
        assert!(format!("{error:#}").contains("Choose your TotK base"));
        let inputs = SetupInputs {
            base: Some(destination.join("missing.nsp")),
            ..Default::default()
        };
        assert!(validate_setup_inputs(&destination, &inputs).is_err());
        assert!(!temp.path().join("manager").try_exists()?);
        assert_eq!(fs::read(destination.join("totk.exe"))?, b"never executed");
        Ok(())
    }

    #[test]
    fn setup_targets_the_selected_folder_using_only_its_bundled_tools() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let destination = temp.path().join("selected game");
        let command = setup_command(temp.path(), &destination, None)?;
        assert_eq!(
            command.get_program(),
            temp.path().join("python").join("python.exe")
        );
        let expected = [
            "-B".as_ref(),
            "-m".as_ref(),
            "installer".as_ref(),
            "--dest".as_ref(),
            destination.as_os_str(),
            "--root".as_ref(),
            temp.path().as_os_str(),
        ];
        assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
        assert_eq!(command.get_current_dir(), Some(temp.path()));
        assert!(!destination.try_exists()?);
        Ok(())
    }

    #[test]
    fn fresh_installs_pass_local_inputs_and_resume_at_a_stable_empty_target() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let destination = temp.path().join("Games").join("TotKRecomp");
        fs::create_dir_all(&destination)?;
        fs::write(destination.join("totk.exe"), b"unmanaged installation kept")?;
        let inputs = SetupInputs {
            base: Some(temp.path().join("base game.nsp")),
            update: Some(temp.path().join("update.nsp")),
            keys: Some(temp.path().join("prod.keys")),
            firmware: Some(temp.path().join("firmware")),
        };
        for path in [&inputs.base, &inputs.update, &inputs.keys]
            .into_iter()
            .flatten()
        {
            fs::write(path, b"synthetic input; never executed")?;
        }
        fs::create_dir(
            inputs
                .firmware
                .as_ref()
                .context("Missing fixture firmware")?,
        )?;
        assert!(needs_setup(&destination)?);
        validate_setup_inputs(&destination, &inputs)?;
        let target = fresh_target(&destination, &install::tests::totk_release_fixture()?, None)?;
        assert!(target.starts_with(&destination));
        assert!(!target.try_exists()?);
        let command = setup_command(temp.path(), &target, Some(&inputs))?;
        let args: Vec<_> = command.get_args().collect();
        assert!(args.windows(2).any(|args| args == ["--lang", "en-US"]));
        for (flag, path) in [
            ("--base", &inputs.base),
            ("--update", &inputs.update),
            ("--keys", &inputs.keys),
            ("--firmware", &inputs.firmware),
        ] {
            let path = path.as_ref().context("Missing fixture input")?;
            assert!(
                args.windows(2)
                    .any(|args| args == [flag.as_ref(), path.as_os_str()])
            );
        }
        let pending = PendingInstall {
            version: 1,
            requested: destination.clone(),
            target: target.clone(),
        };
        files::atomic_write(
            &temp.path().join(INSTALL_PENDING),
            &serde_json::to_vec(&pending)?,
        )?;
        assert!(ensure_complete_at(temp.path(), &destination).is_err());
        assert!(ensure_complete_at(temp.path(), &target).is_err());
        ensure_complete_at(temp.path(), &temp.path().join("other game"))?;
        assert_eq!(
            fresh_target(
                &destination,
                &install::tests::totk_release_fixture()?,
                Some(pending)
            )?,
            target
        );
        assert!(!target.try_exists()?);
        assert_eq!(
            fs::read(destination.join("totk.exe"))?,
            b"unmanaged installation kept"
        );
        let unrelated = PendingInstall {
            version: 1,
            requested: destination.clone(),
            target: temp.path().join("unrelated"),
        };
        assert!(
            fresh_target(
                &destination,
                &install::tests::totk_release_fixture()?,
                Some(unrelated)
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn completed_updates_require_matching_version_and_preserved_progress() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let destination = temp.path().join("game");
        receipt(&destination, "0.4.1+abcdef12")?;
        fs::write(destination.join("totk.exe"), b"never executed")?;
        fs::write(destination.join("totk.ini"), b"[language]\nlang=zh-CN\n")?;
        let profile = destination.join("profile");
        let slot = profile
            .join("save")
            .join(format!("{:032x}", 1))
            .join("slot_00");
        fs::create_dir_all(&slot)?;
        fs::write(profile.join("account.uid"), 1_u128.to_le_bytes())?;
        fs::write(slot.join("progress.sav"), b"synthetic progress")?;
        let before = preserved(&destination)?;
        let log = temp.path().join(STDOUT);
        fs::write(&log, b"{\"type\":\"end\",\"result\":\"ok\"}\n")?;
        assert_eq!(
            verify_updated(&destination, "0.4.1+abcdef12", &before, &log)?,
            destination.join("totk.exe")
        );
        assert!(verify_updated(&destination, "0.4.0", &before, &log).is_err());
        fs::write(&log, b"{\"type\":\"end\",\"result\":\"failed\"}\n")?;
        assert!(verify_updated(&destination, "0.4.1+abcdef12", &before, &log).is_err());
        fs::write(&log, b"{\"type\":\"step\",\"result\":\"ok\"}\n")?;
        assert!(verify_updated(&destination, "0.4.1+abcdef12", &before, &log).is_err());
        fs::write(&log, b"{\"type\":\"end\",\"result\":\"ok\"}\n")?;
        fs::write(destination.join("totk.ini"), b"[language]\nlang=zh-Hans\n")?;
        verify_updated(&destination, "0.4.1+abcdef12", &before, &log)?;
        fs::write(slot.join("progress.sav"), b"changed progress")?;
        assert!(verify_updated(&destination, "0.4.1+abcdef12", &before, &log).is_err());
        Ok(())
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn setup_archives_support_the_upstream_wrapper_and_large_file_inventory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("setup.zip");
        let mut writer = ZipWriter::new(File::create(&archive)?);
        for (name, bytes) in [
            ("TotK-Setup.exe", b"never executed".as_slice()),
            ("python/python.exe", b"never executed".as_slice()),
            ("installer/__main__.py", b"never executed".as_slice()),
            ("VERSION", b"0.4.1+abcdef12\n".as_slice()),
        ] {
            writer.start_file(
                format!("TotK-Setup-0.4.1-windows/{name}"),
                SimpleFileOptions::default(),
            )?;
            writer.write_all(bytes)?;
        }
        for index in 0..600 {
            writer.start_file(
                format!("TotK-Setup-0.4.1-windows/runtime/header-{index}.h"),
                SimpleFileOptions::default(),
            )?;
        }
        writer.finish()?;
        let unpack = temp.path().join("unpacked");
        fs::create_dir(&unpack)?;
        let mut release = install::tests::totk_release_fixture()?;
        let setup = unpack_setup(&archive, &unpack, &release)?;
        assert_eq!(setup, unpack.join("TotK-Setup-0.4.1-windows"));
        assert!(setup.join("runtime").join("header-599.h").is_file());
        release.tag = "v0.4.0".into();
        assert!(unpack_setup(&archive, &temp.path().join("other"), &release).is_err());
        Ok(())
    }

    #[test]
    fn unfinished_updates_remain_blocked_across_restarts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        ensure_complete(temp.path())?;
        files::atomic_write(&temp.path().join(PENDING), b"{}")?;
        let error = ensure_complete(temp.path())
            .err()
            .context("Incomplete update was accepted")?;
        assert!(format!("{error:#}").contains("Run Install / update TotK Recomp again"));
        assert!(temp.path().join(PENDING).is_file());
        Ok(())
    }

    #[test]
    #[cfg(all(windows, target_arch = "x86_64"))]
    #[ignore = "Verifies a supplied official public setup ZIP and runs only its --help, never the game"]
    fn official_setup_can_be_verified_without_installing_or_running_the_game() -> Result<()> {
        let archive =
            PathBuf::from(std::env::var_os("TWILIGHT_TOTK_SETUP_ARCHIVE").context(
                "Set TWILIGHT_TOTK_SETUP_ARCHIVE to the latest official Windows setup ZIP",
            )?);
        let release = install::latest_totk_release()?;
        ensure!(
            install::digest_file(&archive, release.size)? == release.sha256,
            "Official archive digest differs"
        );
        let temp = tempfile::tempdir()?;
        let unpack = temp.path().join("unpacked");
        fs::create_dir(&unpack)?;
        let setup = unpack_setup(&archive, &unpack, &release)?;
        let destination = temp.path().join("game-not-created");
        let status = setup_command(&setup, &destination, None)?
            .arg("--help")
            .status()?;
        ensure!(status.success(), "Official setup --help failed");
        let help = fs::read_to_string(setup.join(STDOUT))?;
        ensure!(
            help.contains("--dest") && help.contains("--root"),
            "Official setup does not support the expected interface"
        );
        ensure!(
            !destination.try_exists()?,
            "The read-only setup check created a game installation"
        );
        println!(
            "Verified {}: official setup extracted and CLI checked; no game was installed or run.",
            release.tag
        );
        Ok(())
    }
}

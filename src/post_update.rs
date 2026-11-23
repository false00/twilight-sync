//! What follows a game update. TotK's prepared shaders are built into the graphics driver's cache for the
//! folder the game now runs from, and the installs, install backups and build workspaces no game uses are
//! removed.

use crate::{
    files::{atomic_write, ensure_safe_path},
    game_layout::{self, mentions_backup, regular_metadata, source_version, within},
    play::ProcessMonitor,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

const WARMED: &str = "totk-shaders.json";
const PREPARED: &str = "shader-prepared.bin";
// Upstream vk_pipeline.c: while the driver builds, each worker names its variant in a slot of this file.
// A run that dies leaves it behind, and TotK's next start skips the variants it names from then on.
const JOURNAL: &str = "shader-prepare-journal.bin";
const WARM_RETRIES: usize = 3;
const WARM_LIMIT: Duration = Duration::from_secs(30 * 60);
const BACKUP_PREFIX: &str = ".twilight-sync-backup-";
const MAX_ENTRIES: usize = 4096;
const MAX_METADATA: u64 = 16 * 1024 * 1024;
const GAMES: [&str; 4] = ["totk.exe", "dusklight.exe", "Triforce.exe", "longshot.exe"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Warmed {
    version: u8,
    executable: PathBuf,
    prepared: u64,
    prepared_at: u64,
    drivers: String,
    state: State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    // A warm-up runs, or ran when Twilight Sync stopped.
    Running,
    Warmed,
    Stopped,
}

/// A managed game folder and the install in it a game uses. Versions beside an unknown install stay.
pub(crate) struct Managed {
    pub folder: PathBuf,
    pub active: Option<PathBuf>,
}

pub(crate) struct Plan {
    warm: Option<PathBuf>,
    remove: Vec<PathBuf>,
    // Install backups kept because a settings file still names them.
    referenced: Vec<PathBuf>,
    games: PathBuf,
    managed: Vec<Managed>,
    keep: Vec<PathBuf>,
    metadata: Vec<PathBuf>,
}

pub(crate) struct Outcome {
    pub summary: String,
    pub failures: Vec<String>,
    pub interrupted: bool,
}

enum Warm {
    Done(String),
    Interrupted,
}

enum Run {
    Ended(ExitStatus),
    // Stopped mid-build because a game started or the time limit passed.
    Stopped,
}

impl Plan {
    /// The work left after an update, if any. `metadata` lists the files besides Twilight Sync's own
    /// settings that can name a game folder, and `totk` is the active TotK install.
    pub(crate) fn new(
        root: &Path,
        games: PathBuf,
        managed: Vec<Managed>,
        keep: Vec<PathBuf>,
        metadata: Vec<PathBuf>,
        totk: Option<&Path>,
    ) -> Result<Option<Self>> {
        let warm = match totk {
            // A build still in its version folder moves into place first; setup prepared it there.
            Some(install) if !staged(install) && warm_needed(root, install)? => {
                Some(install.to_path_buf())
            }
            _ => None,
        };
        let (remove, referenced) = superseded(root, &games, &managed, &keep, &metadata)?;
        Ok((warm.is_some() || !remove.is_empty()).then_some(Self {
            warm,
            remove,
            referenced,
            games,
            managed,
            keep,
            metadata,
        }))
    }

    pub(crate) fn describe(&self) -> String {
        let folders = match self.remove.len() {
            0 => String::new(),
            1 => "1 superseded game folder".into(),
            count => format!("{count} superseded game folders"),
        };
        match (self.warm.is_some(), folders.is_empty()) {
            (true, true) => {
                "Preparing TotK's shaders for its folder so play starts without shader stutter..."
                    .into()
            }
            (true, false) => {
                format!("Preparing TotK's shaders for its folder, then removing {folders}...")
            }
            _ => format!("Removing {folders}..."),
        }
    }
}

/// Clears what a warm-up left when Twilight Sync stopped during it. Its job ended the run mid-build, and
/// TotK's next start would take the variants its journal names for driver crashes and skip them for good.
pub(crate) fn recover(root: &Path) -> Result<()> {
    let Some(mut last) = read_warmed(root)? else {
        return Ok(());
    };
    // A TotK running now started after that run, and its start already took the journal.
    if last.state != State::Running || ProcessMonitor::default().named_game_running("totk.exe") {
        return Ok(());
    }
    if let Some(install) = last.executable.parent() {
        let journal = install.join(JOURNAL);
        ensure_safe_path(&journal)?;
        remove_file(&journal)?;
    }
    last.state = State::Stopped;
    write_warmed(root, &last)
}

pub(crate) fn run(root: &Path, plan: &Plan, log: &Path) -> Result<Outcome> {
    run_with(
        root,
        plan,
        log,
        &mut |install| {
            let mut command = Command::new(install.join("totk.exe"));
            command.arg("--warm-shaders");
            command
        },
        &mut games_running,
    )
}

/// Whether a supported game runs outside Twilight Sync's build and warm-up processes.
pub(crate) fn games_running() -> bool {
    ProcessMonitor::default().named_games_running(&GAMES)
}

fn run_with(
    root: &Path,
    plan: &Plan,
    log: &Path,
    command: &mut dyn FnMut(&Path) -> Command,
    running: &mut dyn FnMut() -> bool,
) -> Result<Outcome> {
    let mut done = Vec::new();
    let mut failures = Vec::new();
    let mut interrupted = false;
    if let Some(install) = &plan.warm {
        match warm(root, install, log, &mut || command(install), running) {
            Ok(Warm::Done(summary)) => done.push(summary),
            Ok(Warm::Interrupted) => interrupted = true,
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    let mut count = 0;
    let mut bytes = 0;
    if !interrupted {
        // Settings can change while the shaders are prepared.
        let (current, _) =
            superseded(root, &plan.games, &plan.managed, &plan.keep, &plan.metadata)?;
        for folder in plan.remove.iter().filter(|folder| current.contains(folder)) {
            if running() {
                interrupted = true;
                break;
            }
            match remove_folder(folder) {
                Ok(size) => {
                    count += 1;
                    bytes += size;
                }
                Err(error) => failures.push(format!("Removing {}: {error:#}", folder.display())),
            }
        }
    }
    if count > 0 {
        done.push(format!(
            "Removed {count} superseded game folder{} ({:.1} GiB).",
            if count == 1 { "" } else { "s" },
            bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        ));
    }
    if !plan.referenced.is_empty() {
        let folders: Vec<_> = plan
            .referenced
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        done.push(format!(
            "Kept {}: a saved setting still names {}.",
            folders.join(", "),
            if folders.len() == 1 { "it" } else { "them" }
        ));
    }
    if interrupted {
        done.push("A game started, so the rest continues after it closes.".into());
    }
    Ok(Outcome {
        summary: done.join(" "),
        failures,
        interrupted,
    })
}

fn staged(install: &Path) -> bool {
    install
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("game"))
        && install
            .parent()
            .and_then(name)
            .is_some_and(|name| source_version(name, "source-"))
}

// The driver keys its shader cache by the executable's path and is emptied by a driver update.
fn warm_needed(root: &Path, install: &Path) -> Result<bool> {
    Ok(match identity(install)? {
        Some(current) => read_warmed(root)?.as_ref() != Some(&current),
        None => false,
    })
}

fn warm(
    root: &Path,
    install: &Path,
    log: &Path,
    command: &mut dyn FnMut() -> Command,
    running: &mut dyn FnMut() -> bool,
) -> Result<Warm> {
    let Some(current) = identity(install)? else {
        return Ok(Warm::Done(
            "TotK has no prepared shader list to warm.".into(),
        ));
    };
    write_warmed(
        root,
        &Warmed {
            state: State::Running,
            ..current.clone()
        },
    )?;
    let result = warm_runs(install, log, command, running);
    let state = if matches!(result, Ok(Warm::Done(_))) {
        State::Warmed
    } else {
        State::Stopped
    };
    write_warmed(root, &Warmed { state, ..current })?;
    result
}

// As upstream tools/prepare_shaders.py, a run the driver crashed in is repeated without those variants.
fn warm_runs(
    install: &Path,
    log: &Path,
    command: &mut dyn FnMut() -> Command,
    running: &mut dyn FnMut() -> bool,
) -> Result<Warm> {
    let journal = install.join(JOURNAL);
    ensure_safe_path(&journal)?;
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    let output = File::create(log).with_context(|| format!("Creating {}", log.display()))?;
    let started = Instant::now();
    for _ in 0..=WARM_RETRIES {
        if running() {
            return Ok(Warm::Interrupted);
        }
        let mut command = command();
        command
            .current_dir(install)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output.try_clone()?);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
            command.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
        }
        let status = match attempt(command, started + WARM_LIMIT, running)? {
            Run::Ended(status) => status,
            Run::Stopped => {
                // Its workers were cut off, not crashed: TotK must not skip the variants they name.
                remove_file(&journal)?;
                ensure!(
                    started.elapsed() < WARM_LIMIT,
                    "TotK's shader warm-up did not finish within {} minutes. Log: {}",
                    WARM_LIMIT.as_secs() / 60,
                    log.display()
                );
                return Ok(Warm::Interrupted);
            }
        };
        if status.success() {
            return Ok(Warm::Done(summary(log, started)));
        }
        if !driver_crashed(&journal)? {
            bail!(
                "TotK's shader warm-up stopped ({status}); its shaders build as the game first draws them. Log: {}",
                log.display()
            );
        }
    }
    bail!(
        "The graphics driver kept crashing during TotK's shader warm-up; those shaders build as the game first draws them. Log: {}",
        log.display()
    )
}

fn attempt(
    mut command: Command,
    deadline: Instant,
    running: &mut dyn FnMut() -> bool,
) -> Result<Run> {
    // In a source-build job the run is not taken for play, and it ends with Twilight Sync.
    #[cfg(windows)]
    let job = crate::process_job::Job::for_source_build()?;
    let mut child = command.spawn().context("Starting TotK's shader warm-up")?;
    #[cfg(windows)]
    if let Err(error) = job.assign(&child) {
        child
            .kill()
            .context("Stopping the unassigned shader warm-up")?;
        child.wait()?;
        return Err(error);
    }
    let waited = loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Run::Ended(status)),
            Ok(None) if Instant::now() >= deadline || running() => break Ok(Run::Stopped),
            Ok(None) => thread::sleep(Duration::from_millis(250)),
            Err(error) => {
                break Err(anyhow::Error::new(error).context("Waiting for TotK's shader warm-up"));
            }
        }
    };
    // Closing the job ends the run.
    #[cfg(windows)]
    drop(job);
    #[cfg(not(windows))]
    let _ = child.kill();
    // Its journal can be removed once the process is gone.
    child.wait()?;
    waited
}

/// Folders Twilight Sync made that no game uses: install backups in `games`, version folders beside a
/// known active install, and build workspaces. A folder holding an active install or a path in `keep`
/// stays, and so does one that a settings file names: those backups are returned second. Nothing is
/// planned while a folder move can still need its folders.
fn superseded(
    root: &Path,
    games: &Path,
    managed: &[Managed],
    keep: &[PathBuf],
    metadata: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    if game_layout::pending(root)? {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut settings = Vec::new();
    for path in game_layout::metadata_paths(root).iter().chain(metadata) {
        ensure_safe_path(path)?;
        if let Some(found) = regular_metadata(path)?.filter(|found| found.is_file()) {
            ensure!(
                found.len() <= MAX_METADATA,
                "{} exceeds its size limit",
                path.display()
            );
            settings.push(fs::read(path)?);
        }
    }
    let kept: Vec<&Path> = keep
        .iter()
        .map(PathBuf::as_path)
        .chain(managed.iter().filter_map(|game| game.active.as_deref()))
        .collect();
    let active = |path: &Path| kept.iter().any(|kept| within(kept, path));
    let named = |path: &Path| {
        name(path).is_none_or(|name| settings.iter().any(|bytes| mentions_backup(bytes, name)))
    };
    let mut found = Vec::new();
    let mut referenced = Vec::new();
    for path in folders(games)? {
        if name(&path)
            .and_then(|name| name.strip_prefix(BACKUP_PREFIX))
            .is_some_and(|token| {
                token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            && !active(&path)
        {
            if named(&path) {
                referenced.push(path);
            } else {
                found.push(path);
            }
        }
    }
    for game in managed.iter().filter(|game| game.active.is_some()) {
        for path in folders(&game.folder)? {
            if name(&path).is_some_and(|name| source_version(name, "source-"))
                && !active(&path)
                && !named(&path)
            {
                found.push(path);
            }
        }
    }
    for game in folders(&root.join("source-builds"))? {
        for path in folders(&game)? {
            if name(&path).is_some_and(|name| source_version(name, ""))
                && !active(&path)
                && !named(&path)
            {
                found.push(path);
            }
        }
    }
    found.sort();
    referenced.sort();
    Ok((found, referenced))
}

fn identity(install: &Path) -> Result<Option<Warmed>> {
    let executable = install.join("totk.exe");
    let prepared = install.join(PREPARED);
    ensure_safe_path(&executable)?;
    ensure_safe_path(&prepared)?;
    let (Some(program), Some(list)) =
        (regular_metadata(&executable)?, regular_metadata(&prepared)?)
    else {
        return Ok(None);
    };
    if !program.is_file() || !list.is_file() {
        return Ok(None);
    }
    Ok(Some(Warmed {
        version: 1,
        executable,
        prepared: list.len(),
        prepared_at: list.modified()?.duration_since(UNIX_EPOCH)?.as_secs(),
        drivers: graphics_drivers(),
        state: State::Warmed,
    }))
}

// The installed display drivers and their versions.
fn graphics_drivers() -> String {
    #[cfg(windows)]
    {
        use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
        const DISPLAY: &str =
            r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
        let Ok(class) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(DISPLAY) else {
            return String::new();
        };
        let mut drivers: Vec<String> = class
            .enum_keys()
            .filter_map(Result::ok)
            .filter(|key| key.len() == 4 && key.bytes().all(|byte| byte.is_ascii_digit()))
            .filter_map(|key| {
                let adapter = class.open_subkey(key).ok()?;
                let description: String = adapter.get_value("DriverDesc").ok()?;
                let version: String = adapter.get_value("DriverVersion").ok()?;
                Some(format!("{description} {version}"))
            })
            .collect();
        drivers.sort();
        drivers.join("; ")
    }
    #[cfg(not(windows))]
    String::new()
}

fn read_warmed(root: &Path) -> Result<Option<Warmed>> {
    let path = root.join(WARMED);
    ensure_safe_path(&path)?;
    if !regular_metadata(&path)?.is_some_and(|found| found.is_file() && found.len() <= 64 * 1024) {
        return Ok(None);
    }
    // An unreadable record only means the shaders are warmed again.
    Ok(serde_json::from_slice(&fs::read(&path)?).ok())
}

fn write_warmed(root: &Path, warmed: &Warmed) -> Result<()> {
    atomic_write(&root.join(WARMED), &serde_json::to_vec_pretty(warmed)?)
}

fn driver_crashed(journal: &Path) -> Result<bool> {
    let Some(found) = regular_metadata(journal)? else {
        return Ok(false);
    };
    ensure!(
        found.is_file() && found.len() <= 1024 * 1024,
        "Unexpected TotK shader journal at {}",
        journal.display()
    );
    Ok(fs::read(journal)?
        .chunks(64)
        .any(|slot| slot.iter().take(32).any(|byte| *byte != 0)))
}

fn summary(log: &Path, started: Instant) -> String {
    let seconds = started.elapsed().as_secs();
    match built_variants(log) {
        Some(count) => format!(
            "TotK's {count} prepared shader variants are in the graphics driver's cache for its folder ({seconds} s)."
        ),
        None => format!(
            "TotK's prepared shaders are in the graphics driver's cache for its folder ({seconds} s)."
        ),
    }
}

fn built_variants(log: &Path) -> Option<u64> {
    let mut bytes = Vec::new();
    File::open(log)
        .ok()?
        .take(MAX_METADATA)
        .read_to_end(&mut bytes)
        .ok()?;
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .find_map(|line| {
            let (count, rest) = line.strip_prefix("prepare-shaders: ")?.split_once(' ')?;
            (rest.starts_with("variants built") || rest.starts_with("variants on"))
                .then(|| count.parse().ok())?
        })
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            Err(error).with_context(|| format!("Removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

fn remove_folder(folder: &Path) -> Result<u64> {
    ensure_safe_path(folder)?;
    let Some(found) = regular_metadata(folder)? else {
        return Ok(0);
    };
    ensure!(found.is_dir(), "{} is not a folder", folder.display());
    let mut bytes = 0;
    visit(folder, &mut |path, metadata| {
        bytes += metadata.len();
        if metadata.permissions().readonly() {
            // Git checkouts keep read-only object files.
            let mut permissions = metadata.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
        }
        Ok(())
    })?;
    fs::remove_dir_all(folder)?;
    Ok(bytes)
}

// Every file below `folder`, without entering links or junctions.
fn visit(folder: &Path, each: &mut dyn FnMut(&Path, &fs::Metadata) -> Result<()>) -> Result<()> {
    let mut pending = vec![folder.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            match regular_metadata(&path)? {
                Some(found) if found.is_dir() => pending.push(path),
                Some(found) => each(&path, &found)?,
                None => {}
            }
        }
    }
    Ok(())
}

fn folders(directory: &Path) -> Result<Vec<PathBuf>> {
    ensure_safe_path(directory)?;
    if !regular_metadata(directory)?.is_some_and(|found| found.is_dir()) {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(directory)?.take(MAX_ENTRIES) {
        let path = entry?.path();
        if regular_metadata(&path)?.is_some_and(|metadata| metadata.is_dir()) {
            found.push(path);
        }
    }
    Ok(found)
}

fn name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(directory: &Path) -> Result<()> {
        fs::create_dir_all(directory)?;
        fs::write(
            directory.join("totk.exe"),
            b"synthetic file, never executed",
        )?;
        fs::write(directory.join(PREPARED), b"prepared variants")?;
        Ok(())
    }

    fn fake(mode: &'static str) -> Result<impl FnMut() -> Command> {
        let executable = std::env::current_exe()?;
        Ok(move || {
            let mut command = Command::new(&executable);
            command
                .args([
                    "--exact",
                    "post_update::tests::fake_warm_process",
                    "--ignored",
                    "--nocapture",
                ])
                .env("TWILIGHT_FAKE_WARM", mode);
            command
        })
    }

    #[test]
    #[ignore = "Child process for the shader warm-up tests"]
    fn fake_warm_process() -> Result<()> {
        let Ok(mode) = std::env::var("TWILIGHT_FAKE_WARM") else {
            return Ok(());
        };
        let journal = std::env::current_dir()?.join(JOURNAL);
        let crash = || -> Result<()> {
            let mut slot = [0_u8; 64];
            slot[0] = 1;
            fs::write(&journal, slot)?;
            std::process::exit(3)
        };
        match mode.as_str() {
            "fail" => std::process::exit(4),
            "crash" => crash(),
            "crash-once" if !journal.exists() => crash(),
            "slow" => {
                // A worker hands the driver a variant, and the build takes long.
                fs::write(&journal, [1_u8; 64])?;
                thread::sleep(Duration::from_secs(60));
                std::process::exit(6)
            }
            _ => {
                remove_file(&journal)?;
                println!(
                    "prepare-shaders: 12 variants built into the driver cache on 2 threads in 5 ms"
                );
                println!(
                    "prepare-shaders: 3 variants skipped: the graphics driver crashed while building them in an earlier run"
                );
                Ok(())
            }
        }
    }

    #[test]
    fn warm_up_follows_the_executable_and_retries_only_driver_crashes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let game = root.join("TotKRecomp");
        let log = root.join("warm.log");
        let mut idle = || false;
        install(&game)?;
        assert!(warm_needed(root, &game)?);
        let Warm::Done(summary) = warm(root, &game, &log, &mut fake("crash-once")?, &mut idle)?
        else {
            bail!("The warm-up was interrupted");
        };
        assert!(summary.contains("12 prepared shader variants"), "{summary}");
        assert!(!game.join(JOURNAL).exists());
        assert!(!warm_needed(root, &game)?);
        // The driver keys its cache by the executable's path, so a moved build is warmed again.
        let moved = root.join("moved");
        install(&moved)?;
        assert!(warm_needed(root, &moved)?);
        fs::write(game.join(PREPARED), b"a later preparation")?;
        assert!(warm_needed(root, &game)?);

        let error = warm(root, &game, &log, &mut fake("fail")?, &mut idle)
            .err()
            .context("A failed run must not count as warmed")?;
        assert!(format!("{error:#}").contains("warm-up stopped"));
        assert!(warm_needed(root, &game)?);
        let error = warm(root, &game, &log, &mut fake("crash")?, &mut idle)
            .err()
            .context("Driver crashes are retried a limited number of times")?;
        assert!(format!("{error:#}").contains("kept crashing"));
        // TotK's next start records what the driver crashed on, so the journal stays.
        recover(root)?;
        assert!(game.join(JOURNAL).exists());
        assert!(warm_needed(root, &game)?);
        Ok(())
    }

    #[test]
    fn a_warm_up_cut_off_with_twilight_sync_leaves_no_crash_record() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let game = temp.path().join("TotKRecomp");
        install(&game)?;
        let current = identity(&game)?.context("Missing the prepared install")?;
        write_warmed(
            temp.path(),
            &Warmed {
                state: State::Running,
                ..current
            },
        )?;
        fs::write(game.join(JOURNAL), [1_u8; 64])?;
        recover(temp.path())?;
        assert!(!game.join(JOURNAL).exists());
        assert_eq!(
            read_warmed(temp.path())?.map(|last| last.state),
            Some(State::Stopped)
        );
        assert!(warm_needed(temp.path(), &game)?);
        Ok(())
    }

    #[test]
    fn warm_up_stops_when_a_game_starts_and_leaves_no_crash_record() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let game = temp.path().join("TotKRecomp");
        install(&game)?;
        let journal = game.join(JOURNAL);
        // The game starts while a variant is being built.
        let mut started = || journal.exists();
        let begun = Instant::now();
        let outcome = warm(
            temp.path(),
            &game,
            &temp.path().join("warm.log"),
            &mut fake("slow")?,
            &mut started,
        )?;
        assert!(matches!(outcome, Warm::Interrupted));
        assert!(begun.elapsed() < Duration::from_secs(30));
        assert!(!journal.exists());
        assert!(warm_needed(temp.path(), &game)?);
        Ok(())
    }

    #[test]
    fn nothing_is_warmed_without_a_prepared_list_or_before_a_build_moves_into_place() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("manager");
        let games = temp.path().join("Games");
        fs::create_dir_all(&root)?;
        let folder = games.join("TotKRecomp");
        fs::create_dir_all(&folder)?;
        fs::write(folder.join("totk.exe"), b"synthetic file, never executed")?;
        let plan = |totk: &Path| {
            let managed = vec![Managed {
                folder: folder.clone(),
                active: Some(totk.to_path_buf()),
            }];
            Plan::new(
                &root,
                games.clone(),
                managed,
                Vec::new(),
                Vec::new(),
                Some(totk),
            )
        };
        assert!(plan(&folder)?.is_none());
        let staged = folder
            .join("source-v0.5.0-afae80c908ab-Nmmr7y")
            .join("game");
        install(&staged)?;
        assert!(plan(&staged)?.is_none());
        Ok(())
    }

    #[test]
    fn superseded_folders_twilight_sync_made_are_removed_and_the_rest_stays() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("manager");
        let games = temp.path().join("Games");
        let backup = games.join(format!("{BACKUP_PREFIX}{}", "a".repeat(32)));
        let named = games.join(format!("{BACKUP_PREFIX}{}", "b".repeat(32)));
        let disc = games.join(format!("{BACKUP_PREFIX}{}", "c".repeat(32)));
        let unrelated = games.join(format!("{BACKUP_PREFIX}notatoken"));
        let totk = games.join("TotKRecomp");
        let old_version = totk.join("source-v0.4.1-6151ac4fbc2c-3coTxn");
        let awaiting = totk.join("source-v0.5.0-afae80c908ab-Nmmr7y");
        let longshot = games.join("Longshot");
        let old_longshot = longshot.join("source-v3.0.0-09cbdd979eac-dAxZMd");
        let longshot_install = longshot
            .join("source-v3.1.1-a2dc53859018-gMcWug")
            .join("game");
        let triforce_version = games
            .join("Triforce")
            .join("source-v0.5.1-a750a2102538-7aKp4k");
        let builds = root.join("source-builds");
        let old_workspace = builds.join("totk").join("v0.4.1-6151ac4fbc2c-JmahnP");
        let rebuild = builds.join("totk").join("v0.5.0-afae80c908ab-0E98Qb");
        let orphan = builds.join("triforce").join("v0.5.1-a750a2102538-7aKp4k");
        let tools = builds.join("totk").join("not-a-workspace");
        let dusklight = temp.path().join("Dusklight");
        for path in [
            &backup.join("romfs"),
            &named,
            &disc,
            &unrelated,
            &totk.join("mods"),
            &old_version.join("game"),
            &awaiting.join("game"),
            &old_longshot,
            &longshot_install,
            &triforce_version.join("game"),
            &old_workspace.join("source"),
            &rebuild.join("source"),
            &orphan,
            &tools,
            &dusklight,
        ] {
            fs::create_dir_all(path)?;
        }
        let checkout = old_workspace.join("source").join("pack.idx");
        fs::write(&checkout, b"git object index")?;
        let mut permissions = fs::metadata(&checkout)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&checkout, permissions)?;
        fs::write(
            root.join("settings.json"),
            serde_json::to_vec(&serde_json::json!({ "game_exe": named.join("game.exe") }))?,
        )?;
        // Dusklight's own settings and the TotK build's receipt can name folders too.
        fs::write(
            dusklight.join("config.json"),
            serde_json::to_vec(&serde_json::json!({ "backend.isoPath": disc.join("GZ2E01.iso") }))?,
        )?;
        fs::write(
            totk.join("twilight-source.json"),
            serde_json::to_vec(&serde_json::json!({ "source": rebuild.join("source") }))?,
        )?;
        let managed = || {
            vec![
                Managed {
                    folder: totk.clone(),
                    active: Some(totk.clone()),
                },
                Managed {
                    folder: longshot.clone(),
                    active: Some(longshot_install.clone()),
                },
                Managed {
                    folder: games.join("Triforce"),
                    active: None,
                },
            ]
        };
        let metadata = vec![
            dusklight.join("config.json"),
            totk.join("twilight-source.json"),
            totk.join("build-cache").join("rebuild.txt"),
        ];
        let planned = || {
            Plan::new(
                &root,
                games.clone(),
                managed(),
                vec![awaiting.join("game")],
                metadata.clone(),
                None,
            )
        };

        // A folder move that has not finished can still need any of them.
        fs::write(root.join("game-layout-pending.json"), b"{}")?;
        assert!(planned()?.is_none());
        fs::remove_file(root.join("game-layout-pending.json"))?;

        let plan = planned()?.context("Superseded folders were not found")?;
        let mut found = plan.remove.clone();
        found.sort();
        let mut expected = vec![
            backup.clone(),
            old_version.clone(),
            old_longshot.clone(),
            old_workspace.clone(),
            orphan.clone(),
        ];
        expected.sort();
        assert_eq!(found, expected);
        let mut referenced = plan.referenced.clone();
        referenced.sort();
        assert_eq!(referenced, vec![named.clone(), disc.clone()]);
        assert_eq!(plan.describe(), "Removing 5 superseded game folders...");

        let outcome = run_with(
            &root,
            &plan,
            &temp.path().join("unused.log"),
            &mut |_| unreachable!("Nothing needed warming"),
            &mut || false,
        )?;
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert!(!outcome.interrupted);
        assert!(
            outcome
                .summary
                .starts_with("Removed 5 superseded game folders"),
            "{}",
            outcome.summary
        );
        assert!(
            outcome.summary.contains(&format!(
                "Kept {}, {}: a saved setting still names them.",
                named.display(),
                disc.display()
            )),
            "{}",
            outcome.summary
        );
        for path in &expected {
            assert!(!path.exists(), "{} remains", path.display());
        }
        for path in [
            &named,
            &disc,
            &unrelated,
            &totk.join("mods"),
            &awaiting,
            &longshot_install,
            &triforce_version,
            &rebuild,
            &tools,
        ] {
            assert!(path.exists(), "{} was removed", path.display());
        }
        Ok(())
    }

    #[test]
    fn removal_waits_for_a_game_that_starts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("manager");
        let workspace = root
            .join("source-builds")
            .join("dusklight")
            .join("v1.2.0-0123456789ab-Q1w2E3");
        fs::create_dir_all(&workspace)?;
        let plan = Plan::new(
            &root,
            temp.path().join("Games"),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )?
        .context("The workspace was not found")?;
        let outcome = run_with(
            &root,
            &plan,
            &temp.path().join("unused.log"),
            &mut |_| unreachable!("Nothing needed warming"),
            &mut || true,
        )?;
        assert!(outcome.interrupted);
        assert!(workspace.exists());
        Ok(())
    }
}

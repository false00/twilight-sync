pub mod progress;
mod recipes;
mod runner;
mod setup;
mod tools;
pub use setup::{SdkOutcome, SdkSetup};

use crate::{
    files::{self, ensure_safe_path, valid_hash},
    install, native,
    protocol::GameId,
    save_location,
};
use anyhow::{Context, Result, bail, ensure};
use reqwest::{blocking::Client, redirect::Policy};
use runner::Runner;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tools::{Package, Toolset};

pub const WARNING: &str = "Building from source is slower and may take an hour or more. It reduces reliance on prebuilt game executables, but is NOT confirmed to be safer. Source, build scripts and dependencies run with your Windows account's permissions; this is not a sandbox. Use a separate VM for code you do not trust.";

const SCRATCH: &str = "scratch";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    #[default]
    Official,
    Source,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preference {
    pub method: Method,
    pub managed_tools: bool,
    #[serde(skip_serializing, deserialize_with = "discard_saved_jobs")]
    pub jobs: u8,
    pub game_input: Option<PathBuf>,
    pub update: Option<PathBuf>,
    pub keys: Option<PathBuf>,
    pub firmware: Option<PathBuf>,
    pub renodx_hdr: bool,
    pub nvidia_dlss: bool,
    pub amd_fsr4: bool,
    pub prepare_shaders: bool,
}

impl Default for Preference {
    fn default() -> Self {
        Self {
            method: Method::Official,
            managed_tools: true,
            jobs: 0,
            game_input: None,
            update: None,
            keys: None,
            firmware: None,
            renodx_hdr: true,
            nvidia_dlss: false,
            amd_fsr4: false,
            prepare_shaders: true,
        }
    }
}

fn discard_saved_jobs<'de, D: serde::Deserializer<'de>>(value: D) -> Result<u8, D::Error> {
    // Older versions saved the UI's worker count. Overrides are now per invocation.
    u8::deserialize(value).map(|_| 0)
}

fn compiler_jobs(logical: usize, available: u64) -> usize {
    const GIB: u64 = 1024 * 1024 * 1024;
    // Keep room for the OS and linker. Logical CPUs alone can oversubscribe memory.
    let memory_limit = (available.saturating_sub(2 * GIB) / GIB).clamp(1, 16);
    logical.clamp(1, memory_limit as usize)
}

impl Preference {
    fn compiler_jobs(&self) -> usize {
        if self.jobs != 0 {
            return usize::from(self.jobs);
        }
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        compiler_jobs(cpus, system.available_memory())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub version: u8,
    pub dusklight: Preference,
    pub totk: Preference,
    pub triforce: Preference,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            version: 1,
            dusklight: Preference::default(),
            totk: Preference::default(),
            triforce: Preference {
                method: Method::Source,
                ..Preference::default()
            },
        }
    }
}

impl Preferences {
    pub fn get(&self, game: GameId) -> &Preference {
        match game {
            GameId::Dusklight => &self.dusklight,
            GameId::Totk => &self.totk,
            GameId::Triforce => &self.triforce,
        }
    }

    pub fn set(&mut self, game: GameId, preference: Preference) -> Result<()> {
        validate_preference(&preference)?;
        ensure!(
            game != GameId::Triforce || preference.method == Method::Source,
            "Triforce is managed as a source build"
        );
        match game {
            GameId::Dusklight => self.dusklight = preference,
            GameId::Totk => self.totk = preference,
            GameId::Triforce => self.triforce = preference,
        }
        Ok(())
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("source-builds.json");
        ensure_safe_path(&path)?;
        if !path.try_exists()? {
            return Ok(Self::default());
        }
        let preferences: Self = serde_json::from_slice(&read_bounded(&path, 64 * 1024)?)?;
        ensure!(
            preferences.version == 1,
            "Unsupported source-build preferences"
        );
        for preference in [
            &preferences.dusklight,
            &preferences.totk,
            &preferences.triforce,
        ] {
            validate_preference(preference)?;
        }
        ensure!(
            preferences.triforce.method == Method::Source,
            "Triforce requires the source-build method"
        );
        Ok(preferences)
    }

    pub fn persist(&self, root: &Path) -> Result<()> {
        files::atomic_write(
            &root.join("source-builds.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
}

fn validate_preference(preference: &Preference) -> Result<()> {
    ensure!(
        preference.jobs <= 16,
        "A compiler worker override must be between 1 and 16"
    );
    for path in [
        &preference.game_input,
        &preference.update,
        &preference.keys,
        &preference.firmware,
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            path.is_absolute(),
            "Build inputs must be absolute local paths"
        );
        ensure_safe_path(path)?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub game: GameId,
    pub tag: String,
    pub commit: String,
    pub review_url: String,
}

#[derive(Clone, Debug)]
pub struct Prerequisite {
    pub name: &'static str,
    pub ready: bool,
    pub url: &'static str,
    pub setup: Option<SdkSetup>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub preferences: Preferences,
    pub preferences_valid: bool,
    pub busy: bool,
    pub building: bool,
    pub sdk_setup: Option<SdkSetup>,
    pub game: Option<GameId>,
    pub plan: Option<Plan>,
    pub prerequisites: Vec<Prerequisite>,
    pub message: String,
    pub error: Option<String>,
    pub log: Option<PathBuf>,
    pub built: Option<Built>,
    pub progress: progress::State,
    pub completion: Option<Completion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    Ready(GameId),
    NeedsActivation(GameId),
}

impl State {
    pub fn load(root: &Path) -> Self {
        match Preferences::load(root) {
            Ok(preferences) => Self {
                preferences,
                preferences_valid: true,
                ..Self::default()
            },
            Err(error) => Self {
                error: Some(format!(
                    "Source-build preferences need attention: {error:#}. Reset source-build preferences before building; saves and pairing are unaffected."
                )),
                ..Self::default()
            },
        }
    }
}

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Source build cancelled; the previous installation remains selected")
    }
}

impl std::error::Error for Cancelled {}

pub(super) fn check_cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Err(Cancelled.into());
    }
    Ok(())
}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Cancelled>().is_some()
}

pub fn key(game: GameId) -> &'static str {
    match game {
        GameId::Dusklight => "dusklight",
        GameId::Totk => "totk",
        GameId::Triforce => "triforce",
    }
}

fn repository(game: GameId) -> &'static str {
    match game {
        GameId::Dusklight => "TwilitRealm/dusklight",
        GameId::Totk => "valentderah/totk-pc-recomp",
        GameId::Triforce => "triforce/source",
    }
}

pub fn source_url(game: GameId) -> String {
    match game {
        GameId::Dusklight | GameId::Totk => format!("https://github.com/{}", repository(game)),
        GameId::Triforce => "https://git.triforceproject.com/triforce/source".into(),
    }
}

pub(super) fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag.as_bytes()[0].is_ascii_alphanumeric()
        && !tag.contains("..")
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
}

pub(crate) fn valid_commit(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn metadata<T: DeserializeOwned>(url: &str) -> Result<T> {
    let parsed = reqwest::Url::parse(url)?;
    ensure!(
        parsed.scheme() == "https"
            && matches!(
                parsed.host_str(),
                Some("api.github.com" | "git.triforceproject.com")
            )
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.port_or_known_default() == Some(443),
        "Untrusted source/build-tool metadata host"
    );
    let client = Client::builder()
        .user_agent(concat!(
            "twilight-sync/",
            env!("CARGO_PKG_VERSION"),
            " source builder"
        ))
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()?;
    ensure!(
        response.status().is_success() && response.url().as_str() == url,
        "Source metadata request failed ({}): {url}",
        response.status()
    );
    let mut bytes = Vec::new();
    response.take(512 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 512 * 1024,
        "Source metadata exceeds its limit"
    );
    serde_json::from_slice(&bytes).context("Reading source/build-tool metadata")
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

#[derive(Deserialize)]
struct GitObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct GitReference {
    object: GitObject,
}

pub(super) fn github_revision(repository: &str, tag: &str) -> Result<String> {
    ensure!(valid_tag(tag), "Unsafe source tag");
    let reference: GitReference = metadata(&format!(
        "https://api.github.com/repos/{repository}/git/ref/tags/{tag}"
    ))?;
    let mut object = reference.object;
    for _ in 0..4 {
        ensure!(valid_commit(&object.sha), "Invalid source object identity");
        if object.kind == "commit" {
            return Ok(object.sha);
        }
        ensure!(
            object.kind == "tag",
            "Source tag does not identify a commit"
        );
        let annotated: GitReference = metadata(&format!(
            "https://api.github.com/repos/{repository}/git/tags/{}",
            object.sha
        ))?;
        object = annotated.object;
    }
    bail!("Source tag has too many annotated indirections")
}

pub(super) fn github_latest(repository: &str) -> Result<(String, String)> {
    let release: GithubRelease = metadata(&format!(
        "https://api.github.com/repos/{repository}/releases/latest"
    ))?;
    let commit = github_revision(repository, &release.tag_name)?;
    Ok((release.tag_name, commit))
}

#[derive(Deserialize)]
struct TagCommit {
    sha: String,
}
#[derive(Deserialize)]
struct Tag {
    name: String,
    commit: TagCommit,
}

fn triforce_tag(tags: Vec<Tag>) -> Result<(String, String)> {
    let mut stable = Vec::new();
    for tag in tags {
        ensure!(
            valid_tag(&tag.name) && valid_commit(&tag.commit.sha),
            "Invalid Triforce source tag metadata"
        );
        let version = semver::Version::parse(tag.name.strip_prefix('v').unwrap_or(&tag.name))
            .context("Unsupported Triforce source tag version")?;
        if version.pre.is_empty() {
            stable.push((version, tag));
        }
    }
    stable.sort_by(|left, right| left.0.cmp_precedence(&right.0));
    let (_, tag) = stable
        .pop()
        .context("No stable Triforce source tag is available")?;
    Ok((tag.name, tag.commit.sha))
}

pub fn check(game: GameId) -> Result<(Plan, Vec<Prerequisite>)> {
    let (tag, commit) = match game {
        GameId::Dusklight | GameId::Totk => github_latest(repository(game))?,
        GameId::Triforce => triforce_tag(metadata(
            "https://git.triforceproject.com/api/v1/repos/triforce/source/tags?limit=50",
        )?)?,
    };
    ensure!(
        valid_tag(&tag) && valid_commit(&commit),
        "Invalid approved source revision"
    );
    let review_url = format!("{}/commit/{commit}", source_url(game));
    Ok((
        Plan {
            game,
            tag,
            commit,
            review_url,
        },
        recipes::prerequisites(game)?,
    ))
}

pub(super) fn git_command(git: &Path) -> Command {
    let mut command = Command::new(git);
    command
        .args([
            "-c",
            "credential.helper=",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.file.allow=never",
            "-c",
            "protocol.ext.allow=never",
        ])
        .arg("-c")
        .arg(if cfg!(windows) {
            "core.hooksPath=NUL"
        } else {
            "core.hooksPath=/dev/null"
        })
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "https")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1024")
        .env("GIT_HTTP_LOW_SPEED_TIME", "120");
    command
}

fn checkout(
    git: &Path,
    url: &str,
    tag: &str,
    commit: &str,
    directory: &Path,
    submodules: bool,
    runner: &mut Runner<'_>,
) -> Result<()> {
    ensure!(
        valid_tag(tag) && valid_commit(commit),
        "Invalid pinned source revision"
    );
    ensure_safe_path(directory)?;
    ensure!(
        !directory.try_exists()?,
        "Source checkout must use a fresh build directory"
    );
    runner.run(
        git_command(git)
            .args([
                "clone",
                "--no-checkout",
                "--depth",
                "1",
                "--branch",
                tag,
                "--",
            ])
            .arg(url)
            .arg(directory),
        "Fetching the approved source tag",
    )?;
    let actual = runner.capture(
        git_command(git)
            .arg("-C")
            .arg(directory)
            .args(["rev-parse", "HEAD"]),
        "Verifying the source commit",
    )?;
    ensure!(
        std::str::from_utf8(&actual)?.trim() == commit,
        "The source tag changed after approval; check the source version again"
    );
    runner.run(
        git_command(git)
            .arg("-C")
            .arg(directory)
            .args(["checkout", "--detach", commit]),
        "Checking out the pinned source commit",
    )?;
    if submodules {
        runner.run(
            git_command(git).arg("-C").arg(directory).args([
                "submodule",
                "update",
                "--init",
                "--recursive",
                "--depth",
                "1",
            ]),
            "Fetching the source-pinned dependencies",
        )?;
    }
    Ok(())
}

pub fn prerequisites(game: GameId) -> Result<Vec<Prerequisite>> {
    recipes::prerequisites(game)
}

pub fn prepare_tools(
    root: &Path,
    game: GameId,
    log: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(progress::Update) -> Result<()>,
) -> Result<()> {
    let mut runner = Runner::new(log, cancel, progress)?;
    let mut tools = Toolset::new(root)?;
    let packages: &[Package] = match game {
        GameId::Dusklight => &[Package::Git, Package::Uv, Package::Cmake, Package::Ninja],
        GameId::Totk => &[Package::Git, Package::Uv, Package::Llvm],
        GameId::Triforce => &[
            Package::Git,
            Package::Uv,
            Package::Llvm,
            Package::Sdl2,
            Package::Sdl3,
            Package::Ffmpeg,
        ],
    };
    for package in packages {
        tools.ensure(*package, true, cancel, &mut |text| runner.message(&text))?;
    }
    runner.message("Official tool archives verified and cached. Source builds use fresh extractions; manually installed SDK prerequisites are listed separately.")
}

pub fn validate_inputs(
    game: GameId,
    preference: &Preference,
    previous: Option<&Path>,
) -> Result<()> {
    recipes::validate_input_paths(game, preference, previous)
}

pub struct Request {
    pub plan: Plan,
    pub preference: Preference,
    pub previous_install: Option<PathBuf>,
    pub previous_executable: Option<PathBuf>,
    pub approved: bool,
    pub log: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Built {
    pub game: GameId,
    pub executable: PathBuf,
    pub directory: PathBuf,
    pub tag: String,
    pub commit: String,
    pub sha256: String,
    pub source: Option<PathBuf>,
    pub log: PathBuf,
    pub expected_save: Option<String>,
}

#[derive(Serialize)]
struct Receipt<'a> {
    version: u8,
    game: GameId,
    tag: &'a str,
    commit: &'a str,
    sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a Path>,
    tools: &'a [tools::Provenance],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    source_dependencies: &'a [recipes::SourceDependency],
}

pub fn new_log(root: &Path, game: GameId) -> Result<PathBuf> {
    let directory = root.join("build-logs");
    ensure_safe_path(&directory)?;
    fs::create_dir_all(&directory)?;
    Ok(directory.join(format!(
        "{}-{}-{:016x}.log",
        key(game),
        time::OffsetDateTime::now_utc().unix_timestamp(),
        rand::random::<u64>()
    )))
}

fn native_game(game: GameId) -> Result<native::Game> {
    match game {
        GameId::Totk => Ok(native::Game::Totk),
        GameId::Triforce => Ok(native::Game::Triforce),
        GameId::Dusklight => bail!("Dusklight uses a GCI/raw-card profile"),
    }
}

fn save_hash(game: GameId, directory: &Path, executable: Option<&Path>) -> Result<Option<String>> {
    if game == GameId::Dusklight {
        let profile = save_location::profile(&save_location::default_profile()?, executable)?;
        files::save_hash(&save_location::discover(&profile)?)
    } else {
        let game = native_game(game)?;
        Ok(game
            .snapshot(&directory.join(game.profile_name()))?
            .map(|save| save.hash))
    }
}

pub(super) fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    ensure_safe_path(source)?;
    ensure_safe_path(destination)?;
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf(), 0_usize)];
    let mut count = 0_usize;
    let mut total = 0_u64;
    while let Some((source, destination, depth)) = pending.pop() {
        ensure!(depth <= 32, "Preserved game data is nested too deeply");
        ensure_safe_path(&source)?;
        ensure_safe_path(&destination)?;
        if source.is_dir() {
            fs::create_dir_all(&destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                pending.push((entry.path(), destination.join(entry.file_name()), depth + 1));
            }
        } else {
            let metadata = source.metadata()?;
            count += 1;
            total = total
                .checked_add(metadata.len())
                .context("Preserved game-data size overflow")?;
            ensure!(
                metadata.is_file() && count <= 150_000 && total <= 256 * 1024 * 1024 * 1024,
                "Game data exceeds the managed copy limit"
            );
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &destination)
                .with_context(|| format!("Preserving {}", source.display()))?;
        }
    }
    Ok(())
}

pub(crate) fn preserve_dusklight(executable: &Path, output: &Path) -> Result<()> {
    let directory = executable
        .parent()
        .context("Previous Dusklight executable has no directory")?;
    if directory == output {
        return Ok(());
    }
    for relative in ["data_location.json", "data"] {
        let source = directory.join(relative);
        ensure_safe_path(&source)?;
        if source.try_exists()? {
            copy_tree(&source, &output.join(relative))?;
        }
    }
    Ok(())
}

fn preserve_current_data(
    request: &Request,
    previous: &Path,
    output: &Path,
    executable: &Path,
) -> Result<Option<String>> {
    let game = request.plan.game;
    // Compilation is isolated from saves; take the baseline at the handoff.
    let expected_save = save_hash(game, previous, request.previous_executable.as_deref())?;
    if game == GameId::Dusklight {
        if let Some(executable) = &request.previous_executable {
            preserve_dusklight(executable, output)?;
        }
    } else {
        let native = native_game(game)?;
        for relative in [
            native.profile_name(),
            "mods",
            "shared-fonts",
            "totk.ini",
            "triforce_options.ini",
            "triforce_env.txt",
            "language.txt",
        ] {
            let source = previous.join(relative);
            ensure_safe_path(&source)?;
            if source.try_exists()? {
                copy_tree(&source, &output.join(relative))?;
            }
        }
        if game == GameId::Triforce {
            let path = output.join("triforce_env.txt");
            ensure_safe_path(&path)?;
            let mut environment = if path.try_exists()? {
                read_bounded(&path, 1024 * 1024)?
            } else {
                Vec::new()
            };
            // The final assignment wins; this build has no WebGPU fallback.
            environment.extend_from_slice(b"\nRX_BACKEND=vulkan\n");
            files::atomic_write(&path, &environment)?;
        }
    }
    ensure!(
        save_hash(game, previous, request.previous_executable.as_deref())? == expected_save,
        "Progress changed while copying saves; the previous installation was not replaced or selected"
    );
    ensure!(
        save_hash(game, output, Some(executable))? == expected_save,
        "The staged build does not preserve the current save; activation was refused"
    );
    Ok(expected_save)
}

pub(crate) fn games_stopped() -> Result<()> {
    ensure!(
        !crate::play::ProcessMonitor::default().game_running(),
        "Close Dusklight before changing managed game files"
    );
    native::Game::Totk.ensure_stopped()?;
    native::Game::Triforce.ensure_stopped()
}

pub fn verify_activation(
    built: &Built,
    previous: Option<&Path>,
    executable: Option<&Path>,
) -> Result<()> {
    games_stopped()?;
    let previous = previous
        .map(Path::to_path_buf)
        .unwrap_or(install::default_directory(built.game)?);
    ensure!(
        save_hash(built.game, &previous, executable)? == built.expected_save
            && save_hash(built.game, &built.directory, Some(&built.executable))?
                == built.expected_save,
        "Progress changed after this build; no installation was selected. Both folders remain intact"
    );
    ensure!(
        install::digest_file(&built.executable, built.executable.metadata()?.len())?
            == built.sha256,
        "The completed source-build executable changed before activation"
    );
    Ok(())
}

pub fn stage_configured(
    game: GameId,
    approved_commit: &str,
    jobs: u8,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(progress::Update) -> Result<()>,
) -> Result<Built> {
    let root = crate::config::root_directory()?;
    let _instance = crate::config::acquire_instance(&root)?;
    let settings = crate::config::Settings::load(&root)?;
    let preferences = Preferences::load(&root)?;
    let (plan, _) = check(game)?;
    ensure!(
        plan.commit == approved_commit && valid_commit(approved_commit),
        "The source revision differs from the explicitly approved commit"
    );
    let mut preference = preferences.get(game).clone();
    preference.method = Method::Source;
    preference.jobs = jobs;
    let previous_executable = match game {
        GameId::Dusklight => settings.game_exe.clone(),
        GameId::Totk | GameId::Triforce => {
            let native = native_game(game)?;
            let channel = crate::totk::Channel::for_game(
                root.join(format!("{}-transfers", key(game))),
                install::default_directory(game),
                native,
            );
            Some(channel.executable()?)
        }
    };
    let previous_install = previous_executable
        .as_deref()
        .map(|executable| {
            if game == GameId::Dusklight || !executable.is_file() {
                Ok(executable
                    .parent()
                    .context("Game executable has no directory")?
                    .to_path_buf())
            } else {
                native_game(game)?.installation(executable)
            }
        })
        .transpose()?;
    let built = build(
        &root,
        Request {
            plan,
            preference,
            previous_install: previous_install.clone(),
            previous_executable: previous_executable.clone(),
            approved: true,
            log: new_log(&root, game)?,
        },
        cancel,
        progress,
    )?;
    verify_activation(
        &built,
        previous_install.as_deref(),
        previous_executable.as_deref(),
    )?;
    Ok(built)
}

fn discard_build_files(path: &Path, runner: &Runner<'_>) {
    let result = (|| -> Result<()> {
        ensure_safe_path(path)?;
        if path.try_exists()? {
            fs::remove_dir_all(path)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let warning = format!(
            "Could not finish cleaning up temporary build files at {}: {error:#}",
            path.display()
        );
        if let Err(log_error) = runner.note(&warning) {
            eprintln!("{warning}. Could not write to the build log: {log_error:#}");
        }
    }
}

fn finish_workspace(
    workspace: tempfile::TempDir,
    game: GameId,
    completed: bool,
    runner: &Runner<'_>,
) -> Result<()> {
    if completed && game == GameId::Totk && check_cancelled(runner.cancel).is_ok() {
        // The game's rebuild command uses the checkout and Python runtime, not the package caches.
        discard_build_files(&workspace.path().join(SCRATCH), runner);
        if check_cancelled(runner.cancel).is_ok() {
            let _retained = workspace.keep();
            return Ok(());
        }
    }
    // Disarm the guard so deletion is only attempted after rechecking the path.
    discard_build_files(&workspace.keep(), runner);
    check_cancelled(runner.cancel)
}

pub fn build(
    root: &Path,
    request: Request,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(progress::Update) -> Result<()>,
) -> Result<Built> {
    ensure!(
        request.approved,
        "Review the source commit and accept the build warning before starting"
    );
    ensure!(
        cfg!(all(windows, target_arch = "x86_64")),
        "Managed source builds require Windows x64"
    );
    ensure!(
        valid_tag(&request.plan.tag) && valid_commit(&request.plan.commit),
        "Invalid approved source revision"
    );
    ensure!(
        request.plan.review_url
            == format!(
                "{}/commit/{}",
                source_url(request.plan.game),
                request.plan.commit
            ),
        "The source review URL is not the selected project's pinned commit"
    );
    validate_preference(&request.preference)?;
    recipes::validate_inputs(&request)?;
    let missing: Vec<_> = recipes::prerequisites(request.plan.game)?
        .into_iter()
        .filter(|item| !item.ready)
        .collect();
    ensure!(
        missing.is_empty(),
        "Finish the guided source-build prerequisites first: {}",
        missing
            .iter()
            .map(|item| item.name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    check_cancelled(cancel)?;
    games_stopped()?;
    let game = request.plan.game;
    let previous = request
        .previous_install
        .clone()
        .unwrap_or(install::default_directory(game)?);
    let destination = install::default_directory(game)?;
    fs::create_dir_all(&destination)?;
    let minimum = if game == GameId::Dusklight {
        8 * 1024_u64.pow(3)
    } else {
        64 * 1024_u64.pow(3)
    };
    ensure!(
        fs2::available_space(&destination)? >= minimum,
        "Source build needs at least {} GiB free on the Games drive, plus room for preserved mods/portable data",
        minimum / 1024_u64.pow(3)
    );
    let runs = root.join("source-builds").join(key(game));
    ensure_safe_path(&runs)?;
    fs::create_dir_all(&runs)?;
    let workspace = tempfile::Builder::new()
        .prefix(&format!(
            "{}-{}-",
            request.plan.tag,
            &request.plan.commit[..12]
        ))
        .tempdir_in(&runs)?;
    fs::create_dir(workspace.path().join(SCRATCH))?;
    let source = workspace.path().join("source");
    let rebuild_source = (game == GameId::Totk).then_some(source.as_path());
    let output_stage = tempfile::Builder::new()
        .prefix(&format!(
            "source-{}-{}-",
            request.plan.tag,
            &request.plan.commit[..12]
        ))
        .tempdir_in(&destination)?;
    let output = output_stage.path().join("game");
    fs::create_dir(&output)?;
    let mut runner = Runner::new(&request.log, cancel, progress)?;
    let result: Result<(PathBuf, String, Option<String>)> = (|| {
        let mut tools = Toolset::new(root)?;
        let git = tools.ensure(
            Package::Git,
            request.preference.managed_tools,
            cancel,
            &mut |message| runner.message(&message),
        )?;
        checkout(
            &git.entry,
            &format!("{}.git", source_url(game)),
            &request.plan.tag,
            &request.plan.commit,
            &source,
            game == GameId::Dusklight,
            &mut runner,
        )?;
        let uv = tools.ensure(
            Package::Uv,
            request.preference.managed_tools,
            cancel,
            &mut |message| runner.message(&message),
        )?;
        let source_dependencies = recipes::build(
            &request,
            workspace.path(),
            &source,
            &output,
            &git.entry,
            &uv.entry,
            &mut tools,
            &mut runner,
        )?;
        check_cancelled(cancel)?;
        let executable = output.join(match game {
            GameId::Dusklight => "dusklight.exe",
            GameId::Totk => "totk.exe",
            GameId::Triforce => "Triforce.exe",
        });
        ensure_safe_path(&executable)?;
        crate::self_update::validate_pe(&executable)
            .context("The source build did not produce the expected Windows x64 game")?;
        if game == GameId::Triforce {
            for name in ["triforce_vk.dll", "SDL2.dll", "SDL3.dll"] {
                let path = output.join(name);
                ensure_safe_path(&path)?;
                crate::self_update::validate_pe(&path)
                    .with_context(|| format!("Required Triforce runtime component was not built or staged correctly: {name}"))?;
            }
            ensure!(
                output.join("image.bin").is_file() && output.join("romfs").is_dir(),
                "The Triforce build is missing its required local game data"
            );
        }
        games_stopped()?;
        runner.message(if game == GameId::Triforce {
            "Preserving saves, settings and mods; selecting the locally built Vulkan renderer in the new folder (the previous folder is unchanged)..."
        } else {
            "Preserving saves, local settings and mods in the staged build..."
        })?;
        let expected_save = preserve_current_data(&request, &previous, &output, &executable)?;
        check_cancelled(cancel)?;
        let sha256 = install::digest_file(&executable, executable.metadata()?.len())?;
        files::atomic_write(
            &output.join("twilight-source.json"),
            &serde_json::to_vec_pretty(&Receipt {
                version: 1,
                game,
                tag: &request.plan.tag,
                commit: &request.plan.commit,
                sha256: &sha256,
                source: rebuild_source,
                tools: &tools.provenance,
                source_dependencies: &source_dependencies,
            })?,
        )?;
        runner.message("Cleaning up temporary source build files...")?;
        Ok((executable, sha256, expected_save))
    })();
    let cleanup = finish_workspace(workspace, game, result.is_ok(), &runner);
    let (executable, sha256, expected_save) = result
        .and_then(|verified| cleanup.map(|()| verified))
        .with_context(|| {
            format!(
                "Source build did not complete. The previous installation remains selected. Log: {}",
                request.log.display()
            )
        })?;
    let _installed_directory = output_stage.keep();
    Ok(Built {
        game,
        executable,
        directory: output,
        tag: request.plan.tag,
        commit: request.plan.commit,
        sha256,
        source: rebuild_source.map(Path::to_path_buf),
        log: request.log,
        expected_save,
    })
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    ensure_safe_path(path)?;
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Source-build metadata exceeds its limit"
    );
    Ok(bytes)
}

#[derive(Clone, Debug)]
pub struct Revision {
    pub tag: String,
    pub commit: String,
    pub(crate) sha256: String,
}

pub fn selected_revision(path: &Path, game: GameId) -> Result<Revision> {
    #[derive(Deserialize)]
    struct Label {
        version: u8,
        game: GameId,
        tag: String,
        commit: String,
        sha256: String,
    }
    let label: Label = serde_json::from_slice(&read_bounded(path, 1024 * 1024)?)?;
    ensure!(
        label.version == 1
            && label.game == game
            && valid_tag(&label.tag)
            && valid_commit(&label.commit)
            && valid_hash(&label.sha256),
        "Invalid source-build installation receipt"
    );
    Ok(Revision {
        tag: label.tag,
        commit: label.commit,
        sha256: label.sha256,
    })
}

pub fn selected_version(path: &Path, game: GameId) -> Result<String> {
    let revision = selected_revision(path, game)?;
    Ok(format!(
        "{} (source {})",
        revision.tag,
        &revision.commit[..12]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_build_cleanup_reclaims_only_disposable_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let previous_install = temp.path().join(".twilight-sync-backup-fixture");
        fs::create_dir(&previous_install)?;
        fs::write(previous_install.join("save"), b"previous save")?;
        let older_build = temp.path().join("older-build");
        fs::create_dir(&older_build)?;
        let staged = temp.path().join("game");
        fs::create_dir(&staged)?;
        fs::write(staged.join("triforce_programs.bin"), b"game shader cache")?;

        for (index, (game, completed, cancelled, retained)) in [
            (GameId::Dusklight, true, false, false),
            (GameId::Triforce, true, false, false),
            (GameId::Totk, false, false, false),
            (GameId::Totk, true, true, false),
            (GameId::Totk, true, false, true),
        ]
        .into_iter()
        .enumerate()
        {
            let workspace = tempfile::tempdir_in(temp.path())?;
            let path = workspace.path().to_path_buf();
            let scratch = path.join(SCRATCH);
            fs::create_dir(&scratch)?;
            let package = scratch.join("package");
            fs::write(&package, b"installer dependency")?;
            let source = path.join("source");
            fs::create_dir(&source)?;
            // uv installs packages with hardlinks. Removing the cache must not break the venv.
            let installed_package = source.join("installed-package");
            fs::hard_link(&package, &installed_package)?;
            let log = temp.path().join(format!("build-{index}.log"));
            let cancel = AtomicBool::new(cancelled);
            let mut progress = |_| Ok(());
            let runner = Runner::new(&log, &cancel, &mut progress)?;
            runner.note("Build diagnostics")?;

            let result = finish_workspace(workspace, game, completed, &runner);
            if cancelled {
                assert!(result.as_ref().is_err_and(is_cancelled), "{result:?}");
            } else {
                result?;
            }
            assert!(!scratch.try_exists()?);
            assert_eq!(path.try_exists()?, retained);
            if retained {
                assert_eq!(fs::read(installed_package)?, b"installer dependency");
            }
            assert!(fs::read_to_string(log)?.contains("Build diagnostics"));
        }

        assert_eq!(fs::read(previous_install.join("save"))?, b"previous save");
        assert_eq!(
            fs::read(staged.join("triforce_programs.bin"))?,
            b"game shader cache"
        );
        assert!(older_build.is_dir());
        Ok(())
    }

    #[test]
    #[cfg(windows)]
    fn locked_temporary_build_files_are_reported_without_discarding_the_result() -> Result<()> {
        use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};

        let temp = tempfile::tempdir()?;
        let workspace = tempfile::tempdir_in(temp.path())?;
        let path = workspace.path().to_path_buf();
        let file = path.join("locked");
        fs::write(&file, b"fixture")?;
        let locked = OpenOptions::new().read(true).share_mode(0).open(&file)?;
        let log = temp.path().join("build.log");
        let cancel = AtomicBool::new(false);
        let mut progress = |_| Ok(());
        let runner = Runner::new(&log, &cancel, &mut progress)?;

        finish_workspace(workspace, GameId::Triforce, true, &runner)?;

        drop(locked);
        assert!(file.is_file());
        let message = fs::read_to_string(log)?;
        assert!(message.contains("Could not finish cleaning up temporary build files"));
        assert!(message.contains(&path.display().to_string()));
        Ok(())
    }

    #[test]
    fn dusklight_updates_keep_the_portable_profile_and_its_original_copy() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let previous = temp.path().join("previous");
        let next = temp.path().join("next");
        fs::create_dir_all(previous.join("data"))?;
        fs::create_dir_all(&next)?;
        fs::write(previous.join("dusklight.exe"), b"fixture, never executed")?;
        fs::write(next.join("dusklight.exe"), b"fixture, never executed")?;
        fs::write(
            previous.join("data_location.json"),
            br#"{"version":1,"mode":"portable"}"#,
        )?;
        fs::write(
            previous.join("data").join("config.json"),
            br#"{"game.enableTextureReplacements":true}"#,
        )?;
        fs::write(
            previous.join("data").join("user-save"),
            b"existing portable progress",
        )?;
        preserve_dusklight(&previous.join("dusklight.exe"), &next)?;
        assert_eq!(
            save_location::profile(
                &temp.path().join("default"),
                Some(&next.join("dusklight.exe"))
            )?,
            next.join("data")
        );
        assert_eq!(
            fs::read(next.join("data").join("user-save"))?,
            b"existing portable progress"
        );
        assert_eq!(
            fs::read(previous.join("data").join("user-save"))?,
            b"existing portable progress"
        );
        assert_eq!(
            fs::read(previous.join("data").join("config.json"))?,
            fs::read(next.join("data").join("config.json"))?
        );
        Ok(())
    }

    #[test]
    fn compiler_worker_limit_accounts_for_cpu_and_available_memory() {
        let gib = 1024_u64.pow(3);
        assert_eq!(compiler_jobs(16, 10 * gib), 8);
        assert_eq!(compiler_jobs(2, 24 * gib), 2);
        assert_eq!(compiler_jobs(16, gib), 1);
        assert_eq!(compiler_jobs(32, 64 * gib), 16);
    }

    #[test]
    fn saved_worker_counts_are_migrated_to_auto() -> Result<()> {
        let preferences: Preferences =
            serde_json::from_str(r#"{"triforce":{"method":"source","jobs":4}}"#)?;
        assert_eq!(preferences.triforce.jobs, 0);
        let override_for_this_run = Preference {
            jobs: 7,
            ..preferences.triforce
        };
        assert_eq!(override_for_this_run.compiler_jobs(), 7);
        assert!(
            serde_json::to_value(override_for_this_run)?
                .get("jobs")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn source_tags_and_installed_labels_are_bound_to_the_game_and_commit() -> Result<()> {
        assert!(!valid_tag("../main"));
        let temp = tempfile::tempdir()?;
        let receipt = temp.path().join("twilight-source.json");
        fs::write(
            &receipt,
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "game": GameId::Triforce, "tag": "v0.4.0",
                "commit": "a".repeat(40), "sha256": "b".repeat(64),
            }))?,
        )?;
        assert!(selected_version(&receipt, GameId::Dusklight).is_err());
        assert!(selected_version(&receipt, GameId::Triforce)?.contains("source aaaaaaaaaaaa"));
        let mut progress = |_| -> Result<()> { bail!("No tools should run") };
        let error = build(
            temp.path(),
            Request {
                plan: Plan {
                    game: GameId::Triforce,
                    tag: "v0.4.0".into(),
                    commit: "a".repeat(40),
                    review_url: "not approved".into(),
                },
                preference: Preference::default(),
                previous_install: None,
                previous_executable: None,
                approved: false,
                log: temp.path().join("not-created.log"),
            },
            &AtomicBool::new(false),
            &mut progress,
        )
        .err()
        .context("Unapproved source execution was accepted")?;
        assert!(error.to_string().contains("accept the build warning"));
        assert!(!temp.path().join("not-created.log").try_exists()?);
        Ok(())
    }

    #[test]
    fn cancellation_and_unsupported_or_malformed_source_tags_are_explicit() -> Result<()> {
        assert!(is_cancelled(
            &check_cancelled(&AtomicBool::new(true))
                .err()
                .context("Cancellation ignored")?
        ));
        let tags = vec![
            Tag {
                name: "v0.4.0".into(),
                commit: TagCommit {
                    sha: "a".repeat(40),
                },
            },
            Tag {
                name: "v0.5.0-beta.1".into(),
                commit: TagCommit {
                    sha: "b".repeat(40),
                },
            },
        ];
        assert_eq!(triforce_tag(tags)?.0, "v0.4.0");
        assert!(
            triforce_tag(vec![Tag {
                name: "../main".into(),
                commit: TagCommit {
                    sha: "a".repeat(40)
                }
            }])
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn source_build_preserves_latest_progress_but_refuses_changes_before_activation() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let previous = temp.path().join("previous");
        let next = temp.path().join("next");
        crate::triforce::tests::seed(&previous.join("user"), 3)?;
        let at_build_start = save_hash(GameId::Triforce, &previous, None)?;
        fs::create_dir_all(&next)?;
        let executable = next.join("Triforce.exe");
        fs::write(&executable, b"synthetic file, never executed")?;
        fs::write(previous.join("triforce_options.ini"), b"local graphics")?;
        let environment = b"RX_BACKEND=webgpu\r\nRX_VK_NOOPT=1\r\n";
        fs::write(previous.join("triforce_env.txt"), environment)?;
        let request = Request {
            plan: Plan {
                game: GameId::Triforce,
                tag: "v0.4.0".into(),
                commit: "a".repeat(40),
                review_url: String::new(),
            },
            preference: Preference::default(),
            previous_install: Some(previous.clone()),
            previous_executable: None,
            approved: true,
            log: temp.path().join("log.txt"),
        };
        crate::triforce::tests::seed(&previous.join("user"), 4)?;
        let expected = preserve_current_data(&request, &previous, &next, &executable)?;
        assert_ne!(expected, at_build_start);
        assert_eq!(expected, save_hash(GameId::Triforce, &previous, None)?);
        assert_eq!(
            crate::triforce::snapshot(&next.join("user"))?.map(|save| save.hash),
            expected
        );
        assert_eq!(
            fs::read(next.join("triforce_options.ini"))?,
            b"local graphics"
        );
        assert_eq!(fs::read(previous.join("triforce_env.txt"))?, environment);
        let copied = fs::read_to_string(next.join("triforce_env.txt"))?;
        assert!(copied.contains("RX_VK_NOOPT=1") && copied.ends_with("RX_BACKEND=vulkan\n"));
        let built = Built {
            game: GameId::Triforce,
            executable,
            directory: next.clone(),
            tag: "v0.4.0".into(),
            commit: "a".repeat(40),
            sha256: files::sha256(b"synthetic file, never executed"),
            source: None,
            log: request.log,
            expected_save: expected.clone(),
        };
        verify_activation(&built, Some(&previous), None)?;
        crate::triforce::tests::seed(&previous.join("user"), 7)?;
        assert!(verify_activation(&built, Some(&previous), None).is_err());
        assert_eq!(
            crate::triforce::snapshot(&next.join("user"))?.map(|save| save.hash),
            expected
        );
        Ok(())
    }

    #[test]
    #[ignore = "Reads public source metadata only; executes no upstream code or build tools"]
    fn live_source_metadata_identifies_pinned_commits_for_all_three_games() -> Result<()> {
        for game in [GameId::Dusklight, GameId::Totk, GameId::Triforce] {
            let (plan, _) = check(game)?;
            ensure!(
                valid_commit(&plan.commit) && valid_tag(&plan.tag),
                "Missing stable source identity"
            );
            println!("{game:?}: {} at {}", plan.tag, plan.commit);
        }
        Ok(())
    }
}

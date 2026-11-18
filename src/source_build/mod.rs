mod recipes;
mod runner;
mod setup;
mod tools;
pub use setup::SdkSetup;

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
            jobs: 4,
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
        (1..=16).contains(&preference.jobs),
        "Source builds need 1 to 16 compiler workers"
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
    pub sdk_setup: bool,
    pub game: Option<GameId>,
    pub plan: Option<Plan>,
    pub prerequisites: Vec<Prerequisite>,
    pub message: String,
    pub error: Option<String>,
    pub log: Option<PathBuf>,
    pub built: Option<Built>,
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

fn valid_commit(commit: &str) -> bool {
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
    progress: &mut dyn FnMut(String) -> Result<()>,
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
    pub source: PathBuf,
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
    source: &'a Path,
    tools: &'a [tools::Provenance],
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

fn preserve(request: &Request, previous: &Path, output: &Path) -> Result<()> {
    let game = request.plan.game;
    if game == GameId::Dusklight {
        if let Some(executable) = &request.previous_executable {
            let directory = executable
                .parent()
                .context("Previous Dusklight executable has no directory")?;
            for relative in ["data_location.json", "data"] {
                let source = directory.join(relative);
                ensure_safe_path(&source)?;
                if source.try_exists()? {
                    copy_tree(&source, &output.join(relative))?;
                }
            }
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
    Ok(())
}

fn games_stopped() -> Result<()> {
    ensure!(
        !crate::play::ProcessMonitor::default().game_running(),
        "Close Dusklight before building"
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

pub fn build(
    root: &Path,
    request: Request,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String) -> Result<()>,
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
    let before = save_hash(game, &previous, request.previous_executable.as_deref())?;
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
        .tempdir_in(&runs)?
        .keep();
    let source = workspace.join("source");
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
    let result: Result<(PathBuf, String)> = (|| {
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
        recipes::build(
            &request,
            &workspace,
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
        ensure!(
            save_hash(game, &previous, request.previous_executable.as_deref())? == before,
            "Progress changed while building; the previous installation was not replaced or selected"
        );
        runner.message(if game == GameId::Triforce {
            "Preserving saves, settings and mods; selecting the locally built Vulkan renderer in the new folder (the previous folder is unchanged)..."
        } else {
            "Preserving saves, local settings and mods in the staged build..."
        })?;
        preserve(&request, &previous, &output)?;
        ensure!(
            save_hash(game, &output, Some(&executable))? == before,
            "The staged build does not preserve the current save; activation was refused"
        );
        check_cancelled(cancel)?;
        let sha256 = install::digest_file(&executable, executable.metadata()?.len())?;
        files::atomic_write(
            &output_stage.path().join("twilight-source.json"),
            &serde_json::to_vec_pretty(&Receipt {
                version: 1,
                game,
                tag: &request.plan.tag,
                commit: &request.plan.commit,
                sha256: &sha256,
                source: &source,
                tools: &tools.provenance,
            })?,
        )?;
        Ok((executable, sha256))
    })();
    let (executable, sha256) = result.with_context(|| format!("Source build did not complete. The previous installation remains selected. Log: {}. Retained workspace: {}", request.log.display(), workspace.display()))?;
    let _installed_directory = output_stage.keep();
    Ok(Built {
        game,
        executable,
        directory: output,
        tag: request.plan.tag,
        commit: request.plan.commit,
        sha256,
        source,
        log: request.log,
        expected_save: before,
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

pub fn selected_version(path: &Path, game: GameId) -> Result<String> {
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
    Ok(format!("{} (source {})", label.tag, &label.commit[..12]))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn source_build_preservation_and_activation_never_replace_changed_progress() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let previous = temp.path().join("previous");
        let next = temp.path().join("next");
        crate::triforce::tests::seed(&previous.join("user"), 3)?;
        fs::create_dir_all(&next)?;
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
        preserve(&request, &previous, &next)?;
        let expected = crate::triforce::snapshot(&previous.join("user"))?.map(|save| save.hash);
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
        let executable = next.join("Triforce.exe");
        fs::write(&executable, b"synthetic file, never executed")?;
        let built = Built {
            game: GameId::Triforce,
            executable,
            directory: next.clone(),
            tag: "v0.4.0".into(),
            commit: "a".repeat(40),
            sha256: files::sha256(b"synthetic file, never executed"),
            source: temp.path().join("source"),
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

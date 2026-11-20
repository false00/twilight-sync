use crate::{
    files::{ensure_safe_path, valid_hash},
    protocol::GameId,
};
use anyhow::{Context, Result, ensure};
use reqwest::{blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;
const MAX_TOTK_DOWNLOAD: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED: u64 = 512 * 1024 * 1024;
const OFFICIAL_REPOSITORY: &str = "https://github.com/TwilitRealm/dusklight/releases/download/";
const TOTK_REPOSITORY: &str = "https://github.com/valentderah/totk-pc-recomp/releases/download/";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

pub struct Installed {
    pub executable: PathBuf,
    pub tag: String,
    pub sha256: String,
    pub reused: bool,
}

#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    pub tag: String,
    pub size: u64,
    pub sha256: String,
    pub asset: String,
    pub(crate) url: String,
}

#[derive(Clone, Debug)]
pub struct Version {
    pub tag: String,
    pub commit: Option<String>,
}

impl Version {
    pub fn label(&self) -> String {
        self.commit.as_ref().map_or_else(
            || format!("{} (official)", self.tag),
            |commit| format!("{} (source {})", self.tag, &commit[..12]),
        )
    }

    pub fn comparison(&self, tag: &str, commit: Option<&str>) -> &'static str {
        if self
            .commit
            .as_deref()
            .is_some_and(|old| Some(old) == commit)
        {
            return "This source revision is already installed.";
        }
        let parse = |value: &str| semver::Version::parse(value.strip_prefix('v').unwrap_or(value));
        match (parse(&self.tag), parse(tag)) {
            (Ok(old), Ok(new)) if new.cmp_precedence(&old).is_gt() => "Newer release available.",
            (Ok(old), Ok(new)) if new.cmp_precedence(&old).is_lt() => {
                "This candidate is an older release (downgrade)."
            }
            (Ok(_), Ok(_)) if self.commit.is_some() != commit.is_some() => {
                "Same release version, different build method."
            }
            _ if self.tag == tag && self.commit.is_none() && commit.is_none() => {
                "This official release is already installed."
            }
            _ if self.tag == tag && self.commit.is_some() && commit.is_some() => {
                "Same release tag, different source revision; review the diff."
            }
            (Ok(_), Ok(_)) => "Same release version, different build identity.",
            _ => "Different build; version ordering is not established.",
        }
    }

    pub fn compare_url(&self, game: GameId, commit: &str) -> Option<String> {
        self.compare_project_url(game, crate::source_build::OotProject::Triforce, commit)
    }

    pub fn compare_project_url(
        &self,
        game: GameId,
        project: crate::source_build::OotProject,
        commit: &str,
    ) -> Option<String> {
        let previous = self.commit.as_ref()?;
        (previous != commit
            && crate::source_build::valid_commit(previous)
            && crate::source_build::valid_commit(commit))
        .then(|| {
            format!(
                "{}/compare/{}...{}",
                crate::source_build::project_source_url(game, project),
                previous,
                commit
            )
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct VersionState {
    pub installed: Option<Version>,
    pub error: Option<String>,
}

#[derive(Default)]
pub(crate) struct VersionCache {
    stamp: Option<VersionStamp>,
    pub state: VersionState,
}

type VersionStamp = Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)>;

impl VersionCache {
    pub fn refresh(&mut self, game: GameId, directory: Option<&Path>) {
        let result = (|| -> Result<()> {
            let Some(directory) = directory else {
                self.stamp = None;
                self.state = VersionState::default();
                return Ok(());
            };
            let mut candidates = vec![directory.join("twilight-source.json")];
            if directory.file_name().is_some_and(|name| name == "game")
                && let Some(parent) = directory.parent()
            {
                candidates.push(parent.join("twilight-source.json"));
            }
            candidates.push(directory.join(if game == GameId::Totk {
                "totk-install.json"
            } else {
                "twilight-install.json"
            }));
            let stamp = candidates
                .iter()
                .map(|path| -> Result<_> {
                    ensure_safe_path(path)?;
                    let metadata = match fs::metadata(path) {
                        Ok(metadata) => Some((metadata.modified()?, metadata.len())),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    };
                    Ok((path.clone(), metadata))
                })
                .collect::<Result<Vec<_>>>()?;
            if self.stamp.as_ref() == Some(&stamp) {
                return Ok(());
            }
            let found = stamp.iter().find(|(_, metadata)| metadata.is_some());
            let installed = if let Some((path, Some((_, size)))) = found {
                ensure!(
                    *size <= 1024 * 1024,
                    "Installed version receipt exceeds its limit"
                );
                if path
                    .file_name()
                    .is_some_and(|name| name == "twilight-source.json")
                {
                    let revision = crate::source_build::selected_revision(path, game)?;
                    Some(Version {
                        tag: revision.tag,
                        commit: Some(revision.commit),
                    })
                } else if game == GameId::Totk {
                    Some(Version {
                        tag: crate::totk_update::installed_release(directory)?,
                        commit: None,
                    })
                } else if game == GameId::Dusklight {
                    let receipt: Receipt = serde_json::from_slice(&fs::read(path)?)?;
                    ensure!(
                        receipt.version == 1 && valid_hash(&receipt.archive_sha256),
                        "Invalid installed release receipt"
                    );
                    ensure!(
                        crate::source_build::valid_tag(&receipt.tag),
                        "Invalid installed release tag"
                    );
                    Some(Version {
                        tag: receipt.tag,
                        commit: None,
                    })
                } else {
                    None
                }
            } else {
                None
            };
            self.stamp = Some(stamp);
            self.state = VersionState {
                installed,
                error: None,
            };
            Ok(())
        })();
        if let Err(error) = result {
            self.stamp = None;
            self.state = VersionState {
                installed: None,
                error: Some(format!("Installed version unavailable: {error:#}")),
            };
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstalledFile {
    path: PathBuf,
    size: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u8,
    tag: String,
    archive_sha256: String,
    executable: PathBuf,
    files: Vec<InstalledFile>,
}

pub fn default_directory(game: GameId) -> Result<PathBuf> {
    let home = directories::UserDirs::new().context("Cannot find the user Games directory")?;
    let directory = home.home_dir().join("Games").join(match game {
        GameId::Dusklight => "Dusklight",
        GameId::Totk => "TotKRecomp",
        GameId::Triforce => "Triforce",
    });
    ensure_safe_path(&directory)?;
    Ok(directory)
}

fn allowed_download_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

pub(crate) fn official_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!(
            "twilight-sync/",
            env!("CARGO_PKG_VERSION"),
            " (GitHub release updater)"
        ))
        .https_only(true)
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("Too many download redirects")
            } else if allowed_download_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("Release download redirected outside the GitHub allowlist")
            }
        }))
        .build()?)
}

pub fn latest_release() -> Result<ReleaseInfo> {
    release_with(&official_client()?, GameId::Dusklight)
}

pub fn latest_totk_release() -> Result<ReleaseInfo> {
    release_with(&official_client()?, GameId::Totk)
}

fn release_with(client: &Client, game: GameId) -> Result<ReleaseInfo> {
    let url = match game {
        GameId::Dusklight => "https://api.github.com/repos/TwilitRealm/dusklight/releases/latest",
        GameId::Totk => "https://api.github.com/repos/valentderah/totk-pc-recomp/releases/latest",
        GameId::Triforce => {
            anyhow::bail!("Triforce is built from source; use its source-build controls")
        }
    };
    let metadata = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()?
        .error_for_status()?;
    ensure!(
        metadata.url().as_str() == url,
        "Unexpected release metadata redirect"
    );
    let mut bytes = Vec::new();
    metadata.take(128 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 128 * 1024, "Release metadata exceeds limit");
    let release: Release = serde_json::from_slice(&bytes)?;
    validate_release(release, game)
}

fn validate_release(release: Release, game: GameId) -> Result<ReleaseInfo> {
    ensure!(
        !release.tag_name.is_empty()
            && release.tag_name.len() <= 48
            && release
                .tag_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)),
        "Unsafe release tag"
    );
    let (expected_name, repository, max_download) = match game {
        GameId::Dusklight => {
            let arch = match std::env::consts::ARCH {
                "x86_64" => "x86_64",
                "aarch64" => "arm64",
                _ => anyhow::bail!(
                    "This CPU architecture is not supported by the official Windows installer"
                ),
            };
            (
                format!("Dusklight-{}-win32-{arch}.zip", release.tag_name),
                OFFICIAL_REPOSITORY,
                MAX_DOWNLOAD,
            )
        }
        GameId::Totk => {
            ensure!(
                cfg!(target_arch = "x86_64"),
                "The official TotK Recomp Windows installer requires an x86-64 CPU"
            );
            let version = release
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&release.tag_name);
            semver::Version::parse(version).context("Invalid TotK release version")?;
            (
                format!("TotK-Setup-{version}-windows.zip"),
                TOTK_REPOSITORY,
                MAX_TOTK_DOWNLOAD,
            )
        }
        GameId::Triforce => anyhow::bail!("Triforce does not use a prebuilt game release"),
    };
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == expected_name)
        .context("Official Windows release asset was not found")?;
    ensure!(
        asset.size > 0 && asset.size <= max_download,
        "Release archive exceeds download limit"
    );
    let expected_url = format!("{repository}{}/{}", release.tag_name, asset.name);
    ensure!(
        asset.browser_download_url == expected_url,
        "Asset URL is not in the official repository"
    );
    let digest = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .context("Release has no published SHA-256 digest")?;
    ensure!(valid_hash(digest), "Invalid release SHA-256");
    Ok(ReleaseInfo {
        tag: release.tag_name,
        size: asset.size,
        sha256: digest.to_ascii_lowercase(),
        asset: asset.name.clone(),
        url: asset.browser_download_url.clone(),
    })
}

pub(crate) fn digest_file(path: &Path, expected: u64) -> Result<String> {
    ensure_safe_path(path)?;
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() == expected,
        "Installed program file changed size"
    );
    let mut sha = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= expected, "Installed file grew while verifying");
        sha.update(&buffer[..count]);
    }
    ensure!(total == expected, "Installed file was truncated");
    Ok(hex::encode(sha.finalize()))
}

fn installation_files(root: &Path) -> Result<Vec<InstalledFile>> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            ensure_safe_path(&path)?;
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else {
                ensure!(
                    entry.file_type()?.is_file() && files.len() < 513,
                    "Invalid program installation contents"
                );
                let size = entry.metadata()?.len();
                ensure!(
                    size <= MAX_DOWNLOAD,
                    "Installed program file exceeds its limit"
                );
                files.push(InstalledFile {
                    path: path.strip_prefix(root)?.into(),
                    size,
                    sha256: digest_file(&path, size)?,
                });
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn verify_receipt(root: &Path, release: &ReleaseInfo) -> Result<PathBuf> {
    let path = root.join("twilight-install.json");
    ensure_safe_path(&path)?;
    ensure!(
        path.metadata()?.len() <= 256 * 1024,
        "Installation receipt exceeds limit"
    );
    let receipt: Receipt = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        receipt.version == 1
            && receipt.tag == release.tag
            && receipt.archive_sha256 == release.sha256,
        "Installation receipt differs from the official release"
    );
    ensure!(
        !receipt.files.is_empty() && receipt.files.len() <= 512,
        "Invalid installed file manifest"
    );
    let mut executable_found = false;
    for file in &receipt.files {
        ensure!(
            !file.path.as_os_str().is_empty()
                && file
                    .path
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
                && file.size <= MAX_DOWNLOAD
                && valid_hash(&file.sha256),
            "Invalid installed file receipt"
        );
        executable_found |= file.path == receipt.executable;
    }
    let mut actual = installation_files(root)?;
    actual.retain(|file| file.path != Path::new("twilight-install.json"));
    ensure!(
        actual.len() == receipt.files.len()
            && actual
                .iter()
                .zip(&receipt.files)
                .all(|(a, b)| a.path == b.path && a.sha256 == b.sha256 && a.size == b.size),
        "Program directory contains untracked or changed files; it was not overwritten"
    );
    ensure!(
        executable_found
            && receipt
                .executable
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("dusklight.exe")),
        "Receipt has no valid Dusklight executable"
    );
    Ok(root.join(receipt.executable))
}

pub fn selected_release(
    root: &Path,
    install_directory: &Path,
    executable: Option<&Path>,
) -> Result<Option<String>> {
    let Some(executable) = executable else {
        return Ok(None);
    };
    let legacy = root.join("apps");
    if !executable.starts_with(install_directory) && !executable.starts_with(legacy) {
        return Ok(None);
    }
    let mut cache = VersionCache::default();
    cache.refresh(GameId::Dusklight, executable.parent());
    if let Some(error) = cache.state.error {
        anyhow::bail!(error);
    }
    Ok(cache.state.installed.map(|version| {
        if version.commit.is_some() {
            version.label()
        } else {
            version.tag
        }
    }))
}

pub fn install_official(directory: &Path) -> Result<Installed> {
    let client = official_client()?;
    let release = release_with(&client, GameId::Dusklight)?;
    ensure_safe_path(directory)?;
    fs::create_dir_all(directory)?;
    let destination = directory.join(format!("{}-{}", release.tag, &release.sha256[..12]));
    ensure_safe_path(&destination)?;
    if destination.try_exists()? {
        let executable = verify_receipt(&destination, &release)?;
        return Ok(Installed {
            executable,
            tag: release.tag,
            sha256: release.sha256,
            reused: true,
        });
    }
    let staging = tempfile::Builder::new()
        .prefix("install-")
        .tempdir_in(directory)?;
    let archive_path = staging.path().join("release.zip");
    download_archive(&client, &release, &archive_path)?;
    let unpack = staging.path().join("unpacked");
    fs::create_dir(&unpack)?;
    let relative_executable = extract_archive(&archive_path, &unpack)?;
    ensure!(
        !unpack.join("twilight-install.json").try_exists()?,
        "Release archive uses the updater's reserved receipt name"
    );
    let receipt = Receipt {
        version: 1,
        tag: release.tag.clone(),
        archive_sha256: release.sha256.clone(),
        executable: relative_executable.clone(),
        files: installation_files(&unpack)?,
    };
    crate::files::atomic_write(
        &unpack.join("twilight-install.json"),
        &serde_json::to_vec(&receipt)?,
    )?;
    ensure_safe_path(&destination)?;
    fs::rename(&unpack, &destination).context("Committing verified Dusklight installation")?;
    Ok(Installed {
        executable: destination.join(relative_executable),
        tag: release.tag,
        sha256: release.sha256,
        reused: false,
    })
}

pub(crate) fn download_archive(client: &Client, release: &ReleaseInfo, path: &Path) -> Result<()> {
    download_archive_cancel(client, release, path, &AtomicBool::new(false))
}

pub(crate) fn download_archive_cancel(
    client: &Client,
    release: &ReleaseInfo,
    path: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "Release download cancelled"
    );
    ensure_safe_path(path)?;
    let mut archive_file = File::create_new(path)?;
    let mut response = client.get(&release.url).send()?.error_for_status()?;
    ensure!(
        allowed_download_url(response.url()),
        "Untrusted download host"
    );
    let mut sha = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut total = 0_u64;
    loop {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "Release download cancelled"
        );
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(
            total <= release.size,
            "Release download exceeds declared size"
        );
        archive_file.write_all(&buffer[..count])?;
        sha.update(&buffer[..count]);
    }
    archive_file.sync_all()?;
    ensure!(total == release.size, "Release download was truncated");
    let hash = hex::encode(sha.finalize());
    ensure!(hash == release.sha256, "Release archive SHA-256 mismatch");
    Ok(())
}

fn extract_archive(archive: &Path, destination: &Path) -> Result<PathBuf> {
    extract_package(archive, destination, "dusklight.exe", 512, MAX_EXPANDED)
}

pub(crate) fn extract_package(
    archive: &Path,
    destination: &Path,
    executable_name: &str,
    max_entries: usize,
    max_expanded: u64,
) -> Result<PathBuf> {
    let files = extract_files(archive, destination, max_entries, max_expanded)?;
    let mut executables = files.into_iter().filter(|relative| {
        relative
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(executable_name))
    });
    let executable = executables
        .next()
        .with_context(|| format!("Official release did not contain {executable_name}"))?;
    ensure!(
        executables.next().is_none(),
        "Release contains multiple {executable_name} executables"
    );
    Ok(executable)
}

pub(crate) fn extract_files(
    archive: &Path,
    destination: &Path,
    max_entries: usize,
    max_expanded: u64,
) -> Result<Vec<PathBuf>> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
    ensure!(
        zip.len() <= max_entries,
        "Release archive contains too many entries"
    );
    let mut total = 0_u64;
    let mut files = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        ensure!(entry.name().len() <= 180, "Archive path is too long");
        let relative = entry.enclosed_name().context("Unsafe archive path")?;
        ensure!(!relative.as_os_str().is_empty(), "Empty archive path");
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0xf000;
            ensure!(
                kind == 0 || kind == 0x8000 || kind == 0x4000,
                "Archive links and special files are forbidden"
            );
        }
        total = total
            .checked_add(entry.size())
            .context("Archive size overflow")?;
        ensure!(
            total <= max_expanded && entry.size() <= MAX_DOWNLOAD,
            "Archive expansion limit exceeded"
        );
        let path = destination.join(&relative);
        ensure_safe_path(&path)?;
        if entry.is_dir() {
            fs::create_dir_all(&path)?;
        } else {
            let parent = path.parent().context("Archive entry has no parent")?;
            fs::create_dir_all(parent)?;
            let mut output = File::options().write(true).create_new(true).open(&path)?;
            let expected = entry.size();
            let copied = std::io::copy(&mut entry.by_ref().take(expected + 1), &mut output)?;
            ensure!(copied == expected, "Archive entry length mismatch");
            output.sync_all()?;
            files.push(relative);
        }
    }
    Ok(files)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn installed_revisions_report_upgrades_downgrades_and_safe_source_diffs() {
        let version = Version {
            tag: "v1.2.0".into(),
            commit: Some("a".repeat(40)),
        };
        assert_eq!(
            version.comparison("v1.3.0", Some(&"b".repeat(40))),
            "Newer release available."
        );
        assert!(version.comparison("v1.1.0", None).contains("downgrade"));
        assert!(
            version
                .comparison("v1.2.0", Some(&"a".repeat(40)))
                .contains("already installed")
        );
        assert!(
            version
                .comparison("v1.2.0", Some(&"b".repeat(40)))
                .contains("different source revision")
        );
        assert!(
            version
                .comparison("nightly", None)
                .contains("not established")
        );
        assert!(
            version
                .comparison("v1.2.0", None)
                .contains("different build method")
        );
        assert!(
            version
                .comparison("v1.2.0+build2", None)
                .contains("Same release version")
        );
        for game in [GameId::Dusklight, GameId::Totk, GameId::Triforce] {
            assert_eq!(
                version.compare_url(game, &"b".repeat(40)),
                Some(format!(
                    "{}/compare/{}...{}",
                    crate::source_build::source_url(game),
                    "a".repeat(40),
                    "b".repeat(40)
                ))
            );
        }
        assert!(
            version
                .compare_url(GameId::Triforce, &"a".repeat(40))
                .is_none()
        );
        assert!(
            version
                .compare_url(GameId::Triforce, "invalid?url")
                .is_none()
        );
        assert!(
            Version {
                tag: "v1.2.0".into(),
                commit: None
            }
            .compare_url(GameId::Totk, &"b".repeat(40))
            .is_none()
        );
    }

    #[test]
    fn version_cache_handles_legacy_and_canonical_receipts_and_surfaces_corruption() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let stage = temp.path().join("source-v1");
        let game = stage.join("game");
        fs::create_dir_all(&game)?;
        let receipt = serde_json::to_vec(&serde_json::json!({
            "version":1,"game":GameId::Triforce,"tag":"v1.0.0",
            "commit":"a".repeat(40),"sha256":"b".repeat(64)
        }))?;
        let mut cache = VersionCache::default();
        cache.refresh(GameId::Triforce, Some(&game));
        assert!(cache.state.installed.is_none() && cache.state.error.is_none());
        fs::write(stage.join("twilight-source.json"), &receipt)?;
        cache.refresh(GameId::Triforce, Some(&game));
        assert_eq!(
            cache
                .state
                .installed
                .as_ref()
                .map(|version| version.tag.as_str()),
            Some("v1.0.0")
        );
        fs::write(game.join("twilight-source.json"), &receipt)?;
        fs::remove_file(stage.join("twilight-source.json"))?;
        cache.refresh(GameId::Triforce, Some(&game));
        assert!(cache.state.error.is_none());
        fs::write(game.join("twilight-source.json"), b"broken receipt")?;
        cache.refresh(GameId::Triforce, Some(&game));
        assert!(cache.state.installed.is_none() && cache.state.error.is_some());
        fs::write(game.join("twilight-source.json"), &receipt)?;
        cache.refresh(GameId::Triforce, Some(&game));
        assert!(cache.state.installed.is_some() && cache.state.error.is_none());
        Ok(())
    }

    fn release_fixture() -> Release {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        };
        let name = format!("Dusklight-v9.9.9-win32-{arch}.zip");
        Release {
            tag_name: "v9.9.9".into(),
            assets: vec![Asset {
                browser_download_url: format!("{OFFICIAL_REPOSITORY}v9.9.9/{name}"),
                name,
                size: 100,
                digest: Some(format!("sha256:{}", "a".repeat(64))),
            }],
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn totk_metadata() -> Release {
        let name = "TotK-Setup-0.4.1-windows.zip";
        Release {
            tag_name: "v0.4.1".into(),
            assets: vec![Asset {
                name: name.into(),
                browser_download_url: format!("{TOTK_REPOSITORY}v0.4.1/{name}"),
                size: 300 * 1024 * 1024,
                digest: Some(format!("sha256:{}", "b".repeat(64))),
            }],
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn totk_release_fixture() -> Result<ReleaseInfo> {
        validate_release(totk_metadata(), GameId::Totk)
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn totk_releases_require_the_exact_windows_setup_and_its_published_digest() -> Result<()> {
        let release = totk_release_fixture()?;
        assert_eq!(release.asset, "TotK-Setup-0.4.1-windows.zip");
        assert_eq!(release.tag, "v0.4.1");
        let mut bad = totk_metadata();
        bad.assets[0].browser_download_url =
            format!("{OFFICIAL_REPOSITORY}v0.4.1/TotK-Setup-0.4.1-windows.zip");
        assert!(validate_release(bad, GameId::Totk).is_err());
        let mut bad = totk_metadata();
        bad.assets[0].digest = None;
        assert!(validate_release(bad, GameId::Totk).is_err());
        let mut bad = totk_metadata();
        bad.assets[0].name = "TotK-Setup-0.4.1-x86_64.AppImage".into();
        assert!(validate_release(bad, GameId::Totk).is_err());
        let mut bad = totk_metadata();
        bad.assets[0].size = MAX_TOTK_DOWNLOAD + 1;
        assert!(validate_release(bad, GameId::Totk).is_err());
        Ok(())
    }

    #[test]
    fn latest_release_requires_the_official_asset_url_and_published_digest() -> Result<()> {
        let plan = validate_release(release_fixture(), GameId::Dusklight)?;
        assert_eq!(plan.tag, "v9.9.9");
        assert_eq!(plan.sha256, "a".repeat(64));
        let mut bad = release_fixture();
        bad.assets[0].browser_download_url =
            "https://github.com/other/project/releases/download/bad.zip".into();
        assert!(validate_release(bad, GameId::Dusklight).is_err());
        let mut bad = release_fixture();
        bad.assets[0].digest = None;
        assert!(validate_release(bad, GameId::Dusklight).is_err());
        Ok(())
    }

    #[test]
    fn existing_managed_installs_are_reverified_not_silently_overwritten() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let executable = temp.path().join("dusklight.exe");
        crate::files::atomic_write(&executable, b"synthetic test file, never executed")?;
        let plan = validate_release(release_fixture(), GameId::Dusklight)?;
        let receipt = Receipt {
            version: 1,
            tag: plan.tag.clone(),
            archive_sha256: plan.sha256.clone(),
            executable: PathBuf::from("dusklight.exe"),
            files: installation_files(temp.path())?,
        };
        let receipt_path = temp.path().join("twilight-install.json");
        crate::files::atomic_write(&receipt_path, &serde_json::to_vec(&receipt)?)?;
        assert_eq!(verify_receipt(temp.path(), &plan)?, executable);
        crate::files::atomic_write(&temp.path().join("unexpected.dll"), b"not executable")?;
        assert!(verify_receipt(temp.path(), &plan).is_err());
        fs::remove_file(temp.path().join("unexpected.dll"))?;
        crate::files::atomic_write(&executable, b"changed")?;
        assert!(verify_receipt(temp.path(), &plan).is_err());
        assert_eq!(fs::read(executable)?, b"changed");
        Ok(())
    }

    #[test]
    fn receipt_paths_cannot_escape_the_managed_install() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let plan = validate_release(release_fixture(), GameId::Dusklight)?;
        for path in [
            PathBuf::from("..").join("dusklight.exe"),
            PathBuf::from(r"\Windows\dusklight.exe"),
        ] {
            let receipt = Receipt {
                version: 1,
                tag: plan.tag.clone(),
                archive_sha256: plan.sha256.clone(),
                executable: path.clone(),
                files: vec![InstalledFile {
                    path,
                    size: 1,
                    sha256: "b".repeat(64),
                }],
            };
            crate::files::atomic_write(
                &temp.path().join("twilight-install.json"),
                &serde_json::to_vec(&receipt)?,
            )?;
            assert!(verify_receipt(temp.path(), &plan).is_err());
        }
        Ok(())
    }

    #[test]
    fn selected_releases_recognize_games_and_legacy_directories_without_moving_files() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("manager");
        let games = temp.path().join("Games").join("Dusklight");
        for (directory, tag) in [
            (root.join("apps").join("v1.0.0-legacy"), "v1.0.0"),
            (games.join("v2.0.0-current"), "v2.0.0"),
        ] {
            fs::create_dir_all(&directory)?;
            let executable = directory.join("dusklight.exe");
            fs::write(&executable, b"never executed or moved")?;
            let receipt = Receipt {
                version: 1,
                tag: tag.into(),
                archive_sha256: "a".repeat(64),
                executable: PathBuf::from("dusklight.exe"),
                files: installation_files(&directory)?,
            };
            crate::files::atomic_write(
                &directory.join("twilight-install.json"),
                &serde_json::to_vec(&receipt)?,
            )?;
            assert_eq!(
                selected_release(&root, &games, Some(&executable))?.as_deref(),
                Some(tag)
            );
            assert_eq!(fs::read(&executable)?, b"never executed or moved");
        }
        assert!(
            selected_release(
                &root,
                &games,
                Some(&temp.path().join("custom").join("dusklight.exe"))
            )?
            .is_none()
        );
        Ok(())
    }

    #[test]
    #[ignore = "Downloads the official public release into an isolated temporary directory"]
    fn official_release_installs_and_reuses_in_an_isolated_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("manager");
        let directory = temp.path().join("Games").join("Dusklight");
        let first = install_official(&directory)?;
        ensure!(
            first.executable.is_file()
                && first.executable.starts_with(&directory)
                && !first.reused
                && !root.join("apps").try_exists()?,
            "Official release was not installed"
        );
        ensure!(
            selected_release(&root, &directory, Some(&first.executable))?.as_deref()
                == Some(first.tag.as_str()),
            "Installed version receipt differs"
        );
        let second = install_official(&directory)?;
        ensure!(
            second.reused && second.executable == first.executable && second.sha256 == first.sha256,
            "Existing official build was not verified and reused"
        );
        println!(
            "Official {} installed and reverified in a temporary directory; no program was launched.",
            first.tag
        );
        Ok(())
    }

    #[test]
    fn rejects_untrusted_release_hosts() -> Result<()> {
        for url in [
            "http://github.com/a",
            "https://github.com.evil.test/a",
            "https://evil.test/a",
            "https://user@github.com/a",
        ] {
            assert!(!allowed_download_url(&url.parse()?));
        }
        assert!(allowed_download_url(
            &"https://release-assets.githubusercontent.com/a".parse()?
        ));
        Ok(())
    }

    #[test]
    fn refuses_archive_traversal() -> Result<()> {
        use zip::{ZipWriter, write::SimpleFileOptions};
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("bad.zip");
        let mut writer = ZipWriter::new(File::create(&archive)?);
        writer.start_file("../dusklight.exe", SimpleFileOptions::default())?;
        writer.write_all(b"not an executable")?;
        writer.finish()?;
        let destination = temp.path().join("unpacked");
        fs::create_dir(&destination)?;
        assert!(extract_archive(&archive, &destination).is_err());
        assert!(!temp.path().join("dusklight.exe").try_exists()?);
        Ok(())
    }

    #[test]
    fn rejects_archive_links_and_excess_entry_count() -> Result<()> {
        use zip::{ZipWriter, write::SimpleFileOptions};
        let temp = tempfile::tempdir()?;
        let destination = temp.path().join("unpacked");
        fs::create_dir(&destination)?;
        let archive = temp.path().join("link.zip");
        let mut writer = ZipWriter::new(File::create(&archive)?);
        writer.add_symlink(
            "dusklight.exe",
            "../outside.exe",
            SimpleFileOptions::default(),
        )?;
        writer.finish()?;
        let error = extract_archive(&archive, &destination)
            .err()
            .context("Archive link was accepted")?;
        assert!(
            error.to_string().contains("links and special files"),
            "{error:#}"
        );
        let archive = temp.path().join("many.zip");
        let mut writer = ZipWriter::new(File::create(&archive)?);
        for index in 0..513 {
            writer.start_file(format!("file-{index}.txt"), SimpleFileOptions::default())?;
        }
        writer.finish()?;
        let error = extract_archive(&archive, &destination)
            .err()
            .context("Excess entries were accepted")?;
        assert!(error.to_string().contains("too many entries"), "{error:#}");
        Ok(())
    }
}

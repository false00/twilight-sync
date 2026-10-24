use crate::files::{ensure_safe_path, valid_hash};
use anyhow::{Context, Result, ensure};
use reqwest::{blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;
const MAX_EXPANDED: u64 = 512 * 1024 * 1024;
const OFFICIAL_REPOSITORY: &str = "https://github.com/TwilitRealm/dusklight/releases/download/";

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
    url: String,
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
    release_with(&official_client()?)
}

fn release_with(client: &Client) -> Result<ReleaseInfo> {
    let metadata = client
        .get("https://api.github.com/repos/TwilitRealm/dusklight/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .send()?
        .error_for_status()?;
    ensure!(
        metadata.url().as_str()
            == "https://api.github.com/repos/TwilitRealm/dusklight/releases/latest",
        "Unexpected release metadata redirect"
    );
    let mut bytes = Vec::new();
    metadata.take(128 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 128 * 1024, "Release metadata exceeds limit");
    let release: Release = serde_json::from_slice(&bytes)?;
    validate_release(release)
}

fn validate_release(release: Release) -> Result<ReleaseInfo> {
    ensure!(
        !release.tag_name.is_empty()
            && release.tag_name.len() <= 48
            && release
                .tag_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)),
        "Unsafe release tag"
    );
    #[cfg(target_arch = "x86_64")]
    let suffix = "-win32-x86_64.zip";
    #[cfg(target_arch = "aarch64")]
    let suffix = "-win32-arm64.zip";
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    anyhow::bail!("This CPU architecture is not supported by the official Windows installer");
    let expected_name = format!("Dusklight-{}{}", release.tag_name, suffix);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == expected_name)
        .context("Official Windows release asset was not found")?;
    ensure!(
        asset.size > 0 && asset.size <= MAX_DOWNLOAD,
        "Release archive exceeds download limit"
    );
    let expected_url = format!("{OFFICIAL_REPOSITORY}{}/{}", release.tag_name, asset.name);
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

fn digest_file(path: &Path, expected: u64) -> Result<String> {
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

pub fn selected_release(root: &Path, executable: Option<&Path>) -> Result<Option<String>> {
    let Some(executable) = executable else {
        return Ok(None);
    };
    let apps = root.join("apps");
    if !executable.starts_with(&apps) {
        return Ok(None);
    }
    let relative = executable.strip_prefix(&apps)?;
    let Some(std::path::Component::Normal(version)) = relative.components().next() else {
        return Ok(None);
    };
    let receipt = apps.join(version).join("twilight-install.json");
    ensure_safe_path(&receipt)?;
    if !receipt.try_exists()? {
        return Ok(Some(format!(
            "{} (legacy install)",
            version.to_string_lossy()
        )));
    }
    ensure!(
        receipt.metadata()?.len() <= 256 * 1024,
        "Installation receipt exceeds limit"
    );
    let receipt: Receipt = serde_json::from_slice(&fs::read(receipt)?)?;
    ensure!(receipt.version == 1, "Unsupported installation receipt");
    Ok(Some(receipt.tag))
}

pub fn install_official(root: &Path) -> Result<Installed> {
    let client = official_client()?;
    let release = release_with(&client)?;
    let apps = root.join("apps");
    ensure_safe_path(&apps)?;
    fs::create_dir_all(&apps)?;
    let destination = apps.join(format!("{}-{}", release.tag, &release.sha256[..12]));
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
        .tempdir_in(&apps)?;
    let archive_path = staging.path().join("release.zip");
    let mut archive_file = File::create(&archive_path)?;
    let mut response = client.get(&release.url).send()?.error_for_status()?;
    ensure!(
        allowed_download_url(response.url()),
        "Untrusted download host"
    );
    let mut sha = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut total = 0_u64;
    loop {
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
    drop(archive_file);
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
        archive_sha256: hash.clone(),
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
        sha256: hash,
        reused: false,
    })
}

fn extract_archive(archive: &Path, destination: &Path) -> Result<PathBuf> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
    ensure!(
        zip.len() <= 512,
        "Release archive contains too many entries"
    );
    let mut total = 0_u64;
    let mut executable = None;
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
            total <= MAX_EXPANDED && entry.size() <= MAX_DOWNLOAD,
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
            if relative
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("dusklight.exe"))
            {
                ensure!(
                    executable.is_none(),
                    "Release contains multiple Dusklight executables"
                );
                executable = Some(relative);
            }
        }
    }
    executable.context("Official release did not contain dusklight.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn latest_release_requires_the_official_asset_url_and_published_digest() -> Result<()> {
        let plan = validate_release(release_fixture())?;
        assert_eq!(plan.tag, "v9.9.9");
        assert_eq!(plan.sha256, "a".repeat(64));
        let mut bad = release_fixture();
        bad.assets[0].browser_download_url =
            "https://github.com/other/project/releases/download/bad.zip".into();
        assert!(validate_release(bad).is_err());
        let mut bad = release_fixture();
        bad.assets[0].digest = None;
        assert!(validate_release(bad).is_err());
        Ok(())
    }

    #[test]
    fn existing_managed_installs_are_reverified_not_silently_overwritten() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let executable = temp.path().join("dusklight.exe");
        crate::files::atomic_write(&executable, b"synthetic test file, never executed")?;
        let plan = validate_release(release_fixture())?;
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
        let plan = validate_release(release_fixture())?;
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
    #[ignore = "Downloads the official public release into an isolated temporary directory"]
    fn official_release_installs_and_reuses_in_an_isolated_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let first = install_official(temp.path())?;
        ensure!(
            first.executable.is_file() && !first.reused,
            "Official release was not installed"
        );
        ensure!(
            selected_release(temp.path(), Some(&first.executable))?.as_deref()
                == Some(first.tag.as_str()),
            "Installed version receipt differs"
        );
        let second = install_official(temp.path())?;
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

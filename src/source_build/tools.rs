use super::{check_cancelled, metadata, valid_tag};
use crate::{
    files::{ensure_safe_path, valid_hash},
    install::{self, ReleaseInfo},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::atomic::AtomicBool,
};

#[derive(Clone, Debug, Serialize)]
pub(super) struct Provenance {
    pub repository: String,
    pub tag: String,
    pub asset: String,
    pub sha256: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Package {
    Git,
    Uv,
    Cmake,
    Ninja,
    Llvm,
    Sdl2,
    Sdl3,
    Ffmpeg,
}

impl Package {
    fn repository(self) -> &'static str {
        match self {
            Self::Git => "git-for-windows/git",
            Self::Uv => "astral-sh/uv",
            Self::Cmake => "Kitware/CMake",
            Self::Ninja => "ninja-build/ninja",
            Self::Llvm => "mstorsjo/llvm-mingw",
            Self::Sdl2 => "libsdl-org/sdl2-compat",
            Self::Sdl3 => "libsdl-org/SDL",
            Self::Ffmpeg => "Takoroka/ffmpeg-av1",
        }
    }

    fn release(self) -> &'static str {
        match self {
            Self::Sdl2 => "tags/release-2.32.74",
            Self::Sdl3 => "tags/release-3.4.20",
            Self::Ffmpeg => "tags/9.0.2-av1-1",
            _ => "latest",
        }
    }

    fn accepts(self, name: &str) -> bool {
        match self {
            Self::Git => {
                name.starts_with("MinGit-")
                    && name.ends_with("-64-bit.zip")
                    && !name.contains("busybox")
            }
            Self::Uv => name == "uv-x86_64-pc-windows-msvc.zip",
            Self::Cmake => name.starts_with("cmake-") && name.ends_with("-windows-x86_64.zip"),
            Self::Ninja => name == "ninja-win.zip",
            Self::Llvm => name.starts_with("llvm-mingw-") && name.ends_with("-ucrt-x86_64.zip"),
            Self::Sdl2 => name == "sdl2-compat-devel-2.32.74-VC.zip",
            Self::Sdl3 => name == "SDL3-devel-3.4.20-VC.zip",
            Self::Ffmpeg => name == "ffmpeg-windows-amd64-9.0.2-av1-1.tar.zst",
        }
    }

    fn entry(self) -> PathBuf {
        match self {
            Self::Git => Path::new("cmd").join("git.exe"),
            Self::Uv => PathBuf::from("uv.exe"),
            Self::Cmake => Path::new("bin").join("cmake.exe"),
            Self::Ninja => PathBuf::from("ninja.exe"),
            Self::Llvm => Path::new("bin").join("clang.exe"),
            Self::Sdl2 => Path::new("include").join("SDL.h"),
            Self::Sdl3 => Path::new("lib").join("x64").join("SDL3.dll"),
            Self::Ffmpeg => Path::new("include")
                .join("libavcodec")
                .join("version_major.h"),
        }
    }
}

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

fn select(package: Package, release: Release) -> Result<ReleaseInfo> {
    ensure!(
        valid_tag(&release.tag_name),
        "Unsafe official build-tool tag"
    );
    let candidates: Vec<_> = release
        .assets
        .into_iter()
        .filter(|asset| package.accepts(&asset.name))
        .collect();
    ensure!(
        candidates.len() == 1,
        "The official {} release has no unique supported Windows x64 package",
        package.repository()
    );
    let asset = candidates
        .into_iter()
        .next()
        .context("Missing selected build-tool asset")?;
    ensure!(
        asset.name.len() <= 180
            && asset
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte)),
        "Unsafe build-tool asset name"
    );
    ensure!(
        asset.size > 0 && asset.size <= 2 * 1024 * 1024 * 1024,
        "Build-tool download exceeds its limit"
    );
    let expected = format!(
        "https://github.com/{}/releases/download/{}/{}",
        package.repository(),
        release.tag_name,
        asset.name
    );
    ensure!(
        asset.browser_download_url == expected,
        "Build-tool URL differs from its official publisher repository"
    );
    let digest = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .context("The official build-tool asset has no published SHA-256 digest")?;
    ensure!(valid_hash(digest), "Invalid official build-tool digest");
    Ok(ReleaseInfo {
        tag: release.tag_name,
        asset: asset.name,
        size: asset.size,
        sha256: digest.to_ascii_lowercase(),
        url: expected,
    })
}

#[derive(Clone)]
pub(super) struct Installed {
    pub root: PathBuf,
    pub entry: PathBuf,
}

pub(super) struct Toolset {
    root: PathBuf,
    directories: Vec<tempfile::TempDir>,
    packages: Vec<(Package, Installed)>,
    pub provenance: Vec<Provenance>,
}

impl Toolset {
    pub fn new(root: &Path) -> Result<Self> {
        let root = root.join("build-tools");
        ensure_safe_path(&root)?;
        fs::create_dir_all(root.join("archives"))?;
        Ok(Self {
            root,
            directories: Vec::new(),
            packages: Vec::new(),
            provenance: Vec::new(),
        })
    }

    pub fn ensure(
        &mut self,
        package: Package,
        allow_downloads: bool,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(String) -> Result<()>,
    ) -> Result<Installed> {
        check_cancelled(cancel)?;
        if let Some((_, installed)) = self.packages.iter().find(|(kind, _)| *kind == package) {
            return Ok(installed.clone());
        }
        progress(format!(
            "Checking the published {} dependency package...",
            package.repository()
        ))?;
        let url = format!(
            "https://api.github.com/repos/{}/releases/{}",
            package.repository(),
            package.release()
        );
        let release: Release = metadata(&url)?;
        let release = select(package, release)?;
        let extension = if package == Package::Ffmpeg {
            "tar.zst"
        } else {
            "zip"
        };
        let archive = self
            .root
            .join("archives")
            .join(format!("{}.{extension}", release.sha256));
        ensure_safe_path(&archive)?;
        if archive.try_exists()? {
            ensure!(
                install::digest_file(&archive, release.size)? == release.sha256,
                "Cached build-tool archive changed; remove {} and retry the approved download",
                archive.display()
            );
        } else {
            ensure!(
                allow_downloads,
                "This build needs the official {} tools. Enable approved tool downloads or prepare the verified tool cache first.",
                package.repository()
            );
            progress(format!(
                "Downloading and verifying {} ({:.0} MiB)...",
                release.asset,
                release.size as f64 / 1_048_576.0
            ))?;
            let stage = tempfile::Builder::new()
                .prefix("download-")
                .tempdir_in(&self.root)?;
            let download = stage.path().join("package.zip");
            let result = install::download_archive_cancel(
                &install::official_client()?,
                &release,
                &download,
                cancel,
            );
            check_cancelled(cancel)?;
            result?;
            fs::rename(download, &archive)
                .context("Retaining the verified official tool archive")?;
        }
        check_cancelled(cancel)?;
        progress(format!(
            "Preparing fresh verified {} tools...",
            package.repository()
        ))?;
        let directory = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(&self.root)?;
        let paths = if package == Package::Ffmpeg {
            extract_tar_zstd(&archive, directory.path(), cancel)?
        } else {
            install::extract_files(&archive, directory.path(), 150_000, 8 * 1024 * 1024 * 1024)?
        };
        check_cancelled(cancel)?;
        let suffix = package.entry();
        let matching: Vec<_> = paths
            .iter()
            .filter(|path| path.ends_with(&suffix))
            .collect();
        ensure!(
            matching.len() == 1,
            "Official tool archive has no unique expected {}",
            suffix.display()
        );
        let entry = directory.path().join(matching[0]);
        let mut root = entry.clone();
        for _ in suffix.components() {
            root = root
                .parent()
                .context("Invalid build-tool package layout")?
                .into();
        }
        if entry
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            crate::self_update::validate_pe(&entry)
                .context("The verified build tool is not a Windows x64 program")?;
        }
        let installed = Installed { root, entry };
        self.provenance.push(Provenance {
            repository: package.repository().into(),
            tag: release.tag,
            asset: release.asset,
            sha256: release.sha256,
        });
        self.directories.push(directory);
        self.packages.push((package, installed.clone()));
        Ok(installed)
    }
}

fn extract_tar_zstd(
    archive: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<PathBuf>> {
    ensure_safe_path(archive)?;
    ensure_safe_path(destination)?;
    check_cancelled(cancel)?;
    let decoder = zstd::stream::read::Decoder::new(File::open(archive)?)?;
    let mut tar = tar::Archive::new(decoder);
    let mut files = Vec::new();
    let mut count = 0_usize;
    let mut total = 0_u64;
    for entry in tar.entries()? {
        check_cancelled(cancel)?;
        let mut entry = entry?;
        count += 1;
        ensure!(
            count <= 150_000,
            "Dependency archive contains too many entries"
        );
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "Dependency archive links and special files are forbidden"
        );
        let stored = entry.path()?.into_owned();
        let relative: PathBuf = stored
            .components()
            .filter(|part| *part != Component::CurDir)
            .collect();
        if relative.as_os_str().is_empty() && kind.is_dir() {
            continue;
        }
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                && relative.as_os_str().len() <= 240,
            "Unsafe dependency archive path"
        );
        let size = entry.size();
        total = total
            .checked_add(size)
            .context("Dependency archive size overflow")?;
        ensure!(
            size <= 512 * 1024 * 1024 && total <= 8 * 1024 * 1024 * 1024,
            "Dependency archive expansion limit exceeded"
        );
        let path = destination.join(&relative);
        ensure_safe_path(&path)?;
        if kind.is_dir() {
            fs::create_dir_all(&path)?;
        } else {
            fs::create_dir_all(path.parent().context("Archive file has no parent")?)?;
            let mut output = File::create_new(&path)?;
            let copied = std::io::copy(&mut entry.by_ref().take(size + 1), &mut output)?;
            ensure!(copied == size, "Dependency archive entry length mismatch");
            output.sync_all()?;
            files.push(relative);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Release {
        Release {
            tag_name: "0.13.0".into(),
            assets: vec![Asset {
                name: "uv-x86_64-pc-windows-msvc.zip".into(),
                browser_download_url: "https://github.com/astral-sh/uv/releases/download/0.13.0/uv-x86_64-pc-windows-msvc.zip".into(),
                size: 128,
                digest: Some(format!("sha256:{}", "a".repeat(64))),
            }],
        }
    }

    #[test]
    fn build_tools_require_the_exact_official_repository_and_a_published_digest() -> Result<()> {
        select(Package::Uv, fixture())?;
        let mut bad = fixture();
        bad.assets[0].digest = None;
        assert!(select(Package::Uv, bad).is_err());
        let mut bad = fixture();
        bad.assets[0].browser_download_url = "https://github.com/other/project/tool.zip".into();
        assert!(select(Package::Uv, bad).is_err());
        let mut bad = fixture();
        bad.assets.push(Asset {
            name: bad.assets[0].name.clone(),
            browser_download_url: bad.assets[0].browser_download_url.clone(),
            size: 128,
            digest: bad.assets[0].digest.clone(),
        });
        assert!(select(Package::Uv, bad).is_err());
        Ok(())
    }

    #[test]
    fn codec_archive_extraction_rejects_escape_links_special_files_and_oversized_entries()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("codec.tar.zst");
        let cancel = AtomicBool::new(false);
        let write_archive =
            |name: &str, kind: tar::EntryType, size: u64, payload: &[u8]| -> Result<()> {
                let mut header = tar::Header::new_gnu();
                // Raw names exercise the extractor, not the tar writer's path validation.
                header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
                header.set_mode(0o644);
                header.set_size(size);
                header.set_entry_type(kind);
                header.set_cksum();
                let mut raw = header.as_bytes().to_vec();
                raw.extend_from_slice(payload);
                raw.resize(512 + payload.len().div_ceil(512) * 512 + 1024, 0);
                fs::write(&archive, zstd::stream::encode_all(raw.as_slice(), 0)?)?;
                Ok(())
            };
        let destination = temp.path().join("valid");
        let contents = b"synthetic codec data";
        write_archive(
            "lib/avcodec.lib",
            tar::EntryType::Regular,
            contents.len() as u64,
            contents,
        )?;
        assert_eq!(
            extract_tar_zstd(&archive, &destination, &cancel)?,
            vec![Path::new("lib").join("avcodec.lib")]
        );
        assert_eq!(
            fs::read(destination.join("lib").join("avcodec.lib"))?,
            contents
        );
        assert!(
            extract_tar_zstd(
                &archive,
                &temp.path().join("cancelled"),
                &AtomicBool::new(true)
            )
            .is_err()
        );
        for (index, (name, kind, size)) in [
            ("../escape.lib", tar::EntryType::Regular, 0),
            ("/escape.lib", tar::EntryType::Regular, 0),
            ("C:/escape.lib", tar::EntryType::Regular, 0),
            ("link.lib", tar::EntryType::Symlink, 0),
            ("link.lib", tar::EntryType::Link, 0),
            ("pipe", tar::EntryType::Fifo, 0),
            ("huge.lib", tar::EntryType::Regular, 512 * 1024 * 1024 + 1),
        ]
        .into_iter()
        .enumerate()
        {
            write_archive(name, kind, size, b"")?;
            assert!(
                extract_tar_zstd(&archive, &temp.path().join(format!("bad-{index}")), &cancel)
                    .is_err(),
                "{name} was accepted"
            );
        }
        assert!(!temp.path().join("escape.lib").try_exists()?);
        Ok(())
    }

    #[test]
    #[ignore = "Downloads the disclosed codec supplier's archive into a temporary directory; executes no binary"]
    fn verified_codec_archive_contains_the_required_headers_and_static_libraries() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut tools = Toolset::new(temp.path())?;
        let codecs = tools.ensure(Package::Ffmpeg, true, &AtomicBool::new(false), &mut |_| {
            Ok(())
        })?;
        for relative in [
            Path::new("include").join("libavcodec").join("avcodec.h"),
            Path::new("lib").join("avcodec.lib"),
            Path::new("lib").join("avutil.lib"),
            Path::new("lib").join("swscale.lib"),
            Path::new("lib").join("swresample.lib"),
        ] {
            ensure!(
                codecs.root.join(&relative).is_file(),
                "Missing required codec data: {}",
                relative.display()
            );
        }
        println!(
            "Takoroka/ffmpeg-av1 codec archive digest and SDK layout verified; no binary was executed."
        );
        Ok(())
    }

    #[test]
    #[ignore = "Downloads official SDL SDK data into a temporary directory; no binary is executed"]
    fn official_sdl_sdk_layout_matches_the_source_recipe() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut tools = Toolset::new(temp.path())?;
        let cancel = AtomicBool::new(false);
        let sdl2 = tools.ensure(Package::Sdl2, true, &cancel, &mut |_| Ok(()))?;
        ensure!(
            sdl2.root.join("lib").join("x64").join("SDL2.dll").is_file(),
            "SDL2 compatibility runtime was not found"
        );
        let sdl3 = tools.ensure(Package::Sdl3, true, &cancel, &mut |_| Ok(()))?;
        ensure!(sdl3.entry.is_file(), "SDL3 runtime was not found");
        println!("Official SDL2 compatibility and SDL3 SDK layouts verified; no binary was run.");
        Ok(())
    }
}

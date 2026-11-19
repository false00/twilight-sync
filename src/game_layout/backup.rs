use super::{metadata_paths, pending, read};
use crate::{files::ensure_safe_path, install, protocol::GameId, source_build};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

pub(super) fn mentions_backup(bytes: &[u8], name: &str) -> bool {
    bytes
        .windows(name.len())
        .any(|window| window.eq_ignore_ascii_case(name.as_bytes()))
}

pub(crate) struct SourceBackup {
    pub directory: PathBuf,
    pub(super) destination: PathBuf,
    pub(super) executable: PathBuf,
    pub(super) game: GameId,
}

impl SourceBackup {
    pub fn compact(&self, root: &Path) -> Result<String> {
        ensure!(!pending(root)?, "Game folder promotion has not finished");
        ensure_safe_path(&self.directory)?;
        ensure_safe_path(&self.destination)?;
        let name = self
            .directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("Invalid source backup folder name")?;
        for path in metadata_paths(root) {
            ensure_safe_path(&path)?;
            if path.try_exists()? && mentions_backup(&read(&path)?, name) {
                return Ok("The previous folder was kept because it is still referenced.".into());
            }
        }
        let receipt = self.directory.join("twilight-source.json");
        ensure_safe_path(&receipt)?;
        if !receipt.try_exists()? {
            return Ok(
                "The previous folder was kept because it has no source-build receipt.".into(),
            );
        }
        let revision = source_build::selected_revision(&receipt, self.game)?;
        let program = self.directory.join(&self.executable);
        ensure_safe_path(&program)?;
        let Some(metadata) = regular_metadata(&program)? else {
            return Ok("The previous folder was kept because its program was not found.".into());
        };
        if !metadata.is_file() {
            return Ok("The previous folder was kept because its program is not a file.".into());
        }
        let locked = open_for_comparison(&program)?;
        let size = locked.metadata()?.len();
        if install::digest_file(&program, size)? != revision.sha256 {
            return Ok("The previous folder was kept because its program was modified.".into());
        }

        source_build::games_stopped()?;
        let mut cleanup = Compactor::new();
        cleanup.remove(&program, size)?;
        drop(locked);
        if self.game == GameId::Triforce {
            for name in [
                "triforce_programs.bin",
                "triforce_programs.idx",
                "triforce_prepared.bin",
                "triforce_pipelines.bin",
                "triforce_pipelines.bin.built",
                "triforce_shaders.bin",
                "mkw_vk_pipelines.bin",
                "mkw_vk_pipelines.bin.for",
                "mkw_vk_pipelines_play.bin",
                "mkw_vk_pipelines_play.bin.for",
            ] {
                let path = self.directory.join(name);
                if let Some(metadata) = regular_metadata(&path)?
                    && metadata.is_file()
                {
                    cleanup.remove(&path, metadata.len())?;
                }
            }
            for name in [".triforce_exefs", "image.bin"] {
                cleanup.remove_duplicates(
                    &self.directory.join(name),
                    &self.destination.join(name),
                    0,
                )?;
            }
        }
        if matches!(self.game, GameId::Triforce | GameId::Totk) {
            cleanup.remove_duplicates(
                &self.directory.join("romfs"),
                &self.destination.join("romfs"),
                0,
            )?;
        }
        Ok(format!(
            "Removed {} old program, cache, or duplicate data files ({:.2} GiB by file size). User data and unrecognized files were kept.",
            cleanup.removed,
            cleanup.bytes as f64 / (1024_u64.pow(3) as f64)
        ))
    }
}

fn regular_metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Ok(None);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Ok(None);
                }
            }
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Inspecting {}", path.display())),
    }
}

fn open_for_comparison(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        // Keep both copies stable while comparing and unlink only the old name.
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    options
        .open(path)
        .with_context(|| format!("Opening {}", path.display()))
}

struct Compactor {
    removed: usize,
    bytes: u64,
    visited: usize,
    old_buffer: Vec<u8>,
    new_buffer: Vec<u8>,
}

impl Compactor {
    fn new() -> Self {
        Self {
            removed: 0,
            bytes: 0,
            visited: 0,
            old_buffer: vec![0; 64 * 1024],
            new_buffer: vec![0; 64 * 1024],
        }
    }

    fn remove(&mut self, path: &Path, bytes: u64) -> Result<()> {
        ensure_safe_path(path)?;
        fs::remove_file(path).with_context(|| format!("Removing old file {}", path.display()))?;
        self.removed += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        Ok(())
    }

    fn remove_duplicates(&mut self, old: &Path, new: &Path, depth: usize) -> Result<()> {
        self.visited += 1;
        ensure!(
            self.visited <= 500_000 && depth <= 32,
            "Old game data exceeds the duplicate-check limit"
        );
        let (Some(a), Some(b)) = (regular_metadata(old)?, regular_metadata(new)?) else {
            return Ok(());
        };
        if a.is_dir() && b.is_dir() {
            for entry in fs::read_dir(old)? {
                let name = entry?.file_name();
                self.remove_duplicates(&old.join(&name), &new.join(name), depth + 1)?;
            }
            ensure_safe_path(old)?;
            match fs::remove_dir(old) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => {}
                Err(error) => return Err(error).context("Removing an empty old data folder"),
            }
        } else if a.is_file() && b.is_file() && a.len() == b.len() {
            ensure_safe_path(old)?;
            ensure_safe_path(new)?;
            let mut previous = open_for_comparison(old)?;
            let mut current = open_for_comparison(new)?;
            let size = previous.metadata()?.len();
            if current.metadata()?.len() == size
                && self.matches(&mut previous, &mut current, size)?
            {
                self.remove(old, size)?;
            }
        }
        Ok(())
    }

    fn matches(&mut self, previous: &mut File, current: &mut File, mut size: u64) -> Result<bool> {
        while size > 0 {
            let count = size.min(self.old_buffer.len() as u64) as usize;
            previous.read_exact(&mut self.old_buffer[..count])?;
            current.read_exact(&mut self.new_buffer[..count])?;
            if self.old_buffer[..count] != self.new_buffer[..count] {
                return Ok(false);
            }
            size -= count as u64;
        }
        Ok(previous.read(&mut [0])? == 0 && current.read(&mut [0])? == 0)
    }
}

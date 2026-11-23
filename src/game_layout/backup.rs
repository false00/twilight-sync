use super::{metadata_paths, pending, read};
use crate::{
    files::{ensure_safe_path, valid_hash},
    install,
    protocol::GameId,
    source_build,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::{
    collections::HashMap,
    fs::{self, File, Metadata, OpenOptions},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

pub(crate) fn mentions_backup(bytes: &[u8], name: &str) -> bool {
    bytes
        .windows(name.len())
        .any(|window| window.eq_ignore_ascii_case(name.as_bytes()))
}

/// "<prefix><tag>-<12 hex commit>-<6 random>": the version folders and workspaces source builds create.
pub(crate) fn source_version(name: &str, prefix: &str) -> bool {
    let Some((revision, suffix)) = name
        .strip_prefix(prefix)
        .and_then(|rest| rest.rsplit_once('-'))
    else {
        return false;
    };
    let Some((tag, commit)) = revision.rsplit_once('-') else {
        return false;
    };
    source_build::valid_tag(tag)
        && commit.len() == 12
        && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        && suffix.len() == 6
        && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

pub(crate) struct SourceBackup {
    pub directory: PathBuf,
    pub(super) destination: PathBuf,
    pub(super) executable: PathBuf,
    pub(super) game: GameId,
    pub(super) replacement: Option<PathBuf>,
    pub(super) extra_metadata: Vec<PathBuf>,
}

impl SourceBackup {
    pub(crate) fn require_no_extra_source_versions(
        managed: &Path,
        selected: Option<&Path>,
    ) -> Result<()> {
        ensure_safe_path(managed)?;
        for entry in fs::read_dir(managed)? {
            let entry = entry?;
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|name| source_version(name, "source-"))
            {
                continue;
            }
            let wrapper = entry.path();
            ensure_safe_path(&wrapper)?;
            let install = wrapper.join("game");
            ensure!(
                selected.is_some_and(
                    |selected| within(selected, &install) && within(&install, selected)
                ),
                "Another managed source version remains at {}. Resolve the old folder before starting a new build.",
                wrapper.display()
            );
        }
        Ok(())
    }

    pub(crate) fn compact_source_siblings(
        root: &Path,
        managed: &Path,
        game: GameId,
        active: &Path,
        exclude: Option<&Path>,
        metadata: &[PathBuf],
    ) -> Result<Vec<String>> {
        ensure!(!pending(root)?, "Game folder promotion has not finished");
        ensure_safe_path(managed)?;
        ensure_safe_path(active)?;
        let destination = active.parent().context("Missing active game directory")?;
        ensure!(
            within(active, managed) && active.is_file(),
            "Missing active managed game"
        );
        let project = (game == GameId::Triforce)
            .then(|| source_build::OotProject::from_executable(active))
            .transpose()?;
        let installed =
            source_build::selected_revision(&destination.join("twilight-source.json"), game)?;
        ensure!(
            project.is_none_or(|project| project == installed.oot_project),
            "Wrong active OOT project"
        );
        ensure!(
            install::digest_file(active, active.metadata()?.len())? == installed.sha256,
            "The active program no longer matches its source receipt"
        );
        let filename = active
            .file_name()
            .context("Missing active executable filename")?;
        let selected_at = active.metadata()?.created()?;
        let mut results = Vec::new();
        let mut candidates = 0;
        // Only inspect immediate versioned children of this one managed project.
        for entry in fs::read_dir(managed)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with("source-") {
                continue;
            }
            candidates += 1;
            ensure!(
                candidates <= 128,
                "Too many versioned build folders to check"
            );
            let wrapper = entry.path();
            let directory = wrapper.join("game");
            if within(active, &directory)
                || exclude.is_some_and(|exclude| {
                    within(&directory, exclude) && within(exclude, &directory)
                })
                || !regular_metadata(&wrapper)?.is_some_and(|m| m.is_dir())
                || !regular_metadata(&directory)?.is_some_and(|m| m.is_dir())
            {
                continue;
            }
            let receipt = directory.join("twilight-source.json");
            let Ok(revision) = source_build::selected_revision(&receipt, game) else {
                continue;
            };
            if project.is_some_and(|project| project != revision.oot_project)
                || Self::source_wrapper(&directory, &revision).is_none()
            {
                continue;
            }
            let executable = directory.join(filename);
            if !executable.try_exists()? {
                anyhow::bail!(
                    "Earlier cleanup left data in {} without its verified executable. Inspect and resolve that folder before retrying.",
                    directory.display()
                );
            }
            if !regular_metadata(&executable)?.is_some_and(|m| {
                m.is_file() && m.created().is_ok_and(|created| created <= selected_at)
            }) {
                continue;
            }
            let backup =
                Self::previous(root, managed, game, &executable, active, metadata.to_vec())
                    .context("Invalid previous versioned build")?;
            results.push(
                backup
                    .compact(root)
                    .with_context(|| format!("Retiring {}", directory.display()))?,
            );
        }
        Ok(results)
    }

    pub(crate) fn previous(
        root: &Path,
        managed: &Path,
        game: GameId,
        previous: &Path,
        replacement: &Path,
        extra_metadata: Vec<PathBuf>,
    ) -> Option<Self> {
        if game == GameId::Triforce
            && source_build::OotProject::from_executable(previous).ok()?
                != source_build::OotProject::from_executable(replacement).ok()?
        {
            return None;
        }
        let destination = replacement.parent()?;
        let legacy = root.join("apps");
        if !within(destination, managed)
            || within(previous, replacement)
            || (within(previous, destination) && destination != managed)
            || !(within(previous, managed) || within(previous, &legacy))
        {
            return None;
        }
        let mut directory = previous.parent()?;
        // Official archives can nest their executable below the receipt root.
        if game == GameId::Dusklight && !directory.join("twilight-source.json").exists() {
            let mut candidate = directory;
            while within(candidate, managed) || (candidate != legacy && within(candidate, &legacy))
            {
                if candidate.join("twilight-install.json").exists() {
                    directory = candidate;
                    break;
                }
                candidate = candidate.parent()?;
            }
        }
        // Only a stable, known Games directory may contain the new install.
        if within(destination, directory) && directory != managed {
            return None;
        }
        Some(Self {
            directory: directory.into(),
            destination: destination.into(),
            executable: previous.strip_prefix(directory).ok()?.into(),
            game,
            replacement: Some(replacement.into()),
            extra_metadata,
        })
    }

    pub fn compact(&self, root: &Path) -> Result<String> {
        ensure!(!pending(root)?, "Game folder promotion has not finished");
        ensure_safe_path(&self.directory)?;
        ensure_safe_path(&self.destination)?;
        if let Some(replacement) = &self.replacement {
            ensure_safe_path(replacement)?;
            ensure!(
                replacement.is_file(),
                "The replacement program was not found"
            );
        }
        let mut protected = if within(&self.directory, &self.destination) {
            self.replacement.iter().cloned().collect()
        } else {
            vec![self.destination.clone()]
        };
        for path in metadata_paths(root)
            .into_iter()
            .chain(self.extra_metadata.iter().cloned())
        {
            ensure_safe_path(&path)?;
            if !path.try_exists()? {
                continue;
            }
            let bytes = read(&path)?;
            if self.replacement.is_none() {
                let name = self
                    .directory
                    .file_name()
                    .and_then(|n| n.to_str())
                    .context("Invalid source backup folder name")?;
                if mentions_backup(&bytes, name) {
                    return Ok(
                        "The previous folder was kept because it is still referenced.".into(),
                    );
                }
            } else {
                collect_references(
                    &serde_json::from_slice(&bytes)?,
                    &self.directory,
                    &self.destination,
                    &mut protected,
                );
            }
        }
        let program = self.directory.join(&self.executable);
        if overlaps_any(&program, &protected) {
            anyhow::bail!(
                "The old program or its folder is still referenced: {}. Update the path before retrying cleanup.",
                program.display()
            );
        }
        let receipt = self.directory.join("twilight-source.json");
        ensure_safe_path(&receipt)?;
        if !receipt.try_exists()? {
            if self.game == GameId::Dusklight
                && self.directory.join("twilight-install.json").try_exists()?
            {
                return self.compact_release(&program, &protected);
            }
            anyhow::bail!(
                "Cannot verify ownership of {}: no supported installation receipt.",
                self.directory.display()
            );
        }
        let revision = source_build::selected_revision(&receipt, self.game)?;
        if self.game == GameId::Triforce {
            ensure!(
                source_build::OotProject::from_executable(&program)? == revision.oot_project,
                "The previous OOT program does not match the project in its receipt"
            );
        }
        source_build::games_stopped()?;
        let wrapper = Self::source_wrapper(&self.directory, &revision);
        let mut cleanup = Compactor::new(protected);
        if let Some(wrapper) = wrapper {
            ensure!(
                fs::read_dir(wrapper)?
                    .all(|entry| entry.is_ok_and(|entry| entry.path() == self.directory)),
                "Unexpected data beside the old game in {}. Move it out before retrying cleanup.",
                wrapper.display()
            );
            cleanup.plan = Some(HashMap::new());
        }
        let Some((program_lock, program_size)) =
            cleanup.verify(&program, &revision.sha256, None)?
        else {
            anyhow::bail!(
                "Cannot verify the old program at {}: modified or missing.",
                program.display()
            );
        };
        if wrapper.is_some() {
            cleanup.remove(&receipt, receipt.metadata()?.len())?;
            for (name, hash) in &revision.runtime_libraries {
                let path = self.directory.join(name);
                if let Some((_lock, size)) = cleanup.verify(&path, hash, None)? {
                    cleanup.remove(&path, size)?;
                }
            }
        }
        if self.game == GameId::Triforce
            && revision.oot_project == source_build::OotProject::Triforce
        {
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
                cleanup.remove_cache(&self.directory.join(name))?;
            }
        }
        if self.game == GameId::Triforce
            && revision.oot_project == source_build::OotProject::Longshot
        {
            if wrapper.is_some() {
                cleanup
                    .remove_longshot_install_record(&self.directory, &revision.runtime_libraries)?;
                cleanup.remove_prompt_outputs(&self.directory.join("prompts"))?;
                for name in ["logs", "bug-reports"] {
                    cleanup.preserve_missing(
                        &self.directory.join(name),
                        &self.destination.join(name),
                        0,
                    )?;
                }
            }
            // Upstream stores these generated files in cache/, or loose in older releases.
            let cache = self.directory.join("cache");
            let mut locations = vec![self.directory.as_path()];
            if regular_metadata(&cache)?.is_some_and(|m| m.is_dir()) {
                locations.push(&cache);
            }
            for directory in locations {
                for name in [
                    "longshot_programs.bin",
                    "longshot_programs.idx",
                    "longshot_programs.zst.bin",
                    "longshot_programs.zst.idx",
                    "longshot_module_ids.bin",
                    "longshot_pipelines.bin",
                    "longshot_pipelines.bin.predicted",
                    "longshot_pipelines.bin.predicted_keys",
                    "longshot_pipelines.bin.built",
                    "longshot_computes.bin",
                    "longshot_compute_variants.bin",
                    "longshot_pipeline_states.bin",
                    "longshot_pipeline_states_v1.bin",
                    "longshot_pipeline_states_shipped.bin",
                    "longshot_prepared.bin",
                    "longshot_vk_pipelines.bin",
                    "longshot_vk_pipelines.stamp",
                    "longshot_textures_drawn.bin",
                    "mkw_vk_pipelines.bin",
                    "mkw_vk_pipelines.stamp",
                    "longshot_shaders.bin",
                    "longshot_load_start.txt",
                ] {
                    cleanup.remove_cache(&directory.join(name))?;
                }
                for entry in fs::read_dir(directory)?.take(4096) {
                    let entry = entry?;
                    let name = entry.file_name();
                    if name.to_str().is_some_and(Self::longshot_play_cache) {
                        cleanup.remove_cache(&entry.path())?;
                    }
                }
            }
            let prompts = cache.join("prompts");
            if regular_metadata(&prompts)?.is_some_and(|m| m.is_dir()) {
                for name in [
                    "Common.blarc.key",
                    "Common.blarc.zs",
                    "Common.bntx.zs",
                    "Common.key",
                    "External.bfotf.zs",
                ] {
                    cleanup.remove_cache(&prompts.join(name))?;
                }
                cleanup.remove_empty_dir(&prompts)?;
            }
            cleanup.remove_empty_dir(&cache)?;
        }
        if wrapper.is_none() && self.game == GameId::Triforce {
            let (exefs, renderer) = match revision.oot_project {
                source_build::OotProject::Triforce => (".triforce_exefs", "triforce_vk.dll"),
                source_build::OotProject::Longshot => (".longshot_exefs", "longshot_vk.dll"),
            };
            for name in [exefs, "image.bin", renderer, "SDL2.dll", "SDL3.dll"] {
                cleanup.remove_duplicates(
                    &self.directory.join(name),
                    &self.destination.join(name),
                    0,
                )?;
            }
        }
        if wrapper.is_none() && matches!(self.game, GameId::Triforce | GameId::Totk) {
            cleanup.remove_duplicates(
                &self.directory.join("romfs"),
                &self.destination.join("romfs"),
                0,
            )?;
        }
        if let Some(wrapper) = wrapper {
            cleanup.remove(&program, program_size)?;
            cleanup.inspect_version(&self.directory, &self.destination, 0)?;
            cleanup.retire_version(&self.directory, &program, &receipt, program_lock)?;
            cleanup.remove_empty_dir(wrapper)?;
            ensure!(
                !wrapper.try_exists()?,
                "Old version folder remains: {}",
                wrapper.display()
            );
            return Ok(cleanup.summary(&self.directory));
        }
        ensure!(
            cleanup.locked == 0,
            "{} duplicate files are still in use; {} Retry cleanup after Windows releases them.",
            cleanup.locked,
            cleanup.summary(&self.directory)
        );
        cleanup.remove(&program, program_size)?;
        drop(program_lock);
        Ok(cleanup.summary(&self.directory))
    }

    fn source_wrapper<'a>(
        directory: &'a Path,
        revision: &source_build::Revision,
    ) -> Option<&'a Path> {
        let wrapper = directory.parent()?;
        let name = wrapper.file_name()?.to_str()?;
        let suffix = name.strip_prefix(&format!(
            "source-{}-{}-",
            revision.tag,
            &revision.commit[..12]
        ))?;
        (directory.file_name()? == "game"
            && suffix.len() == 6
            && suffix.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then_some(wrapper)
    }

    fn longshot_play_cache(name: &str) -> bool {
        name.strip_prefix("longshot_vk_pipelines_play_")
            .or_else(|| name.strip_prefix("mkw_vk_pipelines_play_"))
            .and_then(|suffix| suffix.strip_suffix(".bin"))
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    }

    fn compact_release(&self, program: &Path, protected: &[PathBuf]) -> Result<String> {
        let (files, versioned) = install::previous_release_files(&self.directory, program)?;
        let executable = files
            .iter()
            .find(|f| f.path == self.executable)
            .context("Missing previous release program record")?;
        source_build::games_stopped()?;
        let mut cleanup = Compactor::new(protected.to_vec());
        if versioned {
            cleanup.plan = Some(HashMap::new());
        }
        let Some((program_lock, program_size)) =
            cleanup.verify(program, &executable.sha256, Some(executable.size))?
        else {
            anyhow::bail!(
                "Cannot verify the old program at {}: modified or missing.",
                program.display()
            );
        };
        for file in &files {
            if file.path != self.executable {
                let path = self.directory.join(&file.path);
                if let Some((_lock, size)) = cleanup.verify(&path, &file.sha256, Some(file.size))? {
                    cleanup.remove(&path, size)?;
                }
            }
        }
        if versioned {
            let receipt = self.directory.join("twilight-install.json");
            cleanup.remove(program, program_size)?;
            cleanup.remove(&receipt, receipt.metadata()?.len())?;
            cleanup.inspect_version(&self.directory, &self.destination, 0)?;
            cleanup.retire_version(&self.directory, program, &receipt, program_lock)?;
            return Ok(cleanup.summary(&self.directory));
        }
        cleanup.remove(program, program_size)?;
        drop(program_lock);
        Ok(cleanup.summary(&self.directory))
    }
}

pub(crate) fn within(path: &Path, base: &Path) -> bool {
    #[cfg(windows)]
    {
        let mut path = path.components();
        base.components().all(|part| {
            path.next()
                .is_some_and(|item| item.as_os_str().eq_ignore_ascii_case(part.as_os_str()))
        })
    }
    #[cfg(not(windows))]
    path.starts_with(base)
}

fn overlaps_any(path: &Path, protected: &[PathBuf]) -> bool {
    protected
        .iter()
        .any(|other| within(path, other) || within(other, path))
}

fn collect_references(
    value: &serde_json::Value,
    old: &Path,
    new: &Path,
    protected: &mut Vec<PathBuf>,
) {
    match value {
        serde_json::Value::Object(fields) => {
            for value in fields.values() {
                collect_references(value, old, new, protected);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_references(value, old, new, protected);
            }
        }
        serde_json::Value::String(value) => {
            let path = Path::new(value);
            if path.is_absolute() && within(path, old) && (within(old, new) || !within(path, new)) {
                protected.push(path.into());
            }
        }
        _ => {}
    }
}

pub(crate) fn regular_metadata(path: &Path) -> Result<Option<Metadata>> {
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
    preserved: usize,
    locked: usize,
    bytes: u64,
    visited: usize,
    old_buffer: Vec<u8>,
    new_buffer: Vec<u8>,
    protected: Vec<PathBuf>,
    plan: Option<HashMap<PathBuf, (u64, std::time::SystemTime)>>,
    directories: Vec<PathBuf>,
}

impl Compactor {
    fn remove_cache(&mut self, path: &Path) -> Result<()> {
        if !overlaps_any(path, &self.protected)
            && let Some(metadata) = regular_metadata(path)?
            && metadata.is_file()
        {
            self.remove(path, metadata.len())?;
        }
        Ok(())
    }

    fn new(protected: Vec<PathBuf>) -> Self {
        Self {
            removed: 0,
            preserved: 0,
            locked: 0,
            bytes: 0,
            visited: 0,
            old_buffer: vec![0; 64 * 1024],
            new_buffer: vec![0; 64 * 1024],
            protected,
            plan: None,
            directories: Vec::new(),
        }
    }
}

impl Compactor {
    fn remove_longshot_install_record(
        &mut self,
        directory: &Path,
        libraries: &[(String, String)],
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct InstalledFile {
            sha1: String,
        }
        let path = directory.join(".longshot_installed.json");
        if !regular_metadata(&path)?.is_some_and(|m| m.is_file()) {
            return Ok(());
        }
        let installed: HashMap<String, InstalledFile> = serde_json::from_slice(&read(&path)?)
            .context("Reading Longshot's installation record")?;
        ensure!(
            installed.len() <= 4096,
            "Longshot installation record exceeds its limit"
        );
        for name in source_build::oot_runtime_libraries(source_build::OotProject::Longshot) {
            // Older source receipts did not record DLL hashes; never override a newer SHA-256 receipt.
            if libraries.iter().any(|(recorded, _)| recorded == name) {
                continue;
            }
            let Some(record) = installed.get(name) else {
                continue;
            };
            ensure!(
                record.sha1.len() == 40 && record.sha1.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid Longshot installed file hash: {name}"
            );
            let file = directory.join(name);
            if overlaps_any(&file, &self.protected)
                || !regular_metadata(&file)?.is_some_and(|m| m.is_file())
            {
                continue;
            }
            ensure_safe_path(&file)?;
            let mut locked = open_for_comparison(&file)?;
            let size = locked.metadata()?.len();
            let mut hash = Sha1::new();
            loop {
                let count = locked.read(&mut self.old_buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&self.old_buffer[..count]);
            }
            if hex::encode(hash.finalize()).eq_ignore_ascii_case(&record.sha1) {
                self.remove(&file, size)?;
            }
        }
        self.remove_cache(&path)
    }

    fn remove_prompt_outputs(&mut self, directory: &Path) -> Result<()> {
        #[derive(Deserialize)]
        struct PromptRecord {
            outputs: HashMap<PathBuf, Option<(u64, String)>>,
        }
        let key = directory.join(".prompt_key");
        if !regular_metadata(&key)?.is_some_and(|m| m.is_file()) {
            return Ok(());
        }
        let record: PromptRecord = serde_json::from_slice(&read(&key)?)
            .context("Reading Longshot's generated prompt record")?;
        ensure!(
            record.outputs.len() <= 4096,
            "Longshot prompt record exceeds its limit"
        );
        for (relative, output) in record.outputs {
            ensure!(
                !relative.as_os_str().is_empty()
                    && relative
                        .components()
                        .all(|part| { matches!(part, std::path::Component::Normal(_)) }),
                "Unsafe generated prompt path: {}",
                relative.display()
            );
            if let Some((size, hash)) = output {
                ensure!(valid_hash(&hash), "Invalid generated prompt hash");
                let path = directory.join(relative);
                if let Some((_lock, size)) = self.verify(&path, &hash, Some(size))? {
                    self.remove(&path, size)?;
                }
            }
        }
        self.remove_cache(&key)
    }

    fn preserve_missing(&mut self, old: &Path, new: &Path, depth: usize) -> Result<()> {
        if self.protected.iter().any(|path| within(old, path)) {
            return Ok(());
        }
        self.visited += 1;
        ensure!(
            depth <= 32 && self.visited <= 500_000,
            "Old diagnostics exceed the cleanup scan limit"
        );
        let Some(metadata) = regular_metadata(old)? else {
            return Ok(());
        };
        ensure_safe_path(old)?;
        ensure_safe_path(new)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(old)? {
                let entry = entry?;
                self.preserve_missing(&entry.path(), &new.join(entry.file_name()), depth + 1)?;
            }
        } else if metadata.is_file() && !new.try_exists()? {
            let parent = new.parent().context("Missing diagnostic destination")?;
            fs::create_dir_all(parent)?;
            let mut previous = open_for_comparison(old)?;
            let mut copy = tempfile::NamedTempFile::new_in(parent)?;
            ensure!(
                std::io::copy(&mut previous, &mut copy)? == metadata.len(),
                "Old diagnostic changed during preservation: {}",
                old.display()
            );
            copy.as_file().sync_all()?;
            copy.persist_noclobber(new)
                .with_context(|| format!("Preserving old diagnostics at {}", new.display()))?;
            self.preserved += 1;
        }
        Ok(())
    }

    fn verify(
        &self,
        path: &Path,
        hash: &str,
        expected: Option<u64>,
    ) -> Result<Option<(File, u64)>> {
        if overlaps_any(path, &self.protected)
            || !regular_metadata(path)?.is_some_and(|m| m.is_file())
        {
            return Ok(None);
        }
        ensure_safe_path(path)?;
        let locked = open_for_comparison(path)?;
        let size = locked.metadata()?.len();
        if expected.is_some_and(|expected| size != expected)
            || install::digest_file(path, size)? != hash
        {
            return Ok(None);
        }
        Ok(Some((locked, size)))
    }

    fn remove(&mut self, path: &Path, bytes: u64) -> Result<()> {
        ensure_safe_path(path)?;
        if let Some(plan) = &mut self.plan {
            let metadata = path.metadata()?;
            ensure!(
                metadata.len() == bytes,
                "File changed during cleanup: {}",
                path.display()
            );
            plan.insert(path.to_path_buf(), (bytes, metadata.modified()?));
            return Ok(());
        }
        fs::remove_file(path).with_context(|| format!("Removing old file {}", path.display()))?;
        self.removed += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        Ok(())
    }

    fn remove_duplicates(&mut self, old: &Path, new: &Path, depth: usize) -> Result<()> {
        if self.protected.iter().any(|path| within(old, path)) {
            return Ok(());
        }
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
            self.remove_empty_dir(old)?;
        } else if a.is_file()
            && b.is_file()
            && a.len() == b.len()
            && !overlaps_any(old, &self.protected)
        {
            ensure_safe_path(old)?;
            ensure_safe_path(new)?;
            let mut previous = open_for_comparison(old)?;
            let mut current = open_for_comparison(new)?;
            let size = previous.metadata()?.len();
            if current.metadata()?.len() == size
                && self.matches(&mut previous, &mut current, size)?
            {
                match self.remove(old, size) {
                    Ok(()) => {}
                    #[cfg(windows)]
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.raw_os_error() == Some(32)) =>
                    {
                        self.locked += 1;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn remove_empty_dir(&self, path: &Path) -> Result<()> {
        if self.plan.is_none()
            && !overlaps_any(path, &self.protected)
            && regular_metadata(path)?.is_some_and(|m| m.is_dir())
        {
            ensure_safe_path(path)?;
            match fs::remove_dir(path) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Removing empty folder {}", path.display()));
                }
            }
        }
        Ok(())
    }

    fn inspect_version(&mut self, old: &Path, new: &Path, depth: usize) -> Result<()> {
        self.visited += 1;
        ensure!(
            depth <= 32 && self.visited <= 500_000,
            "Old version exceeds the cleanup scan limit"
        );
        let metadata = regular_metadata(old)?
            .with_context(|| format!("Unsupported path in old version: {}", old.display()))?;
        ensure!(
            !self.protected.iter().any(|path| within(old, path)),
            "Old version is still referenced at {}. Update the path before retrying cleanup.",
            old.display()
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(old)? {
                let entry = entry?;
                self.inspect_version(&entry.path(), &new.join(entry.file_name()), depth + 1)?;
            }
            self.directories.push(old.into());
        } else {
            ensure!(metadata.is_file(), "Unsupported file: {}", old.display());
            if !self
                .plan
                .as_ref()
                .is_some_and(|plan| plan.contains_key(old))
            {
                self.remove_duplicates(old, new, depth)?;
            }
            ensure!(
                self.plan
                    .as_ref()
                    .is_some_and(|plan| plan.contains_key(old)),
                "Old version has data not verified in the replacement: {}. Preserve or move it before retrying cleanup.",
                old.display()
            );
        }
        Ok(())
    }

    fn retire_version(
        &mut self,
        directory: &Path,
        program: &Path,
        receipt: &Path,
        program_lock: File,
    ) -> Result<()> {
        source_build::games_stopped()?;
        let mut plan = self.plan.take().context("Missing version cleanup plan")?;
        for (path, &(size, modified)) in &plan {
            ensure_safe_path(path)?;
            let metadata = path.metadata()?;
            ensure!(
                metadata.len() == size && metadata.modified()? == modified,
                "Old version changed during cleanup: {}",
                path.display()
            );
        }
        let program_size = plan.remove(program).context("Missing previous program")?.0;
        let receipt_size = plan.remove(receipt).context("Missing previous receipt")?.0;
        for (path, (size, _)) in plan {
            self.remove(&path, size)?;
        }
        self.remove(program, program_size)?;
        drop(program_lock);
        for path in &self.directories {
            if path != directory {
                self.remove_empty_dir(path)?;
                ensure!(
                    !path.try_exists()?,
                    "Old version gained files during cleanup: {}",
                    path.display()
                );
            }
        }
        self.remove(receipt, receipt_size)?;
        fs::remove_dir(directory)
            .with_context(|| format!("Removing old version folder {}", directory.display()))
    }

    fn summary(&self, previous: &Path) -> String {
        let preserved = if self.preserved > 0 {
            format!(
                " Preserved {} missing diagnostic files in the replacement.",
                self.preserved
            )
        } else {
            String::new()
        };
        let residual = if previous.is_dir() {
            format!(
                " Remaining data and unrecognized files were kept at {}.",
                previous.display()
            )
        } else {
            String::new()
        };
        format!(
            "Removed {} old program, cache, or duplicate data files ({:.2} GiB by file size).{preserved}{residual}",
            self.removed,
            self.bytes as f64 / (1024_u64.pow(3) as f64)
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::sha256;
    use serde_json::json;

    fn seed_source(path: &Path, game: GameId, project: &str, executable: &str) -> Result<PathBuf> {
        fs::create_dir_all(path)?;
        let exe = path.join(executable);
        fs::write(&exe, b"old verified program")?;
        fs::write(
            path.join("twilight-source.json"),
            serde_json::to_vec(&json!({
                "version": 1, "game": game, "oot_project": project,
                "tag": "v1.2.3", "commit": "a".repeat(40),
                "sha256": sha256(b"old verified program"),
            }))?,
        )?;
        Ok(exe)
    }

    #[test]
    fn retires_longshot_without_touching_triforce_or_unique_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let managed = root.join("Games").join("Longshot");
        let old = managed
            .join("source-v1.2.3-aaaaaaaaaaaa-Abc123")
            .join("game");
        let new = managed
            .join("source-v1.2.4-bbbbbbbbbbbb-Def456")
            .join("game");
        let alternate = root.join("Games").join("Triforce");
        let old_exe = seed_source(&old, GameId::Triforce, "longshot", "longshot.exe")?;
        let new_exe = seed_source(&new, GameId::Triforce, "longshot", "longshot.exe")?;
        let alternate_exe = seed_source(&alternate, GameId::Triforce, "triforce", "Triforce.exe")?;
        fs::write(old.join("longshot_vk.dll"), b"old verified renderer")?;
        fs::write(new.join("longshot_vk.dll"), b"replacement renderer")?;
        let receipt = old.join("twilight-source.json");
        let mut label: serde_json::Value = serde_json::from_slice(&fs::read(&receipt)?)?;
        label["runtime_libraries"] = json!([["longshot_vk.dll", sha256(b"old verified renderer")]]);
        fs::write(receipt, serde_json::to_vec(&label)?)?;
        for directory in [&old, &new] {
            fs::create_dir(directory.join("romfs"))?;
            fs::write(directory.join("romfs").join("same"), b"same game data")?;
            fs::write(directory.join("romfs").join("referenced"), b"same but kept")?;
        }
        fs::write(old.join("romfs").join("unique"), b"old payload")?;
        fs::write(old.join("longshot_env.txt"), b"user settings")?;
        fs::create_dir(old.join("cache"))?;
        fs::write(
            old.join("cache").join("longshot_programs.zst.bin"),
            b"generated",
        )?;
        fs::write(
            old.join("cache").join("longshot_vk_pipelines_play_17.bin"),
            b"generated",
        )?;
        fs::write(old.join("cache").join("custom.bin"), b"not a known cache")?;
        fs::create_dir(old.join("mods"))?;
        fs::write(old.join("mods").join("local"), b"user mod")?;
        fs::write(
            root.join("source-builds.json"),
            serde_json::to_vec(&json!({
                "triforce_install": alternate, "longshot_install": new,
                "triforce": { "game_input": old.join("romfs").join("referenced") }
            }))?,
        )?;
        assert!(
            SourceBackup::previous(
                root,
                &managed,
                GameId::Triforce,
                &alternate_exe,
                &new_exe,
                vec![]
            )
            .is_none()
        );
        let error = SourceBackup::compact_source_siblings(
            root,
            &managed,
            GameId::Triforce,
            &new_exe,
            None,
            &[],
        )
        .err()
        .context("Unaccounted-for data must block retirement")?;
        assert!(format!("{error:#}").contains(&old.display().to_string()));
        for path in [
            old_exe.clone(),
            old.join("romfs").join("same"),
            old.join("cache").join("longshot_programs.zst.bin"),
            old.join("romfs").join("referenced"),
            old.join("romfs").join("unique"),
            old.join("longshot_env.txt"),
            old.join("mods").join("local"),
            old.join("cache").join("custom.bin"),
            new_exe.clone(),
            alternate_exe,
        ] {
            assert!(path.is_file(), "{path:?}");
        }
        fs::remove_file(root.join("source-builds.json"))?;
        for relative in [
            "romfs/referenced",
            "romfs/unique",
            "longshot_env.txt",
            "mods/local",
            "cache/custom.bin",
        ] {
            let destination = new.join(relative);
            fs::create_dir_all(destination.parent().context("Missing fixture parent")?)?;
            fs::copy(old.join(relative), destination)?;
        }
        fs::create_dir(old.join("user"))?;
        fs::create_dir(new.join("user"))?;
        let old_save = old.join("user").join("progress.sav");
        let new_save = new.join("user").join("progress.sav");
        fs::write(&old_save, b"old progress")?;
        fs::write(&new_save, b"different progress")?;
        let error = SourceBackup::compact_source_siblings(
            root,
            &managed,
            GameId::Triforce,
            &new_exe,
            None,
            &[],
        )
        .err()
        .context("Different progress must block retirement")?;
        assert!(format!("{error:#}").contains("progress.sav"));
        assert_eq!(fs::read(&new_save)?, b"different progress");
        assert!(old_exe.is_file());
        fs::copy(old_save, new_save)?;
        SourceBackup::compact_source_siblings(
            root,
            &managed,
            GameId::Triforce,
            &new_exe,
            None,
            &[],
        )?;
        assert!(!old.parent().context("Missing old wrapper")?.exists());
        assert_eq!(fs::read(new.join("mods").join("local"))?, b"user mod");
        Ok(())
    }

    #[test]
    fn legacy_longshot_retirement_verifies_generated_assets_and_preserves_diagnostics() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let managed = root.join("Games").join("Longshot");
        let old = managed
            .join("source-v1.2.3-aaaaaaaaaaaa-Abc123")
            .join("game");
        let new = managed
            .join("source-v1.2.4-bbbbbbbbbbbb-Def456")
            .join("game");
        let old_exe = seed_source(&old, GameId::Triforce, "longshot", "longshot.exe")?;
        let new_exe = seed_source(&new, GameId::Triforce, "longshot", "longshot.exe")?;
        fs::write(old.join("longshot_vk.dll"), b"old renderer")?;
        fs::write(new.join("longshot_vk.dll"), b"new renderer")?;
        fs::write(
            old.join(".longshot_installed.json"),
            serde_json::to_vec(&json!({
                "longshot_vk.dll": {"sha1": hex::encode(Sha1::digest(b"old renderer"))}
            }))?,
        )?;
        let new_record = serde_json::to_vec(&json!({
            "longshot_vk.dll": {"sha1": hex::encode(Sha1::digest(b"new renderer"))}
        }))?;
        fs::write(new.join(".longshot_installed.json"), &new_record)?;
        for directory in [&old, &new] {
            fs::create_dir_all(directory.join("prompts"))?;
            fs::create_dir_all(directory.join("cache").join("prompts"))?;
            fs::create_dir_all(directory.join("logs"))?;
            fs::create_dir_all(directory.join("bug-reports").join("pending"))?;
            fs::create_dir_all(directory.join("user"))?;
            fs::write(
                directory.join("user").join("progress.sav"),
                b"saved progress",
            )?;
        }
        let prompt = old.join("prompts").join("External.bfotf");
        fs::write(&prompt, b"generated old font")?;
        fs::write(
            new.join("prompts").join("External.bfotf"),
            b"generated new font",
        )?;
        fs::write(
            old.join("prompts").join("rsizetable.zs"),
            b"generated table",
        )?;
        fs::write(
            old.join("prompts").join(".prompt_key"),
            serde_json::to_vec(&json!({"outputs": {
                "External.bfotf": [18, sha256(b"generated old font")],
                "rsizetable.zs": [15, sha256(b"generated table")]
            }}))?,
        )?;
        fs::write(
            new.join("prompts").join(".prompt_key"),
            b"new prompt record",
        )?;
        fs::write(
            old.join("longshot_pipeline_states_shipped.bin"),
            b"old shipped cache",
        )?;
        fs::write(
            new.join("longshot_pipeline_states_shipped.bin"),
            b"new shipped cache",
        )?;
        fs::write(
            old.join("cache").join("longshot_load_start.txt"),
            b"old timings",
        )?;
        fs::write(
            old.join("cache").join("prompts").join("Common.key"),
            b"generated prompt cache",
        )?;
        fs::write(
            old.join("logs").join("longshot_log.txt"),
            b"old diagnostics",
        )?;
        fs::write(old.join("bug-reports").join("shown.txt"), b"")?;
        fs::write(
            old.join("bug-reports").join("pending").join("report.txt"),
            b"pending report",
        )?;
        fs::write(old.join("custom.json"), b"unrecognized user data")?;
        let cleanup =
            SourceBackup::previous(root, &managed, GameId::Triforce, &old_exe, &new_exe, vec![])
                .context("Expected a previous Longshot build")?;
        let error = cleanup
            .compact(root)
            .err()
            .context("Custom data must block cleanup")?;
        assert!(format!("{error:#}").contains("custom.json"));
        assert!(old_exe.is_file());
        assert_eq!(fs::read(&prompt)?, b"generated old font");
        assert_eq!(fs::read(new.join(".longshot_installed.json"))?, new_record);
        fs::copy(old.join("custom.json"), new.join("custom.json"))?;

        let new_log = new.join("logs").join("longshot_log.txt");
        fs::write(&new_log, b"new diagnostics")?;
        let error = cleanup
            .compact(root)
            .err()
            .context("Existing diagnostics must not be overwritten")?;
        assert!(format!("{error:#}").contains("longshot_log.txt"));
        assert_eq!(fs::read(&new_log)?, b"new diagnostics");
        assert_eq!(
            fs::read(old.join("logs").join("longshot_log.txt"))?,
            b"old diagnostics"
        );
        assert!(old_exe.is_file());
        fs::write(&new_log, b"old diagnostics")?;

        fs::write(&prompt, b"customized old font")?;
        let error = cleanup
            .compact(root)
            .err()
            .context("Modified prompt assets must be kept")?;
        assert!(format!("{error:#}").contains("External.bfotf"));
        assert!(old_exe.is_file());
        fs::write(&prompt, b"generated old font")?;
        let summary = cleanup.compact(root)?;
        assert!(!old.parent().context("Missing old wrapper")?.exists());
        assert!(summary.contains("Removed"));
        assert_eq!(
            fs::read(new.join("logs").join("longshot_log.txt"))?,
            b"old diagnostics"
        );
        assert_eq!(
            fs::read(new.join("bug-reports").join("pending").join("report.txt"))?,
            b"pending report",
        );
        assert_eq!(
            fs::read(new.join("user").join("progress.sav"))?,
            b"saved progress"
        );
        assert_eq!(fs::read(new.join(".longshot_installed.json"))?, new_record);
        assert_eq!(
            fs::read(new.join("prompts").join("External.bfotf"))?,
            b"generated new font"
        );
        assert_eq!(
            fs::read(new.join("longshot_pipeline_states_shipped.bin"))?,
            b"new shipped cache"
        );
        Ok(())
    }

    #[test]
    fn longshot_install_records_do_not_override_receipts_or_references() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let managed = root.join("Games").join("Longshot");
        let old = managed
            .join("source-v1.2.3-aaaaaaaaaaaa-Abc123")
            .join("game");
        let new = managed
            .join("source-v1.2.4-bbbbbbbbbbbb-Def456")
            .join("game");
        let old_exe = seed_source(&old, GameId::Triforce, "longshot", "longshot.exe")?;
        let new_exe = seed_source(&new, GameId::Triforce, "longshot", "longshot.exe")?;
        fs::write(old.join("longshot_vk.dll"), b"modified renderer")?;
        fs::write(new.join("longshot_vk.dll"), b"replacement renderer")?;
        let receipt = old.join("twilight-source.json");
        let mut label: serde_json::Value = serde_json::from_slice(&fs::read(&receipt)?)?;
        label["runtime_libraries"] = json!([["longshot_vk.dll", sha256(b"verified renderer")]]);
        fs::write(&receipt, serde_json::to_vec(&label)?)?;
        let record = old.join(".longshot_installed.json");
        fs::write(
            &record,
            serde_json::to_vec(&json!({
                "longshot_vk.dll": {"sha1": hex::encode(Sha1::digest(b"modified renderer"))}
            }))?,
        )?;
        let cleanup =
            SourceBackup::previous(root, &managed, GameId::Triforce, &old_exe, &new_exe, vec![])
                .context("Expected a previous Longshot build")?;
        let error = cleanup
            .compact(root)
            .err()
            .context("The SHA-256 receipt must take priority")?;
        assert!(format!("{error:#}").contains("longshot_vk.dll"));
        assert!(old_exe.is_file());
        assert_eq!(fs::read(old.join("longshot_vk.dll"))?, b"modified renderer");

        fs::write(old.join("longshot_vk.dll"), b"verified renderer")?;
        fs::write(
            root.join("source-builds.json"),
            serde_json::to_vec(&json!({"triforce": {"game_input": record}}))?,
        )?;
        let error = cleanup
            .compact(root)
            .err()
            .context("Referenced metadata must be kept")?;
        assert!(format!("{error:#}").contains("still referenced"));
        assert!(old_exe.is_file());
        assert!(record.is_file());
        Ok(())
    }

    #[test]
    fn longshot_prompt_records_cannot_authorize_cleanup_outside_prompts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let managed = root.join("Games").join("Longshot");
        let old = managed
            .join("source-v1.2.3-aaaaaaaaaaaa-Abc123")
            .join("game");
        let new = managed
            .join("source-v1.2.4-bbbbbbbbbbbb-Def456")
            .join("game");
        let old_exe = seed_source(&old, GameId::Triforce, "longshot", "longshot.exe")?;
        let new_exe = seed_source(&new, GameId::Triforce, "longshot", "longshot.exe")?;
        fs::write(old.join("progress.sav"), b"unique progress")?;
        fs::create_dir(old.join("prompts"))?;
        fs::write(
            old.join("prompts").join(".prompt_key"),
            serde_json::to_vec(&json!({"outputs": {
                "../progress.sav": [15, sha256(b"unique progress")]
            }}))?,
        )?;
        let cleanup =
            SourceBackup::previous(root, &managed, GameId::Triforce, &old_exe, &new_exe, vec![])
                .context("Expected a previous Longshot build")?;
        let error = cleanup
            .compact(root)
            .err()
            .context("Prompt paths must not escape their directory")?;
        assert!(format!("{error:#}").contains("Unsafe generated prompt path"));
        assert!(old_exe.is_file());
        assert_eq!(fs::read(old.join("progress.sav"))?, b"unique progress");
        Ok(())
    }

    #[test]
    fn another_version_blocks_building_even_after_partial_cleanup() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let managed = temp.path();
        let active = managed
            .join("source-v1.2.3-aaaaaaaaaaaa-Abc123")
            .join("game");
        seed_source(&active, GameId::Triforce, "longshot", "longshot.exe")?;
        fs::create_dir(managed.join("source-my-notes"))?;
        SourceBackup::require_no_extra_source_versions(managed, Some(&active))?;
        let leftover = managed
            .join("source-v1.2.2-bbbbbbbbbbbb-Def456")
            .join("game");
        fs::create_dir_all(&leftover)?;
        fs::write(leftover.join("progress.sav"), b"unresolved save")?;
        let error = SourceBackup::require_no_extra_source_versions(managed, Some(&active))
            .err()
            .context("A leftover version must block a new build even without a receipt")?;
        assert!(error.to_string().contains("Def456"));
        assert_eq!(fs::read(leftover.join("progress.sav"))?, b"unresolved save");
        assert!(active.join("longshot.exe").is_file());
        Ok(())
    }

    #[test]
    fn stable_parent_is_preserved_and_empty_versioned_build_is_removed() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let managed = root.join("Games").join("TotKRecomp");
        let stage = managed.join("source-v1.2.3-aaaaaaaaaaaa-Abc123");
        let old_exe = seed_source(&managed, GameId::Totk, "triforce", "totk.exe")?;
        let staged_exe = seed_source(&stage.join("game"), GameId::Totk, "triforce", "totk.exe")?;
        fs::write(managed.join("custom.txt"), b"local custom file")?;
        fs::write(
            root.join("settings.json"),
            serde_json::to_vec(&json!({"next": staged_exe}))?,
        )?;
        SourceBackup::previous(root, &managed, GameId::Totk, &old_exe, &staged_exe, vec![])
            .context("Expected the stable predecessor")?
            .compact(root)?;
        assert!(!old_exe.exists());
        assert!(staged_exe.is_file());
        assert_eq!(fs::read(managed.join("custom.txt"))?, b"local custom file");

        let next_exe = seed_source(
            &managed.join("next").join("game"),
            GameId::Totk,
            "triforce",
            "totk.exe",
        )?;
        let cleanup =
            SourceBackup::previous(root, &managed, GameId::Totk, &staged_exe, &next_exe, vec![])
                .context("Expected the old staged build")?;
        let error = cleanup
            .compact(root)
            .err()
            .context("Referenced predecessor must not be retired")?;
        assert!(error.to_string().contains("still referenced"));
        assert!(staged_exe.is_file());
        fs::remove_file(root.join("settings.json"))?;
        cleanup.compact(root)?;
        assert!(!stage.exists());
        assert!(next_exe.is_file());
        Ok(())
    }

    #[test]
    fn official_release_cleanup_uses_manifest_not_directory_contents() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let managed = root.join("Games").join("Dusklight");
        let old = managed.join("v1.2.3-aaaaaaaaaaaa");
        fs::create_dir_all(old.join("bin"))?;
        let exe = old.join("bin").join("dusklight.exe");
        for name in [
            "bin\\dusklight.exe",
            "owned.dll",
            "edited.dll",
            "custom.ini",
        ] {
            fs::write(old.join(name), b"verified")?;
        }
        let hash = sha256(b"verified");
        let files = ["bin\\dusklight.exe", "owned.dll", "edited.dll"]
            .map(|path| json!({"path": path, "sha256": hash, "size": 8}));
        fs::write(
            old.join("twilight-install.json"),
            serde_json::to_vec(&json!({
                "version": 1, "tag": "v1.2.3", "archive_sha256": "a".repeat(64),
                "executable": "bin\\dusklight.exe", "files": files
            }))?,
        )?;
        fs::write(old.join("edited.dll"), b"modified")?;
        let next = seed_source(
            &managed.join("next"),
            GameId::Dusklight,
            "triforce",
            "dusklight.exe",
        )?;
        let cleanup =
            SourceBackup::previous(root, &managed, GameId::Dusklight, &exe, &next, vec![])
                .context("Expected the official predecessor")?;
        assert!(cleanup.compact(root).is_err());
        assert!(exe.exists());
        assert!(old.join("owned.dll").exists());
        assert_eq!(fs::read(old.join("edited.dll"))?, b"modified");
        assert_eq!(fs::read(old.join("custom.ini"))?, b"verified");
        fs::write(next.with_file_name("edited.dll"), b"modified")?;
        fs::write(next.with_file_name("custom.ini"), b"verified")?;
        cleanup.compact(root)?;
        assert!(!old.exists());
        assert!(next.is_file());
        Ok(())
    }
}

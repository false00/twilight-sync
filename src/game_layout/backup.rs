use super::{metadata_paths, pending, read};
use crate::{
    files::{atomic_write, ensure_safe_path},
    install,
    protocol::GameId,
    source_build, totk_update,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
};

const RETAINED_CLEANUP: &str = "source-cleanup-retained.json";

pub(crate) struct CleanupProgress<'a> {
    pub path: &'a Path,
    pub bytes: u64,
    pub removing: bool,
}

pub(crate) type CleanupReporter<'a> = dyn FnMut(CleanupProgress<'_>) -> Result<()> + 'a;

pub(crate) fn retained_cleanup(root: &Path) -> Result<Vec<PathBuf>> {
    let path = root.join(RETAINED_CLEANUP);
    ensure_safe_path(&path)?;
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let directories: Vec<PathBuf> =
        serde_json::from_slice(&read(&path)?).context("Reading retained cleanup folders")?;
    ensure!(
        directories.len() <= 512,
        "Too many retained cleanup folders"
    );
    for directory in &directories {
        ensure_safe_path(directory)?;
    }
    Ok(directories)
}

pub(crate) fn retain_cleanup(root: &Path, directory: &Path, retain: bool) -> Result<()> {
    ensure_safe_path(directory)?;
    let mut directories = retained_cleanup(root)?;
    directories.retain(|path| path != directory);
    if retain {
        directories.push(directory.into());
    }
    ensure!(
        directories.len() <= 512,
        "Too many retained cleanup folders"
    );
    atomic_write(
        &root.join(RETAINED_CLEANUP),
        &serde_json::to_vec(&directories)?,
    )
}

pub(crate) fn mentions_backup(bytes: &[u8], name: &str) -> bool {
    bytes
        .windows(name.len())
        .any(|window| window.eq_ignore_ascii_case(name.as_bytes()))
}

/// "<prefix><tag>-<12 hex commit>-<6 random>": managed source versions and workspaces.
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

    #[cfg(test)]
    pub(crate) fn compact_source_siblings(
        root: &Path,
        managed: &Path,
        game: GameId,
        active: &Path,
        exclude: Option<&Path>,
        metadata: &[PathBuf],
    ) -> Result<Vec<String>> {
        Self::compact_source_siblings_with_progress(
            root,
            managed,
            game,
            active,
            exclude,
            metadata,
            &mut |_| Ok(()),
        )
    }

    pub(crate) fn compact_source_siblings_with_progress(
        root: &Path,
        managed: &Path,
        game: GameId,
        active: &Path,
        exclude: Option<&Path>,
        metadata: &[PathBuf],
        progress: &mut CleanupReporter<'_>,
    ) -> Result<Vec<String>> {
        ensure!(!pending(root)?, "Game folder promotion has not finished");
        ensure_safe_path(managed)?;
        ensure_safe_path(active)?;
        ensure!(
            within(active, managed) && active.is_file(),
            "Missing active managed game"
        );
        let project = (game == GameId::Triforce)
            .then(|| source_build::OotProject::from_executable(active))
            .transpose()?;
        let installed = source_build::selected_revision(
            &active
                .parent()
                .context("Missing active game directory")?
                .join("twilight-source.json"),
            game,
        )?;
        ensure!(
            project.is_none_or(|project| project == installed.oot_project),
            "Wrong active OOT project"
        );
        let filename = active
            .file_name()
            .context("Missing active executable filename")?;
        let selected_at = active.metadata()?.created()?;
        let mut results = Vec::new();
        let mut candidates = 0;
        for entry in fs::read_dir(managed)? {
            let entry = entry?;
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|name| source_version(name, "source-"))
            {
                continue;
            }
            candidates += 1;
            ensure!(
                candidates <= 128,
                "Too many versioned build folders to check"
            );
            let wrapper = entry.path();
            let directory = wrapper.join("game");
            if within(active, &wrapper) || exclude.is_some_and(|exclude| within(exclude, &wrapper))
            {
                continue;
            }
            ensure_safe_path(&wrapper)?;
            let executable = directory.join(filename);
            let age = regular_metadata(&executable)?.unwrap_or(fs::metadata(&wrapper)?);
            if !age.created().is_ok_and(|created| created <= selected_at) {
                continue;
            }
            if !directory.try_exists()? {
                let name = wrapper
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("Invalid retired version folder")?;
                let mut referenced = false;
                for path in metadata_paths(root)
                    .into_iter()
                    .chain(metadata.iter().cloned())
                {
                    ensure_safe_path(&path)?;
                    if path.try_exists()? && mentions_backup(&read(&path)?, name) {
                        referenced = true;
                        break;
                    }
                }
                if referenced {
                    results.push(format!(
                        "The previous folder was kept because it is still referenced: {}.",
                        wrapper.display()
                    ));
                    continue;
                }
                let saved = preserve_retired_saves(root, &wrapper, &[], progress)?;
                source_build::games_stopped()?;
                progress(CleanupProgress {
                    path: &wrapper,
                    bytes: 0,
                    removing: true,
                })?;
                crate::post_update::remove_folder(&wrapper)?;
                results.push(cleanup_summary(&wrapper, saved.as_deref(), 0));
                continue;
            }
            let receipt = directory.join("twilight-source.json");
            if receipt.try_exists()? {
                let revision = source_build::selected_revision(&receipt, game)?;
                if project.is_some_and(|project| project != revision.oot_project) {
                    continue;
                }
            }
            let backup =
                Self::previous(root, managed, game, &executable, active, metadata.to_vec())
                    .context("Invalid previous versioned build")?;
            results.push(
                backup
                    .compact_with_progress(root, progress)
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

    #[cfg(test)]
    pub fn compact(&self, root: &Path) -> Result<String> {
        self.compact_with_progress(root, &mut |_| Ok(()))
    }

    pub(crate) fn compact_with_progress(
        &self,
        root: &Path,
        progress: &mut CleanupReporter<'_>,
    ) -> Result<String> {
        ensure!(!pending(root)?, "Game folder promotion has not finished");
        ensure_safe_path(&self.directory)?;
        ensure_safe_path(&self.destination)?;
        ensure!(
            self.directory != root && self.directory != root.join("apps"),
            "Cleanup requires a specific retired installation folder"
        );
        if let Some(replacement) = &self.replacement {
            ensure_safe_path(replacement)?;
            ensure!(
                replacement.is_file(),
                "The replacement program was not found"
            );
        }
        let mut protected = vec![self.destination.clone()];
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
                    .and_then(|name| name.to_str())
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
        let wrapper = self.directory.parent().filter(|parent| {
            self.directory
                .file_name()
                .is_some_and(|name| name == "game")
                && parent
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| source_version(name, "source-"))
        });
        let receipt = self.directory.join("twilight-source.json");
        ensure_safe_path(&receipt)?;
        if receipt.try_exists()? {
            let revision = source_build::selected_revision(&receipt, self.game)?;
            if self.game == GameId::Triforce {
                ensure!(
                    source_build::OotProject::from_executable(&program)? == revision.oot_project,
                    "The previous OOT program does not match the project in its receipt"
                );
            }
            if let Some(wrapper) = wrapper {
                let prefix = format!("source-{}-{}-", revision.tag, &revision.commit[..12]);
                ensure!(
                    wrapper
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&prefix)),
                    "The old version folder does not match its source receipt"
                );
            }
        } else if self.game == GameId::Dusklight
            && self.directory.join("twilight-install.json").try_exists()?
        {
            install::previous_release_files(&self.directory, &program)?;
        } else if self.game == GameId::Totk
            && self.directory.join("totk-install.json").try_exists()?
        {
            totk_update::installed_release(&self.directory)?;
        } else {
            ensure!(
                wrapper.is_some(),
                "Cannot verify ownership of {}: no supported installation receipt.",
                self.directory.display()
            );
        }
        source_build::games_stopped()?;
        let retired = wrapper.unwrap_or(&self.directory);
        if !within(&self.destination, retired) {
            if protected.iter().any(|path| within(path, retired)) {
                return Ok(format!(
                    "The previous folder was kept because it is still referenced: {}.",
                    retired.display()
                ));
            }
            let saved = preserve_retired_saves(root, retired, &[], progress)?;
            source_build::games_stopped()?;
            progress(CleanupProgress {
                path: retired,
                bytes: 0,
                removing: true,
            })?;
            crate::post_update::remove_folder(retired)?;
            return Ok(cleanup_summary(retired, saved.as_deref(), 0));
        }
        let mut targets = Vec::new();
        let mut kept = protected;
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let path = entry.path();
            if overlaps_any(&path, &kept)
                || entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| source_version(name, "source-"))
            {
                kept.push(path);
            } else {
                targets.push(path);
            }
        }
        let saved = preserve_retired_saves(root, &self.directory, &kept, progress)?;
        source_build::games_stopped()?;
        for path in targets {
            progress(CleanupProgress {
                path: &path,
                bytes: 0,
                removing: true,
            })?;
            crate::post_update::remove_path(&path)?;
        }
        Ok(cleanup_summary(
            &self.directory,
            saved.as_deref(),
            kept.len(),
        ))
    }
}

fn cleanup_summary(previous: &Path, saved: Option<&Path>, kept: usize) -> String {
    let saved = saved.map_or_else(String::new, |path| {
        format!(
            " Previous save data was verified and retained at {}.",
            path.display()
        )
    });
    let kept = if kept == 0 {
        String::new()
    } else {
        " Active, referenced, and pending-version paths were kept.".into()
    };
    format!(
        "Removed obsolete game files from {} without comparing game assets.{saved}{kept}",
        previous.display()
    )
}

pub(crate) fn preserve_retired_saves(
    root: &Path,
    folder: &Path,
    excluded: &[PathBuf],
    progress: &mut CleanupReporter<'_>,
) -> Result<Option<PathBuf>> {
    ensure_safe_path(folder)?;
    progress(CleanupProgress {
        path: folder,
        bytes: 0,
        removing: false,
    })?;
    let mut installs = vec![folder.to_path_buf(), folder.join("game")];
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| source_version(name, "source-"))
        {
            installs.push(entry.path().join("game"));
        }
    }
    let mut sources = Vec::new();
    for install in installs {
        if excluded.iter().any(|path| within(&install, path)) {
            continue;
        }
        for profile in ["user", "profile"] {
            sources.push(install.join(profile));
        }
        let mut profiles = vec![
            install.clone(),
            install.join("data"),
            install.join("bin").join("data"),
        ];
        for directory in [install.clone(), install.join("bin")] {
            let descriptor = directory.join("data_location.json");
            ensure_safe_path(&descriptor)?;
            if !descriptor.try_exists()? {
                continue;
            }
            let value: serde_json::Value = serde_json::from_slice(&read(&descriptor)?)?;
            ensure!(
                value["version"] == 1
                    && value["mode"]
                        .as_str()
                        .is_some_and(|mode| matches!(mode, "default" | "portable" | "custom")),
                "Cannot locate retired Dusklight saves from {}",
                descriptor.display()
            );
            for key in ["customPath", "previousPath"] {
                if let Some(path) = value[key].as_str().filter(|path| !path.is_empty()) {
                    let path = PathBuf::from(path);
                    ensure_safe_path(&path)?;
                    if within(&path, folder) {
                        profiles.push(path);
                    }
                }
            }
        }
        for profile in profiles {
            for region in ["USA", "EUR", "JAP"] {
                for card in ["A", "B"] {
                    sources.push(profile.join(region).join(format!("Card {card}")));
                    sources.push(profile.join(format!("MemoryCard{card}.{region}.raw")));
                }
            }
        }
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for source in sources {
        if found.iter().any(|parent| source.starts_with(parent)) || overlaps_any(&source, excluded)
        {
            continue;
        }
        ensure_safe_path(&source)?;
        if source.try_exists()? {
            found.retain(|previous| !previous.starts_with(&source));
            found.push(source);
        }
    }
    if found.is_empty() {
        return Ok(None);
    }
    let backups = root.join("retired-saves");
    ensure_safe_path(&backups)?;
    fs::create_dir_all(&backups)?;
    ensure_safe_path(&backups)?;
    let backup = tempfile::Builder::new()
        .prefix("retired-")
        .tempdir_in(&backups)?;
    let mut files = 0;
    let mut bytes = 0;
    for source in found {
        let destination = backup.path().join(source.strip_prefix(folder)?);
        copy_saved_path(&source, &destination, progress, &mut files, &mut bytes, 0)?;
    }
    atomic_write(
        &backup.path().join("source.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "source": folder,
            "saved_files": files,
            "saved_bytes": bytes,
        }))?,
    )?;
    Ok(Some(backup.keep()))
}

fn copy_saved_path(
    source: &Path,
    destination: &Path,
    progress: &mut CleanupReporter<'_>,
    files: &mut usize,
    total: &mut u64,
    depth: usize,
) -> Result<()> {
    progress(CleanupProgress {
        path: source,
        bytes: 0,
        removing: false,
    })?;
    ensure!(depth <= 32, "Retired save data is nested too deeply");
    ensure_safe_path(source)?;
    ensure_safe_path(destination)?;
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_saved_path(
                &entry.path(),
                &destination.join(entry.file_name()),
                progress,
                files,
                total,
                depth + 1,
            )?;
        }
        return Ok(());
    }
    *files += 1;
    *total = total
        .checked_add(metadata.len())
        .context("Retired save size overflow")?;
    ensure!(
        metadata.is_file()
            && *files <= 150_000
            && metadata.len() <= 128 * 1024 * 1024
            && *total <= 256 * 1024 * 1024 * 1024,
        "Retired save data exceeds its backup limit"
    );
    let parent = destination
        .parent()
        .context("Missing retired-save backup folder")?;
    fs::create_dir_all(parent)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    let mut previous = options.open(source)?;
    let mut copy = tempfile::NamedTempFile::new_in(parent)?;
    let mut hash = Sha256::new();
    let mut copied = 0;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let count = previous.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied += count as u64;
        ensure!(copied <= metadata.len(), "Retired save grew during backup");
        copy.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        progress(CleanupProgress {
            path: source,
            bytes: count as u64,
            removing: false,
        })?;
    }
    ensure!(
        copied == metadata.len(),
        "Retired save changed during backup"
    );
    copy.as_file().sync_all()?;
    ensure!(
        install::digest_file(copy.path(), copied)? == hex::encode(hash.finalize()),
        "Retired-save backup verification failed"
    );
    copy.persist_noclobber(destination)
        .with_context(|| format!("Preserving retired save {}", source.display()))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::sha256;
    use serde_json::json;

    fn seed_source(path: &Path, game: GameId, project: &str, executable: &str) -> Result<PathBuf> {
        fs::create_dir_all(path)?;
        let executable = path.join(executable);
        fs::write(&executable, b"old verified program")?;
        fs::write(
            path.join("twilight-source.json"),
            serde_json::to_vec(&json!({
                "version": 1, "game": game, "oot_project": project,
                "tag": "v1.2.3", "commit": "a".repeat(40),
                "sha256": sha256(b"old verified program"),
            }))?,
        )?;
        Ok(executable)
    }

    #[test]
    fn all_games_retire_different_assets_and_preserve_old_saves_without_replacing_current_progress()
    -> Result<()> {
        for (game, project, name, executable, save) in [
            (
                GameId::Dusklight,
                "triforce",
                "Dusklight",
                "dusklight.exe",
                "data\\USA\\Card A\\01-GZ2E-gczelda2.gci",
            ),
            (
                GameId::Totk,
                "triforce",
                "TotKRecomp",
                "totk.exe",
                "profile\\save\\account\\progress.sav",
            ),
            (
                GameId::Triforce,
                "triforce",
                "Triforce",
                "Triforce.exe",
                "user\\save\\account\\progress.sav",
            ),
            (
                GameId::Triforce,
                "longshot",
                "Longshot",
                "longshot.exe",
                "user\\save\\account\\progress.sav",
            ),
        ] {
            let temp = tempfile::tempdir()?;
            let root = temp.path();
            let managed = root.join("Games").join(name);
            let old_wrapper = managed.join("source-v1.2.3-aaaaaaaaaaaa-Abc123");
            let old = old_wrapper.join("game");
            let new = managed
                .join("source-v1.2.4-bbbbbbbbbbbb-Def456")
                .join("game");
            let old_exe = seed_source(&old, game, project, executable)?;
            let new_exe = seed_source(&new, game, project, executable)?;
            fs::write(&old_exe, b"modified old program must not block removal")?;
            for (directory, progress) in [
                (&old, b"old progress".as_slice()),
                (&new, b"current progress"),
            ] {
                let path = directory.join(save);
                fs::create_dir_all(path.parent().context("Missing save parent")?)?;
                fs::write(path, progress)?;
                fs::write(directory.join("assets.bin"), progress)?;
                fs::create_dir(directory.join("mods"))?;
                fs::write(directory.join("mods").join("custom"), progress)?;
            }
            let unrelated = root.join("unmanaged");
            fs::create_dir(&unrelated)?;
            fs::write(unrelated.join("keep"), b"unrelated installation")?;
            let mut read = 0;
            let cleanup = SourceBackup::previous(root, &managed, game, &old_exe, &new_exe, vec![])
                .context("Missing previous managed build")?;
            let detail = cleanup.compact_with_progress(root, &mut |update| {
                if update.bytes > 0 {
                    assert!(!update.path.ends_with("assets.bin"));
                    assert!(!update.path.ends_with(executable));
                    read += update.bytes;
                }
                Ok(())
            })?;
            assert!(!old_wrapper.exists(), "{name}: {detail}");
            assert_eq!(fs::read(new.join(save))?, b"current progress");
            assert_eq!(
                fs::read(new.join("mods").join("custom"))?,
                b"current progress"
            );
            assert!(new_exe.is_file() && unrelated.join("keep").is_file());
            let archive = fs::read_dir(root.join("retired-saves"))?
                .next()
                .context("Missing retired saves")??
                .path();
            assert_eq!(fs::read(archive.join("game").join(save))?, b"old progress");
            assert_eq!(read, b"old progress".len() as u64);
            assert!(detail.contains("without comparing game assets"));
        }
        Ok(())
    }

    #[test]
    fn official_dusklight_and_totk_installs_use_saves_only_retirement() -> Result<()> {
        for (game, project, executable, save) in [
            (
                GameId::Dusklight,
                "Dusklight",
                "dusklight.exe",
                "custom-profile\\MemoryCardA.USA.raw",
            ),
            (
                GameId::Totk,
                "TotKRecomp",
                "totk.exe",
                "profile\\save\\account\\progress.sav",
            ),
        ] {
            let temp = tempfile::tempdir()?;
            let root = temp.path();
            let managed = root.join("Games").join(project);
            let previous = managed.join("v1.2.3-aaaaaaaaaaaa");
            let old = seed_source(&previous, game, "triforce", executable)?;
            fs::remove_file(previous.join("twilight-source.json"))?;
            if game == GameId::Dusklight {
                fs::write(
                    previous.join("data_location.json"),
                    serde_json::to_vec(&json!({
                        "version": 1, "mode": "custom",
                        "customPath": previous.join("custom-profile"),
                    }))?,
                )?;
                fs::write(
                    previous.join("twilight-install.json"),
                    serde_json::to_vec(&json!({
                        "version": 1, "tag": "v1.2.3", "archive_sha256": "a".repeat(64),
                        "executable": executable,
                        "files": [{"path": executable, "size": 20, "sha256": sha256(b"old verified program")}]
                    }))?,
                )?;
            } else {
                fs::write(
                    previous.join("totk-install.json"),
                    serde_json::to_vec(&json!({
                        "format": 1, "release": "v1.2.3", "steps": {"complete": true}
                    }))?,
                )?;
            }
            let path = previous.join(save);
            fs::create_dir_all(path.parent().context("Missing previous save folder")?)?;
            fs::write(path, b"previous progress")?;
            fs::write(previous.join("old-settings.ini"), b"discard old settings")?;
            let next = seed_source(&managed.join("next"), game, "triforce", executable)?;
            SourceBackup::previous(root, &managed, game, &old, &next, vec![])
                .context("Missing previous official installation")?
                .compact(root)?;
            assert!(!previous.exists() && next.is_file());
            let backup = fs::read_dir(root.join("retired-saves"))?
                .next()
                .context("Missing previous save backup")??
                .path();
            assert_eq!(fs::read(backup.join(save))?, b"previous progress");
        }
        Ok(())
    }

    #[test]
    fn save_backup_failure_or_cancellation_keeps_the_old_install() -> Result<()> {
        for cancel in [false, true] {
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
            fs::create_dir_all(old.join("user").join("save"))?;
            let bytes = vec![9; 192 * 1024];
            let save = old.join("user").join("save").join("progress.sav");
            fs::write(&save, &bytes)?;
            if !cancel {
                fs::write(root.join("retired-saves"), b"backup storage is unavailable")?;
            }
            let cleanup = SourceBackup::previous(
                root,
                &managed,
                GameId::Triforce,
                &old_exe,
                &new_exe,
                vec![],
            )
            .context("Missing previous build")?;
            let mut read = 0;
            let error = cleanup
                .compact_with_progress(root, &mut |update| {
                    assert!(
                        !update.removing,
                        "Deletion began before saves were preserved"
                    );
                    read += update.bytes;
                    if cancel && update.bytes > 0 {
                        return Err(source_build::Cancelled.into());
                    }
                    Ok(())
                })
                .err()
                .context("Failed save preservation did not stop removal")?;
            assert_eq!(source_build::is_cancelled(&error), cancel);
            if cancel {
                assert!(read > 0 && read < bytes.len() as u64);
            }
            assert_eq!(fs::read(save)?, bytes);
            assert!(old_exe.is_file() && new_exe.is_file());
        }
        Ok(())
    }

    #[test]
    fn siblings_keep_other_projects_newer_builds_and_referenced_versions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let managed = root.join("Games").join("Longshot");
        let old_wrapper = managed.join("source-v1.2.3-aaaaaaaaaaaa-Abc123");
        let old = old_wrapper.join("game");
        seed_source(&old, GameId::Triforce, "longshot", "longshot.exe")?;
        let active = managed
            .join("source-v1.2.4-bbbbbbbbbbbb-Def456")
            .join("game");
        let active_exe = seed_source(&active, GameId::Triforce, "longshot", "longshot.exe")?;
        let triforce = root.join("Games").join("Triforce");
        let triforce_exe = seed_source(&triforce, GameId::Triforce, "triforce", "Triforce.exe")?;
        std::thread::sleep(std::time::Duration::from_millis(25));
        let newer = managed
            .join("source-v1.2.5-cccccccccccc-Ghi789")
            .join("game");
        let newer_exe = seed_source(&newer, GameId::Triforce, "longshot", "longshot.exe")?;
        fs::write(
            root.join("source-builds.json"),
            serde_json::to_vec(&json!({
                "game_input": old.join("romfs")
            }))?,
        )?;
        SourceBackup::compact_source_siblings(
            root,
            &managed,
            GameId::Triforce,
            &active_exe,
            None,
            &[],
        )?;
        assert!(old.is_dir());
        fs::remove_file(root.join("source-builds.json"))?;
        fs::remove_file(old.join("twilight-source.json"))?;
        fs::remove_file(old.join("longshot.exe"))?;
        SourceBackup::compact_source_siblings(
            root,
            &managed,
            GameId::Triforce,
            &active_exe,
            None,
            &[],
        )?;
        assert!(
            !old_wrapper.exists(),
            "A partially removed old version must be resumable"
        );
        assert!(active_exe.is_file() && newer_exe.is_file() && triforce_exe.is_file());
        Ok(())
    }

    #[test]
    fn save_backup_preserves_files_at_the_supported_nesting_limit() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let install = root.join("old-install");
        let mut folder = install.join("user");
        for _ in 0..31 {
            folder.push("s");
        }
        fs::create_dir_all(&folder)?;
        let save = folder.join("progress.sav");
        fs::write(&save, b"nested saved progress")?;
        let backup = preserve_retired_saves(root, &install, &[], &mut |_| Ok(()))?
            .context("Missing nested save backup")?;
        assert_eq!(
            fs::read(backup.join(save.strip_prefix(&install)?))?,
            b"nested saved progress"
        );
        assert!(save.is_file());
        Ok(())
    }

    #[test]
    fn stable_parent_cleanup_preserves_active_and_pending_version_folders() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let managed = root.join("Games").join("TotKRecomp");
        let old = seed_source(&managed, GameId::Totk, "triforce", "totk.exe")?;
        let active = managed
            .join("source-v1.2.4-bbbbbbbbbbbb-Def456")
            .join("game");
        let next = seed_source(&active, GameId::Totk, "triforce", "totk.exe")?;
        let pending = managed
            .join("source-v1.2.5-cccccccccccc-Ghi789")
            .join("game");
        let pending_exe = seed_source(&pending, GameId::Totk, "triforce", "totk.exe")?;
        fs::write(managed.join("old-assets.bin"), b"old assets")?;
        let cleanup = SourceBackup::previous(root, &managed, GameId::Totk, &old, &next, vec![])
            .context("Missing stable predecessor")?;
        cleanup.compact(root)?;
        assert!(!old.exists() && !managed.join("old-assets.bin").exists());
        assert!(next.is_file() && pending_exe.is_file());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn non_save_assets_need_not_be_readable_during_retirement() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
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
        let asset = old.join("assets.bin");
        fs::write(&asset, b"old unreadable assets")?;
        let _deny_reads = OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_DELETE)
            .open(&asset)?;
        SourceBackup::previous(root, &managed, GameId::Triforce, &old_exe, &new_exe, vec![])
            .context("Missing previous build")?
            .compact(root)?;
        assert!(!old.exists() && new_exe.is_file());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Creates an isolated 4,096-file cleanup benchmark"]
    fn large_file_tree_cleanup_benchmark() -> Result<()> {
        use std::time::Instant;
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetProcessIoCounters, IO_COUNTERS,
        };
        fn counters() -> Result<IO_COUNTERS> {
            let mut counters = IO_COUNTERS::default();
            // The process pseudo-handle and initialized output storage remain valid for this call.
            ensure!(
                unsafe { GetProcessIoCounters(GetCurrentProcess(), &mut counters) } != 0,
                "Reading benchmark I/O counters: {}",
                std::io::Error::last_os_error()
            );
            Ok(counters)
        }
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
        let bytes = vec![37; 16 * 1024];
        for directory in [&old, &new] {
            for group in 0..64 {
                let folder = directory.join("romfs").join(format!("area-{group:02}"));
                fs::create_dir_all(&folder)?;
                for file in 0..64 {
                    fs::write(folder.join(format!("data-{file:02}.bin")), &bytes)?;
                }
            }
        }
        let cleanup =
            SourceBackup::previous(root, &managed, GameId::Triforce, &old_exe, &new_exe, vec![])
                .context("Missing previous benchmark build")?;
        let before = counters()?;
        let started = Instant::now();
        let mut read = 0;
        let summary = cleanup.compact_with_progress(root, &mut |update| {
            read += update.bytes;
            Ok(())
        })?;
        let elapsed = started.elapsed();
        let after = counters()?;
        println!(
            "cleanup benchmark: files=4096, elapsed={elapsed:.3?}, saved_bytes_read={read}, metadata_io={}, read_io={}\n{summary}",
            after.OtherOperationCount - before.OtherOperationCount,
            after.ReadOperationCount - before.ReadOperationCount,
        );
        assert_eq!(
            read, 0,
            "A save-free installation must not read game assets"
        );
        assert!(!old.parent().context("Missing old wrapper")?.exists() && new_exe.is_file());
        Ok(())
    }
}

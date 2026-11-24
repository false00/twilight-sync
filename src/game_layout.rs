mod backup;

use crate::{
    files::{atomic_write, ensure_safe_path, sha256},
    install,
    protocol::GameId,
    source_build,
};
use anyhow::{Context, Result, ensure};
pub(crate) use backup::{
    CleanupProgress, CleanupReporter, SourceBackup, mentions_backup, preserve_retired_saves,
    regular_metadata, retain_cleanup, retained_cleanup, source_version, within,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

const JOURNAL: &str = "game-layout-pending.json";
const MAX_METADATA: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    path: PathBuf,
    before: String,
    after: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Migration {
    version: u8,
    pub game: GameId,
    from: PathBuf,
    pub destination: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
    executable: PathBuf,
    digest: String,
    had_destination: bool,
    updates: Vec<Update>,
}

fn read(path: &Path) -> Result<Vec<u8>> {
    ensure_safe_path(path)?;
    ensure!(
        path.metadata()?.len() <= MAX_METADATA,
        "Game layout metadata exceeds its limit"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_METADATA + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Reading game layout metadata {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_METADATA,
        "Game layout metadata grew beyond its limit"
    );
    Ok(bytes)
}

pub(crate) fn pending(root: &Path) -> Result<bool> {
    let path = root.join(JOURNAL);
    ensure_safe_path(&path)?;
    Ok(path.try_exists()?)
}

fn remap(path: &Path, from: &Path, destination: &Path, backup: &Path) -> PathBuf {
    for (previous, next) in [(from, destination), (destination, backup)] {
        if let Ok(relative) = path.strip_prefix(previous) {
            return if relative.as_os_str().is_empty() {
                next.into()
            } else {
                next.join(relative)
            };
        }
    }
    path.into()
}

fn remap_json(
    value: &mut serde_json::Value,
    from: &Path,
    destination: &Path,
    backup: &Path,
) -> bool {
    let mut changed = false;
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "save_file"
                        | "game_exe"
                        | "texture_profile"
                        | "iso_file"
                        | "game_input"
                        | "update"
                        | "keys"
                        | "firmware"
                        | "install"
                        | "triforce_install"
                        | "longshot_install"
                        | "customPath"
                        | "previousPath"
                        | "backend.isoPath"
                ) && let Some(path) = value.as_str()
                {
                    let mapped = remap(Path::new(path), from, destination, backup);
                    if mapped != Path::new(path) {
                        *value = serde_json::Value::String(mapped.to_string_lossy().into_owned());
                        changed = true;
                    }
                } else {
                    changed |= remap_json(value, from, destination, backup);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for value in items {
                changed |= remap_json(value, from, destination, backup);
            }
        }
        _ => {}
    }
    changed
}

fn rebuild_command(bytes: &[u8], from: &Path, destination: &Path) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes).context("The TotK rebuild command is not UTF-8")?;
    let mut args: Vec<_> = text.lines().map(str::to_owned).collect();
    let positions: Vec<_> = args
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| (arg == "--rebuild").then_some(index))
        .collect();
    ensure!(
        positions.len() == 1,
        "Unsupported TotK rebuild command; the installation was not moved"
    );
    let target = args
        .get_mut(positions[0] + 1)
        .context("TotK rebuild command has no destination")?;
    ensure!(
        Path::new(target) == from,
        "TotK rebuild command belongs to another installation; it was not changed"
    );
    *target = destination
        .to_str()
        .context("The Games path is not valid Unicode")?
        .into();
    Ok((args.join("\n") + "\n").into_bytes())
}

pub(crate) fn metadata_paths(root: &Path) -> [PathBuf; 4] {
    [
        root.join("settings.json"),
        root.join("source-builds.json"),
        root.join("totk-transfers").join("session.json"),
        root.join("triforce-transfers").join("session.json"),
    ]
}

pub(crate) fn prepare(
    root: &Path,
    game: GameId,
    from: &Path,
    executable: &Path,
    destination: &Path,
    profile: Option<&Path>,
    default_profile: &Path,
) -> Result<Option<Migration>> {
    if from == destination {
        return Ok(None);
    }
    let legacy = root.join("apps");
    if !from.starts_with(destination) && !from.starts_with(&legacy) {
        return Ok(None);
    }
    ensure_safe_path(from)?;
    ensure_safe_path(destination)?;
    #[cfg(windows)]
    ensure!(
        from.components().next() == destination.components().next(),
        "The managed game is on a different drive. Automatic folder relocation was deferred; the working installation remains unchanged"
    );
    let mut version = install::VersionCache::default();
    version.refresh(game, Some(from));
    if let Some(error) = version.state.error {
        anyhow::bail!(error);
    }
    if version.state.installed.is_none() {
        return Ok(None);
    }
    ensure!(
        !destination.try_exists()?
            || from.starts_with(destination)
            || destination.join("twilight-source.json").try_exists()?
            || destination.join("twilight-install.json").try_exists()?
            || destination.join("totk-install.json").try_exists()?
            || fs::read_dir(destination)?.next().transpose()?.is_none(),
        "Cannot move the managed game to {}: this folder already contains an unmanaged installation or other files. Nothing was overwritten; move that folder aside, then restart Twilight Sync.",
        destination.display()
    );
    let parent = destination
        .parent()
        .context("The Games folder has no parent")?;
    fs::create_dir_all(parent)?;
    let token = hex::encode(rand::random::<[u8; 16]>());
    let stage = parent.join(format!(".twilight-sync-stage-{token}"));
    let backup = parent.join(format!(".twilight-sync-backup-{token}"));
    let relative = executable.strip_prefix(from)?.to_path_buf();
    ensure!(
        relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_))),
        "Invalid managed game executable"
    );
    let digest = install::digest_file(executable, executable.metadata()?.len())?;
    let mut migration = Migration {
        version: 1,
        game,
        from: from.into(),
        destination: destination.into(),
        stage,
        backup,
        executable: relative,
        digest,
        had_destination: destination.try_exists()?,
        updates: Vec::new(),
    };
    let mut metadata = metadata_paths(root).to_vec();
    if game == GameId::Dusklight {
        metadata.push(from.join("data_location.json"));
        metadata.push(default_profile.join("data_location.json"));
        if let Some(profile) = profile {
            metadata.push(profile.join("config.json"));
        }
    }
    metadata.sort();
    metadata.dedup();
    for path in metadata {
        ensure_safe_path(&path)?;
        if path.try_exists()? {
            let before = read(&path)?;
            let mut value = serde_json::from_slice(&before)?;
            if remap_json(&mut value, from, destination, &migration.backup) {
                migration.updates.push(Update {
                    path: remap(&path, from, destination, &migration.backup),
                    before: sha256(&before),
                    after: serde_json::to_vec_pretty(&value)?,
                });
            }
        }
    }
    if game == GameId::Totk {
        let path = from.join("build-cache").join("rebuild.txt");
        ensure_safe_path(&path)?;
        if path.try_exists()? {
            let before = read(&path)?;
            migration.updates.push(Update {
                path: destination.join("build-cache").join("rebuild.txt"),
                before: sha256(&before),
                after: rebuild_command(&before, from, destination)?,
            });
        }
    }
    let source_receipt = from.join("twilight-source.json");
    if !source_receipt.try_exists()?
        && let Some(parent) = from.parent()
        && parent.join("twilight-source.json").try_exists()?
    {
        atomic_write(
            &source_receipt,
            &read(&parent.join("twilight-source.json"))?,
        )?;
    }
    let bytes = serde_json::to_vec(&migration)?;
    ensure!(
        bytes.len() as u64 <= MAX_METADATA,
        "The game layout recovery record exceeds its limit"
    );
    atomic_write(&root.join(JOURNAL), &bytes)?;
    Ok(Some(migration))
}

pub(crate) fn load(root: &Path) -> Result<Migration> {
    let migration: Migration = serde_json::from_slice(&read(&root.join(JOURNAL))?)?;
    ensure!(
        migration.version == 1,
        "Unsupported game layout recovery record"
    );
    let destination = install::default_directory(migration.game)?;
    ensure!(
        migration.destination == destination,
        "Game layout recovery has an unexpected destination"
    );
    migration.validate()?;
    Ok(migration)
}

impl Migration {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && crate::files::valid_hash(&self.digest)
                && !self.executable.as_os_str().is_empty()
                && self
                    .executable
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
                && self.from != self.destination
                && !self.destination.starts_with(&self.from),
            "Invalid game layout recovery record"
        );
        for (path, prefix) in [
            (&self.stage, ".twilight-sync-stage-"),
            (&self.backup, ".twilight-sync-backup-"),
        ] {
            ensure!(
                path.parent() == self.destination.parent()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name
                            .strip_prefix(prefix)
                            .is_some_and(|token| token.len() == 32
                                && token.bytes().all(|b| b.is_ascii_hexdigit()))),
                "Invalid game layout staging path"
            );
        }
        for path in [&self.from, &self.destination, &self.stage, &self.backup] {
            ensure_safe_path(path)?;
        }
        Ok(())
    }

    pub fn apply(&self, check_idle: impl Fn() -> Result<()>) -> Result<()> {
        self.validate()?;
        check_idle()?;
        ensure!(
            !self.stage.try_exists()? || !self.from.try_exists()?,
            "Both the original and staged game folders exist; automatic recovery stopped without overwriting either"
        );
        if !self.stage.try_exists()? && self.from.try_exists()? {
            ensure!(
                !self.backup.try_exists()?,
                "Game layout changed after staging; automatic recovery stopped"
            );
            fs::rename(&self.from, &self.stage)
                .context("Staging the managed game without copying its data")?;
        }
        if self.stage.try_exists()? {
            if self.had_destination && !self.backup.try_exists()? {
                check_idle()?;
                fs::rename(&self.destination, &self.backup)
                    .context("Retaining the previous Games folder")?;
            }
            ensure!(
                !self.destination.try_exists()?,
                "The destination appeared during migration; it was not overwritten"
            );
            check_idle()?;
            fs::rename(&self.stage, &self.destination)
                .context("Moving the managed game into its stable Games folder")?;
        }
        let executable = self.destination.join(&self.executable);
        ensure!(
            install::digest_file(&executable, executable.metadata()?.len())? == self.digest,
            "The migrated executable changed; recovery is required before activation"
        );
        check_idle()?;
        for update in &self.updates {
            check_idle()?;
            let bytes = read(&update.path)?;
            if bytes != update.after {
                ensure!(
                    sha256(&bytes) == update.before,
                    "Game settings changed during migration; no settings were overwritten"
                );
                atomic_write(&update.path, &update.after)?;
            }
            check_idle()?;
        }
        Ok(())
    }

    pub fn finish(&self, root: &Path) -> Result<()> {
        fs::remove_file(root.join(JOURNAL)).context("Finishing the durable game layout migration")
    }

    pub fn backup(&self) -> Option<&Path> {
        self.had_destination.then_some(self.backup.as_path())
    }

    pub fn source_backup(&self, built: &source_build::Built) -> Option<SourceBackup> {
        let name = self.backup.file_name()?.to_str()?;
        (self.had_destination
            && self.from.starts_with(&self.destination)
            && self.destination.join(&self.executable).is_file()
            && built.game == self.game
            && built.directory == self.from
            && built.executable == self.from.join(&self.executable)
            && built.sha256 == self.digest
            && !self.updates.iter().any(|update| {
                update.path.starts_with(&self.backup)
                    || backup::mentions_backup(&update.after, name)
            }))
        .then(|| SourceBackup {
            directory: self.backup.clone(),
            destination: self.destination.clone(),
            executable: self.executable.clone(),
            game: self.game,
            replacement: None,
            extra_metadata: Vec::new(),
        })
    }

    pub fn mapped_path(&self, path: &Path) -> PathBuf {
        remap(path, &self.from, &self.destination, &self.backup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn official_totk_migration_keeps_saves_and_rebinds_its_session_and_mod_rebuild() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let manager = temp.path().join("manager");
        let destination = temp.path().join("Games").join("TotKRecomp");
        let from = destination.join("release-old").join("game");
        fs::create_dir_all(manager.join("totk-transfers"))?;
        fs::create_dir_all(from.join("profile"))?;
        fs::create_dir_all(from.join("build-cache"))?;
        fs::write(from.join("totk.exe"), b"fixture, never executed")?;
        fs::write(
            from.join("profile").join("save"),
            b"preserved TotK progress",
        )?;
        fs::write(
            from.join("totk-install.json"),
            br#"{"format":1,"release":"v0.4.1","steps":{"done":{}}}"#,
        )?;
        fs::write(
            from.join("build-cache").join("rebuild.txt"),
            format!(
                "C:\\retained setup\\pythonw.exe\n-c\napproved bootstrap\nC:\\retained setup\n--root\nC:\\retained setup\n--rebuild\n{}\n",
                from.display()
            ),
        )?;
        atomic_write(
            &manager.join("totk-transfers").join("session.json"),
            &serde_json::to_vec(&serde_json::json!({"install":from,"base":"b".repeat(64)}))?,
        )?;
        let migration = prepare(
            &manager,
            GameId::Totk,
            &from,
            &from.join("totk.exe"),
            &destination,
            None,
            &temp.path().join("default"),
        )?
        .context("TotK migration was not planned")?;
        assert_eq!(
            migration
                .updates
                .iter()
                .filter(|update| update.path.ends_with("rebuild.txt"))
                .count(),
            1
        );
        migration.apply(|| Ok(()))?;
        assert_eq!(
            fs::read(destination.join("profile").join("save"))?,
            b"preserved TotK progress"
        );
        let rebuild = fs::read_to_string(destination.join("build-cache").join("rebuild.txt"))?;
        assert!(rebuild.ends_with(&format!("--rebuild\n{}\n", destination.display())));
        assert!(rebuild.starts_with("C:\\retained setup\\pythonw.exe\n"));
        let session: serde_json::Value =
            serde_json::from_slice(&read(&manager.join("totk-transfers").join("session.json"))?)?;
        assert_eq!(session["install"], destination.to_string_lossy().as_ref());
        assert_eq!(session["base"], "b".repeat(64));
        assert_eq!(
            crate::totk_update::installed_release(&destination)?,
            "v0.4.1"
        );
        Ok(())
    }

    #[test]
    fn totk_rebuild_keeps_the_pinned_setup_and_updates_only_its_destination() -> Result<()> {
        let from = Path::new(r"C:\Games\TotKRecomp\release-old\game");
        let destination = Path::new(r"C:\Games\TotKRecomp");
        let args = format!(
            "C:\\verified tools\\pythonw.exe\n-c\napproved bootstrap\nC:\\retained setup\n--root\nC:\\retained setup\n--rebuild\n{}\n",
            from.display()
        );
        let updated = String::from_utf8(rebuild_command(args.as_bytes(), from, destination)?)?;
        assert_eq!(
            updated,
            args.replace(
                &from.display().to_string(),
                &destination.display().to_string()
            )
        );
        assert!(rebuild_command(b"unrecognized command", from, destination).is_err());
        assert!(
            rebuild_command(
                b"python\n--rebuild\nC:\\a different game\n",
                from,
                destination
            )
            .is_err()
        );
        Ok(())
    }

    fn setup(root: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
        let manager = root.join("manager");
        let destination = root.join("Games").join("Triforce");
        let from = destination.join("source-v1-old").join("game");
        fs::create_dir_all(&manager)?;
        fs::create_dir_all(&from)?;
        fs::create_dir_all(from.join("user"))?;
        fs::create_dir_all(from.join("mods"))?;
        fs::write(
            from.join("Triforce.exe"),
            b"verified fixture, never executed",
        )?;
        fs::write(from.join("user").join("save"), b"progress")?;
        fs::write(from.join("mods").join("custom"), b"user modification")?;
        atomic_write(
            &from
                .parent()
                .context("Missing fixture parent")?
                .join("twilight-source.json"),
            &serde_json::to_vec(&serde_json::json!({
                "version":1,"game":GameId::Triforce,"tag":"v1.0.0",
                "commit":"a".repeat(40),"sha256":sha256(b"verified fixture, never executed")
            }))?,
        )?;
        fs::create_dir_all(destination.join("Game"))?;
        fs::write(
            destination.join("Game").join("input.nup"),
            b"local game input",
        )?;
        atomic_write(
            &manager.join("source-builds.json"),
            &serde_json::to_vec(&serde_json::json!({
                "triforce": { "game_input": destination.join("Game").join("input.nup") }
            }))?,
        )?;
        atomic_write(
            &manager.join("settings.json"),
            &serde_json::to_vec(&serde_json::json!({
                "game_exe":from.join("Triforce.exe"),"sync_base":"b".repeat(64)
            }))?,
        )?;
        Ok((manager, destination, from))
    }

    fn plan(root: &Path, manager: &Path, destination: &Path, from: &Path) -> Result<Migration> {
        prepare(
            manager,
            GameId::Triforce,
            from,
            &from.join("Triforce.exe"),
            destination,
            None,
            &root.join("profile"),
        )?
        .context("The managed installation was not recognized")
    }

    fn staged_source(from: &Path) -> source_build::Built {
        source_build::Built {
            game: GameId::Triforce,
            executable: from.join("Triforce.exe"),
            directory: from.into(),
            tag: "v1.0.0".into(),
            commit: "a".repeat(40),
            sha256: sha256(b"verified fixture, never executed"),
            source: None,
            log: from.join("unused.log"),
            expected_save: None,
        }
    }

    fn seed_previous_source(destination: &Path) -> Result<()> {
        let program = b"old fixture, never executed";
        fs::write(destination.join("Triforce.exe"), program)?;
        atomic_write(
            &destination.join("twilight-source.json"),
            &serde_json::to_vec(&serde_json::json!({
                "version":1,"game":GameId::Triforce,"tag":"v0.9.9",
                "commit":"b".repeat(40),"sha256":sha256(program)
            }))?,
        )?;
        fs::write(
            destination.join("triforce_programs.bin"),
            b"regenerable previous shader cache",
        )?;
        Ok(())
    }

    #[test]
    fn source_promotion_preserves_saves_and_removes_all_old_non_save_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (manager, destination, from) = setup(temp.path())?;
        fs::remove_file(manager.join("source-builds.json"))?;
        seed_previous_source(&destination)?;
        let old_romfs = destination.join("romfs");
        let new_romfs = from.join("romfs");
        fs::create_dir(&old_romfs)?;
        fs::create_dir(&new_romfs)?;
        fs::write(old_romfs.join("copy.bin"), b"same game data")?;
        fs::write(new_romfs.join("copy.bin"), b"same game data")?;
        fs::write(old_romfs.join("different.bin"), b"old")?;
        fs::write(new_romfs.join("different.bin"), b"new")?;
        fs::write(old_romfs.join("old-only.bin"), b"unique old data")?;
        fs::write(old_romfs.join("shared.bin"), b"hard-linked game data")?;
        fs::hard_link(old_romfs.join("shared.bin"), new_romfs.join("shared.bin"))?;
        fs::create_dir_all(destination.join("user"))?;
        fs::write(
            destination.join("user").join("earlier-save"),
            b"older progress",
        )?;
        fs::create_dir_all(destination.join("mods"))?;
        fs::write(destination.join("mods").join("mod"), b"old mod")?;
        fs::create_dir(destination.join("mkw_vk_pipelines.bin"))?;
        fs::write(
            destination.join("mkw_vk_pipelines.bin").join("extra"),
            b"not a regular cache file",
        )?;

        let migration = plan(temp.path(), &manager, &destination, &from)?;
        let old = migration
            .source_backup(&staged_source(&from))
            .context("Expected an in-session source promotion")?;
        migration.apply(|| Ok(()))?;
        assert!(old.compact(&manager).is_err());
        assert!(old.directory.join("Triforce.exe").is_file());
        migration.finish(&manager)?;
        old.compact(&manager)?;

        assert!(!old.directory.exists());
        let archive = fs::read_dir(manager.join("retired-saves"))?
            .next()
            .context("Missing previous save backup")??
            .path();
        assert_eq!(
            fs::read(archive.join("user").join("earlier-save"))?,
            b"older progress"
        );
        assert_eq!(
            fs::read(destination.join("romfs\\copy.bin"))?,
            b"same game data"
        );
        assert_eq!(
            fs::read(destination.join("romfs\\shared.bin"))?,
            b"hard-linked game data"
        );
        assert_eq!(fs::read(destination.join("user\\save"))?, b"progress");
        assert_eq!(
            fs::read(destination.join("Triforce.exe"))?,
            b"verified fixture, never executed"
        );
        Ok(())
    }

    #[test]
    fn source_backup_keeps_referenced_folders_but_does_not_compare_old_programs() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (manager, destination, from) = setup(temp.path())?;
        fs::remove_file(manager.join("source-builds.json"))?;
        seed_previous_source(&destination)?;
        let migration = plan(temp.path(), &manager, &destination, &from)?;
        let old = migration
            .source_backup(&staged_source(&from))
            .context("Expected an in-session source promotion")?;
        migration.apply(|| Ok(()))?;
        migration.finish(&manager)?;
        atomic_write(
            &manager.join("source-builds.json"),
            &serde_json::to_vec(&serde_json::json!({
                "triforce": { "game_input": old.directory.join("Game\\input.nup") }
            }))?,
        )?;
        assert!(old.compact(&manager)?.contains("still referenced"));
        assert!(old.directory.join("Triforce.exe").is_file());
        assert!(old.directory.join("triforce_programs.bin").is_file());
        fs::remove_file(manager.join("source-builds.json"))?;
        fs::write(
            old.directory.join("Triforce.exe"),
            b"locally modified executable",
        )?;
        old.compact(&manager)?;
        assert!(!old.directory.exists());
        assert!(destination.join("Triforce.exe").is_file());
        Ok(())
    }

    #[test]
    fn nested_install_is_promoted_and_all_other_data_and_input_paths_survive() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (manager, destination, from) = setup(temp.path())?;
        let migration = plan(temp.path(), &manager, &destination, &from)?;
        assert!(migration.source_backup(&staged_source(&from)).is_none());
        migration.apply(|| Ok(()))?;
        assert_eq!(
            fs::read(destination.join("user").join("save"))?,
            b"progress"
        );
        assert_eq!(
            fs::read(destination.join("mods").join("custom"))?,
            b"user modification"
        );
        assert!(destination.join("twilight-source.json").is_file());
        let backup = migration.backup().context("Missing preserved container")?;
        assert_eq!(
            fs::read(backup.join("Game").join("input.nup"))?,
            b"local game input"
        );
        let settings: serde_json::Value =
            serde_json::from_slice(&read(&manager.join("settings.json"))?)?;
        assert_eq!(
            settings["game_exe"],
            destination.join("Triforce.exe").to_string_lossy().as_ref()
        );
        assert_eq!(settings["sync_base"], "b".repeat(64));
        let preferences: serde_json::Value =
            serde_json::from_slice(&read(&manager.join("source-builds.json"))?)?;
        assert_eq!(
            preferences["triforce"]["game_input"],
            backup
                .join("Game")
                .join("input.nup")
                .to_string_lossy()
                .as_ref()
        );
        migration.apply(|| Ok(()))?;
        migration.finish(&manager)?;
        assert!(!pending(&manager)?);
        Ok(())
    }

    #[test]
    fn interrupted_renames_resume_without_overwriting_data_or_new_settings() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (manager, destination, from) = setup(temp.path())?;
        let migration = plan(temp.path(), &manager, &destination, &from)?;
        let calls = Cell::new(0);
        assert!(
            migration
                .apply(|| {
                    calls.set(calls.get() + 1);
                    ensure!(calls.get() < 3, "Simulated interruption");
                    Ok(())
                })
                .is_err()
        );
        assert!(pending(&manager)?);
        let recovered: Migration = serde_json::from_slice(&read(&manager.join(JOURNAL))?)?;
        recovered.apply(|| Ok(()))?;
        assert_eq!(
            fs::read(destination.join("user").join("save"))?,
            b"progress"
        );
        let changed = br#"{"game_exe":"a user changed this","sync_base":null}"#;
        atomic_write(&manager.join("settings.json"), changed)?;
        assert!(recovered.apply(|| Ok(())).is_err());
        assert_eq!(read(&manager.join("settings.json"))?, changed);
        assert!(pending(&manager)?);
        Ok(())
    }

    #[test]
    fn unmanaged_destinations_and_busy_games_are_never_moved() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (manager, destination, nested) = setup(temp.path())?;
        assert!(
            prepare(
                &manager,
                GameId::Triforce,
                Path::new(r"Z:\unmanaged-game"),
                Path::new(r"Z:\unmanaged-game\Triforce.exe"),
                &destination,
                None,
                &temp.path().join("profile"),
            )?
            .is_none()
        );
        let from = manager.join("apps").join("managed");
        fs::create_dir_all(from.parent().context("Missing apps parent")?)?;
        fs::rename(&nested, &from)?;
        fs::copy(
            nested
                .parent()
                .context("Missing receipt parent")?
                .join("twilight-source.json"),
            from.join("twilight-source.json"),
        )?;
        assert!(plan(temp.path(), &manager, &destination, &from).is_err());
        assert!(from.join("Triforce.exe").is_file());
        assert!(!pending(&manager)?);
        let other = temp.path().join("Games").join("OtherGame");
        let migration = plan(temp.path(), &manager, &other, &from)?;
        assert!(
            migration
                .apply(|| anyhow::bail!("A game is running"))
                .is_err()
        );
        assert!(from.join("Triforce.exe").is_file());
        assert!(!other.try_exists()?);
        fs::create_dir(&migration.stage)?;
        assert!(migration.apply(|| Ok(())).is_err());
        assert!(from.join("Triforce.exe").is_file());
        assert!(!other.try_exists()?);
        Ok(())
    }
}

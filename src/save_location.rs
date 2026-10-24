use crate::files::{self, SaveIdentity, ensure_safe_path};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

pub const SAVE_NAME: &str = "gczelda2";
const GAMES: [&str; 6] = ["GZ2E", "GZ2J", "GZ2P", "RZDE", "RZDJ", "RZDP"];
const DESCRIPTOR_LIMIT: u64 = 16 * 1024;
const CONFIG_LIMIT: u64 = 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Default,
    Portable,
    Custom,
}

#[derive(Deserialize)]
struct Descriptor {
    version: u8,
    mode: Mode,
    #[serde(rename = "customPath")]
    custom_path: Option<PathBuf>,
    #[serde(rename = "previousPath")]
    previous_path: Option<PathBuf>,
}

pub fn default_profile() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let base =
            directories::BaseDirs::new().context("Windows roaming app data is unavailable")?;
        let path = base.data_dir().join("TwilitRealm").join("Dusklight");
        ensure_safe_path(&path)?;
        Ok(path)
    }
    #[cfg(not(windows))]
    bail!("Automatic Dusklight save discovery currently requires Windows");
}

fn read_json(path: &Path, limit: u64) -> Result<Option<Value>> {
    ensure_safe_path(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Opening {}", path.display())),
    };
    ensure!(
        file.metadata()?.is_file(),
        "Profile metadata must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Dusklight profile metadata exceeds its size limit"
    );
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("Reading Dusklight profile metadata {}", path.display()))?;
    ensure!(
        value.is_object(),
        "Dusklight profile metadata must be an object"
    );
    Ok(Some(value))
}

pub fn profile(default: &Path, executable: Option<&Path>) -> Result<PathBuf> {
    ensure_safe_path(default)?;
    let executable_directory = executable
        .map(|path| {
            ensure_safe_path(path)?;
            path.parent()
                .context("Dusklight executable has no directory")
        })
        .transpose()?;
    let mut descriptors = Vec::new();
    if let Some(directory) = executable_directory {
        descriptors.push(directory.join("data_location.json"));
    }
    descriptors.push(default.join("data_location.json"));
    descriptors.push(
        default
            .parent()
            .context("Dusklight preferences have no parent")?
            .join("Dusk")
            .join("data_location.json"),
    );
    let mut active = default.to_path_buf();
    let mut previous = None;
    for path in descriptors {
        let Some(value) = read_json(&path, DESCRIPTOR_LIMIT)? else {
            continue;
        };
        let descriptor: Descriptor = serde_json::from_value(value).with_context(|| {
            format!(
                "Invalid Dusklight data-folder descriptor {}",
                path.display()
            )
        })?;
        ensure!(
            descriptor.version == 1,
            "Unsupported Dusklight data-folder descriptor version"
        );
        previous = descriptor.previous_path;
        active = match descriptor.mode {
            Mode::Default => default.to_path_buf(),
            Mode::Portable => executable_directory
                .context("Locate or install Dusklight so its portable profile can be found automatically")?
                .join("data"),
            Mode::Custom => descriptor.custom_path
                .context("Dusklight's custom profile descriptor has no customPath")?,
        };
        break;
    }
    ensure_safe_path(&active)
        .context("Dusklight's active profile must be a safe local directory")?;
    if let Some(previous) = previous
        && !previous.as_os_str().is_empty()
    {
        ensure!(
            !unmigrated_saves(&previous, &active)?,
            "Dusklight has saves waiting for its own profile migration. Open Dusklight normally once to finish that setup; Sync will not move or overwrite those files"
        );
    }
    let legacy = default
        .parent()
        .context("Dusklight preferences have no parent")?
        .join("Dusk");
    ensure!(
        !unmigrated_saves(&legacy, &active)?,
        "An older Dusk profile has saves waiting for Dusklight's own migration. Open Dusklight normally once to finish its setup; Sync will not treat those saves as an empty profile"
    );
    if active.try_exists()? {
        ensure!(
            active.is_dir(),
            "Dusklight's active profile is not a directory"
        );
    }
    if let Some(config) = read_json(&active.join("config.json"), CONFIG_LIMIT)?
        && let Some(kind) = config.get("backend.cardFileType")
    {
        ensure!(
            kind.as_u64() == Some(1),
            "Dusklight is using raw memory-card storage. Automatic sync requires its standard GCI-folder storage; no raw card or existing save was changed"
        );
    }
    Ok(active)
}

pub fn path_for_identity(profile: &Path, identity: &SaveIdentity) -> Result<PathBuf> {
    ensure!(
        identity.name == SAVE_NAME && GAMES.contains(&identity.game.as_str()),
        "Automatic sync uses Dusklight's standard gczelda2 save, not an alternate named save"
    );
    let region = match identity.game.as_bytes()[3] {
        b'E' => "USA",
        b'J' => "JAP",
        b'P' => "EUR",
        _ => bail!("Unsupported Dusklight save region"),
    };
    let path = profile
        .join(region)
        .join("Card A")
        .join(format!("01-{}-{SAVE_NAME}.gci", identity.game));
    ensure_safe_path(&path)?;
    Ok(path)
}

pub fn discover(profile: &Path) -> Result<PathBuf> {
    let mut found = None;
    for game in GAMES {
        let path = path_for_identity(
            profile,
            &SaveIdentity {
                game: game.into(),
                name: SAVE_NAME.into(),
            },
        )?;
        if path.try_exists()? {
            ensure!(
                path.is_file(),
                "Dusklight's default save is not a regular file"
            );
            ensure!(
                found.is_none(),
                "Multiple standard regional Dusklight saves exist in the active profile. Automatic sync is paused rather than guessing which save to replace"
            );
            found = Some(path);
        }
    }
    match found {
        Some(path) => Ok(path),
        None => path_for_identity(
            profile,
            &SaveIdentity {
                game: "GZ2E".into(),
                name: SAVE_NAME.into(),
            },
        ),
    }
}

fn unmigrated_saves(source: &Path, target: &Path) -> Result<bool> {
    if source == target {
        return Ok(false);
    }
    for game in GAMES {
        let identity = SaveIdentity {
            game: game.into(),
            name: SAVE_NAME.into(),
        };
        if path_for_identity(source, &identity)?.try_exists()?
            && !path_for_identity(target, &identity)?.try_exists()?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn prepare(path: &Path) -> Result<()> {
    ensure_safe_path(path)?;
    let parent = path
        .parent()
        .context("Automatic save has no parent directory")?;
    fs::create_dir_all(parent).context("Creating Dusklight's standard Card A directory")?;
    ensure_safe_path(path)?;
    Ok(())
}

pub fn validate_at_path(profile: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let identity = files::validate_save(bytes)?;
    ensure!(
        path_for_identity(profile, &identity)? == path,
        "Save identity does not match Dusklight's automatically resolved default filename"
    );
    Ok(())
}

pub fn hash(profile: &Path, path: &Path) -> Result<Option<String>> {
    ensure_safe_path(path)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    ensure!(
        path.parent().is_some_and(|parent| parent.is_dir()),
        "Automatic save directory is unavailable"
    );
    let bytes = files::read_save(path)?;
    validate_at_path(profile, path, &bytes)?;
    Ok(Some(files::sha256(&bytes)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    fn write_json(path: &Path, value: Value) -> Result<()> {
        fs::create_dir_all(path.parent().context("No test parent")?)?;
        fs::write(path, serde_json::to_vec(&value)?)?;
        Ok(())
    }

    pub(crate) fn stock_save(game: &str) -> Vec<u8> {
        let mut bytes = files::fixture_save();
        bytes[..4].copy_from_slice(game.as_bytes());
        bytes[8..40].fill(0);
        bytes[8..8 + SAVE_NAME.len()].copy_from_slice(SAVE_NAME.as_bytes());
        bytes
    }

    #[test]
    fn first_run_prepares_the_real_stock_name_without_creating_a_fake_save() -> Result<()> {
        let root = tempfile::tempdir()?;
        let default = root.path().join("TwilitRealm").join("Dusklight");
        assert_eq!(profile(&default, None)?, default);
        let path = discover(&default)?;
        assert_eq!(
            path,
            default
                .join("USA")
                .join("Card A")
                .join("01-GZ2E-gczelda2.gci")
        );
        prepare(&path)?;
        assert!(path.parent().context("No Card A")?.is_dir());
        assert!(!path.exists());
        assert_eq!(hash(&default, &path)?, None);
        Ok(())
    }

    #[test]
    fn follows_official_descriptor_precedence_and_preserves_profile_settings() -> Result<()> {
        let root = tempfile::tempdir()?;
        let default = root.path().join("TwilitRealm").join("Dusklight");
        let executable = root.path().join("game").join("dusklight.exe");
        let custom = root.path().join("my-game-profile");
        write_json(
            &default.join("data_location.json"),
            json!({"version":1,"mode":"custom","customPath":custom}),
        )?;
        assert_eq!(profile(&default, Some(&executable))?, custom);
        let descriptor = executable
            .parent()
            .context("No game directory")?
            .join("data_location.json");
        write_json(
            &descriptor,
            json!({"version":1,"mode":"portable","previousPath":custom}),
        )?;
        let portable = executable
            .parent()
            .context("No game directory")?
            .join("data");
        let config = json!({"backend.cardFileType":1,"video.enableVsync":false});
        write_json(&portable.join("config.json"), config.clone())?;
        assert_eq!(profile(&default, Some(&executable))?, portable);
        assert_eq!(
            read_json(&portable.join("config.json"), CONFIG_LIMIT)?,
            Some(config)
        );
        write_json(&descriptor, json!({"version":1,"mode":"default"}))?;
        assert_eq!(profile(&default, Some(&executable))?, default);
        Ok(())
    }

    #[test]
    fn legacy_descriptor_is_followed_but_no_files_are_migrated() -> Result<()> {
        let root = tempfile::tempdir()?;
        let default = root.path().join("Dusklight");
        let custom = root.path().join("existing");
        write_json(
            &root.path().join("Dusk").join("data_location.json"),
            json!({"version":1,"mode":"custom","customPath":custom}),
        )?;
        let marker = custom.join("keep-my-file.txt");
        fs::create_dir_all(&custom)?;
        fs::write(&marker, b"untouched")?;
        assert_eq!(profile(&default, None)?, custom);
        assert_eq!(fs::read(marker)?, b"untouched");
        assert!(!default.exists());
        Ok(())
    }

    #[test]
    fn invalid_or_unsupported_profiles_do_not_silently_fall_back() -> Result<()> {
        let root = tempfile::tempdir()?;
        let default = root.path().join("Dusklight");
        for value in [
            json!({"version":2,"mode":"default"}),
            json!({"version":1,"mode":"unknown"}),
            json!({"version":1,"mode":"portable"}),
            json!({"version":1,"mode":"custom","customPath":"relative-folder"}),
            json!({"version":1,"mode":"custom"}),
        ] {
            write_json(&default.join("data_location.json"), value)?;
            assert!(profile(&default, None).is_err());
        }
        fs::write(
            default.join("data_location.json"),
            vec![b' '; DESCRIPTOR_LIMIT as usize + 1],
        )?;
        assert!(profile(&default, None).is_err());
        write_json(
            &default.join("data_location.json"),
            json!({"version":1,"mode":"default"}),
        )?;
        write_json(
            &default.join("config.json"),
            json!({"backend.cardFileType":0}),
        )?;
        assert!(profile(&default, None).is_err());
        Ok(())
    }

    #[test]
    fn finds_all_stock_regions_and_never_chooses_between_multiple_saves() -> Result<()> {
        let root = tempfile::tempdir()?;
        let default = root.path().join("Dusklight");
        for game in GAMES {
            let bytes = stock_save(game);
            let identity = files::validate_save(&bytes)?;
            let path = path_for_identity(&default, &identity)?;
            prepare(&path)?;
            fs::write(&path, &bytes)?;
            assert_eq!(discover(&default)?, path);
            assert_eq!(hash(&default, &path)?, Some(files::sha256(&bytes)));
            fs::remove_file(path)?;
        }
        for game in ["GZ2E", "GZ2P"] {
            let path = path_for_identity(&default, &files::validate_save(&stock_save(game))?)?;
            prepare(&path)?;
            fs::write(path, stock_save(game))?;
        }
        assert!(discover(&default).is_err());
        Ok(())
    }

    #[test]
    fn pending_native_migrations_are_reported_without_moving_any_data() -> Result<()> {
        let root = tempfile::tempdir()?;
        let default = root.path().join("Dusklight");
        let previous = root.path().join("previous-profile");
        let bytes = stock_save("GZ2E");
        let path = path_for_identity(&previous, &files::validate_save(&bytes)?)?;
        prepare(&path)?;
        fs::write(&path, &bytes)?;
        write_json(
            &default.join("data_location.json"),
            json!({
                "version":1,"mode":"default","previousPath":previous
            }),
        )?;
        let error = profile(&default, None)
            .err()
            .context("Pending migration was hidden")?;
        assert!(format!("{error:#}").contains("migration"));
        assert_eq!(fs::read(&path)?, bytes);
        assert!(!discover(&default)?.exists());
        Ok(())
    }

    #[test]
    fn corrupt_or_renamed_saves_are_not_treated_as_an_empty_destination() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = discover(root.path())?;
        prepare(&path)?;
        fs::write(&path, files::fixture_save())?;
        assert_eq!(discover(root.path())?, path);
        assert!(hash(root.path(), &path).is_err());
        fs::write(&path, b"corrupt")?;
        assert!(hash(root.path(), &path).is_err());
        assert_eq!(fs::read(path)?, b"corrupt");
        Ok(())
    }
}

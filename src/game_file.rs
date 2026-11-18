use crate::{
    disc,
    files::{atomic_write, ensure_safe_path},
    protocol::GameId,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

pub const MAX_GAME_FILE: u64 = 64 * 1024 * 1024 * 1024;
const MAX_KEYS: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    OotNup,
    TotkBase,
    TotkUpdate,
    TotkKeys,
}

impl Kind {
    pub fn game(self) -> GameId {
        match self {
            Self::OotNup => GameId::Triforce,
            _ => GameId::Totk,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OotNup => "Ocarina of Time game (.nup)",
            Self::TotkBase => "Tears of the Kingdom base game (.nsp)",
            Self::TotkUpdate => "Tears of the Kingdom update (.nsp)",
            Self::TotkKeys => "Console keys (prod.keys)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::OotNup => "nup",
            Self::TotkBase | Self::TotkUpdate => "nsp",
            Self::TotkKeys => "keys",
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Self::OotNup => "Ocarina of Time.nup",
            Self::TotkBase => "Tears of the Kingdom.nsp",
            Self::TotkUpdate => "Tears of the Kingdom update.nsp",
            Self::TotkKeys => "prod.keys",
        }
    }

    pub fn matches_path(self, path: &Path) -> bool {
        if self == Self::TotkKeys {
            return path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("prod.keys"));
        }
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case(self.extension()))
    }

    pub fn validate_size(self, size: u64) -> Result<()> {
        let (minimum, maximum) = match self {
            Self::OotNup => (0xb0, MAX_GAME_FILE),
            Self::TotkBase | Self::TotkUpdate => (0x10, MAX_GAME_FILE),
            Self::TotkKeys => (1, MAX_KEYS),
        };
        ensure!(
            (minimum..=maximum).contains(&size),
            "Unsupported {} size",
            self.label()
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Iso,
    Rvz,
    Game(Kind),
}

impl From<disc::Format> for Format {
    fn from(value: disc::Format) -> Self {
        match value {
            disc::Format::Iso => Self::Iso,
            disc::Format::Rvz => Self::Rvz,
        }
    }
}

impl Format {
    pub fn audit_hash(self, hash: &str) -> Option<&str> {
        (self != Self::Game(Kind::TotkKeys)).then_some(hash)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Iso => "ISO",
            Self::Rvz => "RVZ",
            Self::Game(kind) => kind.label(),
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Iso => "iso",
            Self::Rvz => "rvz",
            Self::Game(kind) => kind.extension(),
        }
    }

    pub fn validate_size(self, size: u64) -> Result<()> {
        match self {
            Self::Iso => disc::Format::Iso.validate_size(size),
            Self::Rvz => disc::Format::Rvz.validate_size(size),
            Self::Game(kind) => kind.validate_size(size),
        }
    }

    pub fn matches_path(self, path: &Path) -> bool {
        match self {
            Self::Game(kind) => kind.matches_path(path),
            _ => path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case(self.extension())),
        }
    }
}

pub struct Verified {
    pub format: Format,
    pub sha256: String,
    pub size: u64,
}

impl From<disc::Verified> for Verified {
    fn from(value: disc::Verified) -> Self {
        Self {
            format: value.format.into(),
            sha256: value.sha256,
            size: value.size,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    pub oot: bool,
    pub totk: bool,
    pub keys: bool,
}

impl Permissions {
    pub fn allows(&self, kind: Kind) -> bool {
        match kind {
            Kind::OotNup => self.oot,
            Kind::TotkBase | Kind::TotkUpdate => self.totk,
            Kind::TotkKeys => self.keys,
        }
    }

    pub fn set(&mut self, kind: Kind, enabled: bool) {
        match kind {
            Kind::OotNup => self.oot = enabled,
            Kind::TotkBase | Kind::TotkUpdate => self.totk = enabled,
            Kind::TotkKeys => self.keys = enabled,
        }
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("game-file-sharing.json");
        ensure_safe_path(&path)?;
        if !path.try_exists()? {
            return Ok(Self::default());
        }
        ensure!(
            path.metadata()?.len() <= 4096,
            "Game-file permissions are too large"
        );
        serde_json::from_slice(&fs::read(path)?).context("Reading game-file transfer permissions")
    }

    pub fn persist(&self, root: &Path) -> Result<()> {
        atomic_write(
            &root.join("game-file-sharing.json"),
            &serde_json::to_vec(self)?,
        )
    }
}

pub fn verify(path: &Path, kind: Kind, cancel: &AtomicBool) -> Result<Verified> {
    ensure_safe_path(path)?;
    ensure!(kind.matches_path(path), "Choose {}", kind.label());
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Choose a regular local game file");
    let size = metadata.len();
    kind.validate_size(size)?;
    validate_container(&mut file, kind, size)?;
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut sha = Sha256::new();
    let mut buffer = zeroize::Zeroizing::new(vec![0; 1024 * 1024]);
    let mut total = 0_u64;
    loop {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "Game-file verification cancelled"
        );
        let count = file.read(&mut buffer[..])?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= size, "Game file changed during verification");
        sha.update(&buffer[..count]);
    }
    ensure!(total == size, "Game file changed during verification");
    Ok(Verified {
        format: Format::Game(kind),
        sha256: hex::encode(sha.finalize()),
        size,
    })
}

pub fn validate_container(file: &mut File, kind: Kind, size: u64) -> Result<()> {
    kind.validate_size(size)?;
    if kind == Kind::TotkKeys {
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        file.take(MAX_KEYS + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 == size, "Console key file changed");
        let text = std::str::from_utf8(&bytes).context("Console keys must be a text file")?;
        let mut count = 0;
        for line in text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let (name, value) = line
                .split_once('=')
                .context("Invalid console key file structure")?;
            let (name, value) = (name.trim(), value.trim());
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    && (2..=8192).contains(&value.len())
                    && value.len().is_multiple_of(2)
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid console key file structure"
            );
            count += 1;
        }
        ensure!(count > 0, "Console key file contains no keys");
        return Ok(());
    }
    let mut header = [0; 0x10];
    file.read_exact(&mut header)?;
    if kind == Kind::OotNup {
        ensure!(
            &header[..4] == b"NUP\x01",
            "The offered OOT file is not a NUP container"
        );
        let mut rest = [0; 0xa0];
        file.read_exact(&mut rest)?;
        let declared = u64::from_le_bytes(rest[0x30..0x38].try_into()?);
        ensure!(
            declared == size && &rest[0x70..0x74] == b"TOC\x01",
            "The NUP container is incomplete or has an invalid header"
        );
    } else {
        ensure!(
            &header[..4] == b"PFS0",
            "The offered TotK file is not an NSP container"
        );
        let count = u32::from_le_bytes(header[4..8].try_into()?) as u64;
        let strings = u32::from_le_bytes(header[8..12].try_into()?) as u64;
        let table_size = count * 24;
        let metadata_size = 16 + table_size + strings;
        ensure!(
            count > 0 && metadata_size <= 16 * 1024 * 1024 && metadata_size < size,
            "Unsupported or truncated NSP file table"
        );
        let mut table = vec![0; (table_size + strings) as usize];
        file.read_exact(&mut table)?;
        let names = &table[table_size as usize..];
        let mut has_nca = false;
        for entry in table[..table_size as usize].as_chunks::<24>().0 {
            let offset = u64::from_le_bytes(entry[..8].try_into()?);
            let length = u64::from_le_bytes(entry[8..16].try_into()?);
            let name_at = u32::from_le_bytes(entry[16..20].try_into()?) as usize;
            ensure!(
                offset <= size - metadata_size && length <= size - metadata_size - offset,
                "NSP entry exceeds the container"
            );
            let name = names.get(name_at..).context("Invalid NSP name offset")?;
            let end = name
                .iter()
                .position(|byte| *byte == 0)
                .context("Unterminated NSP file name")?;
            has_nca |= name[..end].ends_with(b".nca");
        }
        ensure!(has_nca, "NSP container contains no console content");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture(kind: Kind) -> Vec<u8> {
        match kind {
            Kind::OotNup => {
                let mut bytes = vec![0; 512];
                bytes[..4].copy_from_slice(b"NUP\x01");
                bytes[0x40..0x48].copy_from_slice(&512_u64.to_le_bytes());
                bytes[0x80..0x84].copy_from_slice(b"TOC\x01");
                bytes
            }
            Kind::TotkBase | Kind::TotkUpdate => {
                let name = b"synthetic.nca\0";
                let mut bytes = vec![0; 16 + 24 + name.len() + 1];
                bytes[..4].copy_from_slice(b"PFS0");
                bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
                bytes[8..12].copy_from_slice(&(name.len() as u32).to_le_bytes());
                bytes[24..32].copy_from_slice(&1_u64.to_le_bytes());
                bytes[40..40 + name.len()].copy_from_slice(name);
                bytes
            }
            Kind::TotkKeys => format!(
                "# Synthetic test fixture\nsynthetic_key = {}\n",
                "00".repeat(16)
            )
            .into_bytes(),
        }
    }

    #[test]
    fn game_file_validation_rejects_bad_containers_and_truncated_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        for kind in [
            Kind::OotNup,
            Kind::TotkBase,
            Kind::TotkUpdate,
            Kind::TotkKeys,
        ] {
            let path = directory.path().join(kind.file_name());
            let mut bytes = fixture(kind);
            fs::write(&path, &bytes)?;
            let verified = verify(&path, kind, &AtomicBool::new(false))?;
            assert_eq!(verified.sha256, crate::files::sha256(&bytes));
            bytes[0] = b'!';
            if kind == Kind::TotkKeys {
                bytes = b"not a key file".to_vec();
            }
            fs::write(&path, &bytes)?;
            assert!(verify(&path, kind, &AtomicBool::new(false)).is_err());
        }
        let nup = directory.path().join("short.nup");
        fs::write(&nup, &fixture(Kind::OotNup)[..200])?;
        assert!(verify(&nup, Kind::OotNup, &AtomicBool::new(false)).is_err());
        let nsp = directory.path().join("bad.nsp");
        let mut bytes = fixture(Kind::TotkBase);
        bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&nsp, bytes)?;
        assert!(verify(&nsp, Kind::TotkBase, &AtomicBool::new(false)).is_err());
        Ok(())
    }

    #[test]
    fn key_validation_accepts_long_console_keys_without_disclosing_invalid_values() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("prod.keys");
        fs::write(
            &path,
            format!("synthetic_rsa_keypair = {}\n", "ab".repeat(576)),
        )?;
        verify(&path, Kind::TotkKeys, &AtomicBool::new(false))?;
        fs::write(&path, "synthetic_key = DO_NOT_LOG_THIS")?;
        let error = verify(&path, Kind::TotkKeys, &AtomicBool::new(false))
            .err()
            .context("Invalid key accepted")?;
        assert!(!format!("{error:#}").contains("DO_NOT_LOG_THIS"));
        fs::write(&path, fixture(Kind::TotkKeys))?;
        let error = verify(&path, Kind::TotkKeys, &AtomicBool::new(true))
            .err()
            .context("Cancelled verification succeeded")?;
        assert!(error.to_string().contains("cancelled"));
        Ok(())
    }

    #[test]
    fn game_file_permissions_are_opt_in_and_keys_are_independent() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut permissions = Permissions::load(directory.path())?;
        assert!(!permissions.allows(Kind::OotNup));
        permissions.set(Kind::TotkBase, true);
        permissions.persist(directory.path())?;
        let permissions = Permissions::load(directory.path())?;
        assert!(permissions.allows(Kind::TotkUpdate));
        assert!(!permissions.allows(Kind::TotkKeys));
        Ok(())
    }
}

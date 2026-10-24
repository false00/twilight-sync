use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path},
};
use tempfile::NamedTempFile;

pub const SAVE_SIZE: usize = 0x8040;
pub const QUEST_SIZE: usize = 0xa94;
pub const ISO_SIZE: u64 = 1_459_978_240;
pub const CHUNK_SIZE: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveIdentity {
    pub game: String,
    pub name: String,
}

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn validate_save(bytes: &[u8]) -> Result<SaveIdentity> {
    ensure!(
        bytes.len() == SAVE_SIZE,
        "Expected a four-block Dusklight GCI (32,832 bytes)"
    );
    let game = std::str::from_utf8(&bytes[..4])?;
    ensure!(
        matches!(game, "GZ2E" | "GZ2J" | "GZ2P" | "RZDE" | "RZDJ" | "RZDP"),
        "Not a supported Twilight Princess save"
    );
    ensure!(&bytes[4..6] == b"01", "Save maker must be Nintendo (01)");
    ensure!(
        u16::from_be_bytes([bytes[0x38], bytes[0x39]]) == 4,
        "Invalid GCI block count"
    );
    let name_field = &bytes[8..40];
    let end = name_field
        .iter()
        .position(|b| *b == 0)
        .context("Unterminated GCI name")?;
    ensure!(end > 0 && end <= 31, "Invalid GCI name length");
    let name = std::str::from_utf8(&name_field[..end])?;
    ensure!(
        name.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" _-.".contains(&b))
            && name != "."
            && name != "..",
        "Unsafe or unsupported GCI name"
    );
    ensure!(
        bytes[0x2c..0x30] == [0, 0, 0, 0],
        "Unexpected GCI image offset"
    );
    ensure!(
        u32::from_be_bytes(bytes[0x3c..0x40].try_into()?) == 0x2400,
        "Invalid GCI comment address"
    );
    // Both redundant sectors must be valid: do not propagate partially written saves.
    for offset in [0x4040, 0x6040] {
        let sector = &bytes[offset..offset + 0x2000];
        ensure!(
            u32::from_be_bytes(sector[4..8].try_into()?) == 6,
            "Unsupported save data version"
        );
        let (mut high, mut low) = (0_u16, 0_u16);
        for word in sector[..0x1ffc].chunks_exact(2) {
            let word = u16::from_be_bytes([word[0], word[1]]);
            high = high.wrapping_add(word);
            low = low.wrapping_add(!word);
        }
        let checksum = (u32::from(high) << 16) | u32::from(low);
        ensure!(
            u32::from_be_bytes(sector[0x1ffc..].try_into()?) == checksum,
            "Invalid sector checksum"
        );
        for slot in 0..3 {
            let quest = &sector[8 + slot * QUEST_SIZE..8 + (slot + 1) * QUEST_SIZE];
            let (mut high, mut low) = (0_u32, 0_u32);
            for byte in &quest[..QUEST_SIZE - 8] {
                high = high.wrapping_add(u32::from(*byte));
                low = low.wrapping_add(!u32::from(*byte));
            }
            let expected = (u64::from(high) << 32) | u64::from(low);
            ensure!(
                u64::from_be_bytes(quest[QUEST_SIZE - 8..].try_into()?) == expected,
                "Invalid quest-slot checksum"
            );
            for start in [0x1b4, 0x1c5] {
                ensure!(
                    quest[start..start + 17].contains(&0),
                    "Unterminated player or horse name"
                );
            }
        }
    }
    Ok(SaveIdentity {
        game: game.to_owned(),
        name: name.to_owned(),
    })
}

pub fn ensure_safe_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "Path must be absolute");
    ensure!(
        !path.components().any(|c| matches!(c, Component::ParentDir)),
        "Parent traversal is not allowed"
    );
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        ensure!(
            !text.starts_with(r"\\") && !text.starts_with(r"\??\"),
            "UNC and device paths are not allowed"
        );
        ensure!(
            !text.chars().skip(2).any(|c| c == ':'),
            "Alternate data streams are not allowed"
        );
        for component in path.components() {
            if let Component::Normal(name) = component {
                let name = name.to_string_lossy();
                ensure!(
                    !name.ends_with(['.', ' '])
                        && !name
                            .chars()
                            .any(|c| c.is_control() || r#"<>:"|?*"#.contains(c)),
                    "Unsafe Windows path component"
                );
                let stem = name
                    .split('.')
                    .next()
                    .context("Empty path component")?
                    .to_ascii_uppercase();
                ensure!(
                    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                        && !(stem.len() == 4
                            && (stem.starts_with("COM") || stem.starts_with("LPT"))
                            && stem.as_bytes()[3].is_ascii_digit()),
                    "Windows device names are forbidden"
                );
            }
        }
    }
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "Symbolic links are not allowed"
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        metadata.file_attributes() & 0x400 == 0,
                        "Reparse points are not allowed"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Inspecting path"),
        }
    }
    Ok(())
}

pub fn read_save(path: &Path) -> Result<Vec<u8>> {
    ensure_safe_path(path)?;
    ensure!(
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("gci")),
        "Only .gci saves are allowed"
    );
    let file = File::open(path).context("Opening selected save")?;
    ensure!(file.metadata()?.is_file(), "Save must be a regular file");
    let mut bytes = Vec::with_capacity(SAVE_SIZE);
    file.take((SAVE_SIZE + 1) as u64).read_to_end(&mut bytes)?;
    validate_save(&bytes)?;
    Ok(bytes)
}

pub fn save_hash(path: &Path) -> Result<Option<String>> {
    ensure_safe_path(path)?;
    ensure!(
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("gci")),
        "Save destination must be a .gci"
    );
    ensure!(
        path.parent().is_some_and(|p| p.is_dir()),
        "Save directory does not exist"
    );
    if !path.try_exists()? {
        return Ok(None);
    }
    Ok(Some(sha256(&read_save(path)?)))
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_safe_path(path)?;
    let parent = path.parent().context("Destination has no parent")?;
    ensure!(parent.is_dir(), "Destination directory does not exist");
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    ensure_safe_path(path)?;
    temporary
        .persist(path)
        .map_err(|e| e.error)
        .context("Committing atomic file replacement")?;
    Ok(())
}

pub fn validate_iso_header(header: &[u8], size: u64) -> Result<&'static str> {
    ensure!(
        size == ISO_SIZE && header.len() >= 0x440,
        "Only full-size, uncompressed GameCube ISOs are supported"
    );
    ensure!(
        header[0x1c..0x20] == [0xc2, 0x33, 0x9f, 0x3d],
        "Invalid GameCube disc magic"
    );
    ensure!(
        header[6] == 0 && header[7] == 0,
        "Unsupported disc number or revision"
    );
    match &header[..6] {
        b"GZ2E01" => Ok("14e886f08e548a000afde98a3195e788"),
        b"GZ2J01" => Ok("5967dc7a6a553652f4d2050aeef6f368"),
        b"GZ2P01" => Ok("9ef597588b0035ca9e91b333fa9a8a7e"),
        _ => bail!("Not a supported Twilight Princess GameCube disc"),
    }
}

pub fn verify_iso(path: &Path) -> Result<(String, u64)> {
    use xxhash_rust::xxh3::Xxh3;
    ensure_safe_path(path)?;
    ensure!(
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("iso")),
        "Only .iso images are supported"
    );
    let mut file = File::open(path)?;
    ensure!(file.metadata()?.is_file(), "ISO must be a regular file");
    let size = file.metadata()?.len();
    let mut header = [0_u8; 0x440];
    file.read_exact(&mut header)?;
    let expected = validate_iso_header(&header, size)?;
    let (mut sha, mut official) = (Sha256::new(), Xxh3::new());
    sha.update(header);
    official.update(&header);
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut read = header.len() as u64;
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        read += length as u64;
        ensure!(read <= size, "ISO changed during verification");
        sha.update(&buffer[..length]);
        official.update(&buffer[..length]);
    }
    ensure!(read == size, "ISO was truncated during verification");
    ensure!(
        format!("{:032x}", official.digest128()) == expected,
        "ISO does not match the official unmodified disc hash"
    );
    Ok((hex::encode(sha.finalize()), size))
}

#[cfg(test)]
pub fn fixture_save() -> Vec<u8> {
    let mut bytes = vec![0; SAVE_SIZE];
    bytes[..6].copy_from_slice(b"GZ2E01");
    bytes[8..20].copy_from_slice(b"zeldaTp.dat\0");
    bytes[0x38..0x3a].copy_from_slice(&4_u16.to_be_bytes());
    bytes[0x3c..0x40].copy_from_slice(&0x2400_u32.to_be_bytes());
    for offset in [0x4040, 0x6040] {
        let sector = &mut bytes[offset..offset + 0x2000];
        sector[4..8].copy_from_slice(&6_u32.to_be_bytes());
        for slot in 0..3 {
            let quest = &mut sector[8 + slot * QUEST_SIZE..8 + (slot + 1) * QUEST_SIZE];
            let low = 0_u32.wrapping_sub((QUEST_SIZE - 8) as u32);
            quest[QUEST_SIZE - 8..].copy_from_slice(&u64::from(low).to_be_bytes());
        }
        let (mut high, mut low) = (0_u16, 0_u16);
        for word in sector[..0x1ffc].chunks_exact(2) {
            let value = u16::from_be_bytes([word[0], word[1]]);
            high = high.wrapping_add(value);
            low = low.wrapping_add(!value);
        }
        let checksum = (u32::from(high) << 16) | u32::from(low);
        sector[0x1ffc..].copy_from_slice(&checksum.to_be_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_both_sectors_and_all_slots() -> Result<()> {
        let valid = fixture_save();
        validate_save(&valid)?;
        for offset in [0, 4, 0x38, 0x3c, 0x4044, 0x4050, 0x6044, 0x6050] {
            let mut invalid = valid.clone();
            invalid[offset] ^= 1;
            assert!(
                validate_save(&invalid).is_err(),
                "accepted mutation at {offset:x}"
            );
        }
        assert!(validate_save(&valid[..SAVE_SIZE - 1]).is_err());
        let mut oversized = valid;
        oversized.push(0);
        assert!(validate_save(&oversized).is_err());
        Ok(())
    }

    #[test]
    fn refuses_executables_archives_and_arbitrary_data() {
        for magic in [b"MZ".as_slice(), b"\x7fELF", b"PK\x03\x04", b"#!"] {
            let mut bytes = vec![0; SAVE_SIZE];
            bytes[..magic.len()].copy_from_slice(magic);
            assert!(validate_save(&bytes).is_err());
        }
    }

    #[test]
    fn never_overwrites_a_changed_destination() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("save.gci");
        let bytes = fixture_save();
        atomic_write(&path, &bytes)?;
        let mut history = crate::history::History::open(temp.path(), "test", &"a".repeat(64))?;
        assert!(
            history
                .replace_save(
                    &path,
                    &bytes,
                    Some(&"0".repeat(64)),
                    crate::history::Reason::Received,
                    None,
                    None
                )
                .is_err()
        );
        assert_eq!(fs::read(path)?, bytes);
        Ok(())
    }

    #[test]
    fn commits_and_keeps_a_deduplicated_snapshot() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("save.gci");
        let bytes = fixture_save();
        atomic_write(&path, &bytes)?;
        let hash = sha256(&bytes);
        let mut history = crate::history::History::open(temp.path(), "test", &"a".repeat(64))?;
        let (_, captured) = history.replace_save(
            &path,
            &bytes,
            Some(&hash),
            crate::history::Reason::Received,
            None,
            None,
        )?;
        assert_eq!(history.read(&captured.id)?, bytes);
        assert!(!temp.path().join("backups").try_exists()?);
        Ok(())
    }

    #[test]
    fn rejects_unsafe_paths_and_fake_disc_headers() -> Result<()> {
        assert!(ensure_safe_path(Path::new("relative.gci")).is_err());
        let temp = tempfile::tempdir()?;
        assert!(ensure_safe_path(&temp.path().join("..").join("escape.gci")).is_err());
        assert!(validate_iso_header(&[0; 0x440], ISO_SIZE).is_err());
        Ok(())
    }

    fn reseal(bytes: &mut [u8]) -> Result<()> {
        for offset in [0x4040, 0x6040] {
            let sector = &mut bytes[offset..offset + 0x2000];
            for slot in 0..3 {
                let quest = &mut sector[8 + slot * QUEST_SIZE..8 + (slot + 1) * QUEST_SIZE];
                let high: u32 = quest[..QUEST_SIZE - 8].iter().map(|b| u32::from(*b)).sum();
                let low = 0_u32
                    .wrapping_sub(u32::try_from(QUEST_SIZE - 8)?)
                    .wrapping_sub(high);
                quest[QUEST_SIZE - 8..]
                    .copy_from_slice(&((u64::from(high) << 32) | u64::from(low)).to_be_bytes());
            }
            let sum: u32 = sector[..0x1ffc]
                .chunks_exact(2)
                .map(|w| u32::from(u16::from_be_bytes([w[0], w[1]])))
                .sum();
            let high = (sum & 0xffff) as u16;
            let low = 0_u16.wrapping_sub(0xffe).wrapping_sub(high);
            sector[0x1ffc..]
                .copy_from_slice(&((u32::from(high) << 16) | u32::from(low)).to_be_bytes());
        }
        Ok(())
    }

    #[test]
    fn player_and_horse_names_are_bounded_even_with_valid_checksums() -> Result<()> {
        for sector in [0x4040, 0x6040] {
            for slot in 0..3 {
                for field in [0x1b4, 0x1c5] {
                    let mut bytes = fixture_save();
                    let start = sector + 8 + slot * QUEST_SIZE + field;
                    bytes[start..start + 17].fill(b'A');
                    reseal(&mut bytes)?;
                    let error = validate_save(&bytes)
                        .err()
                        .context("Unterminated name was accepted")?;
                    assert!(
                        error
                            .to_string()
                            .contains("Unterminated player or horse name"),
                        "{error:#}"
                    );
                }
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn refuses_windows_device_aliases_and_stream_paths() {
        for path in [
            r"C:\Temp\CON.gci",
            r"C:\Temp\NuL.txt",
            r"C:\Temp\COM1.gci",
            r"C:\Temp\LPT9.zip",
            r"C:\Temp\file.gci:payload",
            r"C:\Temp\ambiguous. \save.gci",
            r"\\server\share\save.gci",
            r"\\?\C:\Temp\save.gci",
        ] {
            assert!(
                ensure_safe_path(Path::new(path)).is_err(),
                "accepted {path}"
            );
        }
    }
}

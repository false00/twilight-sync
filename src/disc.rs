use crate::files::{self, ISO_SIZE, ensure_safe_path};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

#[cfg(windows)]
mod worker;

pub const MAX_IMAGE_SIZE: u64 = ISO_SIZE + 64 * 1024 * 1024;
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(300);
const RESULT_PREFIX: &str = "TWILIGHT_DISC_VERIFIED ";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Iso,
    Rvz,
}

impl Format {
    pub fn from_path(path: &Path) -> Result<Self> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some(extension) if extension.eq_ignore_ascii_case("iso") => Ok(Self::Iso),
            Some(extension) if extension.eq_ignore_ascii_case("rvz") => Ok(Self::Rvz),
            _ => anyhow::bail!("Choose a GameCube .iso or .rvz disc image"),
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Iso => "iso",
            Self::Rvz => "rvz",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Iso => "ISO",
            Self::Rvz => "RVZ",
        }
    }

    pub fn validate_size(self, size: u64) -> Result<()> {
        ensure!(
            match self {
                Self::Iso => size == ISO_SIZE,
                Self::Rvz => (0x148..=MAX_IMAGE_SIZE).contains(&size),
            },
            "Invalid or unsupported {} image size",
            self.label()
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verified {
    pub format: Format,
    pub sha256: String,
    pub size: u64,
}

pub fn verify(path: &Path) -> Result<Verified> {
    verify_with_cancel(path, Arc::new(AtomicBool::new(false)))
}

pub fn verify_with_cancel(path: &Path, cancel: Arc<AtomicBool>) -> Result<Verified> {
    ensure_safe_path(path)?;
    let format = Format::from_path(path)?;
    match format {
        Format::Iso => {
            let (sha256, size) = files::verify_iso(path)?;
            Ok(Verified {
                format,
                sha256,
                size,
            })
        }
        Format::Rvz => verify_rvz(path, cancel),
    }
}

#[cfg(windows)]
pub(crate) fn verify_rvz(path: &Path, cancel: Arc<AtomicBool>) -> Result<Verified> {
    worker::verify(path, cancel)
}

#[cfg(not(windows))]
pub(crate) fn verify_rvz(_: &Path, _: Arc<AtomicBool>) -> Result<Verified> {
    anyhow::bail!("Resource-limited RVZ verification currently requires Windows")
}

#[cfg(windows)]
pub fn worker_entry(path: &Path) -> Result<()> {
    worker::await_gate()?;
    let verified = verify_rvz_direct(path)?;
    println!("{RESULT_PREFIX}{}", serde_json::to_string(&verified)?);
    Ok(())
}

#[cfg(not(windows))]
pub fn worker_entry(_: &Path) -> Result<()> {
    anyhow::bail!("The RVZ verification worker requires Windows")
}

#[cfg(windows)]
fn rvz_preflight(file: &mut std::fs::File, size: u64, expanded_size: u64) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    Format::Rvz.validate_size(size)?;
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0_u8; 0x124];
    file.read_exact(&mut header)
        .context("Reading RVZ container headers")?;
    ensure!(&header[..4] == b"RVZ\x01", "Not an RVZ container");
    let word =
        |offset| -> Result<u32> { Ok(u32::from_be_bytes(header[offset..offset + 4].try_into()?)) };
    let wide =
        |offset| -> Result<u64> { Ok(u64::from_be_bytes(header[offset..offset + 8].try_into()?)) };
    ensure!(
        (0x30000..=0x0100_0000).contains(&word(8)?)
            && word(4)? >= word(8)?
            && (0xdc..=4096).contains(&word(12)?),
        "Unsupported RVZ header version or size"
    );
    ensure!(
        wide(0x24)? == expanded_size && wide(0x2c)? == size,
        "RVZ must reconstruct a full-size supported GameCube disc"
    );
    let base = 0x48;
    ensure!(
        word(base)? == 1 && word(base + 0x90)? == 0,
        "Only GameCube RVZ images are supported"
    );
    ensure!(
        matches!(word(base + 4)?, 0 | 2 | 3 | 4 | 5),
        "Unsupported RVZ compression"
    );
    let chunk = word(base + 12)?;
    ensure!(
        (32 * 1024..=32 * 1024 * 1024).contains(&chunk)
            && if chunk < 2 * 1024 * 1024 {
                chunk.is_power_of_two()
            } else {
                chunk % (2 * 1024 * 1024) == 0
            },
        "RVZ block size exceeds supported bounds"
    );
    ensure!(
        word(base + 0x94)? == 0x30
            && (1..=4096).contains(&word(base + 0xb4)?)
            && (1..=65_536).contains(&word(base + 0xc4)?)
            && header[base + 0xd4] <= 7,
        "RVZ metadata exceeds supported bounds"
    );
    for (offset, length) in [(base + 0xb8, base + 0xc0), (base + 0xc8, base + 0xd0)] {
        let start = wide(offset)?;
        let length = u64::from(word(length)?);
        ensure!(
            length > 0
                && length <= 8 * 1024 * 1024
                && start >= 0x48 + u64::from(word(12)?)
                && start.checked_add(length).is_some_and(|end| end <= size),
            "RVZ metadata range is invalid"
        );
    }
    Ok(())
}

#[cfg(windows)]
fn reconstructed_hash(reader: &mut impl std::io::Read, size: u64) -> Result<([u8; 0x440], String)> {
    use xxhash_rust::xxh3::Xxh3;
    ensure!(size >= 0x440, "Disc is too small");
    let mut header = [0_u8; 0x440];
    reader.read_exact(&mut header)?;
    let mut hash = Xxh3::new();
    hash.update(&header);
    let mut total = header.len() as u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    while total < size {
        let maximum = usize::try_from((size - total).min(buffer.len() as u64))?;
        let count = reader.read(&mut buffer[..maximum])?;
        ensure!(count > 0, "RVZ reconstructed disc is truncated");
        hash.update(&buffer[..count]);
        total += count as u64;
    }
    // nod reports unmapped sectors past disc_size(); consume exactly the validated disc length.
    Ok((header, format!("{:032x}", hash.digest128())))
}

#[cfg(windows)]
fn container_hash(file: &mut std::fs::File, size: u64) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= size, "RVZ changed during verification");
        hash.update(&buffer[..count]);
    }
    ensure!(total == size, "RVZ was truncated during verification");
    Ok(hex::encode(hash.finalize()))
}

#[cfg(windows)]
fn verify_rvz_direct(path: &Path) -> Result<Verified> {
    use std::os::windows::fs::OpenOptionsExt;
    ensure_safe_path(path)?;
    let mut guard = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(path)
        .context("Opening RVZ without concurrent writers")?;
    ensure!(guard.metadata()?.is_file(), "RVZ must be a regular file");
    let size = guard.metadata()?.len();
    rvz_preflight(&mut guard, size, ISO_SIZE)?;
    let before = container_hash(&mut guard, size)?;
    let mut disc = nod::Disc::new_with_options(
        path,
        &nod::OpenOptions {
            rebuild_encryption: true,
            validate_hashes: true,
        },
    )
    .context("Opening RVZ decoder")?;
    ensure!(
        disc.disc_size() == ISO_SIZE,
        "RVZ decoded size differs from the original disc"
    );
    let (header, official) = reconstructed_hash(&mut disc, ISO_SIZE)?;
    let expected = files::validate_iso_header(&header, ISO_SIZE)?;
    ensure!(
        official == expected,
        "RVZ does not reconstruct an official unmodified disc"
    );
    ensure!(
        container_hash(&mut guard, size)? == before,
        "RVZ changed while decoding"
    );
    Ok(Verified {
        format: Format::Rvz,
        sha256: before,
        size,
    })
}

#[cfg(all(test, windows))]
pub(crate) mod tests {
    use super::*;
    use sha1::{Digest, Sha1};
    use std::{fs, io::Cursor, sync::atomic::Ordering};
    use xxhash_rust::xxh3::xxh3_128;

    pub(crate) fn fixture(size: u64, compressed: bool) -> Result<(Vec<u8>, Vec<u8>)> {
        let block_size = 32 * 1024_u64;
        ensure!(
            size >= block_size && size.is_multiple_of(block_size),
            "Invalid fixture size"
        );
        let mut first = vec![0_u8; block_size as usize];
        first[..6].copy_from_slice(b"GZ2E01");
        first[0x1c..0x20].copy_from_slice(&[0xc2, 0x33, 0x9f, 0x3d]);
        let encode = |bytes: &[u8]| -> Result<Vec<u8>> {
            Ok(if compressed {
                zstd::stream::encode_all(Cursor::new(bytes), 3)?
            } else {
                bytes.to_vec()
            })
        };
        let payload = encode(&first)?;
        let groups = u32::try_from(size / block_size)?;
        let mut raw = Vec::new();
        raw.extend_from_slice(&0x80_u64.to_be_bytes());
        raw.extend_from_slice(&(size - 0x80).to_be_bytes());
        raw.extend_from_slice(&0_u32.to_be_bytes());
        raw.extend_from_slice(&groups.to_be_bytes());
        let raw = encode(&raw)?;
        let mut table = vec![0_u8; groups as usize * 12];
        table[..4].copy_from_slice(&(0x124_u32 / 4).to_be_bytes());
        table[4..8].copy_from_slice(
            &(u32::try_from(payload.len())? | if compressed { 0x8000_0000 } else { 0 })
                .to_be_bytes(),
        );
        let table = encode(&table)?;
        let raw_offset = 0x124 + payload.len();
        let group_offset = raw_offset + raw.len();
        let mut image = vec![0_u8; group_offset + table.len()];
        let mut disc = [0_u8; 0xdc];
        disc[..4].copy_from_slice(&1_u32.to_be_bytes());
        disc[4..8].copy_from_slice(&(if compressed { 5_u32 } else { 0 }).to_be_bytes());
        disc[8..12].copy_from_slice(&3_u32.to_be_bytes());
        disc[12..16].copy_from_slice(&u32::try_from(block_size)?.to_be_bytes());
        disc[16..0x90].copy_from_slice(&first[..0x80]);
        disc[0x94..0x98].copy_from_slice(&0x30_u32.to_be_bytes());
        disc[0xa0..0xb4].copy_from_slice(&Sha1::digest([]));
        disc[0xb4..0xb8].copy_from_slice(&1_u32.to_be_bytes());
        disc[0xb8..0xc0].copy_from_slice(&(raw_offset as u64).to_be_bytes());
        disc[0xc0..0xc4].copy_from_slice(&u32::try_from(raw.len())?.to_be_bytes());
        disc[0xc4..0xc8].copy_from_slice(&groups.to_be_bytes());
        disc[0xc8..0xd0].copy_from_slice(&(group_offset as u64).to_be_bytes());
        disc[0xd0..0xd4].copy_from_slice(&u32::try_from(table.len())?.to_be_bytes());
        image[..4].copy_from_slice(b"RVZ\x01");
        image[4..8].copy_from_slice(&0x0100_0000_u32.to_be_bytes());
        image[8..12].copy_from_slice(&0x0100_0000_u32.to_be_bytes());
        image[12..16].copy_from_slice(&0xdc_u32.to_be_bytes());
        image[16..36].copy_from_slice(&Sha1::digest(disc));
        image[0x24..0x2c].copy_from_slice(&size.to_be_bytes());
        let image_size = image.len() as u64;
        image[0x2c..0x34].copy_from_slice(&image_size.to_be_bytes());
        let header_hash = Sha1::digest(&image[..0x34]);
        image[0x34..0x48].copy_from_slice(&header_hash);
        image[0x48..0x124].copy_from_slice(&disc);
        image[0x124..raw_offset].copy_from_slice(&payload);
        image[raw_offset..group_offset].copy_from_slice(&raw);
        image[group_offset..].copy_from_slice(&table);
        Ok((image, first))
    }

    #[test]
    fn rvz_reconstructs_synthetic_disc_bytes_with_and_without_compression() -> Result<()> {
        let root = tempfile::tempdir()?;
        let size = 64 * 1024;
        for compressed in [false, true] {
            let (bytes, first) = fixture(size, compressed)?;
            let path = root.path().join("fixture.rvz");
            fs::write(&path, &bytes)?;
            rvz_preflight(&mut fs::File::open(&path)?, bytes.len() as u64, size)?;
            let mut disc = nod::Disc::new(&path)?;
            assert_eq!(disc.disc_size(), size);
            let (header, hash) = reconstructed_hash(&mut disc, size)?;
            let mut original = first;
            original.resize(size as usize, 0);
            assert_eq!(&header[..], &original[..0x440]);
            assert_eq!(hash, format!("{:032x}", xxh3_128(&original)));
        }
        Ok(())
    }

    #[test]
    fn rvz_rejects_unsafe_header_sizes_types_and_metadata_ranges() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (valid, _) = fixture(64 * 1024, false)?;
        for (offset, replacement) in [
            (0, b"MZ!!".to_vec()),
            (0x48, 2_u32.to_be_bytes().to_vec()),
            (12, u32::MAX.to_be_bytes().to_vec()),
            (0x54, 0_u32.to_be_bytes().to_vec()),
            (0x48 + 0xc4, u32::MAX.to_be_bytes().to_vec()),
            (0x48 + 0xb8, u64::MAX.to_be_bytes().to_vec()),
        ] {
            let mut bytes = valid.clone();
            bytes[offset..offset + replacement.len()].copy_from_slice(&replacement);
            let path = root.path().join("invalid.rvz");
            fs::write(&path, &bytes)?;
            assert!(
                rvz_preflight(&mut fs::File::open(path)?, bytes.len() as u64, 64 * 1024).is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn ordinary_files_and_modified_game_data_never_pass_as_rvz() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("not-a-disc.rvz");
        fs::write(&path, vec![0; 512])?;
        assert!(verify(&path).is_err());
        let (bytes, _) = fixture(ISO_SIZE, true)?;
        fs::write(&path, bytes)?;
        let error = verify(&path)
            .err()
            .context("Expected unsupported-disc rejection")?;
        assert!(
            format!("{error:#}").contains("official unmodified disc"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn disc_extensions_and_physical_size_limits_are_explicit() -> Result<()> {
        assert_eq!(Format::from_path(Path::new("disc.RVZ"))?, Format::Rvz);
        assert_eq!(Format::from_path(Path::new("disc.ISO"))?, Format::Iso);
        for name in ["disc.rvz.exe", "disc.zip", "disc.wia", "disc"] {
            assert!(Format::from_path(Path::new(name)).is_err());
        }
        for size in [0x148, MAX_IMAGE_SIZE] {
            Format::Rvz.validate_size(size)?;
        }
        for size in [0, 0x147, MAX_IMAGE_SIZE + 1, u64::MAX] {
            assert!(Format::Rvz.validate_size(size).is_err());
        }
        Format::Iso.validate_size(ISO_SIZE)?;
        assert!(Format::Iso.validate_size(ISO_SIZE - 1).is_err());
        assert!(Format::Iso.validate_size(ISO_SIZE + 1).is_err());
        Ok(())
    }

    #[test]
    fn cancelled_rvz_verification_does_not_decode() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("cancel.rvz");
        fs::write(&path, vec![0; 512])?;
        let cancel = Arc::new(AtomicBool::new(false));
        cancel.store(true, Ordering::Release);
        assert!(verify_with_cancel(&path, cancel).is_err());
        Ok(())
    }

    #[test]
    fn running_rvz_verification_cancels_promptly() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("running.rvz");
        let (bytes, _) = fixture(ISO_SIZE, true)?;
        fs::write(&path, bytes)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            trigger.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        let result = verify_with_cancel(&path, cancel);
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("Cancellation trigger panicked"))?;
        let error = result
            .err()
            .context("Expected running verification to cancel")?;
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(path.is_file());
        Ok(())
    }

    #[test]
    #[ignore = "Internal subprocess fixture for the resource-limited RVZ verifier"]
    fn bounded_rvz_worker_fixture() -> Result<()> {
        let path =
            std::env::var_os("TWILIGHT_RVZ_TEST_PATH").context("Missing RVZ fixture path")?;
        worker_entry(Path::new(&path))
    }
}

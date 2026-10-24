use crate::files::{self, ensure_safe_path};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

pub const BLOCK: usize = 0x2000;
pub const MAX_SIZE: usize = 16 * 1024 * 1024;
const RESERVED: usize = 5;
const ENTRIES: usize = 127;
const ENTRY: usize = 0x40;
const END: u16 = 0xffff;

struct Save {
    slot: usize,
    header: [u8; ENTRY],
    blocks: Vec<usize>,
}

struct Card<'a> {
    image: &'a [u8],
    directory: usize,
    allocation: usize,
    saves: Vec<Save>,
}

fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn put(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn checksum(bytes: &[u8]) -> (u16, u16) {
    let (mut sum, mut inverse) = (0_u16, 0_u16);
    for bytes in bytes.as_chunks::<2>().0 {
        let value = u16::from_be_bytes(*bytes);
        sum = sum.wrapping_add(value);
        inverse = inverse.wrapping_add(!value);
    }
    (
        if sum == END { 0 } else { sum },
        if inverse == END { 0 } else { inverse },
    )
}

fn valid_metadata(bytes: &[u8], directory: bool) -> bool {
    let (data, offset) = if directory {
        (&bytes[..BLOCK - 4], BLOCK - 4)
    } else {
        (&bytes[4..], 0)
    };
    checksum(data) == (word(bytes, offset), word(bytes, offset + 2))
}

fn stamp(bytes: &mut [u8], directory: bool) {
    let (sum, inverse) = if directory {
        checksum(&bytes[..BLOCK - 4])
    } else {
        checksum(&bytes[4..])
    };
    let offset = if directory { BLOCK - 4 } else { 0 };
    put(bytes, offset, sum);
    put(bytes, offset + 2, inverse);
}

fn mirror(image: &[u8], first: usize, directory: bool) -> Result<usize> {
    let left = &image[first * BLOCK..(first + 1) * BLOCK];
    let right = &image[(first + 1) * BLOCK..(first + 2) * BLOCK];
    match (
        valid_metadata(left, directory),
        valid_metadata(right, directory),
    ) {
        (true, false) => Ok(first),
        (false, true) => Ok(first + 1),
        (false, false) => bail!(
            "Raw card has no valid {} mirror",
            if directory {
                "directory"
            } else {
                "allocation-table"
            }
        ),
        (true, true) => {
            let offset = if directory { BLOCK - 6 } else { 4 };
            let difference = word(right, offset).wrapping_sub(word(left, offset));
            ensure!(
                difference != 0x8000,
                "Raw card mirror counters are ambiguous"
            );
            if difference == 0 {
                ensure!(
                    left == right,
                    "Raw card mirrors disagree at the same update counter"
                );
            }
            Ok(if difference != 0 && difference < 0x8000 {
                first + 1
            } else {
                first
            })
        }
    }
}

fn allocation_entry(table: &[u8], block: usize) -> u16 {
    word(table, 10 + (block - RESERVED) * 2)
}

impl<'a> Card<'a> {
    fn parse(image: &'a [u8]) -> Result<Self> {
        ensure!(
            image.len() >= 512 * 1024 && image.len() <= MAX_SIZE && image.len().is_power_of_two(),
            "Raw card size must be a standard 512 KiB to 16 MiB image"
        );
        let size = usize::from(word(image, 0x22));
        ensure!(
            matches!(size, 4 | 8 | 16 | 32 | 64 | 128) && size * 128 * 1024 == image.len(),
            "Raw card header size does not match its image"
        );
        ensure!(
            checksum(&image[..0x1fc]) == (word(image, 0x1fc), word(image, 0x1fe)),
            "Raw card header checksum is invalid"
        );
        ensure!(word(image, 0x24) <= 1, "Unsupported raw card encoding");
        let directory = mirror(image, 1, true)?;
        let allocation = mirror(image, 3, false)?;
        let data = &image[directory * BLOCK..(directory + 1) * BLOCK];
        let table = &image[allocation * BLOCK..(allocation + 1) * BLOCK];
        let count = image.len() / BLOCK;
        let mut used = vec![false; count];
        let mut saves = Vec::new();
        for slot in 0..ENTRIES {
            let entry = &data[slot * ENTRY..(slot + 1) * ENTRY];
            if entry[..4] == [0xff; 4] {
                continue;
            }
            let length = usize::from(word(entry, 0x38));
            ensure!(
                length > 0 && length <= count - RESERVED,
                "Raw card save has an invalid block count"
            );
            let mut current = usize::from(word(entry, 0x36));
            let mut blocks = Vec::with_capacity(length);
            for index in 0..length {
                ensure!(
                    (RESERVED..count).contains(&current),
                    "Raw card save chain is out of bounds"
                );
                ensure!(
                    !used[current],
                    "Raw card save chains overlap or contain a cycle"
                );
                used[current] = true;
                blocks.push(current);
                let next = allocation_entry(table, current);
                if index + 1 == length {
                    ensure!(
                        next == END,
                        "Raw card chain exceeds its declared save length"
                    );
                } else {
                    ensure!(next != END && next != 0, "Raw card save chain is truncated");
                    current = usize::from(next);
                }
            }
            saves.push(Save {
                slot,
                header: entry.try_into()?,
                blocks,
            });
        }
        let mut free = 0;
        for (block, allocated) in used.iter().enumerate().skip(RESERVED) {
            let value = allocation_entry(table, block);
            if value == 0 {
                free += 1;
                ensure!(!allocated, "Raw card save references a free block");
            } else {
                ensure!(*allocated, "Raw card contains unowned allocated blocks");
            }
        }
        ensure!(
            free == usize::from(word(table, 6)),
            "Raw card free-block count is inconsistent"
        );
        Ok(Self {
            image,
            directory,
            allocation,
            saves,
        })
    }

    fn stock(&self, path: &Path) -> Result<Option<&Save>> {
        let region = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| {
                ["USA", "EUR", "JAP"]
                    .into_iter()
                    .find(|region| name.eq_ignore_ascii_case(&format!("MemoryCardA.{region}.raw")))
            });
        let mut selected = None;
        for save in &self.saves {
            let header = &save.header;
            let supported = [b"GZ2E", b"GZ2J", b"GZ2P", b"RZDE", b"RZDJ", b"RZDP"]
                .iter()
                .any(|game| &header[..4] == game.as_slice());
            if !supported || &header[4..6] != b"01" || &header[8..17] != b"gczelda2\0" {
                continue;
            }
            let actual_region = match header[3] {
                b'J' => "JAP",
                b'P' => "EUR",
                _ => "USA",
            };
            if region.is_some_and(|region| region != actual_region) {
                continue;
            }
            ensure!(
                selected.is_none(),
                "Raw card contains multiple supported Twilight Princess saves"
            );
            selected = Some(save);
        }
        Ok(selected)
    }

    fn extract(&self, save: &Save) -> Result<Vec<u8>> {
        ensure!(
            save.blocks.len() == 4,
            "Twilight Princess save must occupy four raw-card blocks"
        );
        let mut bytes = Vec::with_capacity(files::SAVE_SIZE);
        bytes.extend_from_slice(&save.header);
        for block in &save.blocks {
            bytes.extend_from_slice(&self.image[block * BLOCK..(block + 1) * BLOCK]);
        }
        files::normalize_save(&bytes)
    }

    fn update(&self, bytes: &[u8], path: &Path) -> Result<Vec<u8>> {
        let bytes = files::normalize_save(bytes)?;
        let existing = self.stock(path)?;
        if let Some(save) = existing {
            ensure!(
                save.header[..6] == bytes[..6],
                "Raw card save belongs to another game region or version"
            );
        }
        let mut directory =
            self.image[self.directory * BLOCK..(self.directory + 1) * BLOCK].to_vec();
        let mut table = self.image[self.allocation * BLOCK..(self.allocation + 1) * BLOCK].to_vec();
        let (slot, blocks) = if let Some(save) = existing {
            ensure!(
                save.blocks.len() == 4,
                "Existing raw-card save has an incompatible size"
            );
            (save.slot, save.blocks.clone())
        } else {
            let slot = (0..ENTRIES)
                .find(|slot| directory[slot * ENTRY..slot * ENTRY + 4] == [0xff; 4])
                .context("Raw card directory is full")?;
            let free: Vec<_> = (RESERVED..self.image.len() / BLOCK)
                .filter(|block| allocation_entry(&table, *block) == 0)
                .take(4)
                .collect();
            ensure!(
                free.len() == 4,
                "Raw card needs four free blocks for the Twilight Princess save"
            );
            for (index, block) in free.iter().enumerate() {
                let next = if index == 3 {
                    END
                } else {
                    u16::try_from(free[index + 1])?
                };
                put(&mut table, 10 + (block - RESERVED) * 2, next);
            }
            let free_count = word(&table, 6)
                .checked_sub(4)
                .context("Raw card free count underflow")?;
            let counter = word(&table, 4).wrapping_add(1);
            put(&mut table, 4, counter);
            put(&mut table, 6, free_count);
            put(&mut table, 8, u16::try_from(free[3])?);
            stamp(&mut table, false);
            (slot, free)
        };
        let mut header: [u8; ENTRY] = bytes[..ENTRY].try_into()?;
        put(&mut header, 0x36, u16::try_from(blocks[0])?);
        directory[slot * ENTRY..(slot + 1) * ENTRY].copy_from_slice(&header);
        let counter = word(&directory, BLOCK - 6).wrapping_add(1);
        put(&mut directory, BLOCK - 6, counter);
        stamp(&mut directory, true);
        let mut output = self.image.to_vec();
        for (index, block) in blocks.iter().enumerate() {
            output[block * BLOCK..(block + 1) * BLOCK]
                .copy_from_slice(&bytes[ENTRY + index * BLOCK..ENTRY + (index + 1) * BLOCK]);
        }
        for block in [1, 2] {
            output[block * BLOCK..(block + 1) * BLOCK].copy_from_slice(&directory);
        }
        if existing.is_none() {
            for block in [3, 4] {
                output[block * BLOCK..(block + 1) * BLOCK].copy_from_slice(&table);
            }
        }
        let result = Card::parse(&output)?;
        let selected = result
            .stock(path)?
            .context("Updated raw card lost its save")?;
        ensure!(
            result.extract(selected)? == bytes,
            "Updated raw card save differs from the approved payload"
        );
        Ok(output)
    }
}

fn read_image(file: &File) -> Result<Vec<u8>> {
    ensure!(
        file.metadata()?.is_file(),
        "Raw card must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_SIZE as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_SIZE, "Raw memory card exceeds 16 MiB");
    Ok(bytes)
}

pub fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    ensure_safe_path(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Opening raw memory card"),
    };
    let image = read_image(&file)?;
    let card = Card::parse(&image)?;
    card.stock(path)?.map(|save| card.extract(save)).transpose()
}

pub fn write(path: &Path, bytes: &[u8], expected: Option<&str>) -> Result<()> {
    ensure_safe_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1 | 2 | 4);
    }
    let guard = options.open(path).context(
        "Close Dusklight before updating its raw card; an existing formatted card is required",
    )?;
    fs2::FileExt::try_lock_exclusive(&guard)
        .context("Raw memory card is busy; close Dusklight before syncing it")?;
    let identity = file_identity(&guard)?;
    let image = read_image(&guard)?;
    let card = Card::parse(&image)?;
    let previous = card
        .stock(path)?
        .map(|save| card.extract(save))
        .transpose()?;
    ensure!(
        previous.as_deref().map(files::sha256).as_deref() == expected,
        "Raw-card save changed before replacement"
    );
    let output = card.update(bytes, path)?;
    let current = File::open(path).context("Rechecking the original raw card")?;
    ensure!(
        file_identity(&current)? == identity,
        "Raw card was replaced before atomic commit"
    );
    drop(current);
    commit(path, &output)?;
    ensure!(
        read(path)?.as_deref() == Some(files::normalize_save(bytes)?.as_slice()),
        "Raw-card post-write verification failed"
    );
    drop(guard);
    Ok(())
}

fn commit(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("Raw card has no directory")?)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    ensure_safe_path(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
        let staged = temporary.into_temp_path();
        let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let source: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
        // ReplaceFileW supports the held byte-range lock and preserves the card's file permissions.
        if unsafe {
            ReplaceFileW(
                destination.as_ptr(),
                source.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("Atomically replacing the locked raw card");
        }
    }
    #[cfg(not(windows))]
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Atomically replacing the raw card")?;
    Ok(())
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error()).context("Reading raw-card file identity");
    }
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(not(any(windows, unix)))]
fn file_identity(_: &File) -> Result<(u64, u64)> {
    bail!("Atomic raw-card updates are not supported on this platform")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn empty() -> Vec<u8> {
        let mut image = vec![0xff; 512 * 1024];
        put(&mut image, 0x20, 0);
        put(&mut image, 0x22, 4);
        put(&mut image, 0x24, 0);
        let (sum, inverse) = checksum(&image[..0x1fc]);
        put(&mut image, 0x1fc, sum);
        put(&mut image, 0x1fe, inverse);
        let mut directory = vec![0xff; BLOCK];
        put(&mut directory, BLOCK - 6, 0);
        stamp(&mut directory, true);
        let mut table = vec![0; BLOCK];
        put(&mut table, 6, 59);
        put(&mut table, 8, 4);
        stamp(&mut table, false);
        for index in [1, 2] {
            image[index * BLOCK..(index + 1) * BLOCK].copy_from_slice(&directory);
        }
        for index in [3, 4] {
            image[index * BLOCK..(index + 1) * BLOCK].copy_from_slice(&table);
        }
        image
    }

    pub(crate) fn with_save(bytes: &[u8]) -> Result<Vec<u8>> {
        Card::parse(&empty())?.update(bytes, Path::new("test.raw"))
    }

    fn with_other_game() -> Vec<u8> {
        let mut image = empty();
        for index in [1, 2] {
            let directory = &mut image[index * BLOCK..(index + 1) * BLOCK];
            directory[..ENTRY].fill(0);
            directory[..6].copy_from_slice(b"GAFE01");
            directory[8..18].copy_from_slice(b"other-game");
            put(directory, 0x36, 5);
            put(directory, 0x38, 2);
            stamp(directory, true);
        }
        for index in [3, 4] {
            let table = &mut image[index * BLOCK..(index + 1) * BLOCK];
            put(table, 10, 8);
            put(table, 16, END);
            put(table, 6, 57);
            put(table, 8, 8);
            stamp(table, false);
        }
        image[5 * BLOCK..6 * BLOCK].fill(0x13);
        image[8 * BLOCK..9 * BLOCK].fill(0x37);
        image
    }

    fn independent_extract(image: &[u8]) -> Result<Vec<u8>> {
        let card = gc_memcard::MemoryCard::parse(image)?;
        let save = card
            .saves()
            .iter()
            .find(|save| save.filename == "gczelda2")
            .context("Independent parser did not find the save")?;
        let mut bytes = save.dirent.clone();
        bytes.extend_from_slice(&save.data);
        files::normalize_save(&bytes)
    }

    #[test]
    fn native_format_insertion_and_replacement_round_trip_with_an_independent_parser() -> Result<()>
    {
        let first = crate::save_location::tests::stock_save("GZ2E");
        let image = with_save(&first)?;
        assert_eq!(independent_extract(&image)?, first);
        let mut next = first.clone();
        next[0x100] ^= 3;
        let updated = Card::parse(&image)?.update(&next, Path::new("test.raw"))?;
        assert_eq!(independent_extract(&updated)?, next);
        assert_eq!(&updated[..BLOCK], &image[..BLOCK]);
        assert_eq!(&updated[3 * BLOCK..5 * BLOCK], &image[3 * BLOCK..5 * BLOCK]);
        assert_eq!(&updated[9 * BLOCK..], &image[9 * BLOCK..]);
        Ok(())
    }

    #[test]
    fn fragmented_allocation_preserves_other_games_header_and_every_unrelated_data_block()
    -> Result<()> {
        let original = with_other_game();
        let save = crate::save_location::tests::stock_save("GZ2E");
        let inserted = Card::parse(&original)?.update(&save, Path::new("test.raw"))?;
        let card = Card::parse(&inserted)?;
        let target = card
            .stock(Path::new("test.raw"))?
            .context("Missing inserted save")?;
        assert_eq!(target.blocks, [6, 7, 9, 10]);
        let mut next = save;
        next[0x100] ^= 5;
        let replaced = card.update(&next, Path::new("test.raw"))?;
        let before = gc_memcard::MemoryCard::parse(&original)?;
        let after = gc_memcard::MemoryCard::parse(&replaced)?;
        assert_eq!(before.saves()[0], after.saves()[0]);
        for block in 0..original.len() / BLOCK {
            if ![1, 2, 3, 4].contains(&block) && !target.blocks.contains(&block) {
                assert_eq!(
                    &original[block * BLOCK..(block + 1) * BLOCK],
                    &replaced[block * BLOCK..(block + 1) * BLOCK]
                );
            }
        }
        assert_eq!(independent_extract(&replaced)?, next);
        Ok(())
    }

    #[test]
    fn atomic_card_writes_reject_stale_saves_and_preserve_other_games() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("MemoryCardA.USA.raw");
        let original = with_other_game();
        std::fs::write(&path, &original)?;
        let save = crate::save_location::tests::stock_save("GZ2E");
        write(&path, &save, None)?;
        assert_eq!(read(&path)?, Some(save.clone()));
        let before = std::fs::read(&path)?;
        let mut next = save.clone();
        next[0x100] ^= 1;
        assert!(write(&path, &next, Some(&"f".repeat(64))).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        write(&path, &next, Some(&files::sha256(&save)))?;
        assert_eq!(read(&path)?, Some(next));
        assert_eq!(
            gc_memcard::MemoryCard::parse(&original)?.saves()[0],
            gc_memcard::MemoryCard::parse(&std::fs::read(&path)?)?.saves()[0]
        );
        Ok(())
    }

    #[test]
    fn a_busy_card_or_full_card_is_never_modified() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("MemoryCardA.USA.raw");
        let image = with_other_game();
        std::fs::write(&path, &image)?;
        let guard = OpenOptions::new().read(true).write(true).open(&path)?;
        fs2::FileExt::try_lock_exclusive(&guard)?;
        let save = crate::save_location::tests::stock_save("GZ2E");
        assert!(write(&path, &save, None).is_err());
        drop(guard);
        assert_eq!(std::fs::read(path)?, image);
        let mut full = empty();
        for index in [1, 2] {
            let directory = &mut full[index * BLOCK..(index + 1) * BLOCK];
            directory[..ENTRY].copy_from_slice(&image[BLOCK..BLOCK + ENTRY]);
            put(directory, 0x38, 59);
            stamp(directory, true);
        }
        for index in [3, 4] {
            let table = &mut full[index * BLOCK..(index + 1) * BLOCK];
            for block in RESERVED..64 {
                put(
                    table,
                    10 + (block - RESERVED) * 2,
                    if block == 63 {
                        END
                    } else {
                        u16::try_from(block + 1)?
                    },
                );
            }
            put(table, 6, 0);
            put(table, 8, 63);
            stamp(table, false);
        }
        let before = full.clone();
        assert!(
            Card::parse(&full)?
                .update(&save, Path::new("test.raw"))
                .is_err()
        );
        assert_eq!(full, before);
        Ok(())
    }

    #[test]
    fn damaged_headers_mirrors_and_allocation_chains_are_rejected() -> Result<()> {
        let valid = with_save(&crate::save_location::tests::stock_save("GZ2E"))?;
        for offset in [0x22, 0x1fc, BLOCK + 10, 3 * BLOCK + 10] {
            let mut bytes = valid.clone();
            bytes[offset] ^= 1;
            if offset == BLOCK + 10 {
                bytes[2 * BLOCK + 10] ^= 1;
            }
            if offset == 3 * BLOCK + 10 {
                bytes[4 * BLOCK + 10] ^= 1;
            }
            assert!(Card::parse(&bytes).is_err());
        }
        let mut cyclic = valid;
        for index in [3, 4] {
            let table = &mut cyclic[index * BLOCK..(index + 1) * BLOCK];
            put(table, 10, 5);
            stamp(table, false);
        }
        assert!(Card::parse(&cyclic).is_err());
        let mut overlap = with_other_game();
        for index in [1, 2] {
            let directory = &mut overlap[index * BLOCK..(index + 1) * BLOCK];
            let entry: [u8; ENTRY] = directory[..ENTRY].try_into()?;
            directory[ENTRY..2 * ENTRY].copy_from_slice(&entry);
            stamp(directory, true);
        }
        assert!(Card::parse(&overlap).is_err());
        Ok(())
    }

    #[test]
    fn wrapping_counters_and_one_damaged_mirror_are_supported() -> Result<()> {
        let mut image = with_save(&crate::save_location::tests::stock_save("GZ2E"))?;
        for (index, counter) in [(1, u16::MAX), (2, 0)] {
            let directory = &mut image[index * BLOCK..(index + 1) * BLOCK];
            put(directory, BLOCK - 6, counter);
            stamp(directory, true);
        }
        assert_eq!(Card::parse(&image)?.directory, 2);
        image[BLOCK + 15] ^= 1;
        assert_eq!(Card::parse(&image)?.directory, 2);
        Ok(())
    }

    #[test]
    #[ignore = "Read-only interoperability test for an explicitly supplied private raw-card copy"]
    fn authorized_native_card_copy_round_trip() -> Result<()> {
        let path = std::env::var_os("TWILIGHT_RAW_TEST_PATH")
            .context("Missing private raw-card test-copy path")?;
        let original = read_image(&File::open(&path)?)?;
        let card = Card::parse(&original)?;
        let entry = card
            .stock(Path::new("native.raw"))?
            .context("No Twilight Princess save in native card")?;
        let first = card.extract(entry)?;
        assert_eq!(independent_extract(&original)?, first);
        let mut next = first.clone();
        next[0x100] ^= 1;
        let updated = card.update(&next, Path::new("native.raw"))?;
        assert_eq!(independent_extract(&updated)?, next);
        for block in 0..original.len() / BLOCK {
            if ![1, 2].contains(&block) && !entry.blocks.contains(&block) {
                assert_eq!(
                    &original[block * BLOCK..(block + 1) * BLOCK],
                    &updated[block * BLOCK..(block + 1) * BLOCK],
                    "Unrelated card block {block} changed"
                );
            }
        }
        let restored = Card::parse(&updated)?.update(&first, Path::new("native.raw"))?;
        assert_eq!(independent_extract(&restored)?, first);
        let scratch = tempfile::tempdir()?;
        let destination = scratch.path().join("MemoryCardA.USA.raw");
        std::fs::write(&destination, &original)?;
        write(&destination, &next, Some(&files::sha256(&first)))?;
        assert_eq!(read(&destination)?, Some(next.clone()));
        write(&destination, &first, Some(&files::sha256(&next)))?;
        assert_eq!(read(&destination)?, Some(first));
        assert_eq!(read_image(&File::open(&path)?)?, original);
        Ok(())
    }
}

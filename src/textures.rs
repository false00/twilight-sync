use crate::files::{atomic_write, ensure_safe_path, sha256, valid_hash};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const MAX_FILES: usize = 25_000;
const MAX_ENTRIES: usize = 100_000;
const MAX_PACK: u64 = 16 * 1024 * 1024 * 1024;
const MAX_TEXTURE: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST: u64 = 8 * 1024 * 1024;
const MAX_CONFIG: u64 = 2 * 1024 * 1024;
const MARKER: &str = ".twilight-sync-pack.json";
const SETTING: &str = "game.enableTextureReplacements";

pub struct CatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub creator_url: Option<&'static str>,
    pub suggested_version: Option<&'static str>,
}

pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "henriko-4k",
        name: "Henriko Magnifico - Twilight Princess 4K",
        description: "Henriko Magnifico's public PC 4K texture release with native Dusklight support. Requires more memory than the 1080p edition.",
        creator_url: Some("https://www.henrikomagnifico.com/zelda-twilight-princess-4k#downloads"),
        suggested_version: None,
    },
    CatalogEntry {
        id: "henriko-1080p",
        name: "Henriko Magnifico - Twilight Princess 1080p",
        description: "Henriko Magnifico's public PC 1080p texture release, with lower memory requirements.",
        creator_url: Some("https://www.henrikomagnifico.com/zelda-twilight-princess-4k#downloads"),
        suggested_version: None,
    },
    CatalogEntry {
        id: "tp-definitive-edition-plus",
        name: "TP Definitive Edition+",
        description: "HD texture pack by ValdarTP, with Masterkillua, Team TrueD-TPHD, theb1gm4c, GossipGeist, and Kurow. Creator release: v1.2.0. CC BY-NC-ND 4.0; assets are not redistributed by Twilight Sync.",
        creator_url: Some("https://gamebanana.com/mods/676609"),
        suggested_version: Some("1.2.0"),
    },
    CatalogEntry {
        id: "local-hd",
        name: "HD / custom textures (authorized local import)",
        description: "Install texture-only PNG/DDS assets you are authorized to use from a local ZIP or folder. Assets are not bundled with or redistributed by Twilight Sync.",
        creator_url: None,
        suggested_version: None,
    },
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextureFile {
    name: String,
    hash: String,
    size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Content {
    format: u8,
    source: String,
    files: Vec<TextureFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    id: String,
    content: Content,
    version: String,
    imported: String,
    skipped: usize,
}

#[derive(Clone, Debug)]
pub struct Pack {
    pub id: String,
    pub source: String,
    pub version: String,
    pub imported: String,
    pub files: usize,
    pub bytes: u64,
    pub skipped: usize,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub packs: Vec<Pack>,
    pub active: Option<String>,
    pub unmanaged: bool,
    pub recovery_needed: bool,
    pub notice: String,
    pub cancellable: bool,
}

#[derive(Clone, Debug)]
pub struct Activation {
    pub active: Option<String>,
    pub copied_files: usize,
    pub preserved: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: u8,
    pack: String,
    transaction: String,
    original: Option<String>,
    original_setting: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u8,
    profile: PathBuf,
    transaction: String,
    staged: Option<String>,
    previous: Option<String>,
    previous_owned: bool,
    old_config: String,
    new_config_hash: String,
    new_pack: Option<String>,
    restored_original: Option<String>,
}

pub fn catalog(id: &str) -> Result<&'static CatalogEntry> {
    CATALOG
        .iter()
        .find(|entry| entry.id == id)
        .context("Unknown texture project")
}

fn directory(root: &Path) -> Result<PathBuf> {
    let directory = root.join("textures");
    ensure_safe_path(&directory)?;
    let packs = directory.join("packs");
    ensure_safe_path(&packs)?;
    fs::create_dir_all(packs)?;
    Ok(directory)
}

fn pack_path(root: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        valid_hash(id) && !id.bytes().any(|b| b.is_ascii_uppercase()),
        "Invalid texture pack ID"
    );
    Ok(directory(root)?.join("packs").join(id))
}

fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    ensure_safe_path(path)?;
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "Texture metadata is invalid or oversized"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Texture metadata grew while reading"
    );
    Ok(bytes)
}

fn load_manifest(root: &Path, id: &str) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(&bounded_read(
        &pack_path(root, id)?.join("manifest.json"),
        MAX_MANIFEST,
    )?)?;
    ensure!(
        manifest.id == id && manifest.content.format == 1,
        "Unsupported texture pack manifest"
    );
    catalog(&manifest.content.source)?;
    ensure!(
        crate::config::valid_label(&manifest.version),
        "Invalid texture version label"
    );
    ensure!(
        !manifest.content.files.is_empty() && manifest.content.files.len() <= MAX_FILES,
        "Invalid texture file count"
    );
    ensure!(
        sha256(&serde_json::to_vec(&manifest.content)?) == id,
        "Texture manifest integrity check failed"
    );
    let mut names = BTreeMap::new();
    let mut total = 0_u64;
    for file in &manifest.content.files {
        ensure!(
            texture_key(&file.name)?.is_some() && valid_hash(&file.hash),
            "Invalid texture filename or digest"
        );
        ensure!(
            names.insert(file.name.to_ascii_lowercase(), ()).is_none(),
            "Duplicate texture filename"
        );
        ensure!(
            file.size > 0 && file.size <= MAX_TEXTURE,
            "Invalid texture size"
        );
        total = total
            .checked_add(file.size)
            .context("Texture pack size overflow")?;
        ensure!(total <= MAX_PACK, "Texture pack exceeds storage limit");
    }
    Ok(manifest)
}

fn summary(manifest: &Manifest) -> Pack {
    Pack {
        id: manifest.id.clone(),
        source: manifest.content.source.clone(),
        version: manifest.version.clone(),
        imported: manifest.imported.clone(),
        files: manifest.content.files.len(),
        bytes: manifest.content.files.iter().map(|file| file.size).sum(),
        skipped: manifest.skipped,
    }
}

pub fn validate_profile(profile: &Path) -> Result<()> {
    ensure_safe_path(profile)?;
    ensure!(
        profile.is_dir(),
        "Dusklight profile directory does not exist"
    );
    read_config(profile)?;
    Ok(())
}

pub fn prepare_profile(profile: &Path, check_idle: impl Fn() -> Result<()>) -> Result<()> {
    check_idle()?;
    ensure_safe_path(profile)?;
    fs::create_dir_all(profile).context("Preparing Dusklight's detected profile")?;
    let path = profile.join("config.json");
    ensure_safe_path(&path)?;
    match File::options().create_new(true).write(true).open(&path) {
        Ok(mut file) => {
            file.write_all(b"{}")?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("Preparing Dusklight configuration"),
    }
    validate_profile(profile)
}

fn read_config(profile: &Path) -> Result<(Vec<u8>, serde_json::Map<String, serde_json::Value>)> {
    let bytes = bounded_read(&profile.join("config.json"), MAX_CONFIG)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let object = value
        .as_object()
        .context("Dusklight config.json must be an object")?
        .clone();
    ensure!(
        object
            .get(SETTING)
            .is_none_or(serde_json::Value::is_boolean),
        "Dusklight texture setting is not a boolean"
    );
    Ok((bytes, object))
}

fn read_marker(path: &Path) -> Result<Option<Marker>> {
    ensure_safe_path(path)?;
    let marker = path.join(MARKER);
    if !marker.try_exists()? {
        return Ok(None);
    }
    let value: Marker = serde_json::from_slice(&bounded_read(&marker, 4096)?)?;
    ensure!(
        value.version == 1 && valid_hash(&value.pack) && valid_hash(&value.transaction),
        "Invalid managed texture marker"
    );
    if let Some(original) = &value.original {
        validate_local_name(original)?;
    }
    Ok(Some(value))
}

fn validate_local_name(name: &str) -> Result<()> {
    ensure!(
        name.len() <= 100
            && name.starts_with(".twilight-sync-")
            && !name.ends_with('.')
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)),
        "Invalid texture transaction directory"
    );
    Ok(())
}

fn texture_key(name: &str) -> Result<Option<String>> {
    let path = Path::new(name);
    let Some(extension) = path.extension().and_then(|s| s.to_str()) else {
        return Ok(None);
    };
    if !matches!(extension.to_ascii_lowercase().as_str(), "png" | "dds")
        || !name.starts_with("tex1_")
    {
        return Ok(None);
    }
    ensure!(
        name.len() <= 180 && path.components().count() == 1 && name.is_ascii(),
        "Unsafe texture filename"
    );
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("Invalid texture filename")?;
    Ok(parse_texture_key(stem))
}

fn parse_texture_key(mut stem: &str) -> Option<String> {
    let mut mip = 0_u8;
    if let Some((base, level)) = stem.rsplit_once(".mip") {
        if !level.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        mip = level.parse::<u8>().ok()?;
        if mip == 0 || mip > 14 {
            return None;
        }
        stem = base;
    }
    let parts: Vec<&str> = stem.split('_').collect();
    if !(4..=6).contains(&parts.len()) || parts[0] != "tex1" {
        return None;
    }
    let (width, height) = parts[1].split_once('x')?;
    if !width.bytes().all(|b| b.is_ascii_digit()) || !height.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (width, height) = (width.parse::<u16>().ok()?, height.parse::<u16>().ok()?);
    if width == 0 || height == 0 {
        return None;
    }
    let index = if parts[2] == "m" { 3 } else { 2 };
    let mut remaining = parts.len() - index;
    if remaining != 2 && remaining != 3 {
        return None;
    }
    let hash_value = |value: &str| -> Option<String> {
        if value == "$" {
            return Some(value.into());
        }
        if value.is_empty() || value.len() > 16 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(format!("{:016x}", u64::from_str_radix(value, 16).ok()?))
    };
    let hash = hash_value(parts[index])?;
    let mut format_index = parts.len() - 1;
    if parts[format_index] == "arb" {
        format_index -= 1;
        remaining -= 1;
    }
    if !parts[format_index].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let format = parts[format_index].parse::<u32>().ok()?;
    if remaining != 2 && remaining != 3 {
        return None;
    }
    let palette = if remaining == 3 {
        hash_value(parts[index + 1])?
    } else {
        String::new()
    };
    Some(format!("{width}x{height}|{hash}|{palette}|{format}|{mip}"))
}

fn image_dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width <= 16384 && height <= 16384,
        "Texture dimensions exceed supported bounds"
    );
    ensure!(
        u64::from(width) * u64::from(height) <= 8192 * 8192,
        "Texture exceeds the 64-megapixel safety limit"
    );
    Ok(())
}

fn validate_image(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    ensure!(
        size > 0 && size <= MAX_TEXTURE,
        "Texture file exceeds size limit"
    );
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .context("Texture has no extension")?;
    if extension.eq_ignore_ascii_case("png") {
        let mut signature = [0; 8];
        file.read_exact(&mut signature)?;
        ensure!(&signature == b"\x89PNG\r\n\x1a\n", "Texture is not a PNG");
        let mut position = 8_u64;
        let mut saw_data = false;
        for number in 0..8192 {
            let mut header = [0; 8];
            file.read_exact(&mut header)?;
            let length = u32::from_be_bytes(header[..4].try_into()?) as u64;
            position = position
                .checked_add(length + 12)
                .context("PNG size overflow")?;
            ensure!(
                position <= size && length <= MAX_TEXTURE,
                "PNG chunk exceeds file bounds"
            );
            let kind = &header[4..];
            ensure!(
                number > 0 || (kind == b"IHDR" && length == 13),
                "PNG must start with IHDR"
            );
            let mut crc = crc32fast::Hasher::new();
            crc.update(kind);
            let mut remaining = length;
            let mut buffer = [0; 64 * 1024];
            if kind == b"IHDR" {
                ensure!(number == 0, "Duplicate PNG header");
                let mut data = [0; 13];
                file.read_exact(&mut data)?;
                image_dimensions(
                    u32::from_be_bytes(data[..4].try_into()?),
                    u32::from_be_bytes(data[4..8].try_into()?),
                )?;
                ensure!(
                    matches!(
                        (data[9], data[8]),
                        (0, 1 | 2 | 4 | 8 | 16) | (2 | 4 | 6, 8 | 16) | (3, 1 | 2 | 4 | 8)
                    ),
                    "Unsupported PNG pixel layout"
                );
                ensure!(
                    data[10] == 0 && data[11] == 0 && data[12] <= 1,
                    "Unsupported PNG encoding"
                );
                crc.update(&data);
                remaining = 0;
            }
            while remaining > 0 {
                let count = usize::try_from(remaining.min(buffer.len() as u64))?;
                file.read_exact(&mut buffer[..count])?;
                crc.update(&buffer[..count]);
                remaining -= count as u64;
            }
            let mut checksum = [0; 4];
            file.read_exact(&mut checksum)?;
            ensure!(
                crc.finalize() == u32::from_be_bytes(checksum),
                "PNG chunk CRC mismatch"
            );
            if kind == b"IDAT" {
                saw_data = true;
            }
            if kind == b"IEND" {
                ensure!(
                    length == 0 && saw_data && position == size,
                    "PNG is incomplete or has trailing attachments"
                );
                return Ok(());
            }
        }
        bail!("PNG contains too many chunks or no IEND");
    }
    ensure!(
        extension.eq_ignore_ascii_case("dds") && size >= 128,
        "Texture is not a supported DDS"
    );
    let mut header = [0; 128];
    file.read_exact(&mut header)?;
    let word = |offset: usize| -> Result<u32> {
        Ok(u32::from_le_bytes(header[offset..offset + 4].try_into()?))
    };
    ensure!(
        &header[..4] == b"DDS " && word(4)? == 124 && word(76)? == 32,
        "Invalid DDS header"
    );
    let (width, height) = (word(16)?, word(12)?);
    image_dimensions(width, height)?;
    ensure!(
        word(24)? <= 1 && word(112)? & 0xfe00 == 0,
        "Volume and cube textures are not supported"
    );
    let levels = word(28)?.max(1);
    ensure!(
        levels <= 14 && levels <= 32 - width.max(height).leading_zeros(),
        "Invalid DDS mip count"
    );
    let mut data_start = 128;
    let (block_bytes, pixel_bytes) = match &header[84..88] {
        b"DXT1" => (8_u64, 0_u64),
        b"DXT3" | b"DXT5" | b"ATI2" | b"BC5U" => (16, 0),
        b"DX10" => {
            let mut dx10 = [0; 20];
            file.read_exact(&mut dx10)?;
            let format = u32::from_le_bytes(dx10[..4].try_into()?);
            ensure!(
                u32::from_le_bytes(dx10[4..8].try_into()?) == 3
                    && u32::from_le_bytes(dx10[12..16].try_into()?) == 1,
                "DDS must contain one 2D image"
            );
            data_start += 20;
            match format {
                71 | 72 => (8, 0),
                74 | 75 | 77 | 78 | 83 | 84 | 98 | 99 => (16, 0),
                28 | 29 | 87 | 91 => (0, 4),
                _ => bail!("Unsupported DDS DXGI format"),
            }
        }
        _ => {
            ensure!(
                word(80)? & 0x40 != 0 && matches!(word(88)?, 16 | 32),
                "Unsupported DDS pixel format"
            );
            (0, u64::from(word(88)? / 8))
        }
    };
    let mut expected = data_start;
    for level in 0..levels {
        let (w, h) = (
            u64::from((width >> level).max(1)),
            u64::from((height >> level).max(1)),
        );
        expected += if block_bytes > 0 {
            w.div_ceil(4) * h.div_ceil(4) * block_bytes
        } else {
            w * h * pixel_bytes
        };
    }
    ensure!(
        size == expected,
        "DDS payload does not match its declared dimensions and mip levels"
    );
    Ok(())
}

fn copy_image(mut input: impl Read, destination: &Path, expected: u64) -> Result<String> {
    copy_image_cancellable(&mut input, destination, expected, &AtomicBool::new(false))
}

fn copy_image_cancellable(
    mut input: impl Read,
    destination: &Path,
    expected: u64,
    cancel: &AtomicBool,
) -> Result<String> {
    ensure!(
        expected > 0 && expected <= MAX_TEXTURE,
        "Texture exceeds file size limit"
    );
    ensure_safe_path(destination)?;
    let mut output = File::options()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let mut sha = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "Texture installation was cancelled"
        );
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(
            total <= expected,
            "Texture grew or archive entry exceeds its size"
        );
        output.write_all(&buffer[..count])?;
        sha.update(&buffer[..count]);
    }
    ensure!(total == expected, "Texture was truncated");
    output.sync_all()?;
    drop(output);
    validate_image(destination)
        .with_context(|| format!("Validating texture {}", destination.display()))?;
    Ok(hex::encode(sha.finalize()))
}

struct Importer {
    stage: PathBuf,
    files: BTreeMap<String, TextureFile>,
    keys: BTreeMap<String, String>,
    bytes: u64,
    skipped: usize,
    cancel: Arc<AtomicBool>,
}

impl Importer {
    fn add(&mut self, name: &str, size: u64, input: impl Read) -> Result<()> {
        let Some(key) = texture_key(name)? else {
            self.skipped += 1;
            return Ok(());
        };
        ensure!(
            self.files.len() < MAX_FILES,
            "Texture pack has too many images"
        );
        self.bytes = self
            .bytes
            .checked_add(size)
            .context("Texture size overflow")?;
        ensure!(self.bytes <= MAX_PACK, "Texture pack expands beyond 16 GiB");
        let mut input = BufReader::new(input);
        let header = input.fill_buf()?;
        let extension = if header.starts_with(b"\x89PNG\r\n\x1a\n") {
            "png"
        } else if header.starts_with(b"DDS ") {
            "dds"
        } else {
            bail!(
                "Texture {} does not contain supported PNG or DDS data",
                name
            );
        };
        let name = Path::new(name)
            .with_extension(extension)
            .to_str()
            .context("Invalid normalized texture filename")?
            .to_ascii_lowercase();
        ensure!(
            !self.files.contains_key(&name) && !self.keys.contains_key(&key),
            "Texture selection contains an unresolved duplicate key"
        );
        ensure!(
            fs2::available_space(&self.stage)? > size + 64 * 1024 * 1024,
            "Not enough space to import textures"
        );
        let path = self.stage.join(&name);
        let hash = copy_image_cancellable(input, &path, size, &self.cancel)?;
        self.keys.insert(key, name.clone());
        self.files
            .insert(name.clone(), TextureFile { name, hash, size });
        Ok(())
    }
}

enum ImageSource {
    File(PathBuf),
    Zip(usize),
}

struct Candidate {
    name: String,
    size: u64,
    components: Vec<String>,
    source: ImageSource,
}

fn native_order(left: &[String], right: &[String]) -> std::cmp::Ordering {
    for (left, right) in left.iter().zip(right) {
        let folded = left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase());
        if !folded.is_eq() {
            return folded;
        }
        let original = left.cmp(right);
        if !original.is_eq() {
            return original;
        }
    }
    left.len().cmp(&right.len())
}

fn select_candidate(
    selected: &mut BTreeMap<String, Candidate>,
    relative: &Path,
    size: u64,
    source: ImageSource,
    skipped: &mut usize,
) -> Result<()> {
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .context("Invalid texture source name")?;
    let Some(key) = texture_key(name)? else {
        *skipped += 1;
        return Ok(());
    };
    let candidate = Candidate {
        name: name.into(),
        size,
        source,
        components: relative
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .map(str::to_owned)
                    .context("Texture source path is not UTF-8")
            })
            .collect::<Result<_>>()?,
    };
    if let Some(previous) = selected.get(&key) {
        let order = native_order(&candidate.components, &previous.components);
        ensure!(
            !order.is_eq(),
            "Texture archive contains duplicate entries at the same path"
        );
        *skipped += 1;
        if order.is_gt() {
            return Ok(());
        }
    }
    selected.insert(key, candidate);
    Ok(())
}

pub fn import(
    root: &Path,
    source: &str,
    version: &str,
    input: &Path,
    progress: impl FnMut(usize, u64),
) -> Result<Pack> {
    import_cancellable(
        root,
        source,
        version,
        input,
        Arc::new(AtomicBool::new(false)),
        progress,
    )
}

pub fn import_cancellable(
    root: &Path,
    source: &str,
    version: &str,
    input: &Path,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(usize, u64),
) -> Result<Pack> {
    catalog(source)?;
    ensure!(
        crate::config::valid_label(version),
        "Enter a version label of at most 64 bytes"
    );
    ensure_safe_path(input)?;
    let packs = directory(root)?.join("packs");
    let stage = tempfile::Builder::new()
        .prefix("import-")
        .tempdir_in(&packs)?;
    let mut importer = Importer {
        stage: stage.path().into(),
        files: BTreeMap::new(),
        keys: BTreeMap::new(),
        bytes: 0,
        skipped: 0,
        cancel: cancel.clone(),
    };
    let mut selected = BTreeMap::new();
    let mut archive = None;
    if input.is_dir() {
        let mut pending = vec![(input.to_path_buf(), 0)];
        let mut entries = 0;
        while let Some((path, depth)) = pending.pop() {
            ensure!(
                !cancel.load(Ordering::Acquire),
                "Texture installation was cancelled"
            );
            ensure!(depth <= 24, "Texture folder nesting exceeds limit");
            ensure_safe_path(&path)?;
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                entries += 1;
                ensure!(
                    entries <= MAX_ENTRIES,
                    "Texture folder contains too many entries"
                );
                let path = entry.path();
                ensure_safe_path(&path)?;
                if entry.file_type()?.is_dir() {
                    pending.push((path, depth + 1));
                } else {
                    ensure!(
                        entry.file_type()?.is_file(),
                        "Texture source contains a link or special file"
                    );
                    select_candidate(
                        &mut selected,
                        path.strip_prefix(input)?,
                        entry.metadata()?.len(),
                        ImageSource::File(path.clone()),
                        &mut importer.skipped,
                    )?;
                }
            }
        }
    } else {
        ensure!(
            input
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("zip")),
            "Choose a ZIP or an extracted GZ2 folder; extract other archive formats first"
        );
        let archive_file = File::open(input)?;
        ensure!(
            archive_file.metadata()?.len() <= MAX_PACK,
            "Texture archive exceeds 16 GiB"
        );
        let mut zip = zip::ZipArchive::new(archive_file)?;
        ensure!(
            zip.len() <= MAX_ENTRIES,
            "Texture archive has too many entries"
        );
        for index in 0..zip.len() {
            ensure!(
                !cancel.load(Ordering::Acquire),
                "Texture installation was cancelled"
            );
            let entry = zip.by_index(index)?;
            let relative = entry
                .enclosed_name()
                .context("Unsafe texture archive path")?;
            ensure_safe_path(&stage.path().join(&relative))?;
            if let Some(mode) = entry.unix_mode() {
                ensure!(
                    matches!(mode & 0xf000, 0 | 0x8000 | 0x4000),
                    "Texture archive contains a link or special file"
                );
            }
            if entry.is_dir() {
                continue;
            }
            select_candidate(
                &mut selected,
                &relative,
                entry.size(),
                ImageSource::Zip(index),
                &mut importer.skipped,
            )?;
        }
        archive = Some(zip);
    }
    ensure!(
        selected.len() <= MAX_FILES,
        "Texture pack has too many images"
    );
    let mut required = 0_u64;
    for candidate in selected.values() {
        ensure!(
            candidate.size > 0 && candidate.size <= MAX_TEXTURE,
            "Texture exceeds file size limit"
        );
        required = required
            .checked_add(candidate.size)
            .context("Texture pack size overflow")?;
    }
    ensure!(required <= MAX_PACK, "Texture pack expands beyond 16 GiB");
    ensure!(
        fs2::available_space(stage.path())? > required + 64 * 1024 * 1024,
        "Not enough free space to install the complete texture pack"
    );
    for candidate in selected.into_values() {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "Texture installation was cancelled"
        );
        match candidate.source {
            ImageSource::File(path) => {
                importer.add(&candidate.name, candidate.size, File::open(path)?)?
            }
            ImageSource::Zip(index) => {
                let mut entry = archive
                    .as_mut()
                    .context("Missing texture archive")?
                    .by_index(index)?;
                importer.add(&candidate.name, candidate.size, &mut entry)?;
            }
        }
        progress(importer.files.len(), importer.bytes);
    }
    ensure!(
        !importer.files.is_empty(),
        "No supported tex1_ PNG/DDS textures found; select the pack's GZ2 folder"
    );
    let content = Content {
        format: 1,
        source: source.into(),
        files: importer.files.into_values().collect(),
    };
    let id = sha256(&serde_json::to_vec(&content)?);
    let destination = packs.join(&id);
    ensure_safe_path(&destination)?;
    if destination.try_exists()? {
        let existing = load_manifest(root, &id)?;
        verify_files(&destination, &existing)?;
        return Ok(summary(&existing));
    }
    let manifest = Manifest {
        id,
        content,
        version: version.into(),
        imported: OffsetDateTime::now_utc().format(&Rfc3339)?,
        skipped: importer.skipped,
    };
    atomic_write(
        &stage.path().join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::rename(stage.path(), &destination)?;
    Ok(summary(&manifest))
}

fn hash_file(path: &Path, expected: u64) -> Result<String> {
    ensure_safe_path(path)?;
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() == expected,
        "Installed texture changed size"
    );
    let mut sha = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut total = 0;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(
            total <= expected,
            "Installed texture changed while checking"
        );
        sha.update(&buffer[..count]);
    }
    ensure!(total == expected, "Installed texture was truncated");
    Ok(hex::encode(sha.finalize()))
}

fn verify_files(directory: &Path, manifest: &Manifest) -> Result<()> {
    for file in &manifest.content.files {
        ensure!(
            hash_file(&directory.join(&file.name), file.size)? == file.hash,
            "Installed texture integrity check failed; re-import from your authorized source"
        );
    }
    Ok(())
}

pub fn inventory(root: &Path, profile: Option<&Path>) -> Result<State> {
    let directory = directory(root)?;
    let mut state = State {
        recovery_needed: directory.join("pending.json").try_exists()?,
        ..State::default()
    };
    for entry in fs::read_dir(directory.join("packs"))? {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_hash(&id) {
            continue;
        }
        ensure_safe_path(&entry.path())?;
        ensure!(
            entry.file_type()?.is_dir() && state.packs.len() < 128,
            "Too many installed texture versions"
        );
        state.packs.push(summary(&load_manifest(root, &id)?));
    }
    state
        .packs
        .sort_by(|a, b| b.imported.cmp(&a.imported).then_with(|| a.id.cmp(&b.id)));
    if let Some(profile) = profile {
        ensure_safe_path(profile)?;
        if profile.join("config.json").try_exists()? {
            validate_profile(profile)?;
        } else {
            ensure!(
                !profile.try_exists()? || profile.is_dir(),
                "Dusklight profile is not a directory"
            );
            state.notice = "Dusklight profile detected; texture setup will prepare its configuration when needed.".into();
        }
        let active = profile.join("texture_replacements");
        if active.try_exists()? {
            ensure!(active.is_dir(), "texture_replacements is not a directory");
            if let Some(marker) = read_marker(&active)? {
                state.active = Some(marker.pack);
            } else {
                state.unmanaged = fs::read_dir(&active)?.next().transpose()?.is_some();
            }
        }
    }
    Ok(state)
}

fn owned_name(prefix: &str) -> String {
    format!(
        ".twilight-sync-{prefix}-{}",
        hex::encode(rand::random::<[u8; 16]>())
    )
}

fn journal(root: &Path) -> Result<Option<Journal>> {
    let path = directory(root)?.join("pending.json");
    if !path.try_exists()? {
        return Ok(None);
    }
    let journal: Journal =
        serde_json::from_slice(&bounded_read(&path, MAX_CONFIG * 2 + 16 * 1024)?)?;
    ensure!(
        journal.version == 1
            && valid_hash(&journal.transaction)
            && valid_hash(&journal.new_config_hash),
        "Invalid texture recovery journal"
    );
    ensure_safe_path(&journal.profile)?;
    for name in [
        &journal.staged,
        &journal.previous,
        &journal.restored_original,
    ]
    .into_iter()
    .flatten()
    {
        validate_local_name(name)?;
    }
    ensure!(
        journal.old_config.len() as u64 <= MAX_CONFIG,
        "Oversized texture recovery config"
    );
    Ok(Some(journal))
}

fn retire_owned(root: &Path, path: &Path) -> Result<bool> {
    ensure_safe_path(path)?;
    let Some(marker) = read_marker(path)? else {
        return Ok(false);
    };
    let manifest = load_manifest(root, &marker.pack)?;
    let images = path.join("GZ2");
    let expected: BTreeMap<&str, &TextureFile> = manifest
        .content
        .files
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        ensure_safe_path(&entry.path())?;
        if entry.file_name() != "GZ2" && entry.file_name() != MARKER {
            return Ok(false);
        }
    }
    if !images.is_dir() {
        return Ok(false);
    }
    let mut count = 0;
    for entry in fs::read_dir(&images)? {
        let entry = entry?;
        ensure_safe_path(&entry.path())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Unknown file in managed texture directory"))?;
        let Some(file) = expected.get(name.as_str()) else {
            return Ok(false);
        };
        if !entry.file_type()?.is_file()
            || entry.metadata()?.len() != file.size
            || hash_file(&entry.path(), file.size)? != file.hash
        {
            return Ok(false);
        }
        count += 1;
    }
    if count != expected.len() {
        return Ok(false);
    }
    for file in &manifest.content.files {
        fs::remove_file(images.join(&file.name))?;
    }
    fs::remove_file(path.join(MARKER))?;
    fs::remove_dir(images)?;
    fs::remove_dir(path)?;
    Ok(true)
}

pub fn recover(root: &Path, check_idle: impl Fn() -> Result<()>) -> Result<Vec<PathBuf>> {
    let Some(journal) = journal(root)? else {
        return Ok(Vec::new());
    };
    check_idle()?;
    let profile = &journal.profile;
    let active = profile.join("texture_replacements");
    let config_path = profile.join("config.json");
    let current = bounded_read(&config_path, MAX_CONFIG)?;
    ensure!(
        current == journal.old_config.as_bytes() || sha256(&current) == journal.new_config_hash,
        "Dusklight config changed outside the interrupted switch; recovery requires manual review"
    );
    let mut preserved = Vec::new();
    let previous = journal.previous.as_ref().map(|name| profile.join(name));
    let previous_exists = previous.as_ref().is_some_and(|p| p.exists());
    if active.try_exists()? {
        let marker = read_marker(&active)?;
        let is_new = marker
            .as_ref()
            .is_some_and(|m| m.transaction == journal.transaction);
        if is_new {
            let displaced = profile.join(owned_name("recovery"));
            fs::rename(&active, &displaced)?;
            if !retire_owned(root, &displaced)? {
                preserved.push(displaced);
            }
        } else if journal.new_pack.is_none()
            && previous_exists
            && let Some(original) = &journal.restored_original
        {
            let target = profile.join(original);
            ensure!(
                !target.try_exists()?,
                "Original texture backup is already occupied"
            );
            fs::rename(&active, target)?;
        } else {
            ensure!(
                !previous_exists,
                "Cannot overwrite unrelated active textures during recovery"
            );
        }
    }
    if let Some(previous) = previous
        && previous.try_exists()?
    {
        ensure!(
            !active.try_exists()?,
            "Active texture destination is occupied during recovery"
        );
        fs::rename(previous, &active)?;
    }
    if let Some(staged) = &journal.staged {
        let path = profile.join(staged);
        if path.try_exists()? && !retire_owned(root, &path)? {
            preserved.push(path);
        }
    }
    if current != journal.old_config.as_bytes() {
        atomic_write(&config_path, journal.old_config.as_bytes())?;
    }
    fs::remove_file(directory(root)?.join("pending.json"))?;
    Ok(preserved)
}

pub fn activate(
    root: &Path,
    profile: &Path,
    id: Option<&str>,
    preserve_unmanaged: bool,
    check_idle: impl Fn() -> Result<()>,
) -> Result<Activation> {
    check_idle()?;
    ensure!(
        journal(root)?.is_none(),
        "Recover the interrupted texture switch first"
    );
    validate_profile(profile)?;
    let active = profile.join("texture_replacements");
    ensure_safe_path(&active)?;
    let marker = if active.try_exists()? {
        read_marker(&active)?
    } else {
        None
    };
    let (before, mut config) = read_config(profile)?;
    let previous_setting = config.get(SETTING).and_then(serde_json::Value::as_bool);
    let original_setting = marker
        .as_ref()
        .map_or(previous_setting, |m| m.original_setting);
    let transaction = hex::encode(rand::random::<[u8; 32]>());
    let mut copied_files = 0;
    let mut previous = None;
    let mut original = marker.as_ref().and_then(|m| m.original.clone());
    let previous_owned = marker.is_some();
    if active.try_exists()? {
        ensure!(active.is_dir(), "texture_replacements is not a directory");
        let unmanaged = marker.is_none() && fs::read_dir(&active)?.next().transpose()?.is_some();
        ensure!(
            !unmanaged || preserve_unmanaged,
            "Existing unmanaged textures need explicit permission to preserve them before switching"
        );
        let name = owned_name(if marker.is_some() {
            "previous"
        } else {
            "original"
        });
        previous = Some(name.clone());
        if marker.is_none() {
            original = Some(name);
        }
    }
    let mut staged = None;
    let mut staging_guard = None;
    let mut restored_original = None;
    if let Some(id) = id {
        let manifest = load_manifest(root, id)?;
        let source = pack_path(root, id)?;
        verify_files(&source, &manifest)?;
        let stage = tempfile::Builder::new()
            .prefix(".twilight-sync-stage-")
            .tempdir_in(profile)?;
        let images = stage.path().join("GZ2");
        fs::create_dir(&images)?;
        for file in &manifest.content.files {
            let from = source.join(&file.name);
            let to = images.join(&file.name);
            ensure_safe_path(&from)?;
            match fs::hard_link(&from, &to) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::Unsupported
                        || matches!(error.raw_os_error(), Some(1 | 17 | 50)) =>
                {
                    ensure!(
                        fs2::available_space(&images)? > file.size + 64 * 1024 * 1024,
                        "Insufficient space for cross-volume texture activation"
                    );
                    let hash = copy_image(File::open(&from)?, &to, file.size)?;
                    ensure!(hash == file.hash, "Texture changed during activation");
                    copied_files += 1;
                }
                Err(error) => return Err(error).context("Linking installed textures"),
            }
        }
        let value = Marker {
            version: 1,
            pack: id.into(),
            transaction: transaction.clone(),
            original: original.clone(),
            original_setting,
        };
        atomic_write(&stage.path().join(MARKER), &serde_json::to_vec(&value)?)?;
        staged = Some(
            stage
                .path()
                .file_name()
                .and_then(|s| s.to_str())
                .context("Invalid texture staging name")?
                .to_owned(),
        );
        staging_guard = Some(stage);
        config.insert(SETTING.into(), serde_json::Value::Bool(true));
    } else {
        ensure!(marker.is_some(), "No managed texture pack is active");
        if let Some(name) = &original {
            let path = profile.join(name);
            ensure_safe_path(&path)?;
            ensure!(path.is_dir(), "Preserved original textures are missing");
            restored_original = Some(name.clone());
        }
        if let Some(value) = original_setting {
            config.insert(SETTING.into(), serde_json::Value::Bool(value));
        } else {
            config.remove(SETTING);
        }
    }
    let after = serde_json::to_vec_pretty(&config)?;
    let record = Journal {
        version: 1,
        profile: profile.into(),
        transaction,
        staged: staged.clone(),
        previous: previous.clone(),
        previous_owned,
        old_config: String::from_utf8(before.clone())?,
        new_config_hash: sha256(&after),
        new_pack: id.map(str::to_owned),
        restored_original: restored_original.clone(),
    };
    let journal_path = directory(root)?.join("pending.json");
    atomic_write(&journal_path, &serde_json::to_vec(&record)?)?;
    if let Some(stage) = staging_guard {
        let _owned_path = stage.keep();
    }
    let result = (|| -> Result<()> {
        check_idle()?;
        ensure!(
            bounded_read(&profile.join("config.json"), MAX_CONFIG)? == before,
            "Dusklight config changed during staging"
        );
        if let Some(name) = &previous {
            fs::rename(&active, profile.join(name))?;
        }
        if let Some(name) = staged.as_ref().or(restored_original.as_ref()) {
            fs::rename(profile.join(name), &active)?;
        }
        ensure!(
            bounded_read(&profile.join("config.json"), MAX_CONFIG)? == before,
            "Dusklight config changed before activation commit"
        );
        atomic_write(&profile.join("config.json"), &after)?;
        Ok(())
    })();
    if let Err(error) = result {
        return match recover(root, check_idle) {
            Ok(_) => Err(error).context("Texture switch failed; previous state restored"),
            Err(recovery) => {
                Err(error).context(format!("Texture recovery is pending: {recovery:#}"))
            }
        };
    }
    fs::remove_file(&journal_path)?;
    let mut preserved = Vec::new();
    if let Some(name) = previous {
        let path = profile.join(name);
        if !previous_owned || !retire_owned(root, &path)? {
            preserved.push(path);
        }
    }
    Ok(Activation {
        active: id.map(str::to_owned),
        copied_files,
        preserved,
    })
}

#[cfg(windows)]
pub fn open_creator(id: &str) -> Result<()> {
    let entry = catalog(id)?;
    let url = entry
        .creator_url
        .context("This project supports authorized local imports only")?;
    let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    std::process::Command::new(PathBuf::from(windows).join("explorer.exe"))
        .arg(url)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
pub fn open_creator(_: &str) -> Result<()> {
    bail!("Opening creator downloads currently requires Windows")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};

    #[test]
    fn tp_definitive_edition_plus_has_the_requested_version_and_supports_switching() -> Result<()> {
        let project = catalog("tp-definitive-edition-plus")?;
        assert_eq!(project.name, "TP Definitive Edition+");
        assert_eq!(project.suggested_version, Some("1.2.0"));
        assert_eq!(
            project.creator_url,
            Some("https://gamebanana.com/mods/676609")
        );
        let temp = tempfile::tempdir()?;
        let profile = profile(temp.path())?;
        let pack = import(
            temp.path(),
            project.id,
            project
                .suggested_version
                .context("Missing version preset")?,
            &source(temp.path())?,
            |_, _| {},
        )?;
        assert_eq!(pack.source, project.id);
        assert_eq!(pack.version, "1.2.0");
        activate(temp.path(), &profile, Some(&pack.id), false, || Ok(()))?;
        assert_eq!(
            inventory(temp.path(), Some(&profile))?.active.as_deref(),
            Some(pack.id.as_str())
        );
        activate(temp.path(), &profile, None, false, || Ok(()))?;
        assert_eq!(
            read_config(&profile)?.1.get(SETTING),
            Some(&serde_json::Value::Bool(false))
        );
        Ok(())
    }

    pub(crate) fn png() -> Result<Vec<u8>> {
        fn chunk(output: &mut Vec<u8>, name: &[u8; 4], bytes: &[u8]) -> Result<()> {
            output.extend_from_slice(&u32::try_from(bytes.len())?.to_be_bytes());
            output.extend_from_slice(name);
            output.extend_from_slice(bytes);
            let mut crc = crc32fast::Hasher::new();
            crc.update(name);
            crc.update(bytes);
            output.extend_from_slice(&crc.finalize().to_be_bytes());
            Ok(())
        }
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(
            &mut bytes,
            b"IHDR",
            &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0],
        )?;
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&[0, 255, 0, 0, 255])?;
        chunk(&mut bytes, b"IDAT", &encoder.finish()?)?;
        chunk(&mut bytes, b"IEND", &[])?;
        Ok(bytes)
    }

    pub(crate) fn source(root: &Path) -> Result<PathBuf> {
        let source = root.join("source");
        fs::create_dir(&source)?;
        atomic_write(&source.join("tex1_16x16_0123456789abcdef_14.png"), &png()?)?;
        atomic_write(
            &source.join("bundled-emulator.exe"),
            b"MZ do not install or execute",
        )?;
        Ok(source)
    }

    fn profile(root: &Path) -> Result<PathBuf> {
        let profile = root.join("profile");
        fs::create_dir(&profile)?;
        atomic_write(&profile.join("config.json"), br#"{"game.volume":42,"game.enableTextureReplacements":false,"unknown.preference":"keep"}"#)?;
        Ok(profile)
    }

    #[test]
    fn imports_only_valid_data_and_deduplicates_identical_reimports() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = source(temp.path())?;
        let first = import(
            temp.path(),
            "henriko-4k",
            "test version",
            &source,
            |_, _| {},
        )?;
        let second = import(
            temp.path(),
            "henriko-4k",
            "another label",
            &source,
            |_, _| {},
        )?;
        assert_eq!(first.id, second.id);
        assert_eq!(first.files, 1);
        assert_eq!(first.skipped, 1);
        assert!(
            !pack_path(temp.path(), &first.id)?
                .join("bundled-emulator.exe")
                .try_exists()?
        );
        assert_eq!(inventory(temp.path(), None)?.packs.len(), 1);
        Ok(())
    }

    #[test]
    fn equivalent_numeric_filename_aliases_cannot_hide_conflicting_variants() -> Result<()> {
        assert_eq!(
            texture_key("tex1_016x016_000000000000000a_014.png")?,
            texture_key("tex1_16x16_a_14.dds")?
        );
        assert!(texture_key("tex1_16x16_+a_14.png")?.is_none());
        Ok(())
    }

    #[test]
    fn dolphin_only_material_maps_are_reported_as_skipped_not_installed() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let folder = source(temp.path())?;
        atomic_write(
            &folder.join("tex1_16x16_0123456789abcdef_14_nrm.png"),
            b"unsupported material map",
        )?;
        let pack = import(
            temp.path(),
            "henriko-4k",
            "native selection",
            &folder,
            |_, _| {},
        )?;
        assert_eq!(pack.files, 1);
        assert_eq!(pack.skipped, 2);
        Ok(())
    }

    #[test]
    fn switches_between_installed_versions_without_damaging_the_library() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let profile = profile(temp.path())?;
        let source = source(temp.path())?;
        let first = import(temp.path(), "henriko-4k", "4K", &source, |_, _| {})?;
        let second = import(temp.path(), "henriko-1080p", "1080p", &source, |_, _| {})?;
        activate(temp.path(), &profile, Some(&first.id), false, || Ok(()))?;
        activate(temp.path(), &profile, Some(&second.id), false, || Ok(()))?;
        assert_eq!(
            inventory(temp.path(), Some(&profile))?.active.as_deref(),
            Some(second.id.as_str())
        );
        verify_files(
            &pack_path(temp.path(), &first.id)?,
            &load_manifest(temp.path(), &first.id)?,
        )?;
        activate(temp.path(), &profile, None, false, || Ok(()))?;
        assert_eq!(
            read_config(&profile)?.1.get(SETTING),
            Some(&serde_json::Value::Bool(false))
        );
        Ok(())
    }

    #[test]
    fn interrupted_folder_and_config_switch_recovers_the_exact_original_state() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let profile = profile(temp.path())?;
        let active = profile.join("texture_replacements");
        fs::create_dir(&active)?;
        atomic_write(&active.join("original.txt"), b"user data survives")?;
        let original_config = fs::read(profile.join("config.json"))?;
        let pack = import(
            temp.path(),
            "local-hd",
            "test",
            &source(temp.path())?,
            |_, _| {},
        )?;
        let result = activate(temp.path(), &profile, Some(&pack.id), true, || Ok(()))?;
        let marker = read_marker(&active)?.context("Missing managed marker")?;
        let original = result
            .preserved
            .first()
            .context("Original was not preserved")?
            .file_name()
            .and_then(|s| s.to_str())
            .context("Invalid original name")?
            .to_owned();
        let record = Journal {
            version: 1,
            profile: profile.clone(),
            transaction: marker.transaction,
            staged: Some(".twilight-sync-stage-moved".into()),
            previous: Some(original),
            previous_owned: false,
            old_config: String::from_utf8(original_config.clone())?,
            new_config_hash: sha256(&fs::read(profile.join("config.json"))?),
            new_pack: Some(pack.id),
            restored_original: None,
        };
        atomic_write(
            &directory(temp.path())?.join("pending.json"),
            &serde_json::to_vec(&record)?,
        )?;
        assert!(inventory(temp.path(), Some(&profile))?.recovery_needed);
        recover(temp.path(), || Ok(()))?;
        assert_eq!(fs::read(profile.join("config.json"))?, original_config);
        assert_eq!(
            fs::read(active.join("original.txt"))?,
            b"user data survives"
        );
        assert!(!inventory(temp.path(), Some(&profile))?.recovery_needed);
        Ok(())
    }

    #[test]
    fn game_starting_during_staging_leaves_a_recoverable_journal_without_applying() -> Result<()> {
        use std::cell::Cell;
        let temp = tempfile::tempdir()?;
        let profile = profile(temp.path())?;
        let before = fs::read(profile.join("config.json"))?;
        let pack = import(
            temp.path(),
            "local-hd",
            "test",
            &source(temp.path())?,
            |_, _| {},
        )?;
        let calls = Cell::new(0);
        let check = || {
            let call = calls.get();
            calls.set(call + 1);
            ensure!(call == 0, "external game started");
            Ok(())
        };
        assert!(activate(temp.path(), &profile, Some(&pack.id), false, check).is_err());
        assert_eq!(fs::read(profile.join("config.json"))?, before);
        assert!(!profile.join("texture_replacements").try_exists()?);
        recover(temp.path(), || Ok(()))?;
        assert!(!inventory(temp.path(), Some(&profile))?.recovery_needed);
        Ok(())
    }

    #[test]
    fn validates_dds_payload_size_and_rejects_trailing_attachments() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("texture.dds");
        let mut bytes = vec![0; 136];
        bytes[..4].copy_from_slice(b"DDS ");
        for (offset, value) in [(4, 124_u32), (12, 4), (16, 4), (28, 1), (76, 32), (80, 4)] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[84..88].copy_from_slice(b"DXT1");
        atomic_write(&path, &bytes)?;
        validate_image(&path)?;
        bytes.extend_from_slice(b"MZ");
        atomic_write(&path, &bytes)?;
        assert!(validate_image(&path).is_err());
        Ok(())
    }

    #[test]
    fn switches_real_config_preserves_unmanaged_textures_and_restores_original_setting()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let profile = profile(temp.path())?;
        let original = profile.join("texture_replacements");
        fs::create_dir(&original)?;
        atomic_write(&original.join("my-existing-note.txt"), b"must survive")?;
        let pack = import(
            temp.path(),
            "henriko-4k",
            "v4",
            &source(temp.path())?,
            |_, _| {},
        )?;
        assert!(activate(temp.path(), &profile, Some(&pack.id), false, || Ok(())).is_err());
        let activated = activate(temp.path(), &profile, Some(&pack.id), true, || Ok(()))?;
        assert_eq!(activated.preserved.len(), 1);
        assert_eq!(
            read_config(&profile)?.1.get(SETTING),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            read_config(&profile)?.1.get("unknown.preference"),
            Some(&serde_json::Value::String("keep".into()))
        );
        assert!(
            original
                .join("GZ2")
                .join("tex1_16x16_0123456789abcdef_14.png")
                .is_file()
        );
        activate(temp.path(), &profile, None, false, || Ok(()))?;
        assert_eq!(
            fs::read(original.join("my-existing-note.txt"))?,
            b"must survive"
        );
        assert_eq!(
            read_config(&profile)?.1.get(SETTING),
            Some(&serde_json::Value::Bool(false))
        );
        assert!(read_marker(&original)?.is_none());
        Ok(())
    }

    #[test]
    fn an_external_game_or_tampered_pack_cannot_replace_active_data() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let profile = profile(temp.path())?;
        let pack = import(
            temp.path(),
            "local-hd",
            "authorized import",
            &source(temp.path())?,
            |_, _| {},
        )?;
        let before = fs::read(profile.join("config.json"))?;
        assert!(
            activate(temp.path(), &profile, Some(&pack.id), false, || bail!(
                "game is running"
            ))
            .is_err()
        );
        assert_eq!(fs::read(profile.join("config.json"))?, before);
        let image = pack_path(temp.path(), &pack.id)?.join("tex1_16x16_0123456789abcdef_14.png");
        atomic_write(&image, b"corrupt")?;
        assert!(activate(temp.path(), &profile, Some(&pack.id), false, || Ok(())).is_err());
        assert!(!profile.join("texture_replacements").try_exists()?);
        Ok(())
    }

    #[test]
    fn refuses_zip_traversal_and_resolves_aliases_like_the_native_loader() -> Result<()> {
        use zip::{ZipWriter, write::SimpleFileOptions};
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("bad.zip");
        let mut writer = ZipWriter::new(File::create(&archive)?);
        writer.start_file(
            "../tex1_16x16_0123456789abcdef_14.png",
            SimpleFileOptions::default(),
        )?;
        writer.write_all(&png()?)?;
        writer.finish()?;
        assert!(import(temp.path(), "local-hd", "test", &archive, |_, _| {}).is_err());
        let source = source(temp.path())?;
        atomic_write(
            &source.join("tex1_16x16_m_0123456789abcdef_14.png"),
            &png()?,
        )?;
        let pack = import(temp.path(), "local-hd", "test", &source, |_, _| {})?;
        assert_eq!(pack.files, 1);
        assert_eq!(pack.skipped, 2);
        Ok(())
    }

    #[test]
    fn category_duplicates_use_native_component_order_and_safe_data_extensions() -> Result<()> {
        let root = tempfile::tempdir()?;
        let folder = root.path().join("source");
        let first = folder.join("1 - Main");
        let alternative = folder.join("6 - Alt");
        fs::create_dir_all(&first)?;
        fs::create_dir_all(&alternative)?;
        let name = "tex1_16x16_0123456789abcdef_14.png";
        atomic_write(&first.join(name), &png()?)?;
        atomic_write(&alternative.join(name), b"unused alternate must not win")?;
        let pack = import(root.path(), "local-hd", "test", &folder, |_, _| {})?;
        assert_eq!(pack.files, 1);
        assert_eq!(pack.skipped, 1);
        assert_eq!(
            fs::read(pack_path(root.path(), &pack.id)?.join(name))?,
            png()?
        );
        assert!(native_order(&["a".into(), "z".into()], &["A".into(), "a".into()]).is_gt());
        Ok(())
    }

    #[test]
    fn profile_preparation_preserves_settings_and_cancelled_imports_publish_nothing() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let profile = profile(root.path())?;
        let before = fs::read(profile.join("config.json"))?;
        prepare_profile(&profile, || Ok(()))?;
        assert_eq!(fs::read(profile.join("config.json"))?, before);
        let input = source(root.path())?;
        assert!(
            import_cancellable(
                root.path(),
                "local-hd",
                "test",
                &input,
                Arc::new(AtomicBool::new(true)),
                |_, _| {}
            )
            .is_err()
        );
        assert!(inventory(root.path(), Some(&profile))?.packs.is_empty());
        image_dimensions(16384, 4096)?;
        assert!(image_dimensions(16385, 1).is_err());
        assert!(image_dimensions(16384, 4097).is_err());
        Ok(())
    }

    #[test]
    #[ignore = "Read-only source archive; imports and activates only in an isolated temporary profile"]
    fn supplied_tpde_archive_installs_and_switches_without_manual_extraction() -> Result<()> {
        let path = std::env::var_os("TWILIGHT_TEXTURE_TEST_ARCHIVE")
            .context("Missing authorized archive path")?;
        let input = Path::new(&path);
        let before = input.metadata()?;
        let root = tempfile::tempdir()?;
        let profile = profile(root.path())?;
        let original_config = fs::read(profile.join("config.json"))?;
        let pack = import(
            root.path(),
            "tp-definitive-edition-plus",
            "1.2.0",
            input,
            |_, _| {},
        )?;
        println!(
            "Selected native textures: {}; bytes: {}; skipped: {}",
            pack.files, pack.bytes, pack.skipped
        );
        ensure!(
            pack.files == 5_456 && pack.bytes == 8_622_649_300,
            "Unexpected TPDE archive coverage"
        );
        ensure!(
            pack.skipped > 50,
            "Expected deterministic duplicate/non-texture handling"
        );
        let location = pack_path(root.path(), &pack.id)?;
        ensure!(
            location
                .join("tex1_128x32_d1873c36067db6f1_14.dds")
                .is_file(),
            "Mislabeled DDS was not normalized"
        );
        ensure!(
            location
                .join("tex1_512x256_m_4452fdd502cef8f3_14.dds")
                .metadata()?
                .len()
                == 178_957_100,
            "Large native texture was not preserved"
        );
        activate(root.path(), &profile, Some(&pack.id), false, || Ok(()))?;
        ensure!(
            inventory(root.path(), Some(&profile))?.active.as_deref() == Some(&pack.id),
            "Pack was not activated"
        );
        activate(root.path(), &profile, None, false, || Ok(()))?;
        ensure!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(profile.join("config.json"))?)?
                == serde_json::from_slice::<serde_json::Value>(&original_config)?,
            "Original game settings changed"
        );
        ensure!(
            input.metadata()?.len() == before.len()
                && input.metadata()?.modified()? == before.modified()?,
            "Source ZIP was changed"
        );
        println!(
            "Real TPDE archive: {} selected textures, {} bytes, {} non-selected entries; isolated install/activate/disable passed",
            pack.files, pack.bytes, pack.skipped
        );
        Ok(())
    }

    #[test]
    fn image_validation_rejects_crc_damage_trailing_executables_and_fake_headers() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("image.png");
        let valid = png()?;
        atomic_write(&path, &valid)?;
        validate_image(&path)?;
        let mut bad = valid.clone();
        bad[29] ^= 1;
        atomic_write(&path, &bad)?;
        assert!(validate_image(&path).is_err());
        let mut trailing = valid;
        trailing.extend_from_slice(b"MZ arbitrary attachment");
        atomic_write(&path, &trailing)?;
        assert!(validate_image(&path).is_err());
        atomic_write(&path, b"MZ fake png")?;
        assert!(validate_image(&path).is_err());
        Ok(())
    }
}

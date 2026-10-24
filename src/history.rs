use crate::files::{self, SAVE_SIZE, ensure_safe_path, sha256, valid_hash};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const BUDGET: u64 = 64 * 1024 * 1024;
pub const BLOCK_SIZE: usize = 4096;
const MAX_COMMIT: u64 = 64 * 1024;
const MAX_INDEX: u64 = 4 * 1024 * 1024;
const PAGE_SIZE: usize = 50;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Observed,
    BeforeReceive,
    Received,
    BeforeRestore,
    Restored,
    LegacyBackup,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Observed => "Stable local save",
            Self::BeforeReceive => "Before incoming sync",
            Self::Received => "Received from partner",
            Self::BeforeRestore => "Before restore",
            Self::Restored => "Restored snapshot",
            Self::LegacyBackup => "Migrated backup",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Block {
    hash: String,
    length: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Commit {
    version: u8,
    sequence: u64,
    parent: Option<String>,
    stream: String,
    game: String,
    name: String,
    hash: String,
    blocks: Vec<Block>,
    timestamp: String,
    username: String,
    device: String,
    source: PathBuf,
    reason: Reason,
    peer: Option<String>,
    restored_from: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    id: String,
    stream: String,
    sequence: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u8,
    next_sequence: u64,
    pruned: u64,
    entries: Vec<Reference>,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            version: 1,
            next_sequence: 1,
            pruned: 0,
            entries: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub sequence: u64,
    pub parent: Option<String>,
    pub game: String,
    pub name: String,
    pub hash: String,
    pub timestamp: String,
    pub username: String,
    pub reason: Reason,
    pub restored_from: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Stats {
    pub commits: usize,
    pub blocks: usize,
    pub stored_bytes: u64,
    pub logical_bytes: u64,
    pub budget: u64,
    pub pruned: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            commits: 0,
            blocks: 0,
            stored_bytes: 0,
            logical_bytes: 0,
            budget: BUDGET,
            pruned: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub next: Option<String>,
    pub stats: Stats,
    pub notice: Option<String>,
}

pub struct Capture {
    pub id: String,
    pub added: bool,
    pub pruned: u64,
}

struct Prepared {
    reference: Reference,
    added: bool,
}

struct Accounting {
    bytes: u64,
    blocks: Vec<String>,
}

pub struct History {
    root: PathBuf,
    username: String,
    device: String,
    index: Index,
    budget: u64,
    accounting: HashMap<String, Accounting>,
    object_sizes: HashMap<String, u64>,
    dirty: bool,
}

fn valid_id(value: &str) -> bool {
    valid_hash(value) && !value.bytes().any(|b| b.is_ascii_uppercase())
}

fn stream_id(identity: &files::SaveIdentity) -> String {
    sha256(format!("twilight-history/v1|{}|{}", identity.game, identity.name).as_bytes())
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    ensure_safe_path(path)?;
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= maximum,
        "History file is invalid or oversized"
    );
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= maximum,
        "History file grew while reading"
    );
    Ok(bytes)
}

fn immutable_write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_safe_path(path)?;
    if path.try_exists()? {
        ensure!(
            read_bounded(path, bytes.len() as u64)? == bytes,
            "Immutable history object differs from its digest"
        );
        return Ok(());
    }
    let mut temporary =
        NamedTempFile::new_in(path.parent().context("History object has no directory")?)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(path).map_err(|e| e.error)?;
    Ok(())
}

impl History {
    pub fn open(application_root: &Path, username: &str, device: &str) -> Result<Self> {
        ensure!(valid_id(device), "Invalid snapshot device identity");
        ensure!(
            !username.is_empty() && username.len() <= 256,
            "Invalid snapshot user"
        );
        let root = application_root.join("history");
        for path in [&root, &root.join("objects"), &root.join("commits")] {
            ensure_safe_path(path)?;
            fs::create_dir_all(path)?;
        }
        let index_path = root.join("index.json");
        let index = if index_path.try_exists()? {
            serde_json::from_slice(&read_bounded(&index_path, MAX_INDEX)?)?
        } else {
            for name in ["objects", "commits"] {
                ensure!(
                    fs::read_dir(root.join(name))?.next().transpose()?.is_none(),
                    "Snapshot index is missing; existing history was preserved for recovery"
                );
            }
            let index = Index::default();
            files::atomic_write(&index_path, &serde_json::to_vec(&index)?)?;
            index
        };
        let mut history = Self {
            root,
            username: username.into(),
            device: device.into(),
            index,
            budget: BUDGET,
            accounting: HashMap::new(),
            object_sizes: HashMap::new(),
            dirty: false,
        };
        history.validate_index()?;
        for reference in history.index.entries.clone() {
            let commit = history.commit(&reference.id)?;
            let size = history.commit_path(&reference.id)?.metadata()?.len();
            history.account(&reference.id, &commit, size)?;
        }
        history.collect_garbage()?;
        Ok(history)
    }

    fn object_path(&self, id: &str) -> Result<PathBuf> {
        ensure!(valid_id(id), "Invalid snapshot block ID");
        Ok(self.root.join("objects").join(format!("{id}.z")))
    }

    fn commit_path(&self, id: &str) -> Result<PathBuf> {
        ensure!(valid_id(id), "Invalid snapshot commit ID");
        Ok(self.root.join("commits").join(format!("{id}.json")))
    }

    fn commit(&self, id: &str) -> Result<Commit> {
        let bytes = read_bounded(&self.commit_path(id)?, MAX_COMMIT)?;
        ensure!(
            sha256(&bytes) == id,
            "Snapshot commit integrity check failed"
        );
        let commit: Commit = serde_json::from_slice(&bytes)?;
        ensure!(
            commit.version == 1 && valid_id(&commit.stream) && valid_id(&commit.hash),
            "Invalid snapshot commit"
        );
        ensure!(
            commit.parent.as_deref().is_none_or(valid_id)
                && commit.restored_from.as_deref().is_none_or(valid_id),
            "Invalid snapshot ancestry"
        );
        ensure!(
            valid_id(&commit.device) && commit.peer.as_deref().is_none_or(valid_id),
            "Invalid snapshot attribution"
        );
        ensure!(
            commit.username.len() <= 256 && commit.name.len() <= 31 && commit.game.len() == 4,
            "Invalid snapshot metadata"
        );
        ensure!(
            commit.blocks.len() == SAVE_SIZE.div_ceil(BLOCK_SIZE),
            "Invalid snapshot block count"
        );
        for (index, block) in commit.blocks.iter().enumerate() {
            let expected = (SAVE_SIZE - index * BLOCK_SIZE).min(BLOCK_SIZE);
            ensure!(
                valid_id(&block.hash) && block.length == expected,
                "Invalid snapshot block reference"
            );
        }
        let identity = files::SaveIdentity {
            game: commit.game.clone(),
            name: commit.name.clone(),
        };
        ensure!(
            stream_id(&identity) == commit.stream,
            "Snapshot stream identity differs"
        );
        Ok(commit)
    }

    fn validate_index(&self) -> Result<()> {
        ensure!(
            self.index.version == 1 && self.index.entries.len() <= 50_000,
            "Unsupported or oversized snapshot index"
        );
        ensure!(
            self.index.next_sequence > 0 && self.index.next_sequence < u64::MAX,
            "Invalid snapshot sequence counter"
        );
        let mut ids = HashSet::new();
        let mut heads: HashMap<&str, &str> = HashMap::new();
        let mut previous_sequence = 0;
        for reference in &self.index.entries {
            ensure!(
                valid_id(&reference.id) && valid_id(&reference.stream) && ids.insert(&reference.id),
                "Invalid or duplicate snapshot reference"
            );
            ensure!(
                reference.sequence > previous_sequence
                    && reference.sequence < self.index.next_sequence,
                "Snapshot sequence is not monotonic"
            );
            let commit = self.commit(&reference.id)?;
            ensure!(
                commit.sequence == reference.sequence && commit.stream == reference.stream,
                "Snapshot index differs from commit"
            );
            if let Some(parent) = heads.get(reference.stream.as_str()) {
                ensure!(
                    commit.parent.as_deref() == Some(*parent),
                    "Snapshot parent chain is broken"
                );
            } else if self.index.pruned == 0 {
                ensure!(
                    commit.parent.is_none(),
                    "Unexpected missing snapshot parent"
                );
            }
            heads.insert(&reference.stream, &reference.id);
            previous_sequence = reference.sequence;
        }
        Ok(())
    }

    fn block(&self, block: &Block) -> Result<Vec<u8>> {
        let compressed = read_bounded(&self.object_path(&block.hash)?, (BLOCK_SIZE + 128) as u64)?;
        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut bytes = Vec::with_capacity(block.length);
        decoder
            .by_ref()
            .take((BLOCK_SIZE + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() == block.length && decoder.total_in() == compressed.len() as u64,
            "Snapshot block length or compression stream differs"
        );
        ensure!(
            sha256(&bytes) == block.hash,
            "Snapshot block SHA-256 mismatch"
        );
        Ok(bytes)
    }

    pub fn read(&self, id: &str) -> Result<Vec<u8>> {
        ensure!(
            self.index.entries.iter().any(|entry| entry.id == id),
            "Snapshot is not retained in this history"
        );
        let commit = self.commit(id)?;
        let mut bytes = Vec::with_capacity(SAVE_SIZE);
        for block in &commit.blocks {
            bytes.extend_from_slice(&self.block(block)?);
        }
        ensure!(
            sha256(&bytes) == commit.hash,
            "Reconstructed snapshot SHA-256 mismatch"
        );
        let identity = files::validate_save(&bytes)?;
        ensure!(
            identity.game == commit.game && identity.name == commit.name,
            "Reconstructed save identity differs"
        );
        Ok(bytes)
    }

    fn account(&mut self, id: &str, commit: &Commit, size: u64) -> Result<()> {
        let mut blocks = Vec::new();
        for block in &commit.blocks {
            if !self.object_sizes.contains_key(&block.hash) {
                let path = self.object_path(&block.hash)?;
                ensure_safe_path(&path)?;
                self.object_sizes
                    .insert(block.hash.clone(), path.metadata()?.len());
            }
            blocks.push(block.hash.clone());
        }
        self.accounting.insert(
            id.into(),
            Accounting {
                bytes: size,
                blocks,
            },
        );
        Ok(())
    }

    fn prepare(
        &mut self,
        bytes: &[u8],
        source: &Path,
        reason: Reason,
        peer: Option<&str>,
        restored_from: Option<&str>,
        timestamp: String,
    ) -> Result<Prepared> {
        let identity = files::validate_save(bytes)?;
        ensure_safe_path(source)?;
        ensure!(
            peer.is_none_or(valid_id) && restored_from.is_none_or(valid_id),
            "Invalid snapshot provenance"
        );
        let stream = stream_id(&identity);
        let hash = sha256(bytes);
        let previous = self
            .index
            .entries
            .iter()
            .rev()
            .find(|entry| entry.stream == stream)
            .cloned();
        if let Some(previous) = &previous
            && reason != Reason::Restored
            && self.commit(&previous.id)?.hash == hash
        {
            // Verify the existing reconstruction before trusting deduplication as a backup.
            ensure!(
                self.read(&previous.id)? == bytes,
                "Existing snapshot differs from current save"
            );
            return Ok(Prepared {
                reference: previous.clone(),
                added: false,
            });
        }
        self.dirty = true;
        let mut blocks = Vec::new();
        for bytes in bytes.chunks(BLOCK_SIZE) {
            let hash = sha256(bytes);
            let block = Block {
                hash,
                length: bytes.len(),
            };
            let path = self.object_path(&block.hash)?;
            if path.try_exists()? {
                ensure!(
                    self.block(&block)? == bytes,
                    "Existing shared block is corrupt"
                );
            } else {
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(bytes)?;
                immutable_write(&path, &encoder.finish()?)?;
            }
            blocks.push(block);
        }
        let commit = Commit {
            version: 1,
            sequence: self.index.next_sequence,
            parent: previous.map(|entry| entry.id),
            stream: stream.clone(),
            game: identity.game,
            name: identity.name,
            hash,
            blocks,
            timestamp,
            username: self.username.clone(),
            device: self.device.clone(),
            source: source.into(),
            reason,
            peer: peer.map(str::to_owned),
            restored_from: restored_from.map(str::to_owned),
        };
        let bytes = serde_json::to_vec(&commit)?;
        ensure!(
            bytes.len() as u64 <= MAX_COMMIT,
            "Snapshot metadata exceeds limit"
        );
        let id = sha256(&bytes);
        immutable_write(&self.commit_path(&id)?, &bytes)?;
        self.account(&id, &commit, bytes.len() as u64)?;
        Ok(Prepared {
            reference: Reference {
                id,
                stream,
                sequence: commit.sequence,
            },
            added: true,
        })
    }

    fn publish(&mut self, prepared: Prepared) -> Result<Capture> {
        if !prepared.added {
            return Ok(Capture {
                id: prepared.reference.id,
                added: false,
                pruned: 0,
            });
        }
        let id = prepared.reference.id.clone();
        let mut next = self.index.clone();
        next.next_sequence = next
            .next_sequence
            .checked_add(1)
            .context("Snapshot sequence overflow")?;
        next.entries.push(prepared.reference);
        let mut removed = 0;
        loop {
            let cost = self.cost(&next)?.stored_bytes;
            if cost <= self.budget
                && next.entries.len() <= 50_000
                && serde_json::to_vec(&next)?.len() as u64 <= MAX_INDEX
            {
                break;
            }
            ensure!(
                next.entries.len() > 1,
                "Snapshot budget cannot hold the newest valid save"
            );
            next.entries.remove(0);
            next.pruned = next
                .pruned
                .checked_add(1)
                .context("Snapshot prune counter overflow")?;
            removed += 1;
        }
        let bytes = serde_json::to_vec(&next)?;
        ensure!(
            bytes.len() as u64 <= MAX_INDEX,
            "Snapshot index exceeds limit"
        );
        files::atomic_write(&self.root.join("index.json"), &bytes)?;
        self.index = next;
        if removed > 0 {
            self.prune_cached_objects()?;
        }
        self.dirty = false;
        Ok(Capture {
            id,
            added: true,
            pruned: removed,
        })
    }

    pub fn capture(
        &mut self,
        bytes: &[u8],
        source: &Path,
        reason: Reason,
        peer: Option<&str>,
    ) -> Result<Capture> {
        self.clean_if_dirty()?;
        let prepared = self.prepare(
            bytes,
            source,
            reason,
            peer,
            None,
            OffsetDateTime::now_utc().format(&Rfc3339)?,
        )?;
        self.publish(prepared)
    }

    pub fn replace_save(
        &mut self,
        path: &Path,
        bytes: &[u8],
        expected: Option<&str>,
        reason: Reason,
        peer: Option<&str>,
        restored_from: Option<&str>,
    ) -> Result<(String, Capture)> {
        let normalized = files::normalize_save(bytes)?;
        let bytes = normalized.as_slice();
        let identity = files::validate_save(bytes)?;
        ensure!(
            files::save_hash(path)?.as_deref() == expected,
            "Save changed before snapshot-protected replacement"
        );
        if let Some(previous) = files::read_optional_save(path)? {
            ensure!(
                files::validate_save(&previous)? == identity,
                "Snapshot belongs to another game region or save"
            );
            let before = if reason == Reason::Restored {
                Reason::BeforeRestore
            } else {
                Reason::BeforeReceive
            };
            self.capture(&previous, path, before, peer)?;
        }
        self.clean_if_dirty()?;
        let prepared = self.prepare(
            bytes,
            path,
            reason,
            peer,
            restored_from,
            OffsetDateTime::now_utc().format(&Rfc3339)?,
        )?;
        ensure!(
            files::save_hash(path)?.as_deref() == expected,
            "Save changed while preparing its snapshot"
        );
        files::write_save(path, bytes, expected)?;
        let hash = sha256(bytes);
        ensure!(
            files::save_hash(path)?.as_deref() == Some(hash.as_str()),
            "Post-restore save verification failed"
        );
        let capture = self.publish(prepared)?;
        Ok((hash, capture))
    }

    pub fn restore(
        &mut self,
        id: &str,
        path: &Path,
        expected: Option<&str>,
    ) -> Result<(String, Capture)> {
        let bytes = self.read(id)?;
        self.replace_save(path, &bytes, expected, Reason::Restored, None, Some(id))
    }

    fn cost(&self, index: &Index) -> Result<Stats> {
        let mut stored = serde_json::to_vec(index)?.len() as u64;
        let mut objects = HashSet::new();
        for reference in &index.entries {
            let accounting = self
                .accounting
                .get(&reference.id)
                .context("Missing snapshot storage accounting")?;
            stored += accounting.bytes;
            objects.extend(accounting.blocks.iter().cloned());
        }
        for id in &objects {
            stored += self
                .object_sizes
                .get(id)
                .context("Missing snapshot block accounting")?;
        }
        Ok(Stats {
            commits: index.entries.len(),
            blocks: objects.len(),
            stored_bytes: stored,
            logical_bytes: index.entries.len() as u64 * SAVE_SIZE as u64,
            budget: self.budget,
            pruned: index.pruned,
        })
    }

    pub fn stats(&self) -> Result<Stats> {
        self.cost(&self.index)
    }

    pub fn ensure_ready(&mut self) -> Result<()> {
        self.clean_if_dirty()
    }

    pub fn page(&self, before: Option<&str>) -> Result<Page> {
        let end = match before {
            Some(id) => self
                .index
                .entries
                .iter()
                .position(|entry| entry.id == id)
                .context("History page anchor was pruned; return to Latest")?,
            None => self.index.entries.len(),
        };
        let start = end.saturating_sub(PAGE_SIZE);
        let mut entries = Vec::new();
        for reference in self.index.entries[start..end].iter().rev() {
            let commit = self.commit(&reference.id)?;
            entries.push(Entry {
                id: reference.id.clone(),
                sequence: commit.sequence,
                parent: commit.parent,
                game: commit.game,
                name: commit.name,
                hash: commit.hash,
                timestamp: commit.timestamp,
                username: commit.username,
                reason: commit.reason,
                restored_from: commit.restored_from,
            });
        }
        let next = if start > 0 {
            entries.last().map(|entry| entry.id.clone())
        } else {
            None
        };
        Ok(Page {
            entries,
            next,
            stats: self.stats()?,
            notice: None,
        })
    }

    fn collect_garbage(&self) -> Result<()> {
        let commits: HashSet<&str> = self
            .index
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        let mut blocks = HashSet::new();
        for reference in &self.index.entries {
            blocks.extend(
                self.accounting
                    .get(&reference.id)
                    .context("Missing snapshot accounting")?
                    .blocks
                    .iter()
                    .cloned(),
            );
        }
        for (directory, extension) in [("objects", "z"), ("commits", "json")] {
            let path = self.root.join(directory);
            ensure_safe_path(&path)?;
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let path = entry.path();
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !valid_id(stem) || path.extension().is_none_or(|e| e != extension) {
                    continue;
                }
                ensure_safe_path(&path)?;
                ensure!(
                    entry.file_type()?.is_file(),
                    "History object is not a regular file"
                );
                let keep = if directory == "objects" {
                    blocks.contains(stem)
                } else {
                    commits.contains(stem)
                };
                if !keep {
                    fs::remove_file(path)?;
                }
            }
        }
        Ok(())
    }

    fn retain_accounting(&mut self) {
        let ids: HashSet<&str> = self
            .index
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        self.accounting.retain(|id, _| ids.contains(id.as_str()));
        let blocks: HashSet<&str> = self
            .accounting
            .values()
            .flat_map(|entry| entry.blocks.iter().map(String::as_str))
            .collect();
        self.object_sizes
            .retain(|id, _| blocks.contains(id.as_str()));
    }

    fn clean_if_dirty(&mut self) -> Result<()> {
        if self.dirty {
            self.collect_garbage()?;
            self.retain_accounting();
            self.dirty = false;
        }
        Ok(())
    }

    fn prune_cached_objects(&mut self) -> Result<()> {
        let ids: HashSet<&str> = self
            .index
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        let removed: Vec<String> = self
            .accounting
            .keys()
            .filter(|id| !ids.contains(id.as_str()))
            .cloned()
            .collect();
        let mut blocks = HashSet::new();
        for entry in &self.index.entries {
            blocks.extend(
                self.accounting
                    .get(&entry.id)
                    .context("Missing retained snapshot accounting")?
                    .blocks
                    .iter()
                    .map(String::as_str),
            );
        }
        let unused: Vec<String> = self
            .object_sizes
            .keys()
            .filter(|id| !blocks.contains(id.as_str()))
            .cloned()
            .collect();
        for id in removed {
            let path = self.commit_path(&id)?;
            ensure_safe_path(&path)?;
            fs::remove_file(path)?;
        }
        for id in unused {
            let path = self.object_path(&id)?;
            ensure_safe_path(&path)?;
            fs::remove_file(path)?;
        }
        self.retain_accounting();
        Ok(())
    }

    pub fn migrate_backups(&mut self, directory: &Path) -> Result<usize> {
        ensure_safe_path(directory)?;
        if !directory.try_exists()? {
            return Ok(0);
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !valid_id(stem) || path.extension().is_none_or(|e| e != "gci") {
                continue;
            }
            ensure!(files.len() < 50_000, "Too many legacy backups to migrate");
            files.push((entry.metadata()?.modified()?, path));
        }
        files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut count = 0;
        for (modified, path) in files {
            let bytes = files::read_save(&path)?;
            let hash = sha256(&bytes);
            ensure!(
                path.file_stem().is_some_and(|s| s == hash.as_str()),
                "Legacy backup hash differs; file retained"
            );
            self.clean_if_dirty()?;
            let timestamp = OffsetDateTime::from(modified).format(&Rfc3339)?;
            let prepared =
                self.prepare(&bytes, &path, Reason::LegacyBackup, None, None, timestamp)?;
            let captured = self.publish(prepared)?;
            ensure!(
                self.read(&captured.id)? == bytes,
                "Legacy backup was not retained"
            );
            ensure!(
                files::sha256(&files::read_save(&path)?) == hash,
                "Legacy backup changed during migration"
            );
            fs::remove_file(&path)?;
            count += 1;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(root: &Path) -> Result<History> {
        History::open(root, "test-user", &"a".repeat(64))
    }

    fn version(marker: u8) -> Vec<u8> {
        let mut bytes = files::fixture_save();
        bytes[0x100] = marker;
        bytes
    }

    #[test]
    fn deduplicates_unchanged_content_and_reconstructs_exactly() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut history = open(temp.path())?;
        let source = temp.path().join("save.gci");
        let first = history.capture(&version(1), &source, Reason::Observed, None)?;
        let size = history.stats()?.stored_bytes;
        let same = history.capture(&version(1), &source, Reason::Observed, None)?;
        assert!(!same.added && first.id == same.id);
        assert_eq!(history.stats()?.stored_bytes, size);
        assert_eq!(history.read(&first.id)?, version(1));
        Ok(())
    }

    #[test]
    fn realistic_static_pages_are_shared_and_total_storage_is_measurably_small() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut history = open(temp.path())?;
        let source = temp.path().join("save.gci");
        let mut bytes = version(0);
        let mut random = 0x12345678_u32;
        for byte in &mut bytes[0x80..0x4000] {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            *byte = random.to_le_bytes()[0];
        }
        for marker in 0..100_u8 {
            bytes[0x100] = marker;
            history.capture(&bytes, &source, Reason::Observed, None)?;
        }
        let stats = history.stats()?;
        assert_eq!(stats.commits, 100);
        assert!(
            stats.blocks < 120,
            "blocks were not shared: {}",
            stats.blocks
        );
        assert!(
            stats.stored_bytes * 5 < stats.logical_bytes,
            "stored {} of {} bytes; expected at least 80% savings",
            stats.stored_bytes,
            stats.logical_bytes
        );
        let index_bytes = fs::metadata(history.root.join("index.json"))?.len();
        let mut actual = index_bytes;
        for name in ["objects", "commits"] {
            for entry in fs::read_dir(history.root.join(name))? {
                actual += entry?.metadata()?.len();
            }
        }
        assert_eq!(stats.stored_bytes, actual);
        println!(
            "100 save revisions: {} stored bytes versus {} full-copy bytes ({:.1}% saved)",
            stats.stored_bytes,
            stats.logical_bytes,
            100.0 * (1.0 - stats.stored_bytes as f64 / stats.logical_bytes as f64)
        );
        Ok(())
    }

    #[test]
    fn a_missing_index_never_discards_existing_history_objects() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut history = open(temp.path())?;
        let captured = history.capture(
            &version(1),
            &temp.path().join("save.gci"),
            Reason::Observed,
            None,
        )?;
        let commit_path = history.commit_path(&captured.id)?;
        fs::remove_file(history.root.join("index.json"))?;
        assert!(open(temp.path()).is_err());
        assert!(commit_path.try_exists()?);
        Ok(())
    }

    #[test]
    fn history_pages_cover_every_retained_commit_without_repeats() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut history = open(temp.path())?;
        for marker in 0..55_u8 {
            history.capture(
                &version(marker),
                &temp.path().join("save.gci"),
                Reason::Observed,
                None,
            )?;
        }
        let latest = history.page(None)?;
        assert_eq!(latest.entries.len(), 50);
        let older = history.page(latest.next.as_deref())?;
        assert_eq!(older.entries.len(), 5);
        assert!(older.next.is_none());
        let ids: HashSet<&str> = latest
            .entries
            .iter()
            .chain(&older.entries)
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(ids.len(), 55);
        Ok(())
    }

    #[test]
    fn pruning_is_bounded_and_all_retained_commits_still_restore() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut history = open(temp.path())?;
        history.budget = 12 * 1024;
        let source = temp.path().join("save.gci");
        for marker in 0..40_u8 {
            history.capture(&version(marker), &source, Reason::Observed, None)?;
        }
        let stats = history.stats()?;
        assert!(stats.pruned > 0 && stats.stored_bytes <= history.budget);
        for reference in &history.index.entries {
            files::validate_save(&history.read(&reference.id)?)?;
        }
        let reopened = open(temp.path())?;
        assert_eq!(reopened.index.pruned, history.index.pruned);
        Ok(())
    }

    #[test]
    fn restoring_head_to_an_empty_default_records_a_commit_and_reuses_all_blocks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut history = open(temp.path())?;
        let path = temp.path().join("save.gci");
        let first = history.capture(&version(1), &path, Reason::Observed, None)?;
        let before = history.stats()?;
        let (_, restored) = history.restore(&first.id, &path, None)?;
        let after = history.stats()?;
        assert!(restored.added && restored.id != first.id);
        assert_eq!(after.commits, before.commits + 1);
        assert_eq!(after.blocks, before.blocks);
        let page = history.page(None)?;
        assert_eq!(page.entries[0].reason, Reason::Restored);
        assert_eq!(
            page.entries[0].restored_from.as_deref(),
            Some(first.id.as_str())
        );
        assert_eq!(files::read_save(&path)?, version(1));
        Ok(())
    }

    #[test]
    fn restores_as_a_new_commit_and_keeps_the_replaced_save() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut history = open(temp.path())?;
        let path = temp.path().join("save.gci");
        let first = history.capture(&version(1), &path, Reason::Observed, None)?;
        files::atomic_write(&path, &version(2))?;
        let (hash, restored) = history.restore(&first.id, &path, Some(&sha256(&version(2))))?;
        assert_ne!(first.id, restored.id);
        assert_eq!(hash, sha256(&version(1)));
        assert_eq!(files::read_save(&path)?, version(1));
        let page = history.page(None)?;
        assert_eq!(page.entries[0].reason, Reason::Restored);
        assert_eq!(
            page.entries[0].restored_from.as_deref(),
            Some(first.id.as_str())
        );
        assert_eq!(page.entries[1].hash, sha256(&version(2)));
        assert_eq!(
            page.entries[0].parent.as_deref(),
            Some(page.entries[1].id.as_str())
        );
        Ok(())
    }

    #[test]
    fn rejects_tampering_and_destination_changes_without_overwriting() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut history = open(temp.path())?;
        let path = temp.path().join("save.gci");
        files::atomic_write(&path, &version(2))?;
        let captured = history.capture(&version(1), &path, Reason::Observed, None)?;
        assert!(
            history
                .restore(&captured.id, &path, Some(&"f".repeat(64)))
                .is_err()
        );
        assert_eq!(files::read_save(&path)?, version(2));
        let commit = history.commit(&captured.id)?;
        files::atomic_write(
            &history.object_path(&commit.blocks[0].hash)?,
            b"invalid zlib",
        )?;
        assert!(history.read(&captured.id).is_err());
        assert_eq!(files::read_save(&path)?, version(2));
        Ok(())
    }

    #[test]
    fn migrates_legacy_backups_only_after_verified_snapshot_commit() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let backups = temp.path().join("backups");
        fs::create_dir(&backups)?;
        let bytes = version(7);
        let path = backups.join(format!("{}.gci", sha256(&bytes)));
        files::atomic_write(&path, &bytes)?;
        let mut history = open(temp.path())?;
        assert_eq!(history.migrate_backups(&backups)?, 1);
        assert!(!path.try_exists()?);
        let entry = history.page(None)?.entries.remove(0);
        assert_eq!(history.read(&entry.id)?, bytes);
        assert_eq!(entry.reason, Reason::LegacyBackup);
        Ok(())
    }
}

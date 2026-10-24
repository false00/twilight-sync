use crate::files::ensure_safe_path;
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone)]
pub struct Audit {
    directory: PathBuf,
    username: String,
}

#[derive(Serialize)]
struct Record<'a> {
    timestamp: String,
    username: &'a str,
    action: &'a str,
    file: Option<&'a Path>,
    sha256: Option<&'a str>,
    peer: Option<&'a str>,
    detail: &'a str,
}

impl Audit {
    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn new(root: &Path) -> Result<Self> {
        let directory = root.join("logs");
        ensure_safe_path(&directory)?;
        fs::create_dir_all(&directory)?;
        Ok(Self {
            directory,
            username: std::env::var("USERNAME")
                .or_else(|_| std::env::var("USER"))
                .context("Cannot identify log user")?,
        })
    }

    pub fn record(
        &self,
        action: &str,
        file: Option<&Path>,
        hash: Option<&str>,
        peer: Option<&str>,
        detail: &str,
    ) -> Result<()> {
        let path = self.directory.join("audit.jsonl");
        ensure_safe_path(&path)?;
        if path.try_exists()? && path.metadata()?.len() >= 5 * 1024 * 1024 {
            let oldest = self.directory.join("audit.4.jsonl");
            ensure_safe_path(&oldest)?;
            if oldest.try_exists()? {
                fs::remove_file(oldest)?;
            }
            for number in (1..4).rev() {
                let from = self.directory.join(format!("audit.{number}.jsonl"));
                let to = self.directory.join(format!("audit.{}.jsonl", number + 1));
                ensure_safe_path(&from)?;
                ensure_safe_path(&to)?;
                if from.try_exists()? {
                    fs::rename(from, to)?;
                }
            }
            fs::rename(&path, self.directory.join("audit.1.jsonl"))?;
        }
        let record = Record {
            timestamp: OffsetDateTime::now_utc().format(&Rfc3339)?,
            username: &self.username,
            action,
            file,
            sha256: hash,
            peer,
            detail,
        };
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        serde_json::to_writer(&mut file, &record)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }
}

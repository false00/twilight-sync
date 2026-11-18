use super::{Cancelled, check_cancelled};
use crate::files::ensure_safe_path;
use anyhow::{Context, Result, ensure};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::AtomicBool,
    time::Duration,
};

pub(super) struct Runner<'a> {
    pub log: PathBuf,
    pub cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(String) -> Result<()>,
}

impl<'a> Runner<'a> {
    pub fn new(
        log: &Path,
        cancel: &'a AtomicBool,
        progress: &'a mut dyn FnMut(String) -> Result<()>,
    ) -> Result<Self> {
        ensure_safe_path(log)?;
        File::create_new(log)?;
        Ok(Self {
            log: log.into(),
            cancel,
            progress,
        })
    }

    pub fn message(&mut self, text: &str) -> Result<()> {
        check_cancelled(self.cancel)?;
        writeln!(
            OpenOptions::new().append(true).open(&self.log)?,
            "\n== {text} =="
        )?;
        (self.progress)(text.into())
    }

    pub fn run(&mut self, command: &mut Command, label: &str) -> Result<()> {
        let output = OpenOptions::new().append(true).open(&self.log)?;
        self.execute(command, label, output)
    }

    pub fn capture(&mut self, command: &mut Command, label: &str) -> Result<Vec<u8>> {
        let file = tempfile::NamedTempFile::new_in(
            self.log.parent().context("Build log has no directory")?,
        )?;
        self.execute(command, label, file.as_file().try_clone()?)?;
        let mut bytes = Vec::new();
        File::open(file.path())?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "Build-tool response exceeds its limit"
        );
        Ok(bytes)
    }

    fn execute(&mut self, command: &mut Command, label: &str, output: File) -> Result<()> {
        self.message(label)?;
        let configured: Vec<_> = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(std::ffi::OsStr::to_os_string)))
            .collect();
        command.env_clear();
        for key in [
            "SystemRoot",
            "WINDIR",
            "ComSpec",
            "PATH",
            "PATHEXT",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "LOCALAPPDATA",
            "APPDATA",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramData",
            "OS",
            "ProgramW6432",
            "CommonProgramFiles",
            "CommonProgramFiles(x86)",
            "PROCESSOR_ARCHITECTURE",
            "NUMBER_OF_PROCESSORS",
            "CARGO_HOME",
            "RUSTUP_HOME",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        for (key, value) in configured {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(OpenOptions::new().append(true).open(&self.log)?);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        #[cfg(windows)]
        let job = crate::process_job::Job::new(None, None)?;
        let mut child = command
            .spawn()
            .with_context(|| format!("Starting {label}"))?;
        #[cfg(windows)]
        if let Err(error) = job.assign(&child) {
            child
                .kill()
                .context("Stopping the unassigned build process")?;
            child.wait()?;
            return Err(error);
        }
        let status = loop {
            ensure!(
                self.log.metadata()?.len() <= 256 * 1024 * 1024,
                "Build output exceeded the 256 MiB log limit"
            );
            if check_cancelled(self.cancel).is_err() {
                #[cfg(windows)]
                drop(job);
                #[cfg(not(windows))]
                child.kill()?;
                child.wait()?;
                return Err(Cancelled.into());
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        #[cfg(windows)]
        drop(job);
        ensure!(
            status.success(),
            "{label} failed ({status}). See {}",
            self.log.display()
        );
        Ok(())
    }
}

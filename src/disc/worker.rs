use super::{Format, RESULT_PREFIX, VERIFY_TIMEOUT, Verified};
use crate::files::{ensure_safe_path, valid_hash};
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Write},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::System::{
    JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    },
    Threading::GetCurrentProcess,
};

const MEMORY_LIMIT: usize = 512 * 1024 * 1024;

struct Job(OwnedHandle);

impl Job {
    fn new() -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        ensure!(
            !handle.is_null(),
            "Cannot create the resource-limited RVZ verification job"
        );
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        // This Win32 limits structure contains only numeric POD fields; zero disables unused limits.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.ProcessMemoryLimit = MEMORY_LIMIT;
        let result = unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                u32::try_from(std::mem::size_of_val(&limits))?,
            )
        };
        if result == 0 {
            return Err(std::io::Error::last_os_error())
                .context("Applying RVZ verification resource limits");
        }
        Ok(job)
    }

    fn assign(&self, child: &Child) -> Result<()> {
        let result =
            unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) };
        if result == 0 {
            return Err(std::io::Error::last_os_error())
                .context("Assigning the RVZ verifier to its bounded job");
        }
        Ok(())
    }
}

pub(super) fn await_gate() -> Result<()> {
    let mut gate = [0_u8; 1];
    std::io::stdin()
        .read_exact(&mut gate)
        .context("Waiting for the RVZ verifier resource gate")?;
    ensure!(gate == *b"V", "Invalid RVZ verifier gate");
    let mut in_job = 0;
    let result = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    ensure!(
        result != 0 && in_job != 0,
        "RVZ verification requires a bounded process job"
    );
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let result = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of_mut!(limits).cast(),
            u32::try_from(std::mem::size_of_val(&limits))?,
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("Checking RVZ worker resource limits");
    }
    let required = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    ensure!(
        limits.BasicLimitInformation.LimitFlags & required == required
            && limits.BasicLimitInformation.ActiveProcessLimit == 1
            && limits.ProcessMemoryLimit == MEMORY_LIMIT,
        "RVZ worker does not have the required resource limits"
    );
    Ok(())
}

pub(super) fn verify(path: &Path, cancel: Arc<AtomicBool>) -> Result<Verified> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "RVZ verification was cancelled"
    );
    ensure_safe_path(path)?;
    let size = path.metadata()?.len();
    Format::Rvz.validate_size(size)?;
    let job = Job::new()?;
    let mut command = Command::new(std::env::current_exe()?);
    #[cfg(not(test))]
    command.arg("--verify-rvz-worker").arg(path);
    #[cfg(test)]
    command
        .args([
            "--exact",
            "disc::tests::bounded_rvz_worker_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("TWILIGHT_RVZ_TEST_PATH", path);
    let mut child = command
        .env("RAYON_NUM_THREADS", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x08000000)
        .spawn()
        .context("Starting the bounded RVZ verification process")?;
    let result = (|| -> Result<Verified> {
        job.assign(&child)?;
        let mut input = child
            .stdin
            .take()
            .context("RVZ worker input pipe is missing")?;
        input.write_all(b"V")?;
        drop(input);
        let started = Instant::now();
        let status = loop {
            ensure!(
                !cancel.load(Ordering::Acquire),
                "RVZ verification was cancelled"
            );
            ensure!(
                started.elapsed() < VERIFY_TIMEOUT,
                "RVZ verification exceeded its five-minute limit"
            );
            if let Some(status) = child.try_wait()? {
                break status;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .context("RVZ worker output pipe is missing")?
            .take(4097)
            .read_to_end(&mut output)?;
        let mut errors = Vec::new();
        child
            .stderr
            .take()
            .context("RVZ worker error pipe is missing")?
            .take(4097)
            .read_to_end(&mut errors)?;
        ensure!(
            output.len() <= 4096 && errors.len() <= 4096,
            "RVZ verifier output exceeded its limit"
        );
        ensure!(
            status.success(),
            "RVZ verification failed or exceeded its memory budget: {}\n{}",
            String::from_utf8_lossy(&errors).trim(),
            String::from_utf8_lossy(&output).trim()
        );
        let output =
            std::str::from_utf8(&output).context("Invalid RVZ verifier output encoding")?;
        let records: Vec<&str> = output
            .lines()
            .filter_map(|line| line.strip_prefix(RESULT_PREFIX))
            .collect();
        ensure!(
            records.len() == 1,
            "RVZ verifier returned an invalid result"
        );
        let verified: Verified = serde_json::from_str(records[0])?;
        ensure!(
            verified.format == Format::Rvz && verified.size == size && valid_hash(&verified.sha256),
            "RVZ verifier result differs from the approved file"
        );
        Ok(verified)
    })();
    drop(child.stdin.take());
    drop(job);
    child
        .wait()
        .context("Waiting for the bounded RVZ verifier to exit")?;
    result
}

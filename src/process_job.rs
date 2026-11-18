use anyhow::{Context, Result, ensure};
use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::Child,
    sync::{Arc, Mutex, Weak},
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

pub(crate) struct Job(OwnedHandle);

// Tracking must not keep jobs alive after a runner drops its cancellation guard.
static SOURCE_BUILD_JOBS: Mutex<Vec<Weak<Job>>> = Mutex::new(Vec::new());

impl Job {
    pub fn for_source_build() -> Result<Arc<Self>> {
        let job = Arc::new(Self::new(None, None)?);
        let mut jobs = SOURCE_BUILD_JOBS
            .lock()
            .map_err(|_| anyhow::anyhow!("Source-build job registry is poisoned"))?;
        jobs.retain(|job| job.strong_count() > 0);
        jobs.push(Arc::downgrade(&job));
        Ok(job)
    }

    pub fn new(memory: Option<usize>, processes: Option<u32>) -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("Creating the owned child-process job");
        }

        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        // All fields are numeric POD; zero leaves optional limits disabled.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(memory) = memory {
            ensure!(memory > 0, "Invalid process memory limit");
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits.ProcessMemoryLimit = memory;
        }
        if let Some(processes) = processes {
            ensure!(processes > 0, "Invalid active-process limit");
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.BasicLimitInformation.ActiveProcessLimit = processes;
        }
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
                .context("Configuring the owned child-process job");
        }
        Ok(job)
    }

    pub fn assign(&self, child: &Child) -> Result<()> {
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("Assigning the child process to its owned job");
        }
        Ok(())
    }
}

pub(crate) fn is_source_build_process(pid: u32) -> Result<bool> {
    let jobs: Vec<_> = SOURCE_BUILD_JOBS
        .lock()
        .map_err(|_| anyhow::anyhow!("Source-build job registry is poisoned"))?
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    if jobs.is_empty() {
        return Ok(false);
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error()
            == Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER as i32)
        {
            return Ok(false);
        }
        return Err(error).context("Inspecting a possible source-build process");
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    for job in jobs {
        let mut member = 0;
        if unsafe { IsProcessInJob(process.as_raw_handle(), job.0.as_raw_handle(), &mut member) }
            == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("Checking source-build job membership");
        }
        if member != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

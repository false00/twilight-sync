use anyhow::{Context, Result, ensure};
use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::Child,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

pub(crate) struct Job(OwnedHandle);

impl Job {
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

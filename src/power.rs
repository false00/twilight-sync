use anyhow::{Result, ensure};
use std::{marker::PhantomData, rc::Rc};
use windows_sys::Win32::System::Power::{
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE, SetThreadExecutionState,
};

pub(crate) struct KeepAwake {
    previous: EXECUTION_STATE,
    // Windows execution requirements belong to the calling thread.
    _thread: PhantomData<Rc<()>>,
}

impl KeepAwake {
    pub fn new() -> Result<Self> {
        let requested = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;
        let previous = unsafe { SetThreadExecutionState(requested) };
        ensure!(
            previous != 0,
            "Windows could not prevent automatic sleep during compilation"
        );
        let guard = Self {
            previous,
            _thread: PhantomData,
        };
        if previous & !requested != 0 {
            ensure!(
                unsafe { SetThreadExecutionState(previous | requested) } != 0,
                "Windows could not preserve existing power requirements during compilation"
            );
        }
        Ok(guard)
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if unsafe { SetThreadExecutionState(self.previous | ES_CONTINUOUS) } == 0 {
            eprintln!("Windows could not restore automatic sleep after compilation");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, bail};
    use windows_sys::Win32::System::Power::ES_DISPLAY_REQUIRED;

    fn current_state() -> Result<EXECUTION_STATE> {
        let state = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        ensure!(state != 0, "Reading the test thread's power requirements");
        ensure!(
            unsafe { SetThreadExecutionState(state | ES_CONTINUOUS) } != 0,
            "Restoring the test thread's power requirements"
        );
        Ok(state)
    }

    #[test]
    fn compilation_keeps_system_not_display_awake_until_nested_scopes_end() -> Result<()> {
        std::thread::spawn(|| -> Result<()> {
            let initial = current_state()?;
            let outer = KeepAwake::new()?;
            let during = current_state()?;
            assert_ne!(during & ES_SYSTEM_REQUIRED, 0);
            assert_eq!(during & ES_DISPLAY_REQUIRED, 0);
            let inner = KeepAwake::new()?;
            drop(inner);
            assert_eq!(current_state()?, during);
            drop(outer);
            assert_eq!(current_state()?, initial);
            Ok(())
        })
        .join()
        .map_err(|_| anyhow::anyhow!("Power test worker panicked"))?
    }

    #[test]
    fn compilation_failure_restores_preexisting_power_requirements() -> Result<()> {
        std::thread::spawn(|| -> Result<()> {
            ensure!(
                unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED) } != 0,
                "Setting the test thread's initial power requirements"
            );
            let initial = current_state()?;
            let result: Result<()> = (|| {
                let _awake = KeepAwake::new()?;
                assert_eq!(
                    current_state()?,
                    initial | ES_SYSTEM_REQUIRED | ES_CONTINUOUS
                );
                bail!("Synthetic cancelled or failed compilation")
            })();
            assert!(
                result
                    .err()
                    .context("The synthetic compilation must fail")?
                    .to_string()
                    .contains("Synthetic")
            );
            assert_eq!(current_state()?, initial);
            Ok(())
        })
        .join()
        .map_err(|_| anyhow::anyhow!("Power test worker panicked"))?
    }
}

use crate::files::ensure_safe_path;
use anyhow::{Context, Result, ensure};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const UCRT_PACKAGES: &[&str] = &[
    "mingw-w64-ucrt-x86_64-clang",
    "mingw-w64-ucrt-x86_64-lld",
    "mingw-w64-ucrt-x86_64-ninja",
    "mingw-w64-ucrt-x86_64-gcc",
    "mingw-w64-ucrt-x86_64-make",
    "mingw-w64-ucrt-x86_64-pkgconf",
    "mingw-w64-ucrt-x86_64-sdl3",
    "mingw-w64-ucrt-x86_64-opus",
    "mingw-w64-ucrt-x86_64-libvpx",
    "mingw-w64-ucrt-x86_64-zstd",
    "mingw-w64-ucrt-x86_64-vulkan-devel",
    "mingw-w64-ucrt-x86_64-vulkan-memory-allocator",
    "mingw-w64-ucrt-x86_64-vulkan-validation-layers",
    "mingw-w64-ucrt-x86_64-shaderc",
    "mingw-w64-ucrt-x86_64-spirv-tools",
    "base-devel",
    "git",
    "unzip",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdkSetup {
    DotNet10,
    Msys2Ucrt64,
}

impl SdkSetup {
    pub fn label(self) -> &'static str {
        match self {
            Self::DotNet10 => "Install .NET 10 with winget...",
            Self::Msys2Ucrt64 => "Install / configure MSYS2 automatically...",
        }
    }

    pub fn notice(self) -> &'static str {
        match self {
            Self::DotNet10 => {
                "Install Microsoft's .NET SDK 10 using Windows Package Manager? A console/installer window may open and Windows may ask for administrator approval. Review and accept any package/source terms there. No game code is run."
            }
            Self::Msys2Ucrt64 => {
                "Install MSYS2 to C:\\msys64 if missing, update it twice, and install the documented UCRT64 compiler/runtime packages? Close other MSYS2 terminals and builds first: its core update may close MSYS2 processes. This changes that SDK installation, can take several minutes/GiB, and may ask for Windows/package approval. Package signatures stay enabled. No game build runs."
            }
        }
    }

    fn package(self) -> &'static str {
        match self {
            Self::DotNet10 => "Microsoft.DotNet.SDK.10",
            Self::Msys2Ucrt64 => "MSYS2.MSYS2",
        }
    }

    pub fn script(self) -> String {
        match self {
            Self::DotNet10 => {
                "winget install --id Microsoft.DotNet.SDK.10 --exact --source winget".into()
            }
            Self::Msys2Ucrt64 => format!(
                "$ErrorActionPreference = 'Stop'\n$bash = 'C:\\msys64\\usr\\bin\\bash.exe'\nif (-not (Test-Path -LiteralPath $bash)) {{\n    winget install --id MSYS2.MSYS2 --exact --source winget --scope user --location C:\\msys64 --interactive\n    if ($LASTEXITCODE -ne 0) {{ throw 'MSYS2 installation did not complete.' }}\n}}\nif (-not (Test-Path -LiteralPath $bash)) {{ throw 'MSYS2 must be installed at C:\\msys64.' }}\n$env:MSYSTEM = 'UCRT64'\n$env:CHERE_INVOKING = '1'\nfor ($pass = 1; $pass -le 2; $pass++) {{\n    & $bash -lc 'pacman -Syuu --noconfirm'\n    if ($LASTEXITCODE -ne 0) {{ throw 'MSYS2 core update did not finish. Close MSYS2 windows and rerun this setup.' }}\n}}\n& $bash -lc 'pacman -S --needed --noconfirm {}'\nif ($LASTEXITCODE -ne 0) {{ throw 'UCRT64 package setup failed.' }}\nWrite-Host 'SDK setup finished. Recheck prerequisites in Twilight Sync.'\n",
                UCRT_PACKAGES.join(" ")
            ),
        }
    }

    pub fn run(self, log: &Path, progress: &mut dyn FnMut(String) -> Result<()>) -> Result<()> {
        ensure!(cfg!(windows), "SDK setup requires Windows");
        ensure_safe_path(log)?;
        let mut setup_log = File::create_new(log)?;
        let bash = Path::new(r"C:\msys64\usr\bin\bash.exe");
        if self == Self::DotNet10 || !bash.try_exists()? {
            progress(format!(
                "Installing {} with Windows Package Manager. Complete any console, license or Windows approval prompts...",
                self.package()
            ))?;
            let local =
                std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not available")?;
            // Microsoft's App Execution Alias is an expected OS-managed reparse point.
            let winget = PathBuf::from(local)
                .join("Microsoft")
                .join("WindowsApps")
                .join("winget.exe");
            ensure!(
                winget.try_exists()?,
                "winget is not available. Install Microsoft App Installer or use the copied setup commands."
            );
            let mut command = Command::new(winget);
            let installer_log = log.with_extension("winget.log");
            ensure_safe_path(&installer_log)?;
            ensure!(
                !installer_log.try_exists()?,
                "The SDK installer log already exists"
            );
            writeln!(
                setup_log,
                "Installing {}. Windows Package Manager log: {}",
                self.package(),
                installer_log.display()
            )?;
            command
                .args([
                    "install",
                    "--id",
                    self.package(),
                    "--exact",
                    "--source",
                    "winget",
                    "--interactive",
                ])
                .arg("--log")
                .arg(&installer_log);
            if self == Self::Msys2Ucrt64 {
                command.args(["--scope", "user", "--location", r"C:\msys64"]);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x00000010);
            }
            let status = command
                .status()
                .context("Starting Windows Package Manager")?;
            ensure!(
                status.success(),
                "Windows Package Manager did not finish ({status}); inspect its installer/approval prompts and {}",
                log.display()
            );
        }
        if self == Self::Msys2Ucrt64 {
            ensure_safe_path(bash)?;
            ensure!(
                bash.is_file(),
                "MSYS2 was not installed at C:\\msys64; select that location and retry"
            );
            for pass in 1..=2 {
                progress(format!(
                    "Updating MSYS2 core packages ({pass}/2). Keep other MSYS2 terminals closed..."
                ))?;
                run_pacman(bash, "pacman -Syuu --noconfirm", log)
                    .context("MSYS2 core update did not finish. Close MSYS2 terminals and rerun setup; completed packages are retained")?;
            }
            progress("Installing the required UCRT64 compiler, Vulkan, SDL and codec development packages...".into())?;
            run_pacman(
                bash,
                &format!("pacman -S --needed --noconfirm {}", UCRT_PACKAGES.join(" ")),
                log,
            )?;
        }
        Ok(())
    }
}

fn run_pacman(bash: &Path, script: &str, log: &Path) -> Result<()> {
    let output = OpenOptions::new().append(true).open(log)?;
    let mut command = Command::new(bash);
    command
        .args(["-lc", script])
        .env("MSYSTEM", "UCRT64")
        .env("CHERE_INVOKING", "1")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    // Let SDK/package updates finish normally; cancelling a game build must not kill an SDK installer.
    let status = command
        .status()
        .context("Starting the MSYS2 package manager")?;
    ensure!(
        status.success(),
        "MSYS2 package setup failed ({status}). Log: {}",
        log.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_sdk_commands_target_exact_packages_and_retain_signature_verification() {
        let dotnet = SdkSetup::DotNet10.script();
        assert!(dotnet.contains("--id Microsoft.DotNet.SDK.10 --exact --source winget"));
        let msys = SdkSetup::Msys2Ucrt64.script();
        assert!(msys.contains("--id MSYS2.MSYS2") && msys.contains("--location C:\\msys64"));
        assert!(msys.contains("pacman -Syuu") && msys.contains("$pass -le 2"));
        assert!(msys.contains("mingw-w64-ucrt-x86_64-libvpx"));
        assert!(!msys.contains("skippgp") && !msys.contains("ExecutionPolicy"));
    }
}

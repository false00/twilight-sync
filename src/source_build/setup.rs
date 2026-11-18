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
    VisualStudio,
}

#[derive(Debug, Default)]
pub struct SdkOutcome {
    pub reboot_required: bool,
}

impl SdkSetup {
    pub fn label(self) -> &'static str {
        match self {
            Self::DotNet10 => "Install .NET 10 with winget...",
            Self::Msys2Ucrt64 => "Install / configure MSYS2 automatically...",
            Self::VisualStudio => "Set up Visual Studio C++ tools and Windows SDK...",
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
            Self::VisualStudio => {
                "Install or complete Microsoft's Visual Studio 2026 C++ Build Tools and Windows SDK 10.0.26100? The required components are selected for you. This can download several GiB. Review Microsoft's terms in its installer and approve any Windows administrator prompt yourself. Existing 2026 installations are completed; older Visual Studio versions are retained alongside the new tools. Windows will not restart automatically. No game code is run."
            }
        }
    }

    fn package(self) -> &'static str {
        match self {
            Self::DotNet10 => "Microsoft.DotNet.SDK.10",
            Self::Msys2Ucrt64 => "MSYS2.MSYS2",
            Self::VisualStudio => "Microsoft.VisualStudio.BuildTools",
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
            Self::VisualStudio => r#"$ErrorActionPreference = 'Stop'
$installerDirectory = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer'
$vswhere = Join-Path $installerDirectory 'vswhere.exe'
$setup = Join-Path $installerDirectory 'setup.exe'
$installation = $null
$channel = $null
if (Test-Path -LiteralPath $vswhere) {
    $installation = & $vswhere -latest -version '[18.0,)' -products '*' -property installationPath -utf8
    if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect the existing Visual Studio installation.' }
    $channel = & $vswhere -latest -version '[18.0,)' -products '*' -property channelId -utf8
    if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect the Visual Studio installation channel.' }
}
if ($installation -and (Test-Path -LiteralPath $setup)) {
    if ([string]::IsNullOrWhiteSpace($channel)) { throw 'The Visual Studio installation channel was not found. Open its installer to repair it.' }
    $arguments = @('modify', '--installPath', ('"{0}"' -f $installation.Trim()),
        '--channelId', $channel.Trim(),
        '--add', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
        '--add', 'Microsoft.VisualStudio.Component.Windows11SDK.26100')
    $process = Start-Process -FilePath $setup -ArgumentList $arguments -Wait -PassThru
    $code = $process.ExitCode
} else {
    $winget = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\winget.exe'
    if (-not (Test-Path -LiteralPath $winget)) {
        throw 'Install Microsoft App Installer to get winget, or use the Visual Studio instructions.'
    }
    & $winget install --id Microsoft.VisualStudio.BuildTools --exact --source winget --interactive --override '--wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --add Microsoft.VisualStudio.Component.Windows11SDK.26100'
    $code = $LASTEXITCODE
}
if ($code -eq 3010) {
    Write-Warning 'Setup finished. Restart Windows before building, then recheck prerequisites.'
} elseif ($code -ne 0) {
    throw "Visual Studio setup did not complete (exit $code). Finish or cancel any existing installer, then retry."
}
"#.into(),
        }
    }

    pub fn run(
        self,
        log: &Path,
        progress: &mut dyn FnMut(String) -> Result<()>,
    ) -> Result<SdkOutcome> {
        ensure!(cfg!(windows), "SDK setup requires Windows");
        ensure_safe_path(log)?;
        let mut setup_log = File::create_new(log)?;
        if self == Self::VisualStudio {
            progress("Setting up Visual Studio C++ tools and Windows SDK. Complete Microsoft's installer and any Windows approval prompts; this may take several minutes.".into())?;
            writeln!(
                setup_log,
                "{}\nVisual Studio diagnostic logs are written by its installer under the Windows temporary directory.",
                self.label()
            )?;
            let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
            let mut command = Command::new(
                PathBuf::from(windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
            );
            command
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!("{}\nexit $code", self.script()),
                ])
                .env_remove("PSModulePath");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x00000010);
            }
            let status = command
                .status()
                .context("Starting Visual Studio prerequisite setup")?;
            writeln!(setup_log, "Visual Studio setup returned {status}")?;
            return visual_studio_outcome(status.code());
        }
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
        Ok(SdkOutcome::default())
    }
}

fn visual_studio_outcome(code: Option<i32>) -> Result<SdkOutcome> {
    ensure!(
        matches!(code, Some(0 | 3010)),
        "Visual Studio setup did not finish (exit {code:?}); review its installer/Windows prompts and retry"
    );
    Ok(SdkOutcome {
        reboot_required: code == Some(3010),
    })
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
    #[cfg(windows)]
    fn sdk_scripts_parse_in_windows_powershell_without_running_installers() -> Result<()> {
        let powershell =
            PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot is missing")?)
                .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        for setup in [
            SdkSetup::DotNet10,
            SdkSetup::Msys2Ucrt64,
            SdkSetup::VisualStudio,
        ] {
            let output = Command::new(&powershell).args(["-NoProfile", "-NonInteractive", "-Command",
                "$tokens = $null; $errors = $null; [System.Management.Automation.Language.Parser]::ParseInput($env:TWILIGHT_SDK_SCRIPT, [ref]$tokens, [ref]$errors) | Out-Null; if ($errors.Count) { $errors | Out-String | Write-Error; exit 1 }"])
                .env("TWILIGHT_SDK_SCRIPT", setup.script()).output()?;
            ensure!(
                output.status.success(),
                "{setup:?} script did not parse: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    #[test]
    fn copied_sdk_commands_target_exact_packages_and_retain_signature_verification() {
        let dotnet = SdkSetup::DotNet10.script();
        assert!(dotnet.contains("--id Microsoft.DotNet.SDK.10 --exact --source winget"));
        let msys = SdkSetup::Msys2Ucrt64.script();
        assert!(msys.contains("--id MSYS2.MSYS2") && msys.contains("--location C:\\msys64"));
        assert!(msys.contains("pacman -Syuu") && msys.contains("$pass -le 2"));
        assert!(msys.contains("mingw-w64-ucrt-x86_64-libvpx"));
        assert!(!msys.contains("skippgp") && !msys.contains("ExecutionPolicy"));
        let visual_studio = SdkSetup::VisualStudio.script();
        assert!(
            visual_studio
                .contains("--id Microsoft.VisualStudio.BuildTools --exact --source winget")
        );
        assert!(visual_studio.contains("Microsoft.VisualStudio.Component.Windows11SDK.26100"));
        assert!(visual_studio.contains("Microsoft.VisualStudio.Component.VC.Tools.x86.x64"));
        assert!(visual_studio.contains("'modify'") && visual_studio.contains("-Wait -PassThru"));
        assert!(!visual_studio.contains("--accept-") && !visual_studio.contains("--quiet"));
    }

    #[test]
    fn visual_studio_reboot_and_cancel_results_are_distinct() -> Result<()> {
        assert!(!visual_studio_outcome(Some(0))?.reboot_required);
        assert!(visual_studio_outcome(Some(3010))?.reboot_required);
        assert!(visual_studio_outcome(Some(1602)).is_err());
        assert!(visual_studio_outcome(Some(1618)).is_err());
        assert!(visual_studio_outcome(None).is_err());
        Ok(())
    }
}

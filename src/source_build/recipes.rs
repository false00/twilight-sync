use super::{
    Prerequisite, Request, SCRATCH, checkout, copy_tree,
    runner::Runner,
    tools::{Package, Toolset},
};
use crate::{
    files::{self, ensure_safe_path},
    protocol::GameId,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
};

const VS_URL: &str = "https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio";
const MSYS: &str = r"C:\msys64\ucrt64";

fn on_path(name: &str) -> Result<bool> {
    let Some(path) = std::env::var_os("PATH") else {
        return Ok(false);
    };
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.try_exists()? && candidate.metadata()?.is_file() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn vs_dev(game: GameId) -> Result<Option<PathBuf>> {
    vs_dev_in(&vs_installations()?, game)
}

fn vs_installations() -> Result<Vec<PathBuf>> {
    let mut installations = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles(x86)") {
        let vswhere =
            PathBuf::from(program_files).join(r"Microsoft Visual Studio\Installer\vswhere.exe");
        if vswhere.try_exists()? {
            ensure_safe_path(&vswhere)?;
            let mut command = Command::new(vswhere);
            command.args([
                "-products",
                "*",
                "-sort",
                "-property",
                "installationPath",
                "-utf8",
            ]);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            let output = command
                .output()
                .context("Finding installed Visual Studio build tools")?;
            ensure!(
                output.status.success(),
                "Visual Studio discovery failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            for path in std::str::from_utf8(&output.stdout)?
                .lines()
                .filter(|line| !line.is_empty())
            {
                let path = PathBuf::from(path);
                ensure_safe_path(&path)?;
                installations.push(path);
            }
        }
    }
    for program_files in ["ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
    {
        let root = PathBuf::from(program_files).join("Microsoft Visual Studio");
        if !root.try_exists()? {
            continue;
        }
        for year in fs::read_dir(root)? {
            let year = year?;
            if !year.file_type()?.is_dir() {
                continue;
            }
            for edition in fs::read_dir(year.path())? {
                let edition = edition?;
                if !edition.file_type()?.is_dir() {
                    continue;
                }
                if !installations.contains(&edition.path()) {
                    installations.push(edition.path());
                }
            }
        }
    }
    Ok(installations)
}

fn vs_dev_in(installations: &[PathBuf], game: GameId) -> Result<Option<PathBuf>> {
    for directory in installations {
        let script = directory.join(r"Common7\Tools\VsDevCmd.bat");
        let compilers = directory.join(r"VC\Tools\MSVC");
        if script.is_file() && compilers.try_exists()? {
            for version in fs::read_dir(compilers)? {
                let version = version?;
                if !version.file_type()?.is_dir() {
                    continue;
                }
                if game == GameId::Totk {
                    let parts: Vec<_> = version
                        .file_name()
                        .to_string_lossy()
                        .split('.')
                        .map(str::parse::<u32>)
                        .collect::<std::result::Result<_, _>>()?;
                    if parts.len() < 2 || (parts[0], parts[1]) < (14, 50) {
                        continue;
                    }
                }
                if version.path().join(r"bin\Hostx64\x64\cl.exe").is_file() {
                    ensure_safe_path(&script)?;
                    return Ok(Some(script));
                }
            }
        }
    }
    Ok(None)
}

fn windows_sdk_ready(minimum_build: u32) -> Result<bool> {
    let mut roots = Vec::new();
    #[cfg(windows)]
    {
        use winreg::{
            RegKey,
            enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY},
        };
        match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(
            r"SOFTWARE\Microsoft\Windows Kits\Installed Roots",
            KEY_READ | KEY_WOW64_32KEY,
        ) {
            Ok(key) => match key.get_value::<String, _>("KitsRoot10") {
                Ok(root) => roots.push(PathBuf::from(root)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Reading Windows SDK location"),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Finding Windows SDK registration"),
        }
    }
    for directory in ["ProgramFiles(x86)", "ProgramFiles"]
        .into_iter()
        .filter_map(std::env::var_os)
    {
        roots.push(PathBuf::from(directory).join(r"Windows Kits\10"));
    }
    for root in roots {
        if windows_sdk_ready_at(&root, minimum_build)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn windows_sdk_ready_at(root: &Path, minimum_build: u32) -> Result<bool> {
    let includes = root.join("Include");
    ensure_safe_path(&includes)?;
    if !includes.try_exists()? {
        return Ok(false);
    }
    for entry in fs::read_dir(&includes)? {
        let entry = entry?;
        let name = entry.file_name();
        let Ok(version) = name
            .to_string_lossy()
            .split('.')
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()
        else {
            continue;
        };
        if version.len() != 4 || version[0] != 10 || version[1] != 0 || version[2] < minimum_build {
            continue;
        }
        let mut complete = true;
        for path in [
            entry.path().join(r"um\Windows.h"),
            entry.path().join(r"shared\winerror.h"),
            entry.path().join(r"ucrt\stdio.h"),
            root.join("Lib").join(&name).join(r"um\x64\kernel32.lib"),
            root.join("Lib").join(&name).join(r"ucrt\x64\ucrt.lib"),
            root.join("bin").join(&name).join(r"x64\rc.exe"),
        ] {
            ensure_safe_path(&path)?;
            complete &= path.try_exists()? && path.metadata()?.is_file();
        }
        if complete {
            return Ok(true);
        }
    }
    Ok(false)
}

fn dotnet_root() -> Result<Option<PathBuf>> {
    let mut candidates = Vec::new();
    if let Some(root) = std::env::var_os("DOTNET_ROOT") {
        candidates.push(PathBuf::from(root));
    }
    if let Some(root) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(root).join("dotnet"));
    }
    for root in candidates {
        let sdk = root.join("sdk");
        if !root.join("dotnet.exe").is_file() || !sdk.try_exists()? {
            continue;
        }
        for version in fs::read_dir(sdk)? {
            if version?.file_name().to_string_lossy().starts_with("10.") {
                return Ok(Some(root));
            }
        }
    }
    Ok(None)
}

fn missing_msys2_files(ucrt: &Path) -> Result<Vec<String>> {
    let found = |relative: &Path| -> Result<bool> {
        let path = ucrt.join(relative);
        ensure_safe_path(&path)?;
        Ok(path.try_exists()? && path.metadata()?.is_file())
    };
    let mut missing = Vec::new();
    for relative in [
        Path::new("bin").join("gcc.exe"),
        Path::new("bin").join("ninja.exe"),
        Path::new("bin").join("SDL3.dll"),
        Path::new("include").join("vulkan").join("vulkan.h"),
        Path::new("include").join("opus").join("opus.h"),
        Path::new("include").join("vpx").join("vpx_decoder.h"),
    ] {
        if !found(&relative)? {
            missing.push(ucrt.join(relative).display().to_string());
        }
    }
    let nested = Path::new("include").join("vma").join("vk_mem_alloc.h");
    let flat = Path::new("include").join("vk_mem_alloc.h");
    if !found(&nested)? && !found(&flat)? {
        missing.push(format!(
            "{} or {}",
            ucrt.join(nested).display(),
            ucrt.join(flat).display()
        ));
    }
    Ok(missing)
}

pub(super) fn prerequisites(game: GameId) -> Result<Vec<Prerequisite>> {
    let mut prerequisites = vec![Prerequisite {
        name: "Windows x64 (source builds are not sandboxed)",
        ready: cfg!(all(windows, target_arch = "x86_64")),
        url: "https://learn.microsoft.com/windows/security/application-security/application-isolation/windows-sandbox/",
        setup: None,
        detail: None,
    }];
    if game != GameId::Triforce {
        let compiler = vs_dev(game)?.is_some();
        let sdk = windows_sdk_ready(if game == GameId::Totk { 26100 } else { 0 })?;
        prerequisites.push(Prerequisite {
            name: if game == GameId::Totk {
                "Visual Studio 2026 C++ tools and Windows SDK 26100"
            } else {
                "Visual Studio C++ Build Tools and Windows SDK"
            },
            ready: compiler && sdk,
            url: VS_URL,
            setup: (game == GameId::Totk).then_some(super::SdkSetup::VisualStudio),
            detail: (!(compiler && sdk)).then(|| format!(
                "{}{}",
                if compiler { "" } else if game == GameId::Totk {
                    "Visual Studio 2026 C++ x64 compiler (MSVC 14.50 or newer) is missing. "
                } else { "C++ x64 compiler is missing. " },
                if sdk { "" } else { "A complete Windows SDK (headers, x64 libraries and resource compiler) is missing." },
            )),
        });
    }
    match game {
        GameId::Dusklight => prerequisites.push(Prerequisite {
            name: "Rust toolchain (Aurora dependency)",
            ready: on_path("rustc.exe")? && on_path("cargo.exe")?,
            url: "https://www.rust-lang.org/tools/install",
            setup: None,
            detail: None,
        }),
        GameId::Totk => {
            prerequisites.push(Prerequisite {
                name: ".NET SDK 10 (source-built shader translator)",
                ready: dotnet_root()?.is_some(),
                url: "https://dotnet.microsoft.com/download/dotnet/10.0",
                setup: Some(super::SdkSetup::DotNet10),
                detail: None,
            });
            let missing = if cfg!(windows) {
                missing_msys2_files(Path::new(MSYS))?
            } else {
                vec!["MSYS2 UCRT64 source-build dependencies require Windows".into()]
            };
            prerequisites.push(Prerequisite {
                name: "MSYS2 UCRT64 dependencies at C:\\msys64 (one-time setup)",
                ready: missing.is_empty(),
                url: "https://github.com/valentderah/totk-pc-recomp/blob/main/docs/building.md#toolchain",
                setup: Some(super::SdkSetup::Msys2Ucrt64),
                detail: (!missing.is_empty()).then(|| format!("Missing required files: {}", missing.join("; "))),
            });
        }
        GameId::Triforce => {}
    }
    Ok(prerequisites)
}

pub fn validate_inputs(request: &Request) -> Result<()> {
    validate_input_paths(
        request.plan.game,
        &request.preference,
        request.previous_install.as_deref(),
    )
}

pub(super) fn validate_input_paths(
    game: GameId,
    preference: &super::Preference,
    previous: Option<&Path>,
) -> Result<()> {
    let file = |path: Option<&Path>, label: &str| -> Result<()> {
        let path = path.with_context(|| format!("Choose {label} before building"))?;
        ensure_safe_path(path)?;
        ensure!(path.is_file(), "{label} was not found");
        Ok(())
    };
    match game {
        GameId::Dusklight => {}
        GameId::Totk => {
            file(preference.game_input.as_deref(), "your TotK base NSP/XCI")?;
            file(preference.update.as_deref(), "your TotK update NSP/NSZ")?;
            file(preference.keys.as_deref(), "your console's prod.keys file")?;
            if let Some(firmware) = &preference.firmware {
                ensure_safe_path(firmware)?;
                ensure!(
                    firmware.is_dir(),
                    "The optional firmware folder was not found"
                );
            }
        }
        GameId::Triforce => {
            let input = input_paths(preference, previous)?;
            ensure_safe_path(&input)?;
            ensure!(
                input.is_file()
                    || (input.join("romfs").is_dir()
                        && (input.join(".triforce_exefs").is_dir()
                            || input.join("exefs").is_dir())),
                "Choose your Ocarina of Time .nup or an extracted game folder with romfs and exefs/.triforce_exefs"
            );
        }
    }
    Ok(())
}

fn triforce_input(request: &Request) -> Result<PathBuf> {
    input_paths(&request.preference, request.previous_install.as_deref())
}

fn input_paths(preference: &super::Preference, previous: Option<&Path>) -> Result<PathBuf> {
    if let Some(input) = &preference.game_input {
        return Ok(input.clone());
    }
    let current =
        previous.context("Choose your own Ocarina of Time .nup or existing game folder")?;
    ensure!(
        current.join("romfs").is_dir() && current.join(".triforce_exefs").is_dir(),
        "This Ocarina of Time installation has no retained build inputs; choose your own .nup or extracted game folder"
    );
    Ok(current.to_path_buf())
}

struct ContextData<'a> {
    workspace: &'a Path,
    source: &'a Path,
    output: &'a Path,
    paths: Vec<PathBuf>,
}

impl ContextData<'_> {
    fn command(&self, program: &Path) -> Result<Command> {
        let mut command = Command::new(program);
        let mut paths = self.paths.clone();
        if let Some(current) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&current));
        }
        command
            .current_dir(self.source)
            .env("PATH", std::env::join_paths(paths)?)
            .env("UV_PYTHON_PREFERENCE", "only-managed")
            .env("UV_PYTHON_INSTALL_DIR", self.workspace.join("python"))
            .env("UV_CACHE_DIR", self.workspace.join(SCRATCH).join("uv"))
            .env("UV_LINK_MODE", "hardlink")
            .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
            .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
            .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
            .env("VSCMD_SKIP_SENDTELEMETRY", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_ALLOW_PROTOCOL", "https")
            .env("GIT_TERMINAL_PROMPT", "0");
        Ok(command)
    }

    fn vs_command(&self, script: &str, vs: &Path, python: &Path) -> Result<Command> {
        let windows = std::env::var_os("SystemRoot").context("SystemRoot is not available")?;
        let mut command = self.command(&PathBuf::from(windows).join("System32").join("cmd.exe"))?;
        command.args(["/d", "/s", "/c"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.raw_arg(script);
        }
        #[cfg(not(windows))]
        command.arg(script);
        command
            .env("TS_VSDEV", vs)
            .env("TS_PYTHON", python)
            .env("TS_SOURCE", self.source)
            .env("TS_OUTPUT", self.output);
        Ok(command)
    }
}

fn python(
    context: &ContextData<'_>,
    uv: &Path,
    directory: &Path,
    version: &str,
    requirements: &[PathBuf],
    runner: &mut Runner<'_>,
) -> Result<PathBuf> {
    runner.run(
        context
            .command(uv)?
            .args(["venv", "--python", version])
            .arg(directory),
        "Preparing a fresh managed Python environment",
    )?;
    let python = directory.join("Scripts").join("python.exe");
    let mut install = context.command(uv)?;
    install.args(["pip", "install", "--python"]).arg(&python);
    for requirement in requirements {
        install.arg("-r").arg(requirement);
    }
    if !requirements.is_empty() {
        runner.run(
            &mut install,
            "Installing the project's declared Python dependencies",
        )?;
    }
    Ok(python)
}

fn shader_reference(source: &Path) -> Result<String> {
    let bytes = super::read_bounded(&source.join("tools").join("fetch_refs.sh"), 64 * 1024)?;
    let text = std::str::from_utf8(&bytes)?;
    let lines: Vec<_> = text
        .lines()
        .map(str::split_whitespace)
        .map(Iterator::collect::<Vec<_>>)
        .filter(|fields| {
            fields.first() == Some(&"fetch") && fields.get(1) == Some(&"MacNativeGames")
        })
        .collect();
    ensure!(
        lines.len() == 1,
        "The approved source has no unique shader-support reference"
    );
    let fields = &lines[0];
    ensure!(
        fields.len() == 5
            && fields[2] == "https://github.com/ly05010419/MacNativeGames.git"
            && super::valid_commit(fields[3])
            && fields[4] == "history",
        "The source's shader-support declaration changed; update the manager before building it"
    );
    Ok(fields[3].into())
}

fn fetch_shader_reference(source: &Path, git: &Path, runner: &mut Runner<'_>) -> Result<()> {
    let commit = shader_reference(source)?;
    let reference = source.join("refs").join("MacNativeGames");
    ensure_safe_path(&reference)?;
    fs::create_dir_all(&reference)?;
    for (arguments, label) in [
        (
            vec!["init", "-q"],
            "Preparing the pinned source shader reference",
        ),
        (
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/ly05010419/MacNativeGames.git",
            ],
            "Configuring the source-only shader reference",
        ),
        (
            vec!["fetch", "--no-tags", "origin", commit.as_str()],
            "Fetching the source-pinned shader history",
        ),
        (
            vec!["checkout", "--detach", commit.as_str()],
            "Selecting the verified shader reference commit",
        ),
        (
            vec!["update-ref", "refs/remotes/origin/HEAD", commit.as_str()],
            "Binding the shader reference history",
        ),
    ] {
        runner.run(
            super::git_command(git)
                .arg("-C")
                .arg(&reference)
                .args(arguments),
            label,
        )?;
    }
    Ok(())
}

fn stage_totk_sdk(ucrt: &Path, destination: &Path) -> Result<()> {
    let include = destination.join("include");
    for name in ["SDL3", "opus", "vpx", "vulkan", "vk_video"] {
        copy_tree(&ucrt.join("include").join(name), &include.join(name))?;
    }
    let vma = ucrt.join("include").join("vma");
    ensure_safe_path(&vma)?;
    if vma.join("vk_mem_alloc.h").is_file() {
        copy_tree(&vma, &include.join("vma"))?;
    } else {
        copy_tree(
            &ucrt.join("include").join("vk_mem_alloc.h"),
            &include.join("vk_mem_alloc.h"),
        )?;
    }
    for name in ["zstd.h", "zstd_errors.h"] {
        copy_tree(&ucrt.join("include").join(name), &include.join(name))?;
    }
    for name in [
        "libSDL3.dll.a",
        "libopus.dll.a",
        "libvpx.a",
        "libvulkan-1.dll.a",
        "libzstd.a",
    ] {
        copy_tree(
            &ucrt.join("lib").join(name),
            &destination.join("lib").join(name),
        )?;
    }
    for name in ["SDL3.dll", "libopus-0.dll", "libiconv-2.dll"] {
        copy_tree(
            &ucrt.join("bin").join(name),
            &destination.join("bin").join(name),
        )?;
    }
    Ok(())
}

fn prepare_nuget(workspace: &Path) -> Result<PathBuf> {
    let path = workspace.join("NuGet.Config");
    files::atomic_write(
        &path,
        br#"<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources>
    <clear />
    <add key="nuget.org" value="https://api.nuget.org/v3/index.json" protocolVersion="3" />
  </packageSources>
</configuration>
"#,
    )?;
    Ok(path)
}

fn configure_nuget(command: &mut Command, workspace: &Path, config: &Path) {
    command
        .env("RestoreConfigFile", config)
        .env("NUGET_PACKAGES", workspace.join("nuget").join("packages"))
        .env(
            "NUGET_HTTP_CACHE_PATH",
            workspace.join("nuget").join("http"),
        )
        .env("DOTNET_CLI_HOME", workspace.join("dotnet-home"));
}

fn totk_installer(context: &ContextData<'_>, request: &Request, jobs: usize) -> Result<Command> {
    // Dev-mode layout uses the running interpreter when no embedded Python exists.
    let python = context
        .source
        .join(".venv-py314")
        .join("Scripts")
        .join("python.exe");
    let mut command = context.command(&python)?;
    command
        .args(["-B", "-m", "installer", "--lang", "en-US", "--root"])
        .arg(context.source)
        .arg("--dest")
        .arg(context.output)
        .arg("--base")
        .arg(
            request
                .preference
                .game_input
                .as_ref()
                .context("Missing TotK base game")?,
        )
        .arg("--update")
        .arg(
            request
                .preference
                .update
                .as_ref()
                .context("Missing TotK update")?,
        )
        .arg("--keys")
        .arg(
            request
                .preference
                .keys
                .as_ref()
                .context("Missing console keys file")?,
        )
        .env("TOTK_LIFT_JOBS", jobs.to_string())
        .env("PYTHON_CPU_COUNT", jobs.to_string());
    if let Some(firmware) = &request.preference.firmware {
        command.arg("--firmware").arg(firmware);
    }
    Ok(command)
}

#[derive(Serialize)]
pub(super) struct SourceDependency {
    repository: &'static str,
    tag: &'static str,
    commit: &'static str,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build(
    request: &Request,
    workspace: &Path,
    source: &Path,
    output: &Path,
    git: &Path,
    uv: &Path,
    tools: &mut Toolset,
    runner: &mut Runner<'_>,
) -> Result<Vec<SourceDependency>> {
    let mut context = ContextData {
        workspace,
        source,
        output,
        paths: vec![git.parent().context("Git tool has no directory")?.into()],
    };
    let allowed = request.preference.managed_tools;
    let cancel = runner.cancel;
    let mut source_dependencies = Vec::new();
    match request.plan.game {
        GameId::Dusklight => {
            let cmake = tools.ensure(Package::Cmake, allowed, cancel, &mut |text| {
                runner.message(&text)
            })?;
            let ninja = tools.ensure(Package::Ninja, allowed, cancel, &mut |text| {
                runner.message(&text)
            })?;
            context.paths.push(
                cmake
                    .entry
                    .parent()
                    .context("CMake has no directory")?
                    .into(),
            );
            context.paths.push(ninja.root.clone());
            let python = python(&context, uv, &workspace.join("venv"), "3.14", &[], runner)?;
            runner.run(
                context
                    .command(uv)?
                    .args(["pip", "install", "--python"])
                    .arg(&python)
                    .arg("markupsafe"),
                "Preparing Dusklight's resource generator",
            )?;
            context.paths.insert(
                0,
                python.parent().context("Python has no directory")?.into(),
            );
            let vs = vs_dev(request.plan.game)?
                .context("Install Visual Studio C++ Build Tools first")?;
            let binary = workspace.join("b");
            let mut configure = context.vs_command(
                "call \"%TS_VSDEV%\" -arch=x64 -host_arch=x64 >nul && \"%TS_CMAKE%\" --preset windows-msvc-relwithdebinfo -B \"%TS_BUILD%\" \"-DCMAKE_INSTALL_PREFIX=%TS_OUTPUT%\" \"-DPython3_EXECUTABLE=%TS_PYTHON%\" \"-DCMAKE_MAKE_PROGRAM=%TS_NINJA%\"",
                &vs, &python,
            )?;
            configure
                .env("TS_CMAKE", &cmake.entry)
                .env("TS_BUILD", &binary)
                .env("TS_NINJA", &ninja.entry);
            runner.run(
                &mut configure,
                "Configuring Dusklight from the approved source",
            )?;
            let mut compile = context.vs_command(
                "call \"%TS_VSDEV%\" -arch=x64 -host_arch=x64 >nul && \"%TS_CMAKE%\" --build \"%TS_BUILD%\" --parallel %TS_JOBS%",
                &vs, &python,
            )?;
            let jobs = build_jobs(request, runner)?;
            compile
                .env("TS_CMAKE", &cmake.entry)
                .env("TS_BUILD", &binary)
                .env("TS_JOBS", jobs.to_string());
            runner.run(&mut compile, "Compiling Dusklight locally")?;
            let mut install = context.vs_command(
                "call \"%TS_VSDEV%\" -arch=x64 -host_arch=x64 >nul && \"%TS_CMAKE%\" --install \"%TS_BUILD%\" --prefix \"%TS_OUTPUT%\"",
                &vs, &python,
            )?;
            install
                .env("TS_CMAKE", &cmake.entry)
                .env("TS_BUILD", &binary);
            runner.run(
                &mut install,
                "Staging the source-built Dusklight program and resources",
            )?;
        }
        GameId::Totk => {
            fetch_shader_reference(source, git, runner)?;
            let llvm = tools.ensure(Package::Llvm, allowed, cancel, &mut |text| {
                runner.message(&text)
            })?;
            let llvm_destination = source
                .join("toolchain")
                .join("llvm-mingw-managed")
                .join("llvm-mingw-managed");
            runner.message(
                "Preparing the official compiler and installed MSYS2 development libraries...",
            )?;
            copy_tree(&llvm.root, &llvm_destination)?;
            let deps = source.join("toolchain").join("deps-mingw");
            stage_totk_sdk(Path::new(MSYS), &deps)?;
            context.paths.insert(0, Path::new(MSYS).join("bin"));
            if let Some(dotnet) = dotnet_root()? {
                context.paths.push(dotnet);
            }
            if let Some(program_files) = std::env::var_os("ProgramFiles(x86)") {
                context.paths.push(
                    PathBuf::from(program_files)
                        .join("Microsoft Visual Studio")
                        .join("Installer"),
                );
            }
            let main = python(
                &context,
                uv,
                &source.join(".venv"),
                "3.12",
                &[source.join("tools").join("requirements.txt")],
                runner,
            )?;
            python(
                &context,
                uv,
                &source.join(".venv-py314"),
                "3.14",
                &[source.join("tools").join("requirements-installer.txt")],
                runner,
            )?;
            let vs = vs_dev(request.plan.game)?
                .context("Install Visual Studio C++ Build Tools first")?;
            let scratch = workspace.join(SCRATCH);
            let nuget = prepare_nuget(&scratch)?;
            for (script, label) in [
                (
                    "call \"%TS_VSDEV%\" -arch=x64 -host_arch=x64 >nul && \"%TS_PYTHON%\" tools\\build_hactool.py",
                    "Building the extraction helper from source",
                ),
                (
                    "call \"%TS_VSDEV%\" -arch=x64 -host_arch=x64 >nul && \"%TS_PYTHON%\" tools\\build_xlate.py",
                    "Building the shader translator from source",
                ),
            ] {
                let mut command = context.vs_command(script, &vs, &main)?;
                configure_nuget(&mut command, &scratch, &nuget);
                runner.run(&mut command, label)?;
            }
            let jobs = build_jobs(request, runner)?;
            runner.run(&mut totk_installer(&context, request, jobs)?, "Compiling and preparing TotK from the checked-out source (shader preparation may open a window)")?;
        }
        GameId::Triforce => {
            let llvm = tools.ensure(Package::Llvm, allowed, cancel, &mut |text| {
                runner.message(&text)
            })?;
            let sdl2 = tools.ensure(Package::Sdl2, allowed, cancel, &mut |text| {
                runner.message(&text)
            })?;
            let sdl3 = tools.ensure(Package::Sdl3, allowed, cancel, &mut |text| {
                runner.message(&text)
            })?;
            let codecs = tools.ensure(Package::Ffmpeg, allowed, cancel, &mut |text| {
                runner.message(&text)
            })?;
            let sdl = source.join("third_party").join("sdl2");
            for relative in ["include", "lib"] {
                copy_tree(&sdl2.root.join(relative), &sdl.join(relative))?;
            }
            copy_tree(
                &sdl2.root.join("lib").join("x64").join("SDL2.dll"),
                &sdl.join("SDL2.dll"),
            )?;
            copy_tree(&sdl3.entry, &sdl.join("SDL3.dll"))?;
            let python = python(
                &context,
                uv,
                &workspace.join("venv"),
                "3.14",
                &[source.join("requirements.txt")],
                runner,
            )?;
            let headers = workspace.join("headers");
            fs::create_dir_all(&headers)?;
            let mut locations = Vec::new();
            for (repository, tag, commit, name, environment) in [
                (
                    "KhronosGroup/Vulkan-Headers",
                    "vulkan-sdk-1.4.363.0",
                    "6802bb4733b63ed5efd3adb308a6c885ef180ea1",
                    "vulkan",
                    "HOOKSHOT_VULKAN_HEADERS",
                ),
                (
                    "KhronosGroup/SPIRV-Headers",
                    "vulkan-sdk-1.4.363.0",
                    "496543121ce6419f23d6fa5d7194ba66c36212d2",
                    "spirv",
                    "HOOKSHOT_SPIRV_HEADERS",
                ),
                (
                    "GPUOpen-LibrariesAndSDKs/VulkanMemoryAllocator",
                    "v3.4.0",
                    "3aa921224c154a0d2c43912bc88e1c42ce1f7607",
                    "vma",
                    "HOOKSHOT_VMA_HEADERS",
                ),
            ] {
                let target = headers.join(name);
                checkout(
                    git,
                    &format!("https://github.com/{repository}.git"),
                    tag,
                    commit,
                    &target,
                    false,
                    runner,
                )?;
                source_dependencies.push(SourceDependency {
                    repository,
                    tag,
                    commit,
                });
                locations.push((environment, target));
            }
            let no_wgpu = workspace.join("no-webgpu");
            fs::create_dir(&no_wgpu)?;
            let cpm = workspace.join("cpm");
            let ffmpeg = cpm.join("ffmpeg").join("9.0.2-av1-1-windows-amd64");
            runner.message("Preparing required AV1 movie/audio codec libraries from the verified codec supplier archive...")?;
            copy_tree(&codecs.root, &ffmpeg)?;
            for library in ["avcodec.lib", "avutil.lib", "swscale.lib", "swresample.lib"] {
                ensure!(
                    ffmpeg.join("lib").join(library).is_file(),
                    "Required FFmpeg library is missing: {library}"
                );
            }
            let input = triforce_input(request)?;
            if let Some(previous) = &request.previous_install {
                match stage_triforce_caches(previous, output) {
                    Ok(true) => runner.message(
                        "Staged local shader caches; Triforce will check their compatibility.",
                    )?,
                    Ok(false) => {}
                    Err(error) => runner.message(&format!(
                        "Could not stage local shader caches; preparing shaders normally: {error:#}"
                    ))?,
                }
            }
            let jobs = build_jobs(request, runner)?;
            let mut command = context.command(&python)?;
            command
                .args(triforce_arguments(
                    &input,
                    output,
                    &workspace.join("work"),
                    &request.preference,
                    jobs,
                )?)
                .env("HOOKSHOT_MINGW", &llvm.root)
                .env("WGPU_NATIVE", no_wgpu)
                .env("RX_CPM", cpm)
                .env("RX_RELEASE", "1");
            if request.preference.nvidia_dlss {
                command
                    .env("HOOKSHOT_NGX_SDK", workspace.join("nvidia-ngx"))
                    .env(
                        "HOOKSHOT_STREAMLINE_SDK",
                        workspace.join("nvidia-streamline"),
                    );
            }
            for (environment, location) in locations {
                command.env(environment, location);
            }
            // Vulkan is built locally; the unverified custom wgpu-native binary is not used.
            ensure!(
                !source
                    .parent()
                    .context("Source directory has no parent")?
                    .join("HookshotBuilder-Setup.exe")
                    .try_exists()?,
                "Unexpected prebuilt Triforce launcher in the source workspace"
            );
            let log_start = runner.log.metadata()?.len();
            runner.run(&mut command, "Building Triforce and its Vulkan renderer from source (this can take an hour or more)")?;
            if request.preference.prepare_shaders {
                let mut log = BufReader::new(fs::File::open(&runner.log)?);
                log.seek(SeekFrom::Start(log_start))?;
                verify_triforce_shader_preparation(log)?;
            }
        }
    }
    Ok(source_dependencies)
}

fn stage_triforce_caches(previous: &Path, output: &Path) -> Result<bool> {
    const PROGRAMS: &str = "triforce_programs.bin";
    let size = |name| -> Result<u64> {
        let path = previous.join(name);
        ensure_safe_path(&path)?;
        match path.metadata() {
            Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
            Ok(_) => anyhow::bail!("Expected a regular cache file: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error).with_context(|| format!("Reading {}", path.display())),
        }
    };
    let mut names = Vec::new();
    const MAX_CACHE: u64 = 8 * 1024 * 1024 * 1024;
    if (1..=MAX_CACHE).contains(&size(PROGRAMS)?) {
        names.push(PROGRAMS);
    }
    for (cache, stamp) in [
        ("mkw_vk_pipelines.bin", "mkw_vk_pipelines.bin.for"),
        ("mkw_vk_pipelines_play.bin", "mkw_vk_pipelines_play.bin.for"),
    ] {
        if (33..=MAX_CACHE).contains(&size(cache)?) && size(stamp)? == 16 {
            names.extend([cache, stamp]);
        }
    }
    if names.is_empty() {
        return Ok(false);
    }
    // Let upstream validate its caches, but never carry over a completion marker.
    let staging = tempfile::tempdir_in(output)?;
    for name in &names {
        fs::copy(previous.join(name), staging.path().join(name))?;
    }
    for name in names {
        fs::rename(staging.path().join(name), output.join(name))?;
    }
    Ok(true)
}

fn verify_triforce_shader_preparation(mut log: impl BufRead) -> Result<()> {
    // Upstream can report shader failure while returning success for the executable build.
    let mut line = Vec::new();
    while log.read_until(b'\n', &mut line)? != 0 {
        if line.starts_with(b"the shaders were not prepared now (") {
            anyhow::bail!(
                "Triforce shader preparation did not finish: {}",
                String::from_utf8_lossy(&line).trim_end()
            );
        }
        line.clear();
    }
    Ok(())
}

fn build_jobs(request: &Request, runner: &mut Runner<'_>) -> Result<usize> {
    let jobs = request.preference.compiler_jobs();
    runner.message(&format!("Compiler worker limit: {jobs}"))?;
    Ok(jobs)
}

fn triforce_arguments(
    input: &Path,
    output: &Path,
    work: &Path,
    preference: &super::Preference,
    jobs: usize,
) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = ["-I", "-B", "nx2/make_game.py", "--out"]
        .into_iter()
        .map(Into::into)
        .collect();
    args.push(output.into());
    args.push("--work".into());
    args.push(work.into());
    args.push("--jobs".into());
    args.push(jobs.to_string().into());
    if !preference.renodx_hdr {
        args.push("--no-renodx".into());
    }
    if preference.amd_fsr4 {
        args.push("--fsr".into());
    }
    if preference.nvidia_dlss {
        args.extend(
            ["--fetch-ngx-sdk", "--accept-nvidia-license"]
                .into_iter()
                .map(OsString::from),
        );
    }
    if !preference.prepare_shaders {
        args.push("--no-prepare-shaders".into());
    }
    if input.is_dir() {
        let exefs = if input.join(".triforce_exefs").is_dir() {
            input.join(".triforce_exefs")
        } else {
            input.join("exefs")
        };
        ensure!(
            exefs.is_dir() && input.join("romfs").is_dir(),
            "The selected Ocarina of Time folder has no build inputs"
        );
        args.push("--exefs".into());
        args.push(exefs.into());
        args.push("--romfs".into());
        args.push(input.join("romfs").into());
    } else {
        args.push("--nup".into());
        args.push(input.into());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn windows_sdk_requires_current_headers_libraries_and_resource_compiler() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let files = [
            r"Include\10.0.26100.0\um\Windows.h",
            r"Include\10.0.26100.0\shared\winerror.h",
            r"Include\10.0.26100.0\ucrt\stdio.h",
            r"Lib\10.0.26100.0\um\x64\kernel32.lib",
            r"Lib\10.0.26100.0\ucrt\x64\ucrt.lib",
            r"bin\10.0.26100.0\x64\rc.exe",
        ];
        for file in files {
            assert!(!windows_sdk_ready_at(root, 26100)?);
            let path = root.join(file);
            fs::create_dir_all(path.parent().context("SDK fixture has no parent")?)?;
            fs::write(path, b"synthetic SDK fixture")?;
        }
        assert!(windows_sdk_ready_at(root, 26100)?);
        assert!(!windows_sdk_ready_at(root, 26200)?);
        fs::remove_file(root.join(files[0]))?;
        assert!(!windows_sdk_ready_at(root, 26100)?);
        Ok(())
    }

    #[test]
    #[cfg(windows)]
    fn custom_visual_studio_paths_require_a_complete_current_compiler() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let installation = temp.path().join("Custom developer's tools");
        let script = installation.join(r"Common7\Tools\VsDevCmd.bat");
        fs::create_dir_all(
            script
                .parent()
                .context("Developer-script fixture has no parent")?,
        )?;
        fs::write(&script, b"synthetic developer environment")?;
        let roots = [installation.clone()];
        assert!(vs_dev_in(&roots, GameId::Totk)?.is_none());
        for (version, totk_ready) in [("14.49.1", false), ("14.50.1", true)] {
            let compiler = installation
                .join(r"VC\Tools\MSVC")
                .join(version)
                .join(r"bin\Hostx64\x64\cl.exe");
            fs::create_dir_all(
                compiler
                    .parent()
                    .context("Compiler fixture has no parent")?,
            )?;
            fs::write(compiler, b"synthetic compiler fixture")?;
            assert_eq!(vs_dev_in(&roots, GameId::Totk)?.is_some(), totk_ready);
            assert_eq!(vs_dev_in(&roots, GameId::Dusklight)?, Some(script.clone()));
        }
        Ok(())
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "Uses the installed SDK and verified compiler to compile only synthetic C++ headers, never game code"]
    fn real_totk_sdk_compiles_cpp_with_the_managed_runtime_headers() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let staged = temp.path().join("deps");
        stage_totk_sdk(Path::new(MSYS), &staged)?;
        let root = crate::config::root_path()?;
        let mut tools = Toolset::new(&root)?;
        let llvm = tools.ensure(
            Package::Llvm,
            true,
            &std::sync::atomic::AtomicBool::new(false),
            &mut |_| Ok(()),
        )?;
        let fixture = temp.path().join("sdk.cpp");
        fs::write(&fixture, b"#include <atomic>\n#include <cstring>\n#include <mutex>\n#include <SDL3/SDL.h>\n#include <vk_mem_alloc.h>\nint main() { std::atomic<int> n{0}; return n.load(); }\n")?;
        let output = Command::new(&llvm.entry)
            .args(["-x", "c++", "-fsyntax-only", "-I"])
            .arg(staged.join("include"))
            .arg("-I")
            .arg(staged.join("include").join("vma"))
            .arg(&fixture)
            .output()?;
        ensure!(
            output.status.success(),
            "Managed C++ SDK probe failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        println!(
            "Actual managed Clang and staged MSYS2 SDK compile the C++ runtime headers without CRT shadowing."
        );
        Ok(())
    }

    #[test]
    fn totk_sdk_staging_never_shadows_the_managed_compilers_crt_or_cpp_runtime() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let sdk = temp.path().join("ucrt64");
        for directory in ["SDL3", "opus", "vpx", "vulkan", "vk_video", "vma"] {
            let path = sdk.join("include").join(directory);
            fs::create_dir_all(&path)?;
            fs::write(
                path.join(if directory == "vma" {
                    "vk_mem_alloc.h"
                } else {
                    "fixture.h"
                }),
                b"third-party SDK header",
            )?;
        }
        fs::write(sdk.join("include").join("zstd.h"), b"zstd")?;
        fs::write(sdk.join("include").join("zstd_errors.h"), b"zstd errors")?;
        fs::write(
            sdk.join("include").join("string.h"),
            b"#error wrong C runtime",
        )?;
        fs::write(
            sdk.join("include").join("windows.h"),
            b"#error wrong Windows headers",
        )?;
        fs::create_dir(sdk.join("lib"))?;
        for name in [
            "libSDL3.dll.a",
            "libopus.dll.a",
            "libvpx.a",
            "libvulkan-1.dll.a",
            "libzstd.a",
            "libstdc++.a",
        ] {
            fs::write(sdk.join("lib").join(name), b"synthetic SDK library")?;
        }
        fs::create_dir(sdk.join("bin"))?;
        for name in [
            "SDL3.dll",
            "libopus-0.dll",
            "libiconv-2.dll",
            "unrelated.dll",
        ] {
            fs::write(sdk.join("bin").join(name), b"synthetic SDK runtime")?;
        }
        let staged = temp.path().join("staged");
        stage_totk_sdk(&sdk, &staged)?;
        assert!(
            staged
                .join("include")
                .join("vma")
                .join("vk_mem_alloc.h")
                .is_file()
        );
        assert!(staged.join("lib").join("libvpx.a").is_file());
        assert!(staged.join("bin").join("libiconv-2.dll").is_file());
        for path in [
            staged.join("include").join("string.h"),
            staged.join("include").join("windows.h"),
            staged.join("lib").join("libstdc++.a"),
            staged.join("bin").join("unrelated.dll"),
        ] {
            assert!(!path.try_exists()?);
        }
        Ok(())
    }

    #[test]
    fn dotnet_restore_uses_an_isolated_official_feed_and_cache() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let workspace = temp.path().join("build");
        fs::create_dir(&workspace)?;
        let config = prepare_nuget(&workspace)?;
        let text = fs::read_to_string(&config)?;
        assert!(text.contains("<clear />"));
        assert_eq!(text.matches("<add ").count(), 1);
        assert!(text.contains("https://api.nuget.org/v3/index.json"));
        let mut command = Command::new("not-executed");
        configure_nuget(&mut command, &workspace, &config);
        let environment: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            environment.get(std::ffi::OsStr::new("RestoreConfigFile")),
            Some(&Some(config.as_os_str()))
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("NUGET_PACKAGES")),
            Some(&Some(workspace.join("nuget").join("packages").as_os_str()))
        );
        Ok(())
    }

    #[test]
    fn totk_source_installer_uses_its_python314_environment_and_local_inputs() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        let output = temp.path().join("new game");
        let context = ContextData {
            workspace: temp.path(),
            source: &source,
            output: &output,
            paths: Vec::new(),
        };
        let request = Request {
            plan: super::super::Plan {
                game: GameId::Totk,
                tag: "v0.4.1".into(),
                commit: "a".repeat(40),
                review_url: String::new(),
            },
            preference: super::super::Preference {
                game_input: Some(temp.path().join("base game.nsp")),
                update: Some(temp.path().join("update.nsp")),
                keys: Some(temp.path().join("prod.keys")),
                ..Default::default()
            },
            previous_install: None,
            previous_executable: None,
            approved: true,
            log: temp.path().join("not-created.log"),
        };
        let command = totk_installer(&context, &request, 4)?;
        assert_eq!(
            command.get_program(),
            source
                .join(".venv-py314")
                .join("Scripts")
                .join("python.exe")
        );
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        let uv_cache = context.workspace.join(SCRATCH).join("uv");
        let python = context.workspace.join("python");
        for (key, value) in [
            ("UV_CACHE_DIR", uv_cache.as_os_str()),
            ("UV_PYTHON_INSTALL_DIR", python.as_os_str()),
            ("UV_LINK_MODE", "hardlink".as_ref()),
        ] {
            assert_eq!(env.get(std::ffi::OsStr::new(key)), Some(&Some(value)));
        }
        let args: Vec<_> = command.get_args().collect();
        assert!(args.windows(2).any(|args| args == ["--lang", "en-US"]));
        for (flag, path) in [
            ("--root", source.as_path()),
            ("--dest", output.as_path()),
            (
                "--base",
                request
                    .preference
                    .game_input
                    .as_deref()
                    .context("Missing fixture base")?,
            ),
            (
                "--update",
                request
                    .preference
                    .update
                    .as_deref()
                    .context("Missing fixture update")?,
            ),
            (
                "--keys",
                request
                    .preference
                    .keys
                    .as_deref()
                    .context("Missing fixture keys")?,
            ),
        ] {
            assert!(
                args.windows(2)
                    .any(|args| args == [flag.as_ref(), path.as_os_str()])
            );
        }
        assert!(!request.log.try_exists()?);
        Ok(())
    }

    #[test]
    fn msys2_detection_accepts_both_upstream_vma_layouts_and_reports_missing_dependencies()
    -> Result<()> {
        for nested in [false, true] {
            let temp = tempfile::tempdir()?;
            let ucrt = temp.path().join("ucrt64");
            for relative in [
                Path::new("bin").join("gcc.exe"),
                Path::new("bin").join("ninja.exe"),
                Path::new("bin").join("SDL3.dll"),
                Path::new("include").join("vulkan").join("vulkan.h"),
                Path::new("include").join("opus").join("opus.h"),
                Path::new("include").join("vpx").join("vpx_decoder.h"),
            ] {
                let path = ucrt.join(relative);
                fs::create_dir_all(path.parent().context("Fixture file has no parent")?)?;
                fs::write(path, b"synthetic SDK data, never executed")?;
            }
            let include = ucrt.join("include");
            let header = if nested {
                fs::create_dir(include.join("vma"))?;
                include.join("vma").join("vk_mem_alloc.h")
            } else {
                include.join("vk_mem_alloc.h")
            };
            fs::write(&header, b"synthetic allocator header")?;
            assert!(missing_msys2_files(&ucrt)?.is_empty());
            let staged = temp.path().join("deps-mingw").join("include");
            copy_tree(&include, &staged)?;
            assert_eq!(
                fs::read(staged.join(header.strip_prefix(&include)?))?,
                b"synthetic allocator header"
            );
            fs::remove_file(ucrt.join("bin").join("SDL3.dll"))?;
            let missing = missing_msys2_files(&ucrt)?;
            assert_eq!(
                missing,
                [ucrt.join("bin").join("SDL3.dll").display().to_string()]
            );
            fs::remove_file(header)?;
            let missing = missing_msys2_files(&ucrt)?;
            assert_eq!(missing.len(), 2);
            assert!(missing[1].contains("vk_mem_alloc.h") && missing[1].contains(" or "));
        }
        Ok(())
    }

    #[test]
    fn triforce_reuses_only_shader_cache_data_with_its_stamp() -> Result<()> {
        let current = tempfile::tempdir()?;
        let output = tempfile::tempdir()?;
        fs::write(current.path().join("mkw_vk_pipelines.bin"), [7; 64])?;
        fs::write(current.path().join("triforce_prepared.bin"), [9; 48])?;
        assert!(!stage_triforce_caches(current.path(), output.path())?);
        fs::write(current.path().join("mkw_vk_pipelines.bin.for"), [1; 16])?;
        fs::write(current.path().join("triforce_programs.bin"), [2; 64])?;
        assert!(stage_triforce_caches(current.path(), output.path())?);
        assert_eq!(
            fs::read(output.path().join("mkw_vk_pipelines.bin"))?,
            [7; 64]
        );
        assert_eq!(
            fs::read(output.path().join("mkw_vk_pipelines.bin.for"))?,
            [1; 16]
        );
        assert_eq!(
            fs::read(output.path().join("triforce_programs.bin"))?,
            [2; 64]
        );
        assert!(!output.path().join("triforce_prepared.bin").exists());
        Ok(())
    }

    #[test]
    fn triforce_shader_failure_is_not_counted_as_a_ready_install() -> Result<()> {
        verify_triforce_shader_preparation(&b"@@stage shaders Preparing shaders\n@@done\n"[..])?;
        let error = verify_triforce_shader_preparation(
            &b"the shaders were not prepared now (no usable Vulkan here): the game prepares them at its first start\r\n@@done\n"[..],
        );
        assert!(error.is_err_and(|error| error.to_string().contains("no usable Vulkan here")));
        Ok(())
    }

    #[test]
    fn triforce_reuses_the_play_cache_only_with_its_own_stamp() -> Result<()> {
        let current = tempfile::tempdir()?;
        let output = tempfile::tempdir()?;
        let cache = "mkw_vk_pipelines_play.bin";
        let stamp = "mkw_vk_pipelines_play.bin.for";
        fs::write(current.path().join(cache), [4; 64])?;
        fs::write(current.path().join("mkw_vk_pipelines.bin.for"), [1; 16])?;
        assert!(!stage_triforce_caches(current.path(), output.path())?);
        fs::write(current.path().join(stamp), [9; 15])?;
        assert!(!stage_triforce_caches(current.path(), output.path())?);
        fs::write(current.path().join(stamp), [9; 16])?;
        assert!(stage_triforce_caches(current.path(), output.path())?);
        assert_eq!(fs::read(output.path().join(cache))?, [4; 64]);
        assert_eq!(fs::read(output.path().join(stamp))?, [9; 16]);
        assert!(
            !output
                .path()
                .join("mkw_vk_pipelines.bin.for")
                .try_exists()?
        );
        Ok(())
    }

    #[test]
    fn triforce_source_builds_never_update_an_existing_installation_in_place() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let current = temp.path().join("existing game");
        fs::create_dir_all(current.join(".triforce_exefs"))?;
        fs::create_dir(current.join("romfs"))?;
        let output = temp.path().join("staged output");
        let preference = super::super::Preference {
            renodx_hdr: false,
            prepare_shaders: false,
            ..Default::default()
        };
        let args =
            triforce_arguments(&current, &output, &temp.path().join("work"), &preference, 7)?;
        assert!(args.windows(2).any(|pair| pair == ["--jobs", "7"]));
        assert!(args.contains(&OsString::from("--no-prepare-shaders")));
        assert!(args.contains(&OsString::from("--no-renodx")));
        assert!(args.contains(&current.join(".triforce_exefs").into_os_string()));
        assert!(args.contains(&output.into_os_string()));
        assert!(!args.contains(&OsString::from("--update")));
        assert!(!current.join("Triforce.exe").try_exists()?);
        Ok(())
    }

    #[test]
    fn triforce_feature_flags_keep_upstream_defaults_and_require_nvidia_opt_in() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("own-game.nup");
        let output = temp.path().join("new-game");
        let work = temp.path().join("work");
        let mut preference = super::super::Preference::default();
        let args = triforce_arguments(&input, &output, &work, &preference, 4)?;
        for flag in [
            "--no-renodx",
            "--no-prepare-shaders",
            "--fsr",
            "--fetch-ngx-sdk",
            "--accept-nvidia-license",
        ] {
            assert!(!args.contains(&OsString::from(flag)));
        }
        preference.amd_fsr4 = true;
        preference.nvidia_dlss = true;
        let args = triforce_arguments(&input, &output, &work, &preference, 4)?;
        for flag in ["--fsr", "--fetch-ngx-sdk", "--accept-nvidia-license"] {
            assert!(args.contains(&OsString::from(flag)));
        }
        Ok(())
    }

    #[test]
    fn missing_game_inputs_fail_before_running_project_code() -> Result<()> {
        let temp = tempfile::tempdir()?;
        for game in [GameId::Totk, GameId::Triforce] {
            let request = Request {
                plan: super::super::Plan {
                    game,
                    tag: "v1.0.0".into(),
                    commit: "a".repeat(40),
                    review_url: String::new(),
                },
                preference: super::super::Preference::default(),
                previous_install: None,
                previous_executable: None,
                approved: true,
                log: temp.path().join("not-created.log"),
            };
            assert!(validate_inputs(&request).is_err());
            assert!(!request.log.try_exists()?);
        }
        Ok(())
    }

    #[test]
    #[cfg(windows)]
    fn compiler_environment_and_command_share_one_process_with_quoted_paths() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let directory = temp.path().join("tools and source with spaces");
        fs::create_dir(&directory)?;
        let script = directory.join("local environment.cmd");
        fs::write(&script, b"@set TS_FROM_VS=ready\r\n")?;
        let context = ContextData {
            workspace: &directory,
            source: &directory,
            output: &directory,
            paths: Vec::new(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut progress = |_| Ok(());
        let mut runner = Runner::new(&directory.join("test.log"), &cancel, &mut progress)?;
        let output = runner.capture(
            &mut context.vs_command(
                "call \"%TS_VSDEV%\" >nul && set TS_FROM_VS",
                &script,
                &directory.join("not-run.exe"),
            )?,
            "Checking the locally authored environment fixture",
        )?;
        assert!(std::str::from_utf8(&output)?.contains("TS_FROM_VS=ready"));
        Ok(())
    }
}

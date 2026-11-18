use crate::files::{atomic_write, ensure_safe_path, valid_hash};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};
use zeroize::Zeroize;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub public_key: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayLock {
    pub owner: String,
    pub token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u8,
    pub name: String,
    pub startup: bool,
    pub network: Option<String>,
    pub peer: Option<Peer>,
    // Derived by discovery; persisted only to bind the common sync base to its local save.
    pub save_file: Option<PathBuf>,
    pub game_exe: Option<PathBuf>,
    #[serde(default)]
    pub texture_profile: Option<PathBuf>,
    pub iso_file: Option<PathBuf>,
    pub iso_enabled: bool,
    pub sync_base: Option<String>,
    pub play_lock: Option<PlayLock>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Twilight device".into()),
            startup: true,
            network: None,
            peer: None,
            save_file: None,
            game_exe: None,
            texture_profile: None,
            iso_file: None,
            iso_enabled: false,
            sync_base: None,
            play_lock: None,
        }
    }
}

impl Settings {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("settings.json");
        ensure_safe_path(&path)?;
        if !path.try_exists()? {
            return Ok(Self::default());
        }
        ensure!(
            path.metadata()?.len() <= 16 * 1024,
            "Settings file is too large"
        );
        let settings: Self = serde_json::from_slice(&fs::read(path)?)?;
        ensure!(settings.version == 1, "Unsupported settings version");
        ensure!(valid_label(&settings.name), "Invalid device name");
        for hash in [&settings.network, &settings.sync_base]
            .into_iter()
            .flatten()
        {
            ensure!(valid_hash(hash), "Invalid stored hash");
        }
        if let Some(peer) = &settings.peer {
            ensure!(
                valid_hash(&peer.public_key) && valid_label(&peer.name),
                "Invalid stored peer"
            );
        }
        if let Some(lock) = &settings.play_lock {
            ensure!(
                valid_hash(&lock.owner) && valid_hash(&lock.token),
                "Invalid stored play lock"
            );
        }
        Ok(settings)
    }

    pub fn persist(&self, root: &Path) -> Result<()> {
        atomic_write(
            &root.join("settings.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
}

pub fn valid_label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 64 && !value.chars().any(char::is_control)
}

pub struct Identity {
    private: Vec<u8>,
    pub public: String,
}

impl Drop for Identity {
    fn drop(&mut self) {
        self.private.zeroize();
    }
}

impl Identity {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("identity.dpapi");
        ensure_safe_path(&path)?;
        let private = if path.try_exists()? {
            ensure!(
                path.metadata()?.len() <= 4096,
                "Invalid protected identity size"
            );
            unprotect(&fs::read(&path)?)?
        } else {
            let pair = snow::Builder::new(crate::protocol::noise_params()?).generate_keypair()?;
            atomic_write(&path, &protect(&pair.private)?)?;
            pair.private
        };
        ensure!(private.len() == 32, "Invalid identity key length");
        let resolver = snow::resolvers::DefaultResolver;
        let mut dh = snow::resolvers::CryptoResolver::resolve_dh(
            &resolver,
            &snow::params::DHChoice::Curve25519,
        )
        .context("Curve25519 is unavailable")?;
        dh.set(&private);
        let public = hex::encode(dh.pubkey());
        Ok(Self { private, public })
    }

    pub fn private_key(&self) -> &[u8] {
        &self.private
    }
}

pub fn root_path() -> Result<PathBuf> {
    let home = directories::UserDirs::new().context("Cannot find user home directory")?;
    let root = home.home_dir().join(".twilight-sync");
    ensure_safe_path(&root)?;
    Ok(root)
}

pub fn root_directory() -> Result<PathBuf> {
    let root = root_path()?;
    fs::create_dir_all(&root)?;
    restrict_directory(&root)?;
    Ok(root)
}

pub fn acquire_instance(root: &Path) -> Result<File> {
    let path = root.join("instance.lock");
    ensure_safe_path(&path)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&file).context("Twilight Sync is already running")?;
    Ok(file)
}

#[cfg(windows)]
fn restrict_directory(root: &Path) -> Result<()> {
    ensure_private_directory(root).map(|_| ())
}

#[cfg(windows)]
const PRIVATE_DIRECTORY_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
$system = [System.Security.Principal.SecurityIdentifier]::new('S-1-5-18')
$inherit = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
$full = [System.Security.AccessControl.FileSystemRights]::FullControl
$allow = [System.Security.AccessControl.AccessControlType]::Allow
$none = [System.Security.AccessControl.PropagationFlags]::None
function Test-PrivateDirectory($acl) {
    if (-not $acl.AreAccessRulesProtected) { return $false }
    $descriptor = [System.Security.AccessControl.RawSecurityDescriptor]::new(
        $acl.GetSecurityDescriptorBinaryForm(), 0)
    if ($null -eq $descriptor.DiscretionaryAcl -or $descriptor.DiscretionaryAcl.Count -ne 2) {
        return $false
    }
    $rules = @($acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier]))
    if ($rules.Count -ne 2) { return $false }
    foreach ($identity in @($sid, $system)) {
        $matching = @($rules | Where-Object { $_.IdentityReference.Value -eq $identity.Value })
        if ($matching.Count -ne 1) { return $false }
        $rule = $matching[0]
        if ($rule.IsInherited -or $rule.AccessControlType -ne $allow -or
            $rule.FileSystemRights -ne $full -or $rule.InheritanceFlags -ne $inherit -or
            $rule.PropagationFlags -ne $none) { return $false }
    }
    return $true
}
$root = $env:TWILIGHT_SYNC_PRIVATE_DIRECTORY
$acl = [System.IO.Directory]::GetAccessControl($root)
if (Test-PrivateDirectory $acl) {
    'false'
} else {
    $private = [System.Security.AccessControl.DirectorySecurity]::new()
    $private.SetAccessRuleProtection($true, $false)
    foreach ($identity in @($sid, $system)) {
        $private.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
            $identity, $full, $inherit, $none, $allow))
    }
    [System.IO.Directory]::SetAccessControl($root, $private)
    if (-not (Test-PrivateDirectory ([System.IO.Directory]::GetAccessControl($root)))) {
        throw 'Application data permissions did not become private'
    }
    'true'
}
"#;

#[cfg(windows)]
fn ensure_private_directory(root: &Path) -> Result<bool> {
    use std::os::windows::process::CommandExt;
    let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    let output = std::process::Command::new(
        PathBuf::from(windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
    )
    .args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        PRIVATE_DIRECTORY_SCRIPT,
    ])
    .env("TWILIGHT_SYNC_PRIVATE_DIRECTORY", root)
    .creation_flags(0x08000000)
    .output()
    .context("Checking application data permissions")?;
    ensure!(
        output.status.success(),
        "Cannot verify or repair application data permissions: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .context("Invalid application data permission-check response")
}

#[cfg(not(windows))]
fn restrict_directory(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
fn dpapi(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len())?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // DPAPI owns the output allocation; copy it before releasing it with LocalFree.
    unsafe {
        let result = if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        ensure!(
            result != 0,
            "Windows DPAPI failed: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            !output.pbData.is_null(),
            "Windows DPAPI returned no output allocation"
        );
        let buffer = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let data = buffer.to_vec();
        buffer.zeroize();
        ensure!(
            LocalFree(output.pbData.cast()).is_null(),
            "Windows DPAPI output could not be freed"
        );
        Ok(data)
    }
}

#[cfg(windows)]
fn protect(bytes: &[u8]) -> Result<Vec<u8>> {
    dpapi(bytes, true)
}
#[cfg(windows)]
fn unprotect(bytes: &[u8]) -> Result<Vec<u8>> {
    dpapi(bytes, false)
}
#[cfg(not(windows))]
fn protect(_: &[u8]) -> Result<Vec<u8>> {
    anyhow::bail!("Identity storage currently requires Windows DPAPI")
}
#[cfg(not(windows))]
fn unprotect(_: &[u8]) -> Result<Vec<u8>> {
    anyhow::bail!("Identity storage currently requires Windows DPAPI")
}

#[cfg(windows)]
pub fn set_startup(enabled: bool) -> Result<()> {
    use winreg::{
        RegKey,
        enums::{HKEY_CURRENT_USER, KEY_SET_VALUE},
    };
    let (run, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Run",
        KEY_SET_VALUE,
    )?;
    if enabled {
        let exe = std::env::current_exe()?;
        ensure_safe_path(&exe)?;
        run.set_value(
            "TwilightSync",
            &format!("\"{}\" --background", exe.display()),
        )?;
    } else {
        match run.delete_value("TwilightSync") {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Disabling startup"),
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn set_startup(_: bool) -> Result<()> {
    anyhow::bail!("Startup integration currently requires Windows")
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn private_directory_permissions_skip_unchanged_acls_and_repair_extra_access() -> Result<()> {
        use std::{os::windows::process::CommandExt, process::Command};

        let temp = tempfile::Builder::new()
            .prefix("twilight ACL ' ")
            .tempdir()?;
        let sentinel = temp.path().join("retained.txt");
        fs::write(&sentinel, b"preserve existing contents")?;
        assert!(ensure_private_directory(temp.path())?);
        assert!(!ensure_private_directory(temp.path())?);

        let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
        let output = Command::new(
            PathBuf::from(windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
        )
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            r#"
$ErrorActionPreference = 'Stop'
$root = $env:TWILIGHT_SYNC_PRIVATE_DIRECTORY
$acl = [System.IO.Directory]::GetAccessControl($root)
$users = [System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-545')
$acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
    $users, 'ReadAndExecute', 'ContainerInherit, ObjectInherit', 'None', 'Allow'))
[System.IO.Directory]::SetAccessControl($root, $acl)
"#,
        ])
        .env("TWILIGHT_SYNC_PRIVATE_DIRECTORY", temp.path())
        .creation_flags(0x08000000)
        .output()?;
        ensure!(
            output.status.success(),
            "Cannot prepare ACL test: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(ensure_private_directory(temp.path())?);
        assert!(!ensure_private_directory(temp.path())?);
        assert_eq!(fs::read(sentinel)?, b"preserve existing contents");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn private_directory_permission_errors_are_not_treated_as_a_successful_check() -> Result<()> {
        let temp = tempfile::tempdir()?;
        assert!(ensure_private_directory(&temp.path().join("missing")).is_err());
        Ok(())
    }

    #[test]
    fn older_settings_keep_their_pairing_and_play_lock_when_texture_settings_are_added()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let settings = Settings {
            peer: Some(Peer {
                public_key: "b".repeat(64),
                name: "Other device".into(),
            }),
            play_lock: Some(PlayLock {
                owner: "a".repeat(64),
                token: "c".repeat(64),
            }),
            ..Settings::default()
        };
        let mut value = serde_json::to_value(&settings)?;
        value
            .as_object_mut()
            .context("Settings are not an object")?
            .remove("texture_profile");
        atomic_write(
            &temp.path().join("settings.json"),
            &serde_json::to_vec(&value)?,
        )?;
        let loaded = Settings::load(temp.path())?;
        assert!(loaded.texture_profile.is_none());
        assert_eq!(
            loaded.peer.context("Pairing was lost")?.public_key,
            "b".repeat(64)
        );
        assert_eq!(
            loaded.play_lock.context("Play lock was lost")?.token,
            "c".repeat(64)
        );
        Ok(())
    }
}

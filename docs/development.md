# Development and releases

[Installation and user guide](../README.md) · [Technical guide](guide.md)

## Local validation

Run from the repository root on Windows with Rust 1.95+ and the MSVC build tools:

```powershell
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
powershell -NoProfile -File .\scripts\Test-Installer.ps1
pwsh -NoProfile -File .\scripts\Test-Installer.ps1
cargo build --release --locked
```

Installer tests load local function declarations without running the bootstrap
entry point. They substitute synthetic network responses and PE-shaped test
bytes, never launch those fixtures, never create real shortcuts, and install
only into a dedicated temporary test directory.

The optional live updater test and debug-only native UI smoke commands are
documented in the [technical guide](guide.md#diagnostics-and-tests).

## Windows release packaging

The checked-in Cargo configuration statically links the MSVC C runtime for the
Windows x64 build. Verify the application and installer before publishing:

```powershell
.\scripts\Package-Release.ps1 -Version v0.3.2
```

This writes exactly the supported release assets to the ignored `dist` folder:

- `twilight-sync-windows-x86_64.exe`
- `install.ps1`
- `SHA256SUMS.txt`

The package script refuses a tag that differs from the package version.
Checksums cover both the executable and the bootstrap script.

## Publishing future versions

1. Update the Cargo package version and lockfile, validate the code, and commit.
2. Create and push the matching stable tag, such as `v0.3.1`.
3. The release workflow builds and tests on Windows, creates a draft release,
   uploads all assets, then publishes it as latest.
4. Verify the public one-liner in an isolated folder with `-NoLaunch -NoShortcut`
   before announcing the release.

The release workflow can also be dispatched for an existing matching tag. It
refuses to overwrite an existing release. Review a failed draft before deleting
or replacing anything; published release files should remain immutable.

The GitHub token is supplied by Actions at runtime, never embedded in scripts.
Only reviewed, pinned checkout actions are used. No game assets, save files,
private logs, identity keys, or texture archives belong in source control or
release assets.

## Consent-based app self-updates

The automatic checker uses public release metadata only. Installation is a
separate user-approved action, with strict SemVer comparison and matching GitHub
asset/checksum-file SHA-256 values. The user can disable periodic checks without
changing the game's settings schema.

The staged native helper must acknowledge readiness before the manager exits.
It verifies its own compiled version and original process identity, waits for
clean exit without terminating that process, and uses the installer's lock plus
same-directory atomic replacement. Tests launch isolated PE-shaped copies of
the Rust test harness to exercise real Windows process/file-lock behavior; no
real app profile or startup entries are used. Internal ignored subprocess
fixtures are invoked only by their integration test.

## Licensing and security

Twilight Sync's original code is MIT-licensed. Dependencies retain their own
licenses; retain the third-party notices when distributing binaries.
The notices are embedded in the executable and available through `--licenses`.
When dependencies or package metadata change, regenerate them before packaging:

```powershell
cargo install cargo-about --version 0.9.2 --locked --features cli
pwsh -NoProfile -File .\scripts\Generate-Notices.ps1
```

The generator uses the Windows dependency graph and separately includes the
licenses of every embedded dashboard font. Packaging refuses stale Cargo
manifest/lockfile notice stamps. Fingerprints normalize CRLF to LF so clean
Windows and Linux checkouts agree; original third-party license whitespace is
otherwise preserved.

The bootstrap runs as the current user and does not elevate, alter PATH,
terminate processes, disable certificate checks, or change security policy.
It verifies GitHub's asset digest and the published checksum list before
replacing its managed executable. This does not defend against a compromised
publisher account. The public README offers a manual, inspectable installation
path for users who do not want to run a downloaded script.

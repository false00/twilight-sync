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

### Automatic save setup

```powershell
cargo test --lib --locked -- save_location::tests runtime::tests ui::tests
```

Profile tests use isolated directories and the exact released Dusklight
descriptor/default-path conventions. Runtime tests cover automatic detection,
first receive/restore into the correct region, empty profiles without
placeholder files, pending native migrations, and profile pinning/revalidation.
Discovery is read-only; only an approved receive/restore prepares save folders.
The normal game launch is retained so Dusklight can handle its own profile
migrations and preferences. The read-only `--check-save-location` diagnostic
accepts no custom directory and does not initialize startup or write settings.

### Raw memory cards

```powershell
cargo test --lib --locked -- raw_card::tests files::tests runtime::tests
```

Synthetic native-format fixtures cover fragmented chains, unrelated game saves,
redundant metadata, wrapping counters, corruption, full/busy cards, atomic
replacement, canonical GCI hashing, and mixed raw/GCI sync/restore. A separate
`gc-memcard` parser is a test-only interoperability oracle; the production
adapter never rebuilds or repacks unrelated saves. Both native PC and original
console save-envelope checksums are tested.

The ignored `authorized_native_card_copy_round_trip` test accepts
`TWILIGHT_RAW_TEST_PATH` only when explicitly invoked. It reads that input,
performs write/restore checks in temporary copies, checks an independent parser,
and verifies the supplied input did not change. Never point automatic CI tests
at real profiles or modify a live card for validation.

### Disc-image and Windows icon checks

```powershell
cargo test --lib --locked -- disc::tests protocol::tests runtime::tests ui::icons::tests
python .\scripts\Generate-Icons.py
```

Disc tests generate synthetic RVZ data without copyrighted game assets. They
exercise uncompressed and Zstandard reconstruction, size/metadata limits,
official-catalog rejection, bounded worker resource checks, and cancellation.
Actor tests separately inject worker results to exercise consent, stale-job
rejection, final game/network/permission checks, compressed-byte commits, and
no-clobber behavior. The ignored RVZ subprocess fixture is invoked only by its
parent test. These fixtures do not replace read-only testing of an authorized
real disc image with `--check-iso PATH`, which accepts ISO and RVZ.

The original icon artwork and generated PNG/ICO assets live in `assets`.
The standard-library-only generator produces 16/24/32/48/64/128/256-pixel ICO
frames plus matching tray/window PNGs; keep its geometry and the SVG artwork in
sync. The build script embeds the Windows executable icon/version resources.
The runtime sets a stable application identity and supplies matching window and
tray icons. Verify both PE resources and real native window icons in a debug UI
smoke session; do not modify the running installation for a visual test.

## Windows release packaging

The checked-in Cargo configuration statically links the MSVC C runtime for the
Windows x64 build. Verify the application and installer before publishing:

```powershell
.\scripts\Package-Release.ps1 -Version v0.7.0
```

This writes exactly the supported release assets to the ignored `dist` folder:

- `twilight-sync-windows-x86_64.exe`
- `install.ps1`
- `SHA256SUMS.txt`

The package script refuses a tag that differs from the package version.
Checksums cover both the executable and the bootstrap script.

## Publishing future versions

1. Update the Cargo package version and lockfile, refresh dependency notices,
   validate the code, and commit.
2. Push `main` and wait for CI on that exact commit to pass.
3. Create and push the matching fresh stable tag, such as `v0.7.1`.
4. The release workflow builds and tests on Windows, creates a draft release,
   uploads all assets, then publishes it as latest.
5. Verify the public one-liner in an isolated folder with `-NoLaunch -NoShortcut`
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

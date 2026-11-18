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

### TotK program updates

```powershell
cargo test --lib --locked -- install::tests totk_update::tests totk::tests runtime::tests ui::tests
```

Updater tests use synthetic setup archives and receipts without executing those
fixtures. They cover official asset selection, the larger bundled-tool inventory,
first-install inputs, stable empty/resumable targets, installer arguments,
completion/version checks, save preservation and play/sync interlocks.
They also verify the new `Games\Dusklight` layout without changing legacy
installations and exercise the managed first-install action without opening a
browser. The optional official-package test runs only the bundled setup's
`--help` in an isolated directory; it never installs or launches the game.

### Source builds and Triforce parity

```powershell
cargo test --lib --locked -- source_build:: triforce totk::tests runtime::tests ui:: protocol::game_channel_tests lan::tests
```

Fixtures verify source-commit/approval gating, refusal to run without game inputs,
publisher URL/digest checks, side-by-side command arguments, unchanged-save
activation, common-base preservation, failed/cancelled build recovery, and
Triforce's complete saves, nested transactions, encrypted two-device transfer,
history restore and durable play locks. Triforce metadata tests use synthetic
variable-length tables, exercise progress cards, and reject malformed recognized
metadata before save replacement. SDK commands/approval guards, graphics options,
Vulkan-setting preservation and codec-archive bounds/path checks are covered
without running SDK installers or game builds. Synthetic game files are never executed.
Process monitors default to explicit simulation in unit tests so unrelated games
on a shared test host do not affect temporary fixtures; production monitoring is
unchanged.

Optional live checks execute no upstream code:

```powershell
cargo test --lib --locked source_build::tests::live_source_metadata_identifies_pinned_commits_for_all_three_games -- --ignored --exact --nocapture
cargo test --lib --locked source_build::tools::tests::official_sdl_sdk_layout_matches_the_source_recipe -- --ignored --exact --nocapture
cargo test --lib --locked source_build::tools::tests::verified_codec_archive_contains_the_required_headers_and_static_libraries -- --ignored --exact --nocapture
```

These read public source metadata or download/extract SDL and the disclosed
codec supplier's SDK data into temporary directories without running binaries.
Real game builds
need local licensed inputs, explicit source/tool consent, ample disk and a
graphics session where shader preparation needs one. Do not execute an unreviewed
upstream checkout merely to validate manager changes.

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

### Offline play, forks, and progress

```powershell
cargo test --lib --locked -- save_data::tests history::tests runtime::tests ui::
```

The read-only decoder uses synthetic, checksum-valid PC/console saves matching
the pinned Dusklight layout. Tests cover slot identity, primary-bank selection,
field units, clock rates, corruption rejection, and recorded gains/losses. Actor
tests exercise offline launch/finalization, independent local continuation,
verified previews, common-base disagreement, receiver consent, stale choices,
and prevention of writes during play or after a profile change. Fork history
tests reopen and reconstruct both branches and two-parent resolutions.

Slint tests activate actual component accessibility actions and check navigation,
disabled source controls, both themes, and horizontal bounds/touch targets at
720, 900 and 1200 logical pixels. Safety-model tests keep unverified branches
unselectable and preserve stored reservations. These synthetic tests do not
launch games or read/write real user profiles; use the separate real-source
validator for complete installation evidence.

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

## Texture archive validation

```powershell
cargo test --lib --locked -- textures::tests runtime::tests
```

The importer mirrors the pinned Aurora native directory comparator and
first-key-wins registration rule. Tests cover category aliases, path traversal,
image magic/size/CRC checks, cancellation, automatic profile preparation,
atomic switching, and recovery. Large ZIP64/texture tests can use an authorized
local archive without changing it or a real game profile:

```powershell
$env:TWILIGHT_TEXTURE_TEST_ARCHIVE = 'C:\Users\YourName\Downloads\TPDE+ 1.2.0.zip'
cargo test --release --lib --locked textures::tests::supplied_tpde_archive_installs_and_switches_without_manual_extraction -- --ignored --exact --nocapture
```

The supplied v1.2.0 archive selects exactly 5,456 native texture keys and
8,622,649,300 image bytes. The test installs, activates, disables, and compares
restored configuration values only inside an owned temporary profile. Original
JSON formatting is not a save-setting value and is not assumed to be stable.

## Slint interface and UI validation

The interface lives in `ui\app.slint` and `ui\components.slint`. The Rust bridge
is `src\ui.rs`; state and safety predicates are separated into `src\ui\model.rs`.
The original adventure artwork is in `assets\adventure-*.svg`.

```powershell
cargo test --lib --locked ui::
cargo build --example ui-preview --locked
.\target\debug\examples\ui-preview.exe
```

The preview uses synthetic example data, never the real manager store, saves,
pairing, or startup preferences. It cannot perform real game or network actions.
It exits after 30 minutes or from its Quit control. Its About dialog can save
an example-data PNG; inspect the interface and exported image before using it
in documentation. Do not publish private app captures.

Headless tests use the exact matching Slint testing backend to activate actual
accessible controls, navigate the source wizard, check disabled build actions,
and verify touch-target and horizontal layout bounds in both themes. They do
not replace an interactive native-window check. Slint debug metadata is compiled
only in debug builds; no UI control server is enabled in production.

The generated Slint module has a scoped `clippy::unwrap_used` allowance because
the toolkit generates those calls. Handwritten application code retains the
repository-wide restriction.

Confirmation dialogs are asynchronous and parented to the main window.
Blocking message boxes inside Slint callbacks can deadlock the native UI.
The dispatcher owns the current interaction across the await without holding
a `RefCell` borrow; other commands cannot alter that pending decision.

## Real source installation validation

Tests, metadata requests and installer `--help` checks are not proof that a game
can be built. Run complete source installations with your own supported local
inputs before claiming recipe compatibility:

```powershell
cargo build --example source-check --locked
.\target\debug\examples\source-check.exe totk APPROVED_FULL_COMMIT 6
```

Use `dusklight`, `totk` or `triforce`; find the current full commit with
`--check-source-updates GAME` and review it before approving execution.
The validator uses configured inputs, requires the real manager to be closed,
and uses production process monitoring. It installs side-by-side and checks
runtime output and preserved save hashes without changing active selections
or play reservations. Workspaces and game-derived output stay private.

Record a successful complete build, output receipt and executable/runtime
checks for each title. A later-stage failure is still a failed installation,
even when extraction or compilation partly succeeded.

TotK builds use a build-local official NuGet configuration and cache instead of
depending on user feed settings. Stage only the third-party SDK headers and
libraries: copying the entire MSYS2 include tree shadows LLVM's C/C++ runtime
headers and breaks native compilation.

Dusklight uses a short `workspace\b` CMake binary directory for configure,
build and install. Longer preset output paths exceed MSVC's generated-object
path limits even when the source files themselves can be opened.

## Windows release packaging

The checked-in Cargo configuration statically links the MSVC C runtime for the
Windows x64 build. Verify the application and installer before publishing:

```powershell
.\scripts\Package-Release.ps1 -Version v0.8.0
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
3. Create and push the matching fresh stable tag, such as `v0.8.1`.
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

The generator uses the Windows dependency graph and includes Slint's selected
royalty-free desktop license text. The main About screen displays AboutSlint
attribution. Packaging refuses stale Cargo
manifest/lockfile notice stamps. Fingerprints normalize CRLF to LF so clean
Windows and Linux checkouts agree; original third-party license whitespace is
otherwise preserved.

The bootstrap runs as the current user and does not elevate, alter PATH,
terminate processes, disable certificate checks, or change security policy.
It verifies GitHub's asset digest and the published checksum list before
replacing its managed executable. This does not defend against a compromised
publisher account. The public README offers a manual, inspectable installation
path for users who do not want to run a downloaded script.

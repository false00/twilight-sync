# Twilight Sync technical guide

[Back to the installation guide](../README.md)

**Your adventure, in step. No cloud required.**

A native Rust save-sync companion for [Dusklight](https://github.com/TwilitRealm/dusklight),
the Twilight Princess PC reimplementation. Pair two Windows devices on a trusted
private LAN, compare a code on both screens, and keep the next chapter of your
adventure on the right machine.

Twilight Sync is an independent companion, not an official TwilitRealm project.
It does not include game assets, saves, disc images, or a copy of Dusklight.

> **Early release, not an audited security product.** The implementation has
> automated encrypted two-device tests, but has not had an independent security
> audit. Start with copies of your saves. Structural validation cannot prove that
> a file is harmless to every version or mod of the game.

## What it does

| Feature | Behavior |
| --- | --- |
| Local discovery | Small subnet-broadcast announcements; no cloud directory or relay |
| Router approval | Explicitly pin the gateway MAC, gateway IPv4, and subnet fingerprint |
| Two-sided pairing | Both devices open pairing and independently approve a matching, session-specific code |
| Encrypted events | `twilightProto/1` uses Noise XX, pinned device keys, authenticated encryption, and periodic rekeying |
| Save sync | Automatically sync a changed save when the other copy still matches the common base |
| Conflict safety | No last-writer-wins: the receiver must approve a conflicting replacement |
| Save history | Compressed, shared 4 KiB blocks with parent-linked commits and a 64 MiB retention budget |
| Exclusive play | Both devices durably reserve a play lock before a managed game launch |
| Native desktop | Dark dashboard, discovery/pairing screens, system-tray status and actions |
| Windows startup | Enabled on the first normal launch; toggle in the tray or settings |
| Private audit trail | Rotating JSON-lines logs under your home `.twilight-sync` directory |
| Optional disc transfer | Separate, disabled-by-default, per-transfer consent for verified GameCube ISOs |
| Dusklight updates | Check the latest official release; install/update side-by-side and verify existing managed files before reuse |
| Texture library | Creator-download workflow for Henriko's 4K/1080p packs, authorized local HD imports, and safe per-user switching |

**v0.2 targets Windows 10/11 and one partner per device.** There is no multi-device
mesh, background service, administrator requirement, account, telemetry, NAT
traversal, UPnP, or automatic public firewall rule. The desktop and router probe
are Windows-specific; most core validation and transport tests are portable.

## Build and run

Install Rust **1.95 or newer** with the MSVC toolchain and the Visual Studio C++
Build Tools/Windows SDK. Then, from PowerShell:

```powershell
cargo build --release --locked
.\target\release\twilight-sync.exe
```

For development:

```powershell
cargo run --locked
```

Normal desktop startup creates the private application directory, protects the
device key with Windows DPAPI, and applies the startup preference. It does **not**
trust a router, pair a peer, download Dusklight, modify a save, or transfer an ISO
without the applicable approvals.

Closing the window hides it in the tray. Double-click the moon icon, or use
**Open Twilight Sync**, to reopen it. Quit from the tray after stopping your game.
Keep the built executable in a stable location if Windows startup is enabled.
The startup entry is per-user, not a scheduled task or administrator service.

## Set up two devices

1. Connect both devices to the same private IPv4 LAN. Mark that Windows network
   **Private**. Public profiles, VPN-only interfaces, and virtual adapters are
   deliberately refused.
2. Open **Your devices** on each machine. Check the adapter, router address, MAC,
   and fingerprint, then approve the router separately on each.
3. Allow the app through Windows Firewall **on Private networks only**, if Windows
   prompts. The app does not alter firewall settings.
4. Open the two-minute pairing window on **both** devices. Select the discovered
   partner on one device. Start only one connection attempt at a time.
5. Compare the full eight-digit code on both screens. Approve on **each** device
   only if every digit matches. Reject a mismatch. Neither device can transfer
   game data after only one approval.
6. Select Dusklight's actual `.gci` save on each device, or choose an explicit
   receive destination for a device without a save. Use the game's real Card A
   path, not an unrelated export. Different local directory paths are fine.
7. Locate your existing `dusklight.exe`, or choose **Install official Dusklight**.
   Configure Dusklight with your own disc dump as its official guide describes.
8. If both saves exist and differ initially, use **Send this save** on the copy
   you want. The other device must approve replacement; its old copy is backed up.
9. Launch with **Play Dusklight**. Both managers must be online and their save
   states must match. After exit, the final save is synced before play is unlocked.

For a new game, both users can select the correct, not-yet-existing GCI
destinations. An intentional empty destination is distinguished from a missing
configuration or an invalid save. The manager can reserve exclusive play before
the first save exists; it never manufactures a game save.

Dusklight supports several storage formats, but this release intentionally syncs
**only individual four-block GCI saves** with data version 6. It does not sync raw
memory-card images, `.dusksave` ZIP containers, mod sidecars, scripts, settings, or
entire directories. Use Dusklight's GCI-directory storage. Identify your active
Card A file using Dusklight's save tools/storage configuration; the manager does
not guess a destination from a peer-supplied filename.

### Network compatibility

- RFC1918 IPv4, prefix lengths `/8` through `/30`, with a resolvable same-subnet
  physical default gateway.
- Same directly connected subnet and matching gateway fingerprint on both hosts.
- UDP **42816** for discovery; TCP **42817** for encrypted `twilightProto` events.
- A listener binds to the approved interface's specific IPv4 address, never
  `0.0.0.0`. Outbound connections explicitly bind their source address too.
- No IPv6, routed VLAN peers, guest-network isolation bypass, Internet relays, or
  cross-router pairing in v0.2. A multihomed host uses its preferred eligible
  physical route. If your router presents different MACs to different segments,
  pairing fails closed rather than weakening the check.

## `twilightProto/1`: the protocol

`twilightProto` is a **custom application protocol**, not a new cipher or a new
IP transport. It deliberately uses the OS TCP stack for reliable delivery.
Discovery uses bounded UDP datagrams. An SSE-like persistent stream pushes
state changes, approvals, save offers, receipts, and heartbeats without HTTP,
repeated connections, or network polling.

### 1. Gate the network

The Windows probe selects a physical, nonvirtual adapter with a private profile,
resolves the default router's neighbor entry, and computes:

```text
SHA-256("twilight-router/v1|" + gateway_ipv4 + "|" + normalized_gateway_mac
        + "|" + subnet_network_hex + "/" + prefix_length)
```

The fingerprint does not include the individual host address, so hosts on the
same router/subnet derive the same value. Trust is explicit and local.

Before each outgoing TCP connection or incoming application handshake, a **fresh
router probe** must match the approved fingerprint and source interface.
Incoming sessions must also correspond to an eligible discovery announcement
from a direct-subnet address. Public, loopback, off-subnet, router, subnet, and
broadcast endpoints are refused. Discovery is never accepted as authentication.

While connected, router checks are scheduled every five seconds. A changed or
unverifiable router closes channels and discards staged transfers. A probe older
than twelve seconds disables all network activity, including if the probing
worker stalls. This is sampled verification, not an instantaneous physical-link
attestation. The OS may accept a TCP SYN before the application rejects a session;
preventing that requires OS firewall policy.

### 2. Establish an encrypted channel

The connection starts with the four-byte preface `TWP1`, followed by:

```text
Noise_XX_25519_ChaChaPoly_SHA256
prologue = "twilightProto/1|" + approved_network_fingerprint
```

- **X25519** supplies ephemeral and static Diffie-Hellman keys.
- **ChaCha20-Poly1305** authenticates and encrypts each event.
- **SHA-256** is the Noise handshake hash and the file-integrity hash.
- Noise XX provides forward secrecy and proves possession of the remote static
  key. On the first connection it does not, by itself, identify a trusted person.
- Known partners must match the full previously pinned 32-byte public key. There
  is no automatic key replacement or downgrade.
- New partners require the human comparison and approvals described below.

The private static key is stored as a **current-user DPAPI-protected blob**, not
plaintext JSON. The application directory has inheritance removed and full
access granted to the current user and SYSTEM. DPAPI is not protection against
malware running as that same user, administrators, or a compromised OS.

### 3. Bind human consent to this exact handshake

The comparison code is the first eight bytes of the completed Noise handshake
hash, interpreted as a big-endian integer modulo `100,000,000`, displayed as
`1234 5678`. It is derived independently on both devices and is not a reusable
password. It changes when the handshake transcript changes.

The pairing window lasts 120 seconds and permits at most five handshake attempts.
Only one handshake is in progress at a time, with connection attempts at least
two seconds apart and five-second Noise socket timeouts. Both users must approve,
both identities must be persisted locally, and a readiness exchange must finish
before saves, disc offers, or game locks are accepted.

This is human-assisted authentication: comparing only a few digits, approving
blindly, or accepting a code supplied over an attacker-controlled channel defeats
the intended check. Device names in discovery are explicitly unverified hints.

### 4. Stream bounded authenticated events

Each handshake or encrypted transport frame has a four-byte, big-endian length.
Handshake bodies are capped at 1,024 bytes; transport ciphertext is capped at
65,535 bytes **before allocating a body**. An encrypted frame is:

```text
+----------------------+-----------------------------------------+
| ciphertext length    | Noise-encrypted postcard event          |
| 4 bytes, big-endian  | includes the 16-byte Poly1305 tag        |
+----------------------+-----------------------------------------+
```

Events are compact `postcard` binary values. No HTTP headers or JSON/base64 file
payloads travel over the secure stream. Trailing event bytes are rejected.
Authentication or framing failure closes the session; partial frames are never
retried as if they were whole events.

The event set includes hello, bilateral pairing approval/readiness, save state,
save offer/receipt, lock request/grant/release, crash recovery, explicit ISO
offer/accept/chunk/receipt, bounded errors, and heartbeat. There is no arbitrary
remote path, shell command, executable-install, or directory-listing event.

- A complete save fits in one bounded event.
- ISO chunks contain at most 32 KiB of file data.
- Binary collection lengths are checked before allocating their payload.
- Each direction rekeys after every **4,096 authenticated events**, independently.
- Noise's direction-specific nonces and authentication reject modified/replayed
  ciphertext; reconnecting creates a fresh handshake and transport state.
- Heartbeats run every five seconds; receive inactivity closes a channel after
  twenty seconds. Network/approval loss never expires a play lock.
- Approval deadlines, bounded discovery/device tables, a bounded actor inbox,
  fixed chunk sizes, backpressure, and staged-file cleanup constrain resources.

The framing overhead is just **4 length bytes + 16 authentication bytes**, plus
the encoded event metadata. File data is never base64-expanded. A protocol
version change requires an intentional compatibility change; the app does not try
to guess or silently accept future event layouts.

## Save validation and safe writes

An authenticated sender is not permission to write arbitrary bytes or paths.
Both sending and receiving validate the actual save:

1. Exact **32,832-byte** GCI size and exactly four 8 KiB card blocks.
2. Explicit Twilight Princess game/region IDs and Nintendo maker ID `01`.
3. A bounded, terminated, conservative ASCII GCI save name; expected icon/comment
   offsets. Unknown formats are rejected, not renamed into acceptance.
4. Save data version 6 in **both** redundant sectors.
5. Both sector checksums, plus all **six** quest-slot checksums.
6. NUL termination inside each 17-byte player and horse name field.
7. Transfer SHA-256 and the locally expected destination hash.

Before writing, the manager checks again for a local Dusklight process. The
destination is chosen **only on the receiving device**. UNC/device paths,
alternate data streams, traversal, symbolic links, reparse points, reserved
Windows device names, and ambiguous Windows components are refused.

The old valid save is captured in content-addressed history. The incoming save is
staged in the destination directory, flushed, atomically persisted, and re-read.
A changed destination hash aborts replacement. Unsynced divergent changes need
explicit receiver consent even when the sender presses **Send this save**.
Corrupt or incompatible existing destinations are not silently overwritten.
The oldest history commits are pruned to keep the managed store within its
64 MiB budget; shared blocks are deleted only when no retained commit needs them.

**Checksums are not a malware scanner.** An attacker can calculate valid game
checksums. There is no general test for the absence of all executable sequences
or all game-engine exploits inside arbitrary binary data. These checks reduce
the accepted attack surface, including overlong name fields; they do not prove
semantic safety or replace game updates, OS security, or an independent review.

## One game at a time: consistency over availability

There is no lease TTL that could expire during a long game or network partition.

```text
Both save states agree and both managers are online
    -> requester persists a unique owner/token lock
    -> partner verifies idle state and persists the same lock
    -> partner grants
    -> requester starts Dusklight
    -> game exits
    -> final stable save is transferred and acknowledged
    -> partner verifies the final hash, clears its lock, and acknowledges
    -> requester clears its lock
```

If the game exits without creating its first save, both intentional empty
destinations can also be reconciled and unlocked. If a launch, disk operation,
connection, or process crashes, saved locks remain. Reconnect, stop every game
process, and use **Request crash recovery**. The other user must explicitly
approve recovery too. There is no unilateral offline force-unlock button.

**This guarantee applies to launches through Twilight Sync.** The manager checks
for `dusklight.exe` before granting/starting play and before committing saves,
and polls for outside launches while running. It does not inject into Dusklight,
install a kernel driver, or terminate games automatically. A direct/renamed
executable, modified client, compromised host, or independently launched second
game can bypass cooperative exclusivity. Stronger enforcement requires an
integration inside Dusklight itself.

## Optional ISO sync and official installation

### Disc images

Save sync cannot be used to smuggle an ISO or installer. Disc transfer uses
separate event types and is disabled by default on **each** device.

The sender must select a user-supplied, full-size **1,459,978,240-byte GameCube
ISO** for `GZ2E01`, `GZ2J01`, or `GZ2P01`, revision 0. The entire file is checked
against the official Dusklight disc catalog's XXH3-128 hash before an offer is
sent. The receiver must opt in, approve that offer, select a new local `.iso`
destination, and have enough staging space.

The receiver streams to a temporary file, checks the disc header, exact size,
official XXH3-128 hash, and full transfer SHA-256, and commits without replacing
an existing file. A timeout, mismatch, disconnect, or refused approval discards
staged data. Images are never mounted, extracted, downloaded from third parties,
or executed. **Wii ISOs, RVZ, trimmed images, and modified discs are not supported
in v0.2.** You must dump and own your own original game.

### Dusklight

**Check latest release** and **Install / update Dusklight** are explicit exceptions to the app's otherwise
local-only networking:

- Metadata comes from `api.github.com/repos/TwilitRealm/dusklight/releases/latest`.
- Only the matching official Windows x86-64/ARM64 ZIP asset is accepted.
- The URL must belong to that exact repository/tag/asset.
- HTTPS, a bounded download, and the asset's published SHA-256 digest are required.
- Redirects are limited to the explicit GitHub release-download host allowlist.
- Extraction rejects traversal, links, special files, unsafe Windows names,
  duplicate destinations, more than 512 entries, oversized paths, and expansion
  beyond 512 MiB.
- The verified installation is moved into the private application's `apps`
  directory and selected locally. **It is not automatically run.**
- Each release gets a version-and-archive-hash directory, so updating does not
  overwrite an existing executable or change the game profile, texture selection,
  save path, pairing, or play lock.
- A protected installation receipt records every extracted file's SHA-256.
  Selecting an already-installed build rechecks the receipt and file inventory;
  changed or untracked files cause an explicit error instead of being overwritten.
  Earlier v0.1 installs are left intact; the updater creates a separate managed build.

The published digest is integrity metadata obtained from GitHub over HTTPS, not
an independent publisher signature. This flow still trusts the official
repository, release account, GitHub, and certificate infrastructure. It does not
claim to detect a compromised upstream release. Existing installations can
instead be selected with **Locate dusklight.exe**.

## Texture packs: creator downloads, managed installs

The **Texture packs** page includes:

- **Henriko Magnifico's Twilight Princess 4K** project.
- The same creator's **1080p** edition, with lower-resolution textures.
- **[TP Definitive Edition+ v1.2.0](https://gamebanana.com/mods/676609)**, with a
  dedicated project entry and a `1.2.0` import-version preset.
- **Authorized local HD/custom imports**, including assets you have permission
  to use from your own sources. Nintendo-derived HD assets are not bundled,
  linked for download, or redistributed by Twilight Sync.

The project and its native Dusklight compatibility were checked against the
[creator's own page](https://www.henrikomagnifico.com/zelda-twilight-princess-4k)
and the [4.0 native-support changelog](https://www.henrikomagnifico.com/post/zelda-twilight-princess-4k-texture-pack-4-0-update-the-faithful-update).
TP Definitive Edition+'s project name, `1.2.0` version, and Dusklight
compatibility were checked against its author's GameBanana listing/API.
These are supported choices, not an independently measured popularity ranking.
The app does not claim that an archive imported under a project name has been
authenticated by that creator.
Selecting TP Definitive Edition+ fills the version label with `1.2.0`; it remains
editable for future releases. Obtain the intended version on the author's page
and import only assets you are authorized to use. The preset does not verify a
downloaded archive's release version, and no pack assets are bundled or rehosted.

### Install or update a texture pack

1. Choose **Get latest / updates from creator**. The creator maintains the
   current downloads; select the desktop release and resolution you want.
2. Complete the publisher's normal browser/account/download flow yourself.
   There is no Drive/Patreon scraping, embedded account login, paywall bypass,
   or rehosting. Texture updates are **creator-assisted, not silent automatic
   downloads**.
3. Select the project in Twilight Sync, enter a version label, and confirm you
   have permission to use the assets.
4. Choose **Install ZIP** or **Install extracted GZ2 folder**. For 7z/RAR,
   extract with your own trusted archive utility first. Select one resolution
   and variant, not a bundle containing conflicting versions of the same texture.
5. Choose the actual Dusklight **config.json** to associate its user profile.
   The manager uses that file's directory and its `texture_replacements` child;
   it does not guess the profile from the executable's directory.
6. Stop Dusklight on both paired devices and select **Activate this version**.
   Switches take effect on the next launch through the normal game launcher.

Updating means importing the new authorized archive/folder and activating that
installed version. Identical content is deduplicated by content ID instead of
being installed again under a different label. Older installed versions stay
available for switching back. Texture libraries are independent on each device;
**texture files never travel over twilightProto**.

Only native-style `tex1_...` PNG/DDS images are installed. The importer reports
skipped files: bundled emulators, DLLs, scripts, shaders, installers, documentation,
and Dolphin-only names such as unsupported material maps are not copied.
Path traversal, links, alternate data streams, unsafe names, conflicting
texture keys, excessive entry counts/depth, and oversized files are refused.
PNG structure/chunk CRCs and DDS headers/mip payload lengths are checked, with a
16-megapixel decoded-dimension bound. This is structural validation, not a
guarantee that every image is harmless to every game/graphics-library version.

The import limits are 25,000 selected images, 100,000 source entries, 128 MiB per
image, and 16 GiB per pack. Large packs genuinely use gigabytes; the save-history
budget does not apply to textures or program installations.

### Safe switching and recovery

The active directory is prepared separately and switched only after integrity
checks and a fresh local process check. The manager updates the real flat
`game.enableTextureReplacements` preference while preserving other configuration
keys. It does not live-inject textures or terminate a game.

Existing unmanaged textures require permission before they are moved to a
named original backup beside the profile. **Disable managed pack / restore
original textures** restores that folder and the preference's original value
(or original absence), rather than deleting someone else's texture collection.
Managed directories containing unexpected or modified files are preserved.

Same-volume hard links normally avoid a second copy of the managed images.
Cross-volume/unsupported-hardlink activation uses verified copies and reports
the additional copies. Do not edit managed folders in place: hard links can
share the underlying bytes. Import modified assets as a new version instead.

A private recovery journal covers the folder/configuration switch. If the
process or disk operation is interrupted, **Recover interrupted texture switch**
restores the preceding state before local managed play is allowed again.
If the config was independently edited, recovery refuses to overwrite that edit
and asks for manual review. Unknown or modified directories are not silently
deleted. Ordinary outside launches can still race cooperative process checks;
use the manager's launcher and keep the game stopped during management.

## Compact save snapshots: a commit trail, not full copies

This is a custom **application-level copy-on-write store**, not Windows VSS,
a filesystem driver, or an administrator service:

```text
validated stable GCI
    -> split into 4,096-byte blocks (last block may be shorter)
    -> SHA-256 each uncompressed block
    -> reuse an existing block or store a bounded zlib-compressed object
    -> append an immutable, SHA-256-addressed commit
    -> atomically publish the retained commit index
```

Commits record sequence, parent, game/save identity, timestamp, username, device,
source path, reason, full save hash, and block references. Repeated unchanged
saves add nothing. Image/header pages and unchanged quest data are shared
between revisions. There is no fragile delta chain: reconstructing a retained
snapshot requires only its own referenced blocks.

- Valid local changes are captured after two stable seconds, **including
  in-game autosaves**. Incomplete/invalid in-progress writes are not snapshotted.
  A waiting notice is surfaced instead. Snapshotting never enables in-game sync.
- The current valid save is captured before incoming replacement or restoration.
  A new version is not published as applied until the destination replacement
  has completed and passed its re-read verification.
- **Restore as a new commit** preserves the current valid version and appends
  the restoration to history; it does not rewind or rewrite older commits.
  Both games must be stopped and normal sync/conflict/play-lock rules still apply.
- Restore verifies the commit digest, every decompressed block's size/hash, the
  whole-file hash, and the actual GCI format. A corrupt or incompatible current
  destination must be moved aside first rather than silently overwritten.
- History is browsable in pages of 50 commits. Pruned parents may be shown as
  boundary IDs; retained snapshots do not depend on those deleted commits.

The **64 MiB per-device budget** includes managed compressed objects, commit
files, and index bytes. Oldest commits are pruned first, and garbage collection
removes a shared block only after its final retained reference is gone. Temporary
transaction files and filesystem allocation/directory overhead are separate;
this is a managed-byte budget, not a quota on the entire home folder.

Storage accounting is cached so each new save does not reread the entire history.
Crash-orphan cleanup occurs on open/recovery; normal pruning removes only known
unreferenced objects. A missing/corrupt index is an explicit error, never an
excuse to erase existing history.

Earlier hash-named `.gci` backups are migrated in modification-time order on the
first history use. Each original is removed only after its snapshot can be
reconstructed and compared byte-for-byte; unrecognized files are not deleted.
The same retention budget applies during migration.

Tests enforce deduplication, byte-perfect reconstruction, guarded restore,
pruning with every retained snapshot still readable, pagination, corruption
rejection, and **at least 80% managed-byte savings over full copies** for 100
synthetic revisions with shared, incompressible static image/header pages.
This is a fixture-specific acceptance threshold, not a promised compression
ratio for arbitrary saves. History hashes detect accidental corruption; they
are not signatures or protection against an attacker controlling your user account.

One Windows validation run stored **571,527 bytes rather than 3,283,200 bytes
(82.6% less)** for that 100-revision fixture, including managed block, commit,
and index bytes. Paths/timestamps can slightly change the metadata size.

## Local data and privacy

```text
%USERPROFILE%\.twilight-sync\
    identity.dpapi          Current-user-protected Noise private key
    settings.json           Router pin, public peer key, local paths, common hash, play lock
    instance.lock           Per-user single-manager lock
    logs\
        audit.jsonl         Current structured audit log
        audit.1.jsonl ... audit.4.jsonl
    history\
        index.json         Retained parent-linked commit references
        commits\
            <sha256>.json  Immutable snapshot metadata
        objects\
            <sha256>.z     Shared compressed save blocks
    textures\
        packs\
            <content-id>\  Validated images and their manifest
        pending.json       Present only while texture recovery may be needed
    apps\
        <tag>-<hash>\      Side-by-side official program and file-hash receipt
```

Logs include the local **username, file paths, hashes, public peer keys, timestamps,
security decisions, and errors**. They do not include save payloads, private keys,
or pairing comparison codes. A log rotates at approximately 5 MiB and keeps four
older files. History, textures, retained program versions, and preserved original
texture folders consume additional space. Snapshot metadata also contains
usernames and paths; its compressed game data is not encrypted at rest.
Do not post logs or configuration publicly without redacting personal details.
Device name, public key, network tag, and pairing availability are visible in
local discovery; save contents and paths are not.

Failure to persist lock/configuration safety state disables networking. Audit
errors abort the affected action and are surfaced; if the error itself cannot
be logged, networking is disabled too. A failure after an atomic commit can
leave the valid committed file in place without a peer receipt; reconnection
reconciles hashes rather than inventing a rollback. No transfer is silently
accepted after its validation fails. A forced process kill or power loss can
leave an orphaned staging file; the app does not delete unrelated temporary
files in a user-selected directory.

To remove the app, first stop games, complete/recover any outstanding play lock
with your partner, disable **Start with Windows**, revoke pairing, and quit.
You can then remove the executable and deliberately remove your private
application directory after preserving any backups you want.

## Diagnostics and tests

Read-only diagnostics do not initialize the desktop, register startup, trust a
router, or write application state:

```powershell
cargo run --locked -- --help
cargo run --locked -- --check-save 'C:\Games\Dusklight\Card A\01-GZ2E-zeldaTp.dat.gci'
cargo run --locked -- --check-iso 'D:\My Disc Dumps\Twilight Princess.iso'
cargo run --locked -- --check-network
cargo run --locked -- --check-updates
```

An unsupported/corrupt input exits with an error. Do not treat a `VALID GCI`
result as an exploit-free certificate.

```powershell
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

An optional live updater test downloads the official public program into an
isolated temporary directory, verifies extraction/receipts and reuse, then removes
that test installation. It does not run Dusklight, alter a real profile, or
register startup:

```powershell
cargo test --lib install::tests::official_release_installs_and_reuses_in_an_isolated_directory --locked -- --ignored --exact --nocapture
```

Windows tests create isolated temporary data and loopback Noise sessions. They
exercise both-sided consent, key pinning, encrypted commits, conflicts/backups,
outside-game checks, elapsed save stability, router-change/staleness gates,
crash recovery, final-save-before-unlock ordering, strict parsers, framing bounds,
tamper/replay rejection, rekeying, and the separate ISO permission. They do not
modify real startup settings or require a copyrighted save/disc fixture.

A debug-only, 20-second native UI smoke run is available with
`cargo run --locked -- --ui-smoke`; it creates no real keys, startup entries,
settings, or network sockets. Add `--background` to exercise a hidden startup.
Use `--page textures`, `--page history`, or `--page library` to open a specific
screen deterministically without interacting with real files or creator downloads.
The tray uses a native GUI-thread timer rather than depending on redraws of
the hidden dashboard, so status, incoming consent requests, and shutdown remain
responsive after the window is closed.

**Hardware acceptance still matters:** test discovery, Windows Firewall, sleep/
resume, actual router changes, first-save paths, and game lifecycle on two real
devices before trusting live saves. Full-disc transfers and live official
installation require your own disc and an explicit user-approved download; a
loopback test is not evidence those external systems were exercised.

## Sources and security boundaries

Format details were checked against the upstream
[save manager](https://github.com/TwilitRealm/dusklight/blob/main/src/dusk/save_manager.cpp),
[memory-card checksums](https://github.com/TwilitRealm/dusklight/blob/main/src/m_Do/m_Do_MemCardRWmng.cpp),
[save sizes](https://github.com/TwilitRealm/dusklight/blob/main/include/m_Do/m_Do_MemCard.h),
[player layout](https://github.com/TwilitRealm/dusklight/blob/main/include/d/d_save.h), and
[disc catalog](https://github.com/TwilitRealm/dusklight/blob/main/src/dusk/iso_validate.cpp).
For the game itself, use the [official installation guide](https://twilitrealm.dev/install/).
Cryptography follows the [Noise protocol framework](https://noiseprotocol.org/noise.html)
via the Rust `snow` implementation rather than a home-grown encryption scheme.

Router MACs and discovery messages are spoofable. Network tags are policy
bindings, not secrets, remote attestation, or proof of physical proximity. A
malicious router, bridge/tunnel, administrator, paired peer, or compromised host
can undermine assumptions the app cannot verify. Bilateral comparison, pinned
keys, narrow file types, and fail-closed locking are separate layers, not a claim
that arbitrary LAN traffic or arbitrary game data has become unexploitable.

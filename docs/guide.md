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
| Conflict safety | Verified read-only branch previews, progress comparison, explicit selection, and receiver consent |
| Save history | Shared 4 KiB blocks, fork and reconciliation parents, progress changes, and a 64 MiB retention budget |
| Play | Offline sessions are allowed; matching online saves retain coordinated play locks |
| Progress | Read-only decoding of quest slots, dates, play time, hearts, equipment, items, and milestone records |
| Native desktop | Dark dashboard, discovery/pairing screens, system-tray status and actions |
| Windows startup | Enabled on the first normal launch; toggle in the tray or settings |
| Private audit trail | Rotating JSON-lines logs under your home `.twilight-sync` directory |
| Optional disc transfer | Separate, disabled-by-default, per-transfer consent for verified GameCube ISO / RVZ images |
| Optional game-input transfer | Explicit paired-PC approval for OOT `.nup`, TotK base/update `.nsp`, and separately opted-in console `prod.keys` |
| Dusklight updates | Check the latest official release; install/update side-by-side and verify existing managed files before reuse |
| TotK Recomp updates | Verify the official setup and automatically rebuild an existing native installation, with play and save sync paused |
| Source builds | Stable-tag/commit review, approved official tool archives, per-game local inputs and side-by-side builds for Dusklight, TotK and Triforce |
| Triforce saves | Separate native save channel with common-base sync, branches, history, restore and durable protected play |
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

Normal desktop startup creates the private application directory and checks its
permissions. On Windows, it repairs the directory ACL only when needed, rather
than rewriting inherited permissions across build caches on every launch.
It protects the device key with Windows DPAPI and applies the startup preference.
It does **not**
trust a router, pair a peer, download Dusklight, modify a save, or transfer a disc image
without the applicable approvals.

Closing the window hides it in the tray. Double-click the moon icon, or use
**Open Twilight Sync**, to reopen it. Quit from the tray after stopping your game.
Keep the built executable in a stable location if Windows startup is enabled.
The startup entry is per-user, not a scheduled task or administrator service.

## Set up two devices

1. Connect both devices to the same private IPv4 LAN. Mark that Windows network
   **Private**. Public profiles, VPN-only interfaces, and virtual adapters are
   deliberately refused.
2. Open **Devices** on each machine. Check the adapter, router address, MAC,
   and fingerprint, then approve the router separately on each.
3. Allow the app through Windows Firewall **on Private networks only**, if Windows
   prompts. The app does not alter firewall settings.
4. Open the two-minute pairing window on **both** devices. Choose the discovered
   partner's **Compare codes** button on **one device only**. Do not start a
   connection from both devices at once.
5. Compare the full eight-digit code on both screens. Approve on **each** device
   only if every digit matches. Reject a mismatch. Neither device can transfer
   game data after only one approval.
6. Locate your existing `dusklight.exe`, or choose **Install official Dusklight**.
   Configure Dusklight with your own disc dump as its official guide describes.
7. Save detection is automatic. Sync follows Dusklight's active profile and
   standard Card A save; there are no save-path controls. Each PC can have a
   different local profile location.
8. If both saves have divergent progress, compare the branches. Select a whole
  save explicitly, or keep both and continue the local branch. Both versions
  are preserved; the receiving user must approve replacing their save.
9. Launch with **Play Dusklight**, including offline. Matching online saves use
  the coordinated lock. Offline progress is synced or compared on reconnect.

For a new game, an automatically resolved empty destination is distinguished from
an unavailable profile or an invalid save. The manager can reserve exclusive play
before the first save exists; it never manufactures a game save. The first
received or created save selects its supported regional Card A filename.

### Automatic save locations

The normal Windows profile is `%APPDATA%\TwilitRealm\Dusklight`. A stock US
GameCube save is:

```text
%APPDATA%\TwilitRealm\Dusklight\USA\Card A\01-GZ2E-gczelda2.gci
```

When Dusklight uses raw-card storage, the US card is:

```text
%APPDATA%\TwilitRealm\Dusklight\MemoryCardA.USA.raw
```

The stock internal save name is **`gczelda2`**, not `zeldaTp.dat`. Supported
GameCube/Wii IDs select `USA`, `JAP`, or `EUR` and their matching GCI or raw-card location.
Detection examines only these known standard Card A locations, not arbitrary
directories or the whole home folder.

Sync follows Dusklight's own `data_location.json` precedence: beside the selected
game executable, then the normal profile, then the legacy `TwilitRealm\Dusk`
profile descriptor. Native portable mode uses the executable's `data` folder;
native custom mode uses its recorded safe local profile. Sync does not rewrite
those descriptors, force a different game profile, reset game settings/textures,
or move existing saves. Dusklight remains responsible for its own migrations.
Pending game migrations are reported rather than treated as an empty save.

The `save_file` settings value is derived metadata, not a directory override;
it is recomputed on startup. A
location change clears the old common-sync base while preserving pairing and
durable play locks. Profile binding stays pinned during gameplay and transfers;
the profile is checked again before a protected launch.

Malformed or oversized profile metadata, unsafe paths,
corrupt saves, alternate named saves, and multiple standard regional saves
produce explicit errors. The app will not guess which save to replace.

These conventions are verified against released Dusklight v2.0.3 and its pinned
platform libraries: [profile policy](https://github.com/TwilitRealm/dusklight/blob/v2.0.3/src/dusk/data.cpp),
[stock save name](https://github.com/TwilitRealm/dusklight/blob/v2.0.3/src/m_Do/m_Do_MemCard.cpp),
and [regional Card A layout](https://github.com/encounter/aurora/blob/3227d76c60e1e782ca576610bce61c9e7744d8be/lib/dolphin/card.cpp).

Both **GCI-directory and raw memory-card storage** work directly, including a
raw-card device paired with a GCI-folder device. Only the individual four-block
Twilight Princess save crosses the encrypted connection, using a bounded GCI
payload. The rest of the raw card stays local. `.dusksave` ZIP containers, mod
sidecars, scripts, settings, and entire directories are not transferred.

Raw cards must be standard 512 KiB through 16 MiB GameCube images. The manager
checks the native header, redundant directory/allocation checksums and wrapping
update counters, every allocation chain, block ownership, and free-block count.
It extracts the stock save, validates it, and updates only that save. Other
games, the card header/serial, and unrelated data blocks are preserved. Inserting
a save into an existing empty card requires four free blocks and a directory
slot. Dusklight creates/formats raw cards; open it once if a new raw card does not
exist yet. Sync never reformats an existing card.

Before a raw-card commit, an exclusive file-range lock prevents concurrent card
access. File identity and the expected save hash are checked, a complete image
is staged and flushed, and Windows `ReplaceFileW` atomically replaces the card
while preserving its permissions. The committed save is re-read and verified.
Snapshot history stores only the extracted save, not a full card copy.

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

The listener is polled without blocking, but each accepted channel switches to
blocking I/O before Noise starts. Handshake reads and writes retain five-second
deadlines; encrypted transport reads retain a twenty-second deadline with
five-second heartbeats. Delayed packets wait within those limits rather than
being rejected as nonblocking-socket errors.

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
save offer/receipt, lock request/grant/release, crash recovery, explicit ISO / RVZ
offer/accept/chunk/receipt, bounded errors, and heartbeat. There is no arbitrary
remote path, shell command, executable-install, or directory-listing event.

- A complete save fits in one bounded event.
- Disc-image chunks contain at most 32 KiB of file data. RVZ transfers use the
  original compressed bytes, not an expanded ISO.
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
4. Save data version 6 in **both** redundant sectors: original big-endian or
   Dusklight PC's native little-endian sector envelope. Quest data and its
   checksums remain big-endian in both cases. Mixed/unknown formats are rejected.
5. Both sector checksums, plus all **six** quest-slot checksums.
6. NUL termination inside each 17-byte player and horse name field.
7. Transfer SHA-256 and the locally expected destination hash.

Before writing, the manager checks again for a local Dusklight process. The
destination is derived **only on the receiving device** from its detected
profile and the fixed stock-save convention. UNC/device paths,
alternate data streams, traversal, symbolic links, reparse points, reserved
Windows device names, and ambiguous Windows components are refused.

The old valid save is captured in content-addressed history. The incoming save is
staged in the destination directory, flushed, atomically persisted, and re-read.
A changed destination hash aborts replacement. Storage-only raw allocation
pointers are normalized for cross-device hashing, so identical saves do not
conflict merely because their card blocks differ. Unsynced divergent changes need
explicit receiver consent even when the sender presses **Send this save**.
Corrupt or incompatible existing destinations are not silently overwritten.
The oldest history commits are pruned to keep the managed store within its
64 MiB budget; shared blocks are deleted only when no retained commit needs them.

When both copies have diverged, an authenticated, bounded read-only preview
request retrieves the partner's full validated GCI without installing it. The
local copy and partner branch are captured before presenting a choice. Preview
hashes must match the advertised state; unsolicited/stale responses are ignored.
The active profile and card binding are rechecked before inspection and before
replacement. Automatic replacement of an existing changed save requires both
devices to agree on the common base; disagreement falls back to explicit review.

The prompt has no default winner. **Use partner's save on this PC** replaces
only the local copy after its expected hash is rechecked. **Offer this PC's save
to partner** still requires the partner's approval. **Keep both; play this PC**
leaves both branches intact and starts an independent local session when idle.
Choosing either complete save records both parents on the resolving history
where those branches are retained. There is no byte-wise merge of quest flags,
inventory, or individual slots. Both PCs must run this build for preview frames.

**Checksums are not a malware scanner.** An attacker can calculate valid game
checksums. There is no general test for the absence of all executable sequences
or all game-engine exploits inside arbitrary binary data. These checks reduce
the accepted attack surface, including overlong name fields; they do not prove
semantic safety or replace game updates, OS security, or an independent review.

## Offline play and online coordination

The single **Twilight Sync** desktop shortcut opens the app's library, reusing
the running manager instead of starting a second instance. Choose any game in
**Play**; the shortcut never launches games directly or bypasses local checks.
It uses the Twilight Sync icon, not a game's icon. **Settings > Desktop shortcut >
Create / repair desktop shortcut** restores it without reinstalling a game.
App startup, including the restart after an app update, repairs a missing link
in a worker without blocking the GUI. The installer also creates/restores the
master desktop icon and keeps the Start-menu entry.

Separate per-game shortcuts are no longer created by game installs, updates,
source activation or executable selection. On startup/repair, old app-generated
game links are removed only when their private receipts match their exact paths
and unchanged contents. Unrelated or customized icons are preserved; conflicts
are reported instead of silently overwritten. A matching installer-created
master link can be adopted without rewriting it. Legacy `--open-dusklight`,
`--open-totk` and `--open-triforce` arguments still work for existing custom links.

There is no lease TTL that could expire during a long game or network partition.
An offline launch does not request a partner lease. It validates the detected
profile, card binding, stable save (or intentional empty destination), executable,
and idle local state, then captures existing progress before starting Dusklight.
It never fabricates an initial save. Offline play cannot guarantee that the
other PC is not playing independently.

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

If the game exits without creating its first save, intentional empty destinations
can also be reconciled and unlocked. A session that finishes disconnected may
retire its own lock once its final local save is stable. A connected session
that encounters a fork also retires its completed reservation so reconciliation
can proceed. The peer clears a matching owner reservation only after receiving
authenticated state reporting no running game and no lock. This never clears
an active game or selects a save winner.

After a crash, saved locks remain until recovery. Stop every local Dusklight
process, then use **Request recovery**. Online recovery still requires the other
user's approval. Offline recovery requires a local confirmation, leaves partner
progress untouched, and permits independent local play with reconciliation on
reconnect. A still-running local game, pending launch, or transfer blocks recovery.

**Coordinated exclusivity applies only while both managers participate.** The manager checks
for `dusklight.exe` before granting/starting play and before committing saves,
and polls for outside launches while running. It does not inject into Dusklight,
install a kernel driver, or terminate games automatically. A direct/renamed
executable, modified client, compromised host, or independently launched second
game can bypass cooperative exclusivity. Stronger enforcement requires an
integration inside Dusklight itself.

## Optional game-file sync and official installation

### OOT and TotK setup inputs

Open **Settings > Game-file transfers** on both paired PCs. This replaces the
old ISO-only section: choose **Dusklight** for ISO/RVZ, **TotK** for base/update
NSP and console keys, or **OOT** for NUP. Native game transfer controls also
remain in **Game setup**. Enable **I own this game and allow game-file
transfers** on each PC, select the local input on the sender, and choose
**Offer OOT .nup**, **Offer TotK base .nsp**, or **Offer TotK update .nsp**.
Both managers must support `twilightProto/4` discovery (v0.16.2 or later);
legacy save and disc event encodings are unchanged.

The transfer hub shows the same saved input selections used by builds,
including an existing selected `prod.keys`; choosing or receiving a file
updates that game's build inputs too. Selecting a game or starting a build
never automatically offers files. Sending remains manual.

The receiver approves each offer and chooses a new destination with the
matching extension. Existing files are never overwritten. Files are staged in
that directory, bounded to 64 GiB, checked for NUP/NSP container structure and
transfer SHA-256, then selected as the corresponding local setup input. These
checks do **not** establish title identity, game authenticity, or ownership.
Receipt never extracts, installs, or executes the file. Extracted OOT folders and
TotK input formats other than `.nsp` remain local-only inputs.

Console keys are **separately disabled by default**, even when game-file sharing
is enabled. Both PCs must explicitly allow key sharing; sending and accepting
`prod.keys` each require a sensitive-data confirmation. Key files are capped at
64 KiB and checked as bounded text containing hexadecimal key assignments.
Transfer logs omit key contents and key-file hashes. The chosen destination is
an **unencrypted local file**; protect it like the original. This feature never
shares the app's private pairing identity.

On both PCs, open **Settings > Game-file transfers > TotK** and enable **Also allow
my console keys to this paired PC** before choosing **Offer prod.keys**. If the
receiver's permission is off, the offer is declined with instructions on the
sender; pairing, the connection, and play locks remain intact. Enable the
permission on the receiver and offer the file again. Nothing is received or
enabled automatically.

Transfers use the existing authenticated encrypted paired channel, bounded
32 KiB chunks, backpressure, sender rehashing, receiver verification and
no-overwrite commit. Permissions and idle state are rechecked before sending,
accepting and committing. A timeout, mismatch, disconnect or declined offer
discards uncommitted staging. Only transfer your own games and console keys to
your own trusted paired PC; no public upload service is involved.

### Disc images

Save sync cannot be used to smuggle a disc image or installer. Disc transfer uses
separate event types and is disabled by default on **each** device.

The sender selects a user-supplied GameCube `.iso` or lossless `.rvz` for
`GZ2E01`, `GZ2J01`, or `GZ2P01`, revision 0. Its reconstructed disc must be exactly
**1,459,978,240 bytes** and match the official Dusklight disc catalog's XXH3-128
hash before an offer is sent. The receiver must opt in, approve that offer,
select a new local destination with the matching extension, and have enough
staging space for the transferred file.

The receiver stages the bytes and independently verifies the disc header,
reconstructed size, official XXH3-128 hash, and full transferred-file SHA-256
before committing without replacing an existing file. A timeout, mismatch,
disconnect, or refused approval discards uncommitted staging data. Immediately
before committing, the manager rechecks the trusted channel, router freshness,
permissions, game state, play locks, and competing management operations.

RVZ uses the established `nod` decoder, not a new compression format. Container
headers, metadata ranges, counts, chunk sizes, and codecs are checked first.
Untrusted decompression runs in a separate copy of this application's verifier,
gated behind a Windows Job Object with a **512 MiB process-memory limit**, one
active process, kill-on-close cleanup, and a **five-minute deadline**. Output is
bounded to 4 KiB per pipe. This is resource containment, **not a privilege sandbox
or a guarantee against every decoder vulnerability**.

Verification streams the reconstructed bytes to the hash without writing an
expanded ISO. ISO transfers retain their streaming checks; RVZ verification runs
off the network actor so five-second heartbeats and router checks continue.
Both managers need v0.5.0 or later for RVZ: its offer is a new appended event, while existing
save and ISO event encodings remain unchanged. Older peers safely reject an RVZ
offer; update both devices before sending one.

Images are never mounted, extracted into game files, downloaded from third
parties, or executed. Wii images, unrelated formats, and trimmed, scrubbed, or
modified images that do not reconstruct an approved original disc are rejected.
You must dump and own your own original game.

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
- The verified installation is moved into `%USERPROFILE%\Games\Dusklight`
  and selected locally. **It is not automatically run.**
- Each release gets a version-and-archive-hash directory, so updating does not
  overwrite an existing executable or change the game profile, texture selection,
  save path, pairing, or play lock.
- A protected installation receipt records every extracted file's SHA-256.
  Selecting an already-installed build rechecks the receipt and file inventory;
  changed or untracked files cause an explicit error instead of being overwritten.
  Earlier v0.1 installs are left intact; the updater creates a separate managed build.
- Older builds under `.twilight-sync\apps` remain recognized and are never moved
  automatically. New installs and updates use the user's `Games\Dusklight`
  directory; existing custom selections remain unchanged until explicitly replaced.

The published digest is integrity metadata obtained from GitHub over HTTPS, not
an independent publisher signature. This flow still trusts the official
repository, release account, GitHub, and certificate infrastructure. It does not
claim to detect a compromised upstream release. Existing installations can
instead be selected with **Locate dusklight.exe**.

### Automatic TotK Recomp updates

On **Games**, select **Tears of the Kingdom**, locate the existing native
installation if necessary, then choose **Install / update TotK Recomp**. **Check latest
release** only retrieves metadata. Like Dusklight's updater, these are explicit
Internet exceptions; there is no scheduled or unapproved game-program installation.

For a clean install, select the local base game, supported update and console
keys in Games, then choose **Install / update TotK Recomp**. Setup runs
automatically using its bundled tools, without the separate source-build SDKs.
The missing-installation action opens this managed setup rather than redirecting
to a download. **Download official TotK Recomp setup** remains an optional manual
alternative. Existing custom install folders are respected; updates do not move them.

- Metadata comes from
  `api.github.com/repos/valentderah/totk-pc-recomp/releases/latest`. Only the exact
  `TotK-Setup-<version>-windows.zip` asset from that repository/tag is accepted.
  The published SHA-256 digest, HTTPS and GitHub redirect allowlist are required.
- Downloads are bounded to 512 MiB. Extraction uses the same path/link checks as
  Dusklight, with a separate 16,384-entry and 2 GiB expansion limit for the
  bundled compiler, runtime and Python. The setup's `VERSION` must match the release.
- **This runs the verified upstream installer**, unlike Dusklight's side-by-side
  binary installation. Its bundled Python runs `-m installer --dest <folder>
  --root <verified setup folder>` without a shell or system Python dependency.
  The game executable and shaders are rebuilt in place; shader preparation can
  briefly open a window. Both games must be stopped and pending transfers,
  reservations and recovery completed before updating.
- Existing installations with a supported `totk-install.json` receipt update in
  place. Without that receipt, local inputs are required before any download.
  First setup adds `--base`, `--update`, `--keys` and optional `--firmware`, and
  uses a stable `Games\TotKRecomp\release-<tag>-<sha>-<id>\game` target. It is not
  prepopulated or renamed because upstream rebuild commands can embed its path.
  Existing profile/mods/fonts/settings are copied and saves verified before the
  new executable is selected. Unmanaged previous files are not overwritten.
  No game assets, console keys or optional DLSS library are downloaded by this flow.
- TotK play, incoming saves and automatic synchronization pause while rebuilding;
  the partner receives an unavailable game state. A normal successful setup exit,
  its final success event, the resulting executable and the exact installed
  version are checked. The committed save must still match its pre-update hash.
  The upstream setup preserves settings and mods, including its language-code
  normalization when updating an older configuration.
- Private pending-install metadata under the manager root blocks play/save sync
  for the requested and staged folders after a restart. Legacy game-folder
  update markers are also recognized. Failures retain installer logs.
  Retry **Install / update TotK Recomp** to resume at the same target;
  only verified success clears the marker.
  Newer installed versions are never silently downgraded.
- Verified setup folders, logs and build caches remain under
  `.twilight-sync\apps\totk`; the game's mod-rebuild command needs that setup to
  remain available. They consume additional disk space and are not automatically
  pruned. Failed program rebuilds are resumable, not atomic program rollbacks.

## Source builds: one workflow for all games

**Game setup > Build from source** uses the same check, setup,
input, consent, build/log and cancellation controls for all three games. The
Slint interface groups the workflow into **Tools**, **Game files**, and **Build**.
Dusklight/TotK retain their official-release/setup option. Triforce is built from
source; the manager does not invoke its publisher's Setup.

Source builds are slower and **not confirmed to be safer**. They reduce reliance
on prebuilt game executables, not on all prebuilt software. The code, build scripts
and dependencies run as the user, outside a sandbox. For code you do not trust,
build and run it inside a separate VM. Neither a Git commit nor an archive
checksum is a malware review or independent publisher signature.

The flow:

1. Query the official GitHub release tag for Dusklight/TotK or the stable tags on
   `git.triforceproject.com/triforce/source`. Resolve a full commit and offer its
   review URL. Metadata checks execute no upstream code.
2. Explicitly approve official tool archives. Git, uv, CMake/Ninja or LLVM-MinGW
   and SDL SDK packages come from their publisher repositories over HTTPS and
   require published SHA-256 metadata. Cached archives are rehashed before use;
   each operation extracts new copies rather than trusting a previous mutable
   tool directory. Python/project package setup occurs in an approved build.
3. Finish the displayed one-time SDK prerequisites. Some require the vendor's
   installer/permissions; Twilight Sync does not bypass signing, UAC, antivirus,
   SmartScreen or Windows policy. TotK currently follows the upstream
   `C:\msys64\ucrt64` development layout. Missing Visual Studio, .NET 10 and
   MSYS2 rows offer **Set up** and **Commands**. Visual Studio setup selects the
   2026 C++ tools and Windows SDK 10.0.26100 for you, completing an existing
   2026 installation or installing Build Tools alongside older versions.
   Microsoft's installer still asks for its terms and Windows permissions;
   no restart is forced. If it requests a restart, restart Windows before
   building. Checks validate the compiler, SDK headers, x64 libraries and
   resource compiler, including custom Visual Studio/SDK locations.
   The .NET command is
   `winget install --id Microsoft.DotNet.SDK.10 --exact --source winget`.
   MSYS2 is installed via `MSYS2.MSYS2` if absent, updated twice using fresh
   `bash -lc` sessions, then provisioned with the UCRT64 package list. Close other
   MSYS2 sessions first; core upgrades can close them. Signing checks stay on.
   SDK package updates are not game-build jobs and cannot be cancelled by
   **Cancel source build**. Prerequisites are rechecked after completion.
   When using copied commands outside the app, click **Check source version /
   prerequisites** afterwards to refresh cached results. Missing MSYS2 checks
   expose the exact files under **Details**; both `include\vma\vk_mem_alloc.h` and the flat
   allocator-header layout are accepted and preserved for the upstream compiler.
   Checks stay expanded until you hide them, and installed rows keep their
   places instead of removing controls. Installer progress and errors appear
   at the top of Game setup, separate from game-build cancellation.
4. Supply legally obtained local inputs: no build assets for Dusklight's engine;
   TotK base/update/keys and optional firmware fonts; Triforce `.nup` or extracted
   files. A current Triforce install's `.triforce_exefs` and `romfs` are reused
   automatically when selected inputs are absent.
   **Game files** labels each input separately: Ocarina of Time uses your `.nup`
   game file (Triforce is the PC project's name), while Tears of the Kingdom uses
   a base `.nsp` and a separate supported update plus `prod.keys`. TotK's other
   supported base formats remain available in the picker. Optional firmware
   fonts and extracted OOT folders are under additional input options.
5. A fresh checkout is verified against the approved commit. Dusklight also
   initializes source-pinned submodules. Builds use fixed recipe arguments, not
   a user/peer-provided shell command. Source child processes receive a limited
   OS/build environment, not arbitrary inherited credential environment variables.
   Compiler workers are selected automatically from the PC's available CPU
   threads and memory. Compiling and owned shader-preparation processes are not reported
   as gameplay, so the paired PC can still reserve and play its existing game.
   Game setup shows live stage progress, completed/total work, elapsed time and
   an estimated time left **for the current stage**, not a fabricated whole-build
   countdown. OOT's builder estimates use its measurements from this PC when
   available; otherwise estimates use observed progress after enough work has
   completed. TotK's byte/item counters and CMake/Ninja compiler output are
   recognized too. New phases, reset counters and stalls discard stale estimates;
   silent or unmeasurable phases remain indeterminate. **Show live activity**
   displays recent output inside the app, newest first. This view is bounded and
   refreshed in the build worker, so verbose output does not flood the GUI.
   **Open full log externally**, inside that panel, remains an optional diagnostic
   action; omitted verbose lines remain in the full log.
   Build settings, approvals and local game-input selections are locked during
   the operation; navigation and read-only browsing stay available. After a
   verified build is selected successfully, a persistent green completion banner
   appears across pages with **Go to Play** / **View build** actions. It does not
   launch the game or bypass play reservations. If compilation succeeds but
   activation is blocked, an amber banner explicitly says activation is required
   instead. The completion notice can be dismissed.
   Actual running games and play reservations remain protected; a completed
   build cannot replace the selected installation until play has finished.
   Triforce prepares shaders before the new installation is
   selected by default. It seeds rebuilds with shader caches from the previous
   local install; Triforce checks compatibility and still prepares missing
   shaders. A cold cache can take hours. The advanced graphics option to skip
   preparation moves that work to first launch.
6. Play, incoming replacement and automatic sync are paused during compilation.
   Windows jobs own build process trees so cancellation or manager termination
   stops those helpers. This is lifecycle control, **not isolation from malicious
   scripts**. Copies, downloads or archive verification may take time to finish
   after a cancellation request.
7. Built Windows x64 headers, executable hashes and preserved save hashes are
   checked before selection. Output is kept under a distinct
   `Games\<game>\source-<tag>-<commit>-<id>\game` folder. Profiles/settings/mods are
   copied, not removed from the previous installation. Verified nonempty
   source-build activation retains its existing common sync base.

Only the selected game's build recipe runs. Dusklight uses its documented MSVC
CMake preset/install target. TotK compiles hactool and the shader translator,
then runs the source checkout's development-mode installer, which compiles the
runtime rather than consuming the release's prebuilt runtime library. Its pinned
MacNativeGames shader-reference history is fetched as source; normal gameplay is
not started, although shader preparation may open a window.
The installer runs in its Python 3.14 environment with the pinned upstream
`requirements-installer.txt` packages; Python 3.12 remains separate for the
development build tools.

Triforce uses `python -I -B nx2/make_game.py` against a new output folder, never
`--update` against the working install. Its game and Vulkan renderer are compiled
locally. Official SDL compatibility/runtime SDKs and pinned Khronos/VMA source
headers are used; the custom prebuilt `wgpu_native.dll` fallback is excluded, not
the required Vulkan backend. `triforce_env.txt` in the new folder explicitly
selects Vulkan; other options and the previous folder remain intact.
Required FFmpeg AV1/audio headers/static libraries are provisioned in the CPM
layout from `Takoroka/ffmpeg-av1` tag `9.0.2-av1-1`, with its published archive
SHA-256 verified. This is a separate binary supplier, not FFmpeg.org or a
source-to-binary attestation. Bounded tar/zstd extraction rejects unsafe paths,
links and special files. Required Vulkan/SDL runtime files and game data must
exist before activation.

RenoDX HDR and shader preparation keep upstream defaults. Optional graphics
controls can enable AMD FSR 4 or NVIDIA DLSS/frame generation and their dependency
downloads; NVIDIA's SDK terms require explicit acceptance. HDR or shader
preparation can be disabled deliberately, rather than silently stripping all
optional features from every build. Shader preparation is not normal gameplay.

Logs and a `twilight-source.json` receipt record source/tool provenance. No source
failure falls back to downloading a prebuilt game. Failed workspaces, successful
workspaces and previous installs are retained for diagnosis/rebuild support and
are not automatically pruned. Native builds require at least 64 GiB free; the
portable-data/mod copy is additional. Keep game-derived workspaces private.

### Triforce save channel

Both managers must advertise `twilightProto/3` discovery support before sending
Triforce game envelopes. Existing game IDs and the Noise transport remain
unchanged; older peers retain Dusklight/TotK support without Triforce traffic.
Discovery support is an unauthenticated hint, not a replacement for pinned
Noise identities and pairing approval.

The native adapter targets the current Triforce fixed user under
`<game>\user\save\<user id>`. It normalizes that account for transfer and maps it
back to the game's local identity on receive. Complete `account.sav`, `option.sav`
and slot progress/caption data travel together. The same bounded native bundle,
stable-save rules, rollback copies and 256 MiB history engine are reused, with
separate session/base/history state under `.twilight-sync\triforce-transfers`.

Only committed saves are observed. `save-tx\<user id>\new`, `old` or `ready-*`
requires the game to recover its own transaction first; `work` is never sent.
Inactive accounts, game-owned backups, graphics INI files and mods are excluded.
Unsupported active-account layouts fail closed. Triforce history uses its own
`OOTR` stream identity, never TotK's stream.

The shared native channel supplies approvals, safe common-base fast-forward,
retained divergent branches, explicit reconciliation, history preview/restore,
durable online play grants, release and bilateral recovery. Managed launch selects
the actual `bin\Triforce.exe` when present, suppresses the game's own update check
for that launch, and refuses a Setup-only folder. Open **Play** from the single
Twilight Sync desktop icon. Direct game launches remain outside manager control.

Triforce slot/file comparisons are complete-snapshot comparisons. Its recognized
native table schema (`0x002D80EB`, progress boundary `0x175D8`, caption boundary
`0x2A0`) supplies hearts, rupees, play seconds and caption date/type. File lengths
vary and are not used as a format whitelist. Missing/duplicate required fields,
bad headers, impossible values/dates and mismatched captions fail verification
before replacement. Unknown formats show details unavailable. No quest, map,
stamina or inventory values are guessed. Stored game clocks, not file mtimes,
determine displayed save dates.

## Texture packs: creator downloads, managed installs

The **Texture packs** page includes:

- **Henriko Magnifico's Twilight Princess 4K** project.
- The same creator's **1080p** edition, with lower-resolution textures.
- **[TP Definitive Edition+](https://gamebanana.com/mods/676609)**, including the
  complete v1.2.0 archive and a `1.2.0` local version-label preset.
- **Authorized local HD/custom imports**, including assets you have permission
  to use from your own sources. Nintendo-derived HD assets are not bundled,
  redistributed by Twilight Sync.

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

The **Textures** page is always Dusklight's library, even when another game is
selected. Installed packs appear first; the active pack is marked **Active**.
Use **Install or update a pack** below to choose a creator preset and install
your downloaded files. Version labels and detected profile paths are optional
details, not setup steps.

1. Stop Dusklight on both paired devices.
2. Select the project and confirm permission to use its texture assets.
3. Choose **Install ZIP** and select the complete creator archive. An already
   extracted folder is also supported. There is no need to extract individual
   category/variant folders or select `config.json`.
4. The manager resolves Dusklight's actual profile, validates the archive,
   installs the native-selected textures, and activates the pack. Existing
   unmanaged texture files require approval to preserve them before switching.
5. Use **Activate** beside an installed version to switch later. Changes apply
   on the next normal game launch.

Direct downloads are paused while creator sources are inaccessible. The TPDE+
Google Drive source returned a download-quota error and its second listed mirror
refused access during validation. Hosting quotas, account permissions, paid
access, and anti-bot controls are not bypassed. Automatic downloading is not
included in this update.

Updating means installing the new authorized archive/folder. Identical content is deduplicated by content ID instead of
being installed again under a different label. Older installed versions stay
available for switching back. Texture libraries are independent on each device;
**texture files never travel over twilightProto**.

Only native-style `tex1_...` PNG/DDS images are installed. The importer reports
skipped files: bundled emulators, DLLs, scripts, shaders, installers, documentation,
and Dolphin-only names such as unsupported material maps are not copied.
Category duplicates are selected exactly like released Dusklight/Aurora:
relative path components are sorted case-insensitively with original-case
tie-breaking, and the first registration for each texture key wins. The complete
TPDE+ archive therefore does not need manual variant extraction. Repeated entries
at the exact same archive path are still refused.

Path traversal, links, alternate data streams, unsafe names, excessive entry
counts/depth, and oversized files are refused. PNG structure/chunk CRCs and DDS
headers/mip payload lengths are checked. Valid DDS data mislabeled `.png` is
installed with the correct extension after full structural validation; unrelated
or executable data is not renamed into acceptance.

Images are bounded to 16,384 pixels per dimension and 64 megapixels total, which
supports the pack's 9,216-pixel font atlases and large 8K textures. This is structural validation, not a
guarantee that every image is harmless to every game/graphics-library version.

The import limits are 25,000 selected images, 100,000 source entries, 256 MiB per
image, and 16 GiB per pack. Large packs genuinely use gigabytes; the save-history
budget does not apply to textures or program installations.

Free space for the entire selected pack is checked before copying. Installation
reports progress and can be cancelled before the transactional activation stage.
The archive is opened read-only; only PNG/DDS assets are published into the library.

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

Commits record sequence, parent, optional reconciliation parent, game/save
identity, timestamp, username, device, source path, reason, full save hash, and
block references. Ordinary repeated unchanged saves add nothing; an explicit
reconciliation may add ancestry without changing bytes. Image/header pages and unchanged quest data are shared
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
- A partner preview branches from the retained common save when available; it
  never becomes the parent of subsequent local play. Selection records both
  retained branches. Existing version-1 commits without the optional second
  parent remain readable and keep their original content hashes.

### Read-only progress inspection

Home, the fork prompt, and history decode the reconstructed validated GCI, not
file modification times or guessed progress. The decoder follows Dusklight
v2.0.3's [player layout](https://github.com/TwilitRealm/dusklight/blob/v2.0.3/include/d/d_save.h),
[serialization](https://github.com/TwilitRealm/dusklight/blob/v2.0.3/src/d/d_save.cpp),
[item IDs](https://github.com/TwilitRealm/dusklight/blob/v2.0.3/include/d/d_item_data.h),
and [primary/backup-bank behavior](https://github.com/TwilitRealm/dusklight/blob/v2.0.3/src/m_Do/m_Do_MemCardRWmng.cpp).

Each occupied quest slot exposes player/horse names, in-save dates (shown as
UTC), total time, capacity/current health, heart pieces, rupees, saved stage/room
and form, carried items, recorded equipment, arrows/bombs, Poe souls, golden
insects, Fused Shadows, mirror shards, boss-clear records, deaths, and clear count.
Quest integers remain big-endian inside either sector envelope. Capacity uses
five stored units per heart; current health uses quarter-heart units. Play time
and in-save dates use GameCube/Wii tick rates and the 2000 epoch. Empty slots,
missing clocks, unknown item IDs, and legacy-encoded names are identified rather
than invented. The inspector does not change game data or bypass checksums.

Summaries describe only recorded milestones, not a reconstructed story or a
complete activity log. Comparisons report decoded gains and losses from the
common retained save, or from the other branch when no verified base remains.
Timestamps are informational; clock skew, mods, resets, and non-monotonic game
progress make neither newest date nor greatest play time a safe automatic winner.

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
    update-settings.json    Optional automatic app-update-check preference
    source-builds.json      Local build methods, input paths and worker preferences
    interface.json          Optional Windows/System, Light, or Dark appearance preference
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
        <tag>-<hash>\      Legacy Dusklight build and file-hash receipt (not moved)
        totk\
            <tag>-<hash>-<id>\  Verified upstream setup, build cache and updater logs
    updates\
        stage-<id>\        Verified app-update handoff; failed logs/backups are retained
    triforce-transfers\     Triforce session, common base and separate compact history
    build-tools\
        archives\          Rechecked official tool ZIPs; fresh tool extractions are temporary
    build-logs\            Captured local source-build output
    source-builds\
        <game>\<tag>-<commit>-<id>\  Retained source, Python environments and game-derived work
```

New game installations default to `%USERPROFILE%\Games`:

```text
Games\
    Dusklight\
        <tag>-<hash>\      Side-by-side managed build and file-hash receipt
    TotKRecomp\            Native TotK game, profile, settings and mods
    Triforce\              Native OoT game, user saves and local graphics settings
    <game>\source-<tag>-<commit>-<id>\game\  Side-by-side source-built game
```

Updater caches and application settings remain in `.twilight-sync`; they are not
mixed into the game's installation folder.

Audit logs include the local **username, file paths, hashes, public peer keys, timestamps,
security decisions, and errors**. They do not include save payloads, console-key
contents or key-file hashes, private pairing keys, or pairing comparison codes.
A log rotates at approximately 5 MiB and keeps four
older files. History, textures, retained program versions, and preserved original
texture folders consume additional space. Snapshot metadata also contains
usernames and paths; its compressed game data is not encrypted at rest.
Do not post logs or configuration publicly without redacting personal details.
Device name, public key, network tag, and pairing availability are visible in
local discovery; save contents and paths are not.

Source-build logs additionally capture upstream stdout/stderr. Review and redact
them before sharing: third-party scripts can print anything they can read.
Twilight Sync does not upload game inputs/keys/saves to a public service.
Explicitly approved paired-LAN transfers are supported; an approved source
script has normal user permissions and can make its own network requests.

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

### Twilight Sync app updates

App checks use `https://api.github.com/repos/false00/twilight-sync/releases/latest`,
not the separate Dusklight repository. They run at startup, when you reopen the
dashboard from the app shortcut or tray, and every six hours unless disabled. Repeated opens
within a few seconds share the recent check. Only stable, newer SemVer releases are offered.
Metadata requests time out and failures are reported as unknown update status,
never as a successful "up to date" check.

No binary downloads happen without tray/dashboard confirmation. The updater
requires an exact official asset URL, bounded file size, GitHub SHA-256 digest,
matching `SHA256SUMS.txt` entry, and a Windows x64 PE32+ binary. It never runs an
unchecked remote script or elevates. The verified new executable runs as a
minimal helper, acknowledges readiness, and waits for the original PID and
executable to exit normally before applying a same-directory replacement.

Only activity on this PC, such as a running game, a pending transfer, or a
local installation, blocks handoff. Gameplay on the paired PC and durable play
reservations alone do not block app updates. If a local operation begins while
downloading, the helper is cancelled and the current app stays running.
Installation receipts are updated for the
PowerShell installer; game settings, pairing identities, and save histories are
not migrated or reset. Preference storage is separate from the original game
settings schema. Interrupted/failed update files are preserved for manual review;
only known completed/cancelled stage files are eligible for automatic cleanup.

As with the bootstrap, this trusts the public repository/release publisher and
certificate infrastructure. Checksums are not independent publisher signatures.
This automatic metadata request is an explicit exception to LAN-only save traffic.

### Read-only commands

Read-only diagnostics do not initialize the desktop, register startup, trust a
router, or write application state:

```powershell
cargo run --locked -- --help
cargo run --locked -- --check-save-location
cargo run --locked -- --check-save "$env:APPDATA\TwilitRealm\Dusklight\USA\Card A\01-GZ2E-gczelda2.gci"
cargo run --locked -- --check-iso 'D:\My Disc Dumps\Twilight Princess.iso'
cargo run --locked -- --check-iso 'D:\My Disc Dumps\Twilight Princess.rvz'
cargo run --locked -- --check-network
cargo run --locked -- --check-updates
cargo run --locked -- --check-totk-updates
cargo run --locked -- --check-triforce-save
cargo run --locked -- --check-source-updates dusklight
cargo run --locked -- --check-source-updates totk
cargo run --locked -- --check-source-updates triforce
cargo run --locked -- --check-app-updates
```

An unsupported/corrupt input exits with an error. Do not treat a `VALID DUSKLIGHT SAVE`
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

For TotK, an optional live-package check verifies a locally downloaded copy of the
latest official Windows ZIP, extracts it into a temporary directory, and runs only
the bundled installer's `--help`. It never builds, installs or runs the game:

```powershell
$env:TWILIGHT_TOTK_SETUP_ARCHIVE = 'D:\Downloads\TotK-Setup-<version>-windows.zip'
cargo test --lib --locked totk_update::tests::official_setup_can_be_verified_without_installing_or_running_the_game -- --ignored --exact --nocapture
```

Windows tests create isolated temporary data and loopback Noise sessions. They
exercise both-sided consent, key pinning, encrypted commits, conflicts/backups,
outside-game checks, elapsed save stability, router-change/staleness gates,
crash recovery, final-save-before-unlock ordering, strict parsers, framing bounds,
tamper/replay rejection, rekeying, and the separate disc-image permission. RVZ
fixtures also exercise receiver-side full reconstruction and catalog rejection
over the encrypted transfer, with heartbeats still responsive. They do not
modify real startup settings or require a copyrighted save/disc fixture.

A debug-only, 20-second native UI smoke run is available with
`cargo run --locked -- --ui-smoke`; it creates no real keys, startup entries,
settings, or network sockets. Add `--background` to exercise a hidden startup.
Use `--theme light|dark|system`, `--page overview|devices|library|textures|history|security`,
`--scenario offline|ready|pairing|connecting|attention`, and `--scale 1.5`
to validate themes, populated/empty states, and high-DPI layouts. The isolated
window is titled **Twilight Sync - UI smoke** and never saves appearance changes.
Numeric smoke-test scales from 0.75 through 2 are supported.
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

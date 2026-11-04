# Read-only TotK save details

Twilight Sync never edits game fields to produce previews. Metadata is decoded
from the exact checksum-verified committed snapshot selected by the native
`profile/account.uid`. File modification times and sizes are not evidence of
quest progress or a default conflict winner.

## Verified layouts

The little-endian save magic is `0x01020304`:

| Family | Progress length | Header version | Data-region start |
| --- | ---: | --- | --- |
| 1.0 | 2307552 | `0x0046c3c8` | `0x0003c050` |
| 1.1 / 1.2 | 2307656 | `0x0047e0f4` | `0x0003c088` |
| 1.4 | 2307856 | `0x0049e946` | `0x0003c138` |

The hash/value table starts at `0x28`. The reader checks table/pointer bounds,
duplicate selected hashes, finite numeric values, supported ranges, valid dates
and matching progress/caption format headers. Known layouts must contain the
required progress fields. Missing optional caption fields stay unavailable,
not zero or guessed values.

| Field | Hash | Representation / display |
| --- | --- | --- |
| Maximum life | `0xfbe01da1` | Inline integer, four life units per heart |
| Rupees | `0xa77921d7` | Inline integer |
| Maximum stamina | `0xf9212c74` | Inline float, 1000 per stamina wheel |
| Play time | `0xe573f564` | Inline unsigned seconds |
| Save position | `0xc884818d` | Checked pointer to three floats, raw world coordinates |
| Map/banc | `0x1d6189da` | Checked pointer to a terminated 64-byte identifier |
| Caption date | `0x9811a3f7`, `0xdfd840d3`, `0xbd46f485`, `0x23f3d75e`, `0x27853bf7` | Year, month, day, hour, minute |
| Caption autosave | `0x25f03caa` | Boolean; otherwise manual save |

The date is the game's stored clock, not a trusted global timestamp. Coordinates
are labelled world position. A raw `Dungeon136` identifier is not translated
into an unverified shrine name. Quest flags and inventory are not claimed.

## Format references and independent checks

The format was checked against Marc Robledo's MIT-licensed save-format reader,
not executed or bundled as an editor:

- [Header, field mapping and caption metadata](https://github.com/marcrobledo/savegame-editors/blob/master/zelda-totk/zelda-totk.js)
- [Typed scalar/pointer table reader](https://github.com/marcrobledo/savegame-editors/blob/master/zelda-totk/zelda-totk.variables.js)
- [Published heart/stamina unit scales](https://github.com/marcrobledo/savegame-editors/blob/master/zelda-totk/index.html)
- [Reference project license](https://github.com/marcrobledo/savegame-editors/blob/master/LICENSE)

Reader blobs reviewed: `a6928f12dab95b41d0c9f6c471baa883cbf5ffc3` and
`dbde0ba24404803f8a9fc61ee5f05807e7ec3ed2`. This Rust reader is independently
written and read-only; no JavaScript editor, game data, real save fixture or
copyrighted game asset is distributed with it.

The retained 1.4.3 extraction's `GameDataList.Product.140.byml.zs` independently
confirmed the selected field hashes/types (`Int`, `UInt`, `Float`, `String64`,
`Vector3`) and the 1000.0 default stamina value. That private schema was read
through the verified RomFS index and dictionary pack; it is not included in the
repository. Real values were inspected read-only; tests use synthetic tables.

## Sync safety and compatibility

Decoded details are local-only fields omitted from the existing wire summary,
so older peers can still read the same bounded JSON. Verified partner details
come from the complete checksum-verified partner snapshot, not a JSON claim.

Unknown layouts remain transferable as validated opaque save bundles with a
details-unavailable state. A malformed known layout fails verification before
offering or committing progress. Failed refreshes clear the old hash/summary and
advertise unready state; stale data cannot keep claiming matching saves.
Installation metadata cannot override an unready peer state.

The GUI gives a specific visible/hover reason for disabled Play/Sync controls:
active play, incoming approval, comparison transfer, a retained reservation,
release acknowledgement, missing/unverified saves, an offline partner or already
matching snapshots. Safety checks remain active. `--check-totk-save` emits the
same decoded slot details and canonical content hash without changing saves.
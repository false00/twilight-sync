use crate::files::{self, QUEST_SIZE};
use anyhow::Result;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const GC_TICKS_PER_SECOND: u64 = 40_500_000;
const WII_TICKS_PER_SECOND: u64 = 60_750_000;
const GC_EPOCH: i64 = 946_684_800;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: u8,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub number: u8,
    pub player: String,
    pub horse: String,
    pub play_seconds: Option<u64>,
    pub saved_at: Option<String>,
    pub life_quarters: u16,
    pub hearts: u16,
    pub heart_pieces: u8,
    pub rupees: u16,
    pub stage: String,
    pub room: i8,
    pub wolf: bool,
    pub items: Vec<Item>,
    pub equipment: Vec<String>,
    pub arrows: u8,
    pub bombs: [u8; 3],
    pub poes: u8,
    pub insects: u8,
    pub fused_shadows: u8,
    pub mirror_shards: u8,
    pub boss_clears: u8,
    pub deaths: u16,
    pub clears: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub game: String,
    pub file_name: String,
    pub modified_at: Option<String>,
    pub slots: Vec<Slot>,
}

fn be16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn be32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn be64(bytes: &[u8], offset: usize) -> u64 {
    (u64::from(be32(bytes, offset)) << 32) | u64::from(be32(bytes, offset + 4))
}

fn text(bytes: &[u8]) -> String {
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    match std::str::from_utf8(&bytes[..length]) {
        Ok(value) => value.chars().filter(|value| !value.is_control()).collect(),
        Err(_) => "Legacy-encoded name".into(),
    }
}

fn date(seconds: u64) -> Option<String> {
    if seconds == 0 {
        return None;
    }
    let unix = GC_EPOCH.checked_add(i64::try_from(seconds).ok()?)?;
    OffsetDateTime::from_unix_timestamp(unix)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

pub fn duration_label(seconds: u64) -> String {
    format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
}

pub fn item_name(id: u8) -> String {
    let name = match id {
        0x28 => "Ordon Sword",
        0x29 => "Master Sword",
        0x2a => "Wooden Shield",
        0x2b => "Ordon Shield",
        0x2c => "Hylian Shield",
        0x2e => "Ordon clothing",
        0x2f => "Hero's Clothes",
        0x30 => "Magic Armor",
        0x31 => "Zora Armor",
        0x32 => "Shadow Crystal",
        0x3d => "Coral Earring",
        0x3e => "Hawkeye",
        0x3f => "Wooden Sword",
        0x40 => "Gale Boomerang",
        0x41 => "Spinner",
        0x42 => "Ball and Chain",
        0x43 => "Hero's Bow",
        0x44 => "Clawshot",
        0x45 => "Iron Boots",
        0x46 => "Dominion Rod",
        0x47 => "Double Clawshots",
        0x48 | 0xf8 => "Lantern",
        0x49 => "Light-filled Master Sword",
        0x4a => "Fishing Rod",
        0x4b => "Slingshot",
        0x4c => "Unpowered Dominion Rod",
        0x4f => "Giant Bomb Bag",
        0x50 => "Empty Bomb Bag",
        0x51 => "Bomb Bag",
        0x53 => "Light Arrows",
        0x58 => "Lure Fishing Rod",
        0x59 => "Bomb Arrows",
        0x5a => "Hawkeye Bow",
        0x5b => "Fishing Rod with bee larva",
        0x5c => "Fishing Rod with Coral Earring",
        0x5d => "Fishing Rod with worm",
        0x5e => "Coral Fishing Rod with bee larva",
        0x5f => "Coral Fishing Rod with worm",
        0x60 => "Empty Bottle",
        0x61 | 0x69 => "Red Potion",
        0x62 => "Green Potion",
        0x63 => "Blue Potion",
        0x64 => "Milk",
        0x65 => "Half-full Milk",
        0x66 | 0x68 | 0x6e | 0x6f | 0x9d => "Lantern Oil",
        0x67 => "Water Bottle",
        0x6a => "Nasty Soup",
        0x6b | 0x6d => "Hot Spring Water",
        0x6c => "Bottled Fairy",
        0x70 => "Bombs",
        0x71 => "Water Bombs",
        0x72 => "Bomblings",
        0x73 => "Great Fairy's Tears",
        0x74 => "Worm",
        0x75 => "Poe Soul Bottle",
        0x76 | 0x9e => "Bee Larva",
        0x77 => "Rare Chu Jelly",
        0x78 => "Red Chu Jelly",
        0x79 => "Blue Chu Jelly",
        0x7a => "Green Chu Jelly",
        0x7b | 0x9c => "Yellow Chu Jelly",
        0x7c => "Purple Chu Jelly",
        0x7d => "Simple Soup",
        0x7e => "Good Soup",
        0x7f => "Superb Soup",
        0x80 => "Letter",
        0x81 => "Invoice",
        0x82 => "Wooden Statue",
        0x83 => "Ilia's Charm",
        0x84 => "Horse Call",
        0x90 => "Auru's Memo",
        0x91 => "Ashei's Sketch",
        0xe9 => "Ancient Sky Book",
        0xea => "Sky Writing",
        0xeb => "Completed Sky Book",
        _ => return format!("Item 0x{id:02X}"),
    };
    name.into()
}

fn obtained(quest: &[u8], id: u8) -> bool {
    be32(quest, 0xcc + usize::from(id / 32) * 4) & (1_u32 << (id % 32)) != 0
}

fn slot(quest: &[u8], number: u8, clock: u64) -> Option<Slot> {
    let player = text(&quest[0x1b4..0x1c5]);
    if player.is_empty() {
        return None;
    }
    let ticks = be64(quest, 0x1a8);
    let maximum_life = be16(quest, 0);
    Some(Slot {
        number,
        player,
        horse: text(&quest[0x1c5..0x1d6]),
        play_seconds: (ticks <= i64::MAX as u64).then_some(ticks / clock),
        saved_at: date(be64(quest, 0x28) / clock),
        life_quarters: be16(quest, 2),
        hearts: maximum_life / 5,
        heart_pieces: (maximum_life % 5) as u8,
        rupees: be16(quest, 4),
        stage: text(&quest[0x58..0x60]),
        room: quest[0x61] as i8,
        wolf: quest[0x1e] == 1,
        items: quest[0x9c..0xb4]
            .iter()
            .filter(|id| **id != 0xff)
            .map(|id| Item {
                id: *id,
                name: item_name(*id),
            })
            .collect(),
        equipment: [0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2f, 0x30, 0x31, 0x3f, 0x49]
            .into_iter()
            .filter(|id| obtained(quest, *id))
            .map(item_name)
            .collect(),
        arrows: quest[0xec],
        bombs: [quest[0xed], quest[0xee], quest[0xef]],
        poes: quest[0x10c],
        insects: (0xc0..=0xd7).filter(|id| obtained(quest, *id)).count() as u8,
        fused_shadows: (quest[0x109] & 0x0f).count_ones() as u8,
        mirror_shards: (quest[0x10a] & 0x0f).count_ones() as u8,
        boss_clears: (0..32)
            .filter(|index| quest[0x1f0 + index * 0x20 + 0x1d] & 8 != 0)
            .count() as u8,
        deaths: be16(quest, 0x1b2),
        clears: quest[0x1d6],
    })
}

pub fn inspect(bytes: &[u8]) -> Result<Summary> {
    let identity = files::validate_save(bytes)?;
    let clock = if identity.game.starts_with("RZD") {
        WII_TICKS_PER_SECOND
    } else {
        GC_TICKS_PER_SECOND
    };
    let mut slots = Vec::new();
    for index in 0..3 {
        let start = 0x4048 + index * QUEST_SIZE;
        if let Some(slot) = slot(&bytes[start..start + QUEST_SIZE], index as u8 + 1, clock) {
            slots.push(slot);
        }
    }
    Ok(Summary {
        game: identity.game,
        file_name: identity.name,
        modified_at: date(u64::from(be32(bytes, 0x28))),
        slots,
    })
}

impl Slot {
    pub fn story(&self) -> String {
        let mut facts = Vec::new();
        if self.clears > 0 {
            facts.push("Game completion recorded".into());
        }
        if self.boss_clears > 0 {
            facts.push(format!("{} boss-clear records", self.boss_clears));
        }
        if self
            .equipment
            .iter()
            .any(|name| name.contains("Master Sword"))
        {
            facts.push("Master Sword obtained".into());
        }
        if self.fused_shadows > 0 {
            facts.push(format!(
                "{} Fused Shadow pieces recorded",
                self.fused_shadows
            ));
        }
        if self.mirror_shards > 0 {
            facts.push(format!(
                "{} Mirror of Twilight shards recorded",
                self.mirror_shards
            ));
        }
        if facts.is_empty() {
            facts.push("No major milestones recorded in this slot yet".into());
        }
        facts.join(". ") + "."
    }
}

impl Summary {
    pub fn changes_from(&self, before: &Self) -> Vec<String> {
        let mut changes = Vec::new();
        for current in &self.slots {
            let prefix = format!("Slot {}", current.number);
            let Some(previous) = before
                .slots
                .iter()
                .find(|slot| slot.number == current.number)
            else {
                changes.push(format!("{prefix}: new quest for {}", current.player));
                continue;
            };
            if let (Some(current_time), Some(previous_time)) =
                (current.play_seconds, previous.play_seconds)
                && current_time != previous_time
            {
                let sign = if current_time > previous_time {
                    "+"
                } else {
                    "-"
                };
                changes.push(format!(
                    "{prefix}: {sign}{} played",
                    duration_label(current_time.abs_diff(previous_time))
                ));
            }
            for (label, value, old) in [
                (
                    "heart containers",
                    i64::from(current.hearts),
                    i64::from(previous.hearts),
                ),
                (
                    "heart pieces",
                    i64::from(current.heart_pieces),
                    i64::from(previous.heart_pieces),
                ),
                (
                    "rupees",
                    i64::from(current.rupees),
                    i64::from(previous.rupees),
                ),
                (
                    "Poe souls",
                    i64::from(current.poes),
                    i64::from(previous.poes),
                ),
                (
                    "golden insects",
                    i64::from(current.insects),
                    i64::from(previous.insects),
                ),
                (
                    "Fused Shadow pieces",
                    i64::from(current.fused_shadows),
                    i64::from(previous.fused_shadows),
                ),
                (
                    "mirror shards",
                    i64::from(current.mirror_shards),
                    i64::from(previous.mirror_shards),
                ),
                (
                    "boss-clear records",
                    i64::from(current.boss_clears),
                    i64::from(previous.boss_clears),
                ),
            ] {
                if value != old {
                    changes.push(format!("{prefix}: {:+} {label} ({value})", value - old));
                }
            }
            for item in &current.items {
                if !previous.items.iter().any(|old| old.id == item.id) {
                    changes.push(format!("{prefix}: gained {}", item.name));
                }
            }
            for item in &previous.items {
                if !current.items.iter().any(|new| new.id == item.id) {
                    changes.push(format!("{prefix}: no longer carrying {}", item.name));
                }
            }
            for equipment in &current.equipment {
                if !previous.equipment.contains(equipment) {
                    changes.push(format!("{prefix}: obtained {equipment}"));
                }
            }
            for equipment in &previous.equipment {
                if !current.equipment.contains(equipment) {
                    changes.push(format!("{prefix}: {equipment} not recorded"));
                }
            }
            if current.stage != previous.stage || current.room != previous.room {
                changes.push(format!(
                    "{prefix}: saved at {} / room {}",
                    current.stage, current.room
                ));
            }
        }
        for previous in &before.slots {
            if !self.slots.iter().any(|slot| slot.number == previous.number) {
                changes.push(format!(
                    "Slot {}: quest for {} removed",
                    previous.number, previous.player
                ));
            }
        }
        changes
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use anyhow::Context;

    pub(crate) fn populated(native: bool) -> Result<Vec<u8>> {
        let mut bytes = if native {
            files::fixture_native_save()
        } else {
            files::fixture_save()
        };
        for sector in [0x4048, 0x6048] {
            let quest = &mut bytes[sector..sector + QUEST_SIZE];
            quest[0..2].copy_from_slice(&42_u16.to_be_bytes());
            quest[2..4].copy_from_slice(&27_u16.to_be_bytes());
            quest[4..6].copy_from_slice(&321_u16.to_be_bytes());
            quest[0x28..0x30].copy_from_slice(&(800_000_000 * GC_TICKS_PER_SECOND).to_be_bytes());
            quest[0x58..0x60].copy_from_slice(b"D_MN05\0\0");
            quest[0x61] = 2;
            quest[0x9c..0xb4].fill(0xff);
            quest[0x9c..0xa0].copy_from_slice(&[0x40, 0x43, 0x48, 0x60]);
            quest[0xd0..0xd4].copy_from_slice(&(1_u32 << 9).to_be_bytes());
            quest[0xe4..0xe8].copy_from_slice(&3_u32.to_be_bytes());
            quest[0xec] = 25;
            quest[0x109] = 3;
            quest[0x10a] = 1;
            quest[0x10c] = 17;
            quest[0x1a8..0x1b0].copy_from_slice(&(7_500 * GC_TICKS_PER_SECOND).to_be_bytes());
            quest[0x1b2..0x1b4].copy_from_slice(&3_u16.to_be_bytes());
            quest[0x1b4..0x1b9].copy_from_slice(b"Link\0");
            quest[0x1c5..0x1cb].copy_from_slice(b"Epona\0");
            quest[0x1f0 + 0x1d] = 8;
        }
        files::tests::reseal(&mut bytes)?;
        Ok(bytes)
    }

    #[test]
    fn decodes_real_layout_for_pc_and_console_without_modifying_bytes() -> Result<()> {
        for native in [false, true] {
            let bytes = populated(native)?;
            let original = bytes.clone();
            let summary = inspect(&bytes)?;
            assert_eq!(bytes, original);
            assert_eq!(summary.slots.len(), 1);
            let slot = summary.slots.first().context("Missing slot")?;
            assert_eq!(slot.player, "Link");
            assert_eq!(slot.horse, "Epona");
            assert_eq!(slot.hearts, 8);
            assert_eq!(slot.heart_pieces, 2);
            assert_eq!(slot.life_quarters, 27);
            assert_eq!(slot.rupees, 321);
            assert_eq!(slot.play_seconds, Some(7_500));
            assert_eq!(slot.saved_at.as_deref(), Some("2025-05-08T06:13:20Z"));
            assert_eq!(slot.items[0].name, "Gale Boomerang");
            assert_eq!(slot.equipment, vec!["Master Sword"]);
            assert_eq!(slot.insects, 2);
            assert_eq!(slot.poes, 17);
            assert_eq!(slot.fused_shadows, 2);
            assert_eq!(slot.mirror_shards, 1);
            assert_eq!(slot.boss_clears, 1);
            assert_eq!(slot.deaths, 3);
            assert!(slot.story().contains("Master Sword obtained"));
        }
        Ok(())
    }

    #[test]
    fn empty_and_invalid_saves_never_invent_progress() -> Result<()> {
        assert!(inspect(&files::fixture_save())?.slots.is_empty());
        assert!(inspect(&[]).is_err());
        let mut bytes = populated(false)?;
        bytes[0x4050] ^= 1;
        assert!(inspect(&bytes).is_err());
        assert_eq!(item_name(0x92), "Item 0x92");
        assert_eq!(duration_label(7_500), "2h 05m");
        Ok(())
    }

    #[test]
    fn wii_ticks_use_the_wii_clock_and_negative_play_time_is_unavailable() -> Result<()> {
        let mut bytes = populated(false)?;
        bytes[..4].copy_from_slice(b"RZDE");
        for sector in [0x4048, 0x6048] {
            let quest = &mut bytes[sector..sector + QUEST_SIZE];
            quest[0x28..0x30].copy_from_slice(&(800_000_000 * WII_TICKS_PER_SECOND).to_be_bytes());
            quest[0x1a8..0x1b0].copy_from_slice(&(7_500 * WII_TICKS_PER_SECOND).to_be_bytes());
        }
        files::tests::reseal(&mut bytes)?;
        let summary = inspect(&bytes)?;
        assert_eq!(summary.slots[0].play_seconds, Some(7_500));
        assert_eq!(
            summary.slots[0].saved_at.as_deref(),
            Some("2025-05-08T06:13:20Z")
        );
        bytes[0x4048 + 0x1a8..0x4048 + 0x1b0].copy_from_slice(&u64::MAX.to_be_bytes());
        files::tests::reseal(&mut bytes)?;
        assert_eq!(inspect(&bytes)?.slots[0].play_seconds, None);
        Ok(())
    }

    #[test]
    fn differences_report_gains_losses_and_removed_slots() -> Result<()> {
        let before = inspect(&populated(false)?)?;
        let mut after = before.clone();
        after.slots[0].hearts += 1;
        after.slots[0].play_seconds = Some(11_100);
        after.slots[0].items.remove(0);
        let changes = after.changes_from(&before);
        assert!(
            changes
                .iter()
                .any(|change| change.contains("+1 heart containers"))
        );
        assert!(
            changes
                .iter()
                .any(|change| change.contains("+1h 00m played"))
        );
        assert!(
            changes
                .iter()
                .any(|change| change.contains("no longer carrying Gale Boomerang"))
        );
        after.slots.clear();
        assert_eq!(
            after.changes_from(&before),
            vec!["Slot 1: quest for Link removed"]
        );
        Ok(())
    }

    #[test]
    fn all_three_slots_are_read_from_primary_bank_not_backup() -> Result<()> {
        let mut bytes = populated(false)?;
        let first = bytes[0x4048..0x4048 + QUEST_SIZE].to_vec();
        bytes[0x4048 + 2 * QUEST_SIZE..0x4048 + 3 * QUEST_SIZE].copy_from_slice(&first);
        bytes[0x6048 + 0x1b4] = b'Z';
        files::tests::reseal(&mut bytes)?;
        let summary = inspect(&bytes)?;
        assert_eq!(
            summary
                .slots
                .iter()
                .map(|slot| slot.number)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(summary.slots[0].player, "Link");
        Ok(())
    }
}

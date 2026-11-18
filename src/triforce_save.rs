use crate::totk_save::{Details, MAX_LIFE, PLAY_SECONDS, RUPEES, Table, caption_details, word};
use anyhow::{Context, Result, ensure};

const VERSION: u32 = 0x002d_80eb;
const DATA_START: usize = 0x0001_75d8;
const CAPTION_START: u32 = 0x0000_02a0;

pub fn known_layout(progress: &[u8]) -> bool {
    word(progress, 4).is_ok_and(|version| version == VERSION)
}

pub fn inspect(progress: &[u8], caption: Option<&[u8]>) -> Result<Details> {
    ensure!(
        known_layout(progress),
        "This Triforce save format is not supported for decoded details"
    );
    ensure!(
        (DATA_START..=crate::totk::MAX_FILE_SIZE as usize).contains(&progress.len())
            && word(progress, 8)? as usize == DATA_START,
        "Invalid Triforce progress table layout"
    );
    let table = Table::new(progress, &[MAX_LIFE, RUPEES, PLAY_SECONDS])?;
    let life = table
        .unsigned(MAX_LIFE)?
        .context("Triforce maximum-life metadata is missing")?;
    let rupees = table
        .unsigned(RUPEES)?
        .context("Triforce rupee metadata is missing")?;
    let seconds = table
        .unsigned(PLAY_SECONDS)?
        .context("Triforce play-time metadata is missing")?;
    ensure!(
        life > 0 && life <= 400,
        "Triforce life metadata is outside its supported range"
    );
    ensure!(
        rupees <= 999_999,
        "Triforce rupee metadata is outside its supported range"
    );
    let mut details = Details {
        format: "OoT 1.1".into(),
        max_hearts: Some(life as f32 / 4.0),
        rupees: Some(rupees),
        play_seconds: Some(seconds),
        ..Details::default()
    };
    if let Some(caption) = caption {
        ensure!(
            word(caption, 8)? == CAPTION_START,
            "Invalid Triforce caption table layout"
        );
        caption_details(VERSION, caption, &mut details)?;
    }
    Ok(details)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture(extra_bytes: usize) -> (Vec<u8>, Vec<u8>) {
        let mut progress = vec![0; DATA_START + 512 + extra_bytes];
        for (offset, value) in [(0, 0x0102_0304), (4, VERSION), (8, DATA_START as u32)] {
            progress[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (index, (hash, value)) in [(MAX_LIFE, 48_u32), (RUPEES, 888), (PLAY_SECONDS, 146_558)]
            .into_iter()
            .enumerate()
        {
            let offset = 0x28 + index * 8;
            progress[offset..offset + 4].copy_from_slice(&hash.to_le_bytes());
            progress[offset + 4..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        let mut caption = vec![0; CAPTION_START as usize + 64];
        for (offset, value) in [(0, 0x0102_0304), (4, VERSION), (8, CAPTION_START)] {
            caption[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (index, (hash, value)) in [
            (0x9811_a3f7_u32, 2026_u32),
            (0xdfd8_40d3, 11),
            (0xbd46_f485, 17),
            (0x23f3_d75e, 1),
            (0x2785_3bf7, 20),
            (0x25f0_3caa, 0),
        ]
        .into_iter()
        .enumerate()
        {
            let offset = 0x28 + index * 8;
            caption[offset..offset + 4].copy_from_slice(&hash.to_le_bytes());
            caption[offset + 4..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        (progress, caption)
    }

    #[test]
    fn decodes_native_stats_and_caption_without_requiring_a_fixed_file_length() -> Result<()> {
        for length in [0, 336, 4096] {
            let (progress, caption) = fixture(length);
            let hash = crate::files::sha256(&progress);
            let details = inspect(&progress, Some(&caption))?;
            assert_eq!(details.max_hearts, Some(12.0));
            assert_eq!(details.rupees, Some(888));
            assert_eq!(details.play_seconds, Some(146_558));
            assert_eq!(details.saved_at.as_deref(), Some("2026-11-17 01:20"));
            assert_eq!(details.autosave, Some(false));
            assert!(details.stamina_wheels.is_none());
            assert_eq!(crate::files::sha256(&progress), hash);
        }
        Ok(())
    }

    #[test]
    fn recognized_truncation_missing_fields_and_duplicate_fields_are_rejected() {
        let (mut progress, _) = fixture(0);
        progress.truncate(64);
        assert!(known_layout(&progress) && inspect(&progress, None).is_err());
        let (mut progress, _) = fixture(0);
        progress[0x28..0x30].fill(0);
        assert!(inspect(&progress, None).is_err());
        let (mut progress, _) = fixture(0);
        progress[0x40..0x44].copy_from_slice(&RUPEES.to_le_bytes());
        assert!(inspect(&progress, None).is_err());
        let (mut progress, _) = fixture(0);
        progress[0..4].fill(0);
        assert!(known_layout(&progress) && inspect(&progress, None).is_err());
    }

    #[test]
    fn invalid_stats_and_mismatched_or_invalid_captions_fail_closed() {
        let (mut progress, caption) = fixture(0);
        progress[0x34..0x38].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(inspect(&progress, Some(&caption)).is_err());
        let (progress, mut caption) = fixture(0);
        caption[4..8].copy_from_slice(&0x0049_e946_u32.to_le_bytes());
        assert!(inspect(&progress, Some(&caption)).is_err());
        let (progress, mut caption) = fixture(0);
        caption[0x34..0x38].copy_from_slice(&13_u32.to_le_bytes());
        assert!(inspect(&progress, Some(&caption)).is_err());
        assert!(inspect(b"unknown save format", None).is_err());
    }
}

use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;

const MAX_LIFE: u32 = 0xfbe0_1da1;
const RUPEES: u32 = 0xa779_21d7;
const MAX_STAMINA: u32 = 0xf921_2c74;
const PLAY_SECONDS: u32 = 0xe573_f564;
const POSITION: u32 = 0xc884_818d;
const MAP: u32 = 0x1d61_89da;
const YEAR: u32 = 0x9811_a3f7;
const MONTH: u32 = 0xdfd8_40d3;
const DAY: u32 = 0xbd46_f485;
const HOUR: u32 = 0x23f3_d75e;
const MINUTE: u32 = 0x2785_3bf7;
const AUTOSAVE: u32 = 0x25f0_3caa;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Details {
    pub format: String,
    pub max_hearts: Option<f32>,
    pub rupees: Option<u32>,
    pub stamina_wheels: Option<f32>,
    pub play_seconds: Option<u32>,
    pub position: Option<[f32; 3]>,
    pub map: Option<String>,
    pub saved_at: Option<String>,
    pub autosave: Option<bool>,
}

struct Table<'a> {
    bytes: &'a [u8],
    end: usize,
    fields: BTreeMap<u32, usize>,
}

fn word(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).context("Save offset overflow")?;
    let value: [u8; 4] = bytes
        .get(offset..end)
        .context("Save field is truncated")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid save word"))?;
    Ok(u32::from_le_bytes(value))
}

impl<'a> Table<'a> {
    fn new(bytes: &'a [u8], wanted: &[u32]) -> Result<Self> {
        ensure!(
            bytes.len() >= 0x28 && word(bytes, 0)? == 0x0102_0304,
            "Unrecognized TotK save header"
        );
        let end = word(bytes, 8)? as usize;
        ensure!(
            (0x28..=4 * 1024 * 1024).contains(&end)
                && end <= bytes.len()
                && (end - 0x28).is_multiple_of(8),
            "Invalid TotK save table bounds"
        );
        let mut fields = BTreeMap::new();
        for offset in (0x28..end).step_by(8) {
            let hash = word(bytes, offset)?;
            if wanted.contains(&hash) {
                ensure!(
                    fields.insert(hash, offset + 4).is_none(),
                    "Duplicate TotK metadata field"
                );
            }
        }
        Ok(Self { bytes, end, fields })
    }

    fn unsigned(&self, hash: u32) -> Result<Option<u32>> {
        self.fields
            .get(&hash)
            .map(|offset| word(self.bytes, *offset))
            .transpose()
    }

    fn float(&self, hash: u32) -> Result<Option<f32>> {
        self.unsigned(hash)?
            .map(|bits| {
                let value = f32::from_bits(bits);
                ensure!(value.is_finite(), "TotK metadata has a non-finite value");
                Ok(value)
            })
            .transpose()
    }

    fn pointed(&self, hash: u32, length: usize) -> Result<Option<&'a [u8]>> {
        let Some(offset) = self.unsigned(hash)? else {
            return Ok(None);
        };
        let offset = offset as usize;
        let end = offset
            .checked_add(length)
            .context("TotK metadata pointer overflow")?;
        ensure!(
            offset >= self.end && end <= self.bytes.len(),
            "TotK metadata pointer is outside its data region"
        );
        Ok(Some(&self.bytes[offset..end]))
    }
}

fn format_version(progress: &[u8]) -> Result<&'static str> {
    Ok(
        match (progress.len(), word(progress, 4)?, word(progress, 8)?) {
            (2_307_552, 0x0046_c3c8, 0x0003_c050) => "1.0",
            (2_307_656, 0x0047_e0f4, 0x0003_c088) => "1.1 / 1.2",
            (2_307_856, 0x0049_e946, 0x0003_c138) => "1.4",
            _ => anyhow::bail!("This TotK save format is not supported for decoded details"),
        },
    )
}

pub fn known_layout(progress: &[u8]) -> bool {
    format_version(progress).is_ok()
}

pub fn inspect(progress: &[u8], caption: Option<&[u8]>) -> Result<Details> {
    let format = format_version(progress)?;
    let table = Table::new(
        progress,
        &[MAX_LIFE, RUPEES, MAX_STAMINA, PLAY_SECONDS, POSITION, MAP],
    )?;
    ensure!(
        [MAX_LIFE, RUPEES, MAX_STAMINA, PLAY_SECONDS, POSITION, MAP]
            .iter()
            .all(|hash| table.fields.contains_key(hash)),
        "Required TotK progress metadata is missing"
    );
    let max_hearts = table
        .unsigned(MAX_LIFE)?
        .map(|life| {
            ensure!(
                life > 0 && life <= 400,
                "TotK health metadata is outside its supported range"
            );
            Ok(life as f32 / 4.0)
        })
        .transpose()?;
    let rupees = table
        .unsigned(RUPEES)?
        .map(|rupees| {
            ensure!(
                rupees <= 999_999,
                "TotK rupee metadata is outside its supported range"
            );
            Ok(rupees)
        })
        .transpose()?;
    let stamina_wheels = table
        .float(MAX_STAMINA)?
        .map(|stamina| {
            ensure!(
                (0.0..=20_000.0).contains(&stamina),
                "TotK stamina metadata is outside its supported range"
            );
            Ok(stamina / 1000.0)
        })
        .transpose()?;
    let play_seconds = table.unsigned(PLAY_SECONDS)?;
    let position = table
        .pointed(POSITION, 12)?
        .map(|bytes| {
            let position = [
                f32::from_bits(word(bytes, 0)?),
                f32::from_bits(word(bytes, 4)?),
                f32::from_bits(word(bytes, 8)?),
            ];
            ensure!(
                position
                    .iter()
                    .all(|value| value.is_finite() && value.abs() <= 1_000_000.0),
                "TotK position metadata is invalid"
            );
            Ok(position)
        })
        .transpose()?;
    let map = table
        .pointed(MAP, 64)?
        .map(|bytes| {
            let end = bytes
                .iter()
                .position(|byte| *byte == 0)
                .context("TotK map name is not terminated")?;
            let name = std::str::from_utf8(&bytes[..end]).context("TotK map name is not UTF-8")?;
            ensure!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|value| value.is_ascii_alphanumeric() || matches!(value, '_' | '-')),
                "Invalid TotK map identifier"
            );
            Ok(name.to_owned())
        })
        .transpose()?;
    let mut details = Details {
        format: format.into(),
        max_hearts,
        rupees,
        stamina_wheels,
        play_seconds,
        position,
        map,
        ..Details::default()
    };
    if let Some(caption) = caption {
        ensure!(
            word(caption, 4)? == word(progress, 4)?,
            "TotK progress and caption formats differ"
        );
        let table = Table::new(caption, &[YEAR, MONTH, DAY, HOUR, MINUTE, AUTOSAVE])?;
        if let Some(autosave) = table.unsigned(AUTOSAVE)? {
            ensure!(autosave <= 1, "Invalid TotK save kind");
            details.autosave = Some(autosave == 1);
        }
        if let (Some(year), Some(month), Some(day), Some(hour), Some(minute)) = (
            table.unsigned(YEAR)?,
            table.unsigned(MONTH)?,
            table.unsigned(DAY)?,
            table.unsigned(HOUR)?,
            table.unsigned(MINUTE)?,
        ) {
            if [year, month, day, hour, minute]
                .iter()
                .all(|value| *value == 0)
            {
                details.autosave = None;
                return Ok(details);
            }
            let year = i32::try_from(year).context("Invalid TotK save year")?;
            let month =
                time::Month::try_from(u8::try_from(month).context("Invalid TotK save month")?)?;
            let date = time::Date::from_calendar_date(
                year,
                month,
                u8::try_from(day).context("Invalid TotK save day")?,
            )?;
            let time = time::Time::from_hms(
                u8::try_from(hour).context("Invalid TotK save hour")?,
                u8::try_from(minute).context("Invalid TotK save minute")?,
                0,
            )?;
            details.saved_at = Some(format!("{date} {:02}:{:02}", time.hour(), time.minute()));
        }
    }
    Ok(details)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture() -> (Vec<u8>, Vec<u8>) {
        let mut progress = vec![0; 2_307_856];
        progress[..4].copy_from_slice(&0x0102_0304_u32.to_le_bytes());
        progress[4..8].copy_from_slice(&0x0049_e946_u32.to_le_bytes());
        progress[8..12].copy_from_slice(&0x0003_c138_u32.to_le_bytes());
        let fields = [
            (MAX_LIFE, 16),
            (RUPEES, 81),
            (MAX_STAMINA, 1000_f32.to_bits()),
            (PLAY_SECONDS, 18_293),
            (POSITION, 0x3c138),
            (MAP, 0x3c148),
        ];
        for (index, (hash, value)) in fields.iter().enumerate() {
            let offset = 0x28 + index * 8;
            progress[offset..offset + 4].copy_from_slice(&hash.to_le_bytes());
            progress[offset + 4..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        for (index, value) in [-336_f32, 239_f32, -845_f32].iter().enumerate() {
            progress[0x3c138 + index * 4..0x3c13c + index * 4]
                .copy_from_slice(&value.to_le_bytes());
        }
        progress[0x3c148..0x3c151].copy_from_slice(b"MainField");
        let mut caption = vec![0; 0x100];
        caption[..4].copy_from_slice(&0x0102_0304_u32.to_le_bytes());
        caption[4..8].copy_from_slice(&0x0049_e946_u32.to_le_bytes());
        caption[8..12].copy_from_slice(&0x58_u32.to_le_bytes());
        for (index, (hash, value)) in [
            (YEAR, 2026_u32),
            (MONTH, 11),
            (DAY, 4),
            (HOUR, 14),
            (MINUTE, 1),
            (AUTOSAVE, 1),
        ]
        .iter()
        .enumerate()
        {
            let offset = 0x28 + index * 8;
            caption[offset..offset + 4].copy_from_slice(&hash.to_le_bytes());
            caption[offset + 4..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        (progress, caption)
    }

    #[test]
    fn reads_typed_stats_and_native_caption_time_without_modifying_input() -> Result<()> {
        let (progress, caption) = fixture();
        let before = crate::files::sha256(&progress);
        let details = inspect(&progress, Some(&caption))?;
        assert_eq!(details.max_hearts, Some(4.0));
        assert_eq!(details.rupees, Some(81));
        assert_eq!(details.stamina_wheels, Some(1.0));
        assert_eq!(details.play_seconds, Some(18_293));
        assert_eq!(details.saved_at.as_deref(), Some("2026-11-04 14:01"));
        assert_eq!(details.autosave, Some(true));
        assert_eq!(details.map.as_deref(), Some("MainField"));
        assert_eq!(crate::files::sha256(&progress), before);
        Ok(())
    }

    #[test]
    fn rejects_unknown_formats_bad_pointers_and_duplicate_fields() {
        assert!(inspect(b"not a native save", None).is_err());
        let (mut progress, _) = fixture();
        progress[0x4c..0x50].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(inspect(&progress, None).is_err());
        let (mut progress, _) = fixture();
        progress[0x58..0x5c].copy_from_slice(&RUPEES.to_le_bytes());
        assert!(inspect(&progress, None).is_err());
    }

    #[test]
    fn rejects_nonfinite_values_and_impossible_caption_dates() {
        let (mut progress, caption) = fixture();
        progress[0x3c..0x40].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
        assert!(inspect(&progress, Some(&caption)).is_err());
        let (progress, mut caption) = fixture();
        caption[0x34..0x38].copy_from_slice(&13_u32.to_le_bytes());
        assert!(inspect(&progress, Some(&caption)).is_err());
    }

    #[test]
    fn caption_must_belong_to_the_same_save_format() {
        let (progress, mut caption) = fixture();
        caption[4..8].copy_from_slice(&0x0047_e0f4_u32.to_le_bytes());
        assert!(inspect(&progress, Some(&caption)).is_err());
    }

    #[test]
    fn a_recognized_header_without_its_required_fields_is_not_valid_metadata() {
        let (mut progress, _) = fixture();
        progress[0x28..0x30].fill(0);
        assert!(inspect(&progress, None).is_err());
    }

    #[test]
    fn an_uninitialized_caption_date_is_unavailable_not_an_invented_save_time() -> Result<()> {
        let (progress, mut caption) = fixture();
        for offset in [0x2c, 0x34, 0x3c, 0x44, 0x4c] {
            caption[offset..offset + 4].fill(0);
        }
        let details = inspect(&progress, Some(&caption))?;
        assert!(details.saved_at.is_none() && details.autosave.is_none());
        assert_eq!(details.rupees, Some(81));
        Ok(())
    }
}

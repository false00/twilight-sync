use anyhow::Result;
use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::{Duration, Instant},
};

const LIVE_BYTES: usize = 64 * 1024;
const MAX_LINE: usize = 8192;
const ACTIVITY_LINES: usize = 12;
const ACTIVITY_BYTES: usize = 16 * 1024;
const OMITTED_LINE: &str = "A verbose line was omitted; see the full log for complete output.";
const STALE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub enum Update {
    Stage(String),
    Output(Report),
}

impl std::fmt::Display for Update {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stage(message) => formatter.write_str(message),
            Self::Output(report) => {
                write!(formatter, "{}", report.stage)?;
                if let Some(fraction) = report.fraction {
                    write!(formatter, " - {:.1}%", fraction * 100.0)?;
                }
                if let Some(eta) = report.eta {
                    write!(
                        formatter,
                        " - about {} left in this stage",
                        duration_text(eta)
                    )?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub stage: String,
    pub fraction: Option<f64>,
    pub unmeasurable_progress: bool,
    pub completed: Option<u64>,
    pub total: Option<u64>,
    pub unit: String,
    pub eta: Option<Duration>,
    pub rate: Option<f64>,
    pub quiet_for: Duration,
    pub stalled: bool,
    pub activity: String,
}

impl Report {
    pub fn work_text(&self) -> String {
        if self.unmeasurable_progress {
            return "This phase has no reliable percentage".into();
        }
        let work = match (self.completed, self.total) {
            (Some(done), Some(total)) if self.unit == "bytes" && total >= 1024 => {
                let (scale, unit) = if total >= 1024 * 1024 * 1024 {
                    (1024.0 * 1024.0 * 1024.0, "GiB")
                } else if total >= 1024 * 1024 {
                    (1024.0 * 1024.0, "MiB")
                } else {
                    (1024.0, "KiB")
                };
                format!(
                    "{:.2} / {:.2} {unit}",
                    done as f64 / scale,
                    total as f64 / scale
                )
            }
            (Some(done), Some(total)) => format!("{done} / {total} {}", self.unit),
            _ => {
                return self
                    .fraction
                    .map(|fraction| format!("{:.1}% of this stage", fraction * 100.0))
                    .unwrap_or_else(|| "Waiting for measurable progress".into());
            }
        };
        match self.fraction {
            Some(fraction) => format!("{work} ({:.1}% of stage work)", fraction * 100.0),
            None => work,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub started: Option<Instant>,
    pub report: Report,
    finished: Option<Instant>,
}

impl State {
    pub fn start(&mut self) {
        let now = Instant::now();
        *self = Self {
            started: Some(now),
            ..Self::default()
        };
    }

    pub fn finish(&mut self) {
        self.finished = Some(Instant::now());
    }

    pub fn apply(&mut self, update: Update) -> String {
        match update {
            Update::Stage(stage) => {
                let activity = recent_activity(
                    std::iter::once(stage.as_str()).chain(self.report.activity.lines()),
                );
                self.report = Report {
                    stage,
                    activity,
                    ..Report::default()
                };
            }
            Update::Output(report) => self.report = report,
        }
        self.report.stage.clone()
    }

    pub fn elapsed(&self) -> Duration {
        self.started
            .map(|start| {
                self.finished
                    .unwrap_or_else(Instant::now)
                    .saturating_duration_since(start)
            })
            .unwrap_or_default()
    }
}

fn recent_activity<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut activity = String::new();
    for line in lines.into_iter().take(ACTIVITY_LINES) {
        let line = if line.len() > ACTIVITY_BYTES {
            OMITTED_LINE
        } else {
            line
        };
        let separator = !activity.is_empty();
        if activity.len() + usize::from(separator) + line.len() > ACTIVITY_BYTES {
            break;
        }
        if separator {
            activity.push('\n');
        }
        activity.push_str(line);
    }
    activity
}

pub fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

struct Tracker {
    report: Report,
    key: String,
    baseline: Option<(Duration, f64)>,
    samples: usize,
    advanced_at: Duration,
    output_at: Duration,
    supplied_eta: Option<(Duration, Duration)>,
    explicit_fraction: bool,
    lines: VecDeque<String>,
}

impl Tracker {
    fn new(stage: &str) -> Self {
        let mut tracker = Self {
            report: Report::default(),
            key: String::new(),
            baseline: None,
            samples: 0,
            advanced_at: Duration::ZERO,
            output_at: Duration::ZERO,
            supplied_eta: None,
            explicit_fraction: false,
            lines: VecDeque::new(),
        };
        tracker.stage(stage, stage, Duration::ZERO);
        tracker
    }

    fn activity(&mut self, line: &str) {
        if self.lines.len() == ACTIVITY_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line.into());
    }

    fn stage(&mut self, key: &str, title: &str, at: Duration) {
        if self.key == key {
            return;
        }
        self.key = key.into();
        self.report = Report {
            stage: title.chars().take(240).collect(),
            // Triforce reuses weighted progress ranges for different shader passes.
            unmeasurable_progress: key == "shaders" || key.starts_with("shaders/"),
            ..Report::default()
        };
        self.baseline = None;
        self.samples = 0;
        self.supplied_eta = None;
        self.explicit_fraction = false;
        self.advanced_at = at;
        self.activity(title);
    }

    fn fraction(&mut self, fraction: f64, at: Duration) {
        if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
            return;
        }
        if self
            .report
            .fraction
            .is_some_and(|previous| fraction < previous)
        {
            self.baseline = None;
            self.samples = 0;
            self.supplied_eta = None;
        }
        if self.report.fraction != Some(fraction) {
            self.advanced_at = at;
            self.samples += 1;
        }
        self.baseline.get_or_insert((at, fraction));
        self.report.fraction = Some(fraction);
    }

    fn count(&mut self, done: u64, total: u64, unit: &str, at: Duration) {
        if total == 0 || done > total {
            return;
        }
        if self.report.total != Some(total) || self.report.unit != unit {
            self.baseline = None;
            self.samples = 0;
            self.supplied_eta = None;
            self.explicit_fraction = false;
        }
        self.report.completed = Some(done);
        self.report.total = Some(total);
        self.report.unit = unit.chars().take(32).collect();
        if !self.explicit_fraction {
            self.fraction(done as f64 / total as f64, at);
        }
    }

    fn reported_fraction(&mut self, fraction: f64, at: Duration) {
        if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
            return;
        }
        if !self.explicit_fraction {
            self.baseline = None;
            self.samples = 0;
            self.report.fraction = None;
        }
        self.explicit_fraction = true;
        self.fraction(fraction, at);
    }

    fn feed(&mut self, line: &str, at: Duration) {
        self.output_at = at;
        if let Some(stage) = line.strip_prefix("@@stage ")
            && let Some((key, title)) = stage.split_once(' ')
        {
            self.stage(key, title, at);
            return;
        }
        if self.report.unmeasurable_progress
            && let Some(message) = line.strip_prefix("[hos host] vk: ")
        {
            if self.key == "shaders"
                && ((message.starts_with("prepared ") && message.contains(" shader programs ("))
                    || message.starts_with("the shaders are prepared, but their driver's part "))
            {
                self.stage(
                    "shaders/pipelines",
                    "Preparing Vulkan pipeline libraries",
                    at,
                );
            } else if (message.starts_with("warmed ") && message.contains(" stage libraries of "))
                || message.starts_with("precompile: ")
            {
                self.stage("shaders/finalizing", "Finalizing Vulkan pipeline cache", at);
            }
            self.activity(line);
            return;
        }
        if let Some(value) = line.strip_prefix("@@progress ") {
            if let Ok(fraction) = value.trim().parse::<f64>()
                && fraction.is_finite()
                && (0.0..=1.0).contains(&fraction)
            {
                if !self.report.unmeasurable_progress {
                    self.reported_fraction(fraction, at);
                }
            } else {
                self.activity(line);
            }
            return;
        }
        if let Some(value) = line.strip_prefix("@@eta ") {
            if let Ok(seconds) = value.trim().parse::<f64>()
                && seconds.is_finite()
                && (0.0..=30.0 * 86400.0).contains(&seconds)
            {
                self.supplied_eta = Some((at, Duration::from_secs_f64(seconds)));
            } else {
                self.activity(line);
            }
            return;
        }
        if let Some(value) = line.strip_prefix("@@plan ") {
            let values: std::result::Result<Vec<_>, _> =
                value.split_whitespace().map(str::parse::<u64>).collect();
            if let Ok(values) = values
                && let [changed, total] = values.as_slice()
                && changed <= total
            {
                self.count(total - changed, *total, "modules", at);
                self.activity(&format!(
                    "Build plan: {changed} modules to rebuild; {} reusable.",
                    total - changed
                ));
            } else {
                self.activity(line);
            }
            return;
        }
        if let Some(value) = line.strip_prefix("@@file ") {
            if matches!(value.split_whitespace().last(), Some("new" | "again"))
                && self.report.unit == "modules"
                && let (Some(done), Some(total)) = (self.report.completed, self.report.total)
            {
                self.count(done.saturating_add(1).min(total), total, "modules", at);
            }
            self.activity(&format!("Module: {value}"));
            return;
        }
        if line.starts_with('{')
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
        {
            let kind = value["type"].as_str().unwrap_or_default();
            let step = value["step"].as_str().unwrap_or_default();
            if kind == "step" && value["state"] == "started" {
                self.stage(step, &step.replace('_', " "), at);
                return;
            }
            if kind == "progress" {
                let phase = value["phase"].as_str().unwrap_or_default();
                let key = format!("{step}/{phase}");
                let title = [step, phase]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .map(|part| part.replace('_', " "))
                    .collect::<Vec<_>>()
                    .join(" / ");
                if !title.is_empty() {
                    self.stage(&key, &title, at);
                }
                if let (Some(done), Some(total)) = (value["done"].as_u64(), value["total"].as_u64())
                {
                    self.count(done, total, value["unit"].as_str().unwrap_or("items"), at);
                }
                return;
            }
        }
        if let Some(bracket) = line
            .strip_prefix('[')
            .and_then(|line| line.split_once(']').map(|(value, _)| value))
        {
            if let Some((done, total)) = bracket.split_once('/') {
                if let (Ok(done), Ok(total)) = (done.trim().parse(), total.trim().parse()) {
                    self.count(done, total, "build steps", at);
                }
            } else if let Some(percent) = bracket.trim().strip_suffix('%')
                && let Ok(percent) = percent.trim().parse::<f64>()
            {
                self.reported_fraction(percent / 100.0, at);
            }
        }
        self.activity(line);
    }

    fn snapshot(&self, at: Duration) -> Report {
        let mut report = self.report.clone();
        report.quiet_for = at.saturating_sub(self.output_at);
        report.stalled = report.fraction.is_some_and(|fraction| fraction < 1.0)
            && at.saturating_sub(self.advanced_at) >= STALE;
        report.activity = recent_activity(self.lines.iter().rev().map(String::as_str));
        if let Some((recorded, eta)) = self.supplied_eta
            && at.saturating_sub(recorded) < STALE
        {
            report.eta = eta.checked_sub(at.saturating_sub(recorded));
            if report.eta.is_some() {
                report.stalled = false;
            }
        }
        if report.eta.is_none()
            && !report.stalled
            && let (Some((since, initial)), Some(fraction)) = (self.baseline, report.fraction)
        {
            let seconds = at.saturating_sub(since).as_secs_f64();
            let advance = fraction - initial;
            if seconds >= 5.0 && advance > 0.0 && self.samples >= 2 {
                let remaining = (1.0 - fraction) * seconds / advance;
                if remaining.is_finite() && (0.0..=30.0 * 86400.0).contains(&remaining) {
                    report.eta = Some(Duration::from_secs_f64(remaining));
                    if !self.explicit_fraction {
                        report.rate = report.total.map(|total| total as f64 * advance / seconds);
                    }
                }
            }
        }
        report
    }
}

pub(super) struct Reader {
    file: File,
    pending: Vec<u8>,
    discard_line: bool,
    tracker: Tracker,
}

impl Reader {
    pub fn new(path: &Path, stage: &str) -> Result<Self> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            pending: Vec::new(),
            discard_line: false,
            tracker: Tracker::new(stage),
        })
    }

    pub fn poll(&mut self, at: Duration) -> Result<Report> {
        let length = self.file.metadata()?.len();
        let position = self.file.stream_position()?;
        if length < position || length.saturating_sub(position) > LIVE_BYTES as u64 {
            self.file
                .seek(SeekFrom::Start(length.saturating_sub(LIVE_BYTES as u64)))?;
            self.pending.clear();
            self.discard_line = length > LIVE_BYTES as u64;
            self.tracker = Tracker::new("Processing latest build output");
            self.tracker.activity(
                "Older verbose output omitted from this live view; the full log is retained.",
            );
        }
        let mut bytes = vec![0; LIVE_BYTES];
        let count = self.file.read(&mut bytes)?;
        for &byte in &bytes[..count] {
            if byte == b'\n' || byte == b'\r' {
                self.flush_line(at);
            } else if !self.discard_line {
                if self.pending.len() < MAX_LINE {
                    self.pending.push(byte);
                } else {
                    self.pending.clear();
                    self.discard_line = true;
                    self.tracker.activity(OMITTED_LINE);
                }
            }
        }
        Ok(self.tracker.snapshot(at))
    }

    fn flush_line(&mut self, at: Duration) {
        if !self.discard_line && !self.pending.is_empty() {
            let mut line = String::new();
            let mut escape = false;
            for character in String::from_utf8_lossy(&self.pending).chars() {
                if escape {
                    if character != '[' && ('@'..='~').contains(&character) {
                        escape = false;
                    }
                } else if character == '\u{1b}' {
                    escape = true;
                } else if !character.is_control() || character == '\t' {
                    line.push(character);
                }
            }
            self.tracker.feed(line.trim(), at);
        }
        self.pending.clear();
        self.discard_line = false;
    }

    pub fn finish(&mut self, at: Duration) -> Result<Report> {
        self.poll(at)?;
        self.flush_line(at);
        Ok(self.tracker.snapshot(at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::OpenOptions, io::Write};

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn oot_reuses_cached_module_counts_and_prefers_builder_progress_and_eta() {
        let mut tracker = Tracker::new("Building OOT");
        tracker.feed("@@stage compile Compiling", seconds(0));
        tracker.feed("@@plan 142 500", seconds(0));
        tracker.feed("@@file code_001.c 0.0 cached", seconds(1));
        assert_eq!(tracker.snapshot(seconds(1)).completed, Some(358));
        tracker.feed("@@file code_052.c 108.5 new", seconds(2));
        tracker.feed("@@progress 0.123", seconds(2));
        tracker.feed("@@file code_009.c 110.4 again", seconds(3));
        tracker.feed("@@eta 301", seconds(3));
        let report = tracker.snapshot(seconds(8));
        assert_eq!((report.completed, report.total), (Some(360), Some(500)));
        assert_eq!(report.fraction, Some(0.123));
        assert_eq!(report.eta, Some(seconds(296)));
        assert!(report.rate.is_none());
        tracker.feed("@@stage shaders Preparing shaders", seconds(10));
        assert!(tracker.snapshot(seconds(10)).total.is_none());
        assert!(tracker.snapshot(seconds(10)).eta.is_none());
    }

    #[test]
    fn oot_shader_passes_report_activity_without_a_misleading_percentage_or_eta() {
        let mut tracker = Tracker::new("Building OOT");
        tracker.feed("@@stage shaders Preparing shaders", seconds(0));
        tracker.feed("@@progress 0.001", seconds(1));
        tracker.feed("@@progress 0.699", seconds(1510));
        let report = tracker.snapshot(seconds(1510));
        assert_eq!(report.stage, "Preparing shaders");
        assert!(report.unmeasurable_progress);
        assert!(report.fraction.is_none());
        assert!(report.eta.is_none());

        tracker.feed("@@progress 0.000", seconds(1511));
        tracker.feed(
            "[hos host] vk: prepared 38809 shader programs (46579 new, 88 failed) in 1511.0 s",
            seconds(1511),
        );
        tracker.feed("@@progress 0.001", seconds(1512));
        tracker.feed("@@progress 0.067", seconds(5600));
        let report = tracker.snapshot(seconds(5605));
        assert_eq!(report.stage, "Preparing Vulkan pipeline libraries");
        assert_eq!(report.quiet_for, seconds(5));
        assert!(report.fraction.is_none());
        assert!(report.eta.is_none());
        tracker.feed(
            "[hos host] vk: warmed 85048 stage libraries of 42524 kept shader program variants in 13377.4 s",
            seconds(14888),
        );
        tracker.feed("@@progress 0.900", seconds(14902));
        let report = tracker.snapshot(seconds(14902));
        assert_eq!(report.stage, "Finalizing Vulkan pipeline cache");
        assert!(report.fraction.is_none());
        assert!(report.eta.is_none());
    }

    #[test]
    fn oot_rewarming_for_a_different_driver_does_not_require_a_translation_summary() {
        let mut tracker = Tracker::new("Building OOT");
        tracker.feed("@@stage shaders Preparing shaders", seconds(0));
        tracker.feed(
            "[hos host] vk: the shaders are prepared, but their driver's part was made for another driver or other settings: made again",
            seconds(1),
        );
        assert_eq!(
            tracker.snapshot(seconds(1)).stage,
            "Preparing Vulkan pipeline libraries"
        );
    }

    #[test]
    fn totk_bytes_measure_throughput_and_reset_for_each_phase() {
        let mut tracker = Tracker::new("Building TotK");
        tracker.feed(
            r#"{"type":"step","step":"extract","state":"started"}"#,
            seconds(0),
        );
        tracker.feed(r#"{"type":"progress","step":"extract","phase":"hash_base","done":0,"total":1000,"unit":"bytes"}"#, seconds(0));
        tracker.feed(r#"{"type":"progress","step":"extract","phase":"hash_base","done":250,"total":1000,"unit":"bytes"}"#, seconds(10));
        let report = tracker.snapshot(seconds(10));
        assert_eq!(report.eta, Some(seconds(30)));
        assert_eq!(report.rate, Some(25.0));
        tracker.feed(
            r#"{"type":"progress","step":"extract","phase":"decompress"}"#,
            seconds(11),
        );
        let report = tracker.snapshot(seconds(11));
        assert_eq!(report.stage, "extract / decompress");
        assert!(report.eta.is_none());
        assert!(report.fraction.is_none());
        tracker.feed(r#"{"type":"progress","step":"extract","phase":"decompress","done":0,"total":1,"unit":"files"}"#, seconds(12));
        tracker.feed(r#"{"type":"progress","step":"extract","phase":"decompress","done":1,"total":1,"unit":"files"}"#, seconds(18));
        assert_eq!(tracker.snapshot(seconds(18)).eta, Some(Duration::ZERO));
    }

    #[test]
    fn invalid_counters_stalls_and_resets_never_invent_an_eta() {
        let mut tracker = Tracker::new("Compiling");
        for line in [
            "@@progress NaN",
            "@@progress -0.1",
            "@@progress 1.1",
            "@@plan 1 bad 10",
            "@@plan 2 1",
            "[1/0] invalid",
            "[11/10] invalid",
            "@@eta inf",
        ] {
            tracker.feed(line, seconds(0));
        }
        assert!(tracker.snapshot(seconds(0)).fraction.is_none());
        assert!(tracker.snapshot(seconds(0)).eta.is_none());
        tracker.feed("[1/10] Compiling", seconds(0));
        tracker.feed("[2/10] Compiling", seconds(10));
        assert_eq!(tracker.snapshot(seconds(10)).eta, Some(seconds(80)));
        let report = tracker.snapshot(seconds(71));
        assert!(report.stalled);
        assert!(report.eta.is_none());
        tracker.feed("[1/10] Restarted", seconds(72));
        assert!(tracker.snapshot(seconds(72)).eta.is_none());
        tracker.feed("[2/20] New total", seconds(80));
        assert!(tracker.snapshot(seconds(80)).eta.is_none());
        tracker.feed("@@eta 1", seconds(81));
        assert!(tracker.snapshot(seconds(83)).eta.is_none());
    }

    #[test]
    fn cmake_percentages_are_measured_without_fake_item_counts() {
        let mut tracker = Tracker::new("Compiling");
        tracker.feed("[ 10%] Building CXX object", seconds(0));
        tracker.feed("[ 20%] Building CXX object", seconds(10));
        let report = tracker.snapshot(seconds(10));
        assert_eq!(report.fraction, Some(0.2));
        assert_eq!(report.eta, Some(seconds(80)));
        assert!(report.total.is_none());
        assert!(report.rate.is_none());
    }

    #[test]
    fn live_reader_preserves_partial_utf8_cr_updates_and_unterminated_final_lines() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("build.log");
        File::create(&path)?;
        let mut reader = Reader::new(&path, "Compiling")?;
        let mut writer = OpenOptions::new().append(true).open(&path)?;
        writer.write_all(b"\x1b[32m[1/10] Compiling\x1b[0m\rcaf\xc3")?;
        assert_eq!(reader.poll(seconds(0))?.completed, Some(1));
        writer.write_all(b"\xa9\n@@progress 0.")?;
        assert!(reader.poll(seconds(1))?.activity.contains("caf\u{e9}"));
        writer.write_all(b"5\rfinal line")?;
        assert_eq!(reader.poll(seconds(2))?.fraction, Some(0.5));
        assert!(reader.finish(seconds(3))?.activity.contains("final line"));
        Ok(())
    }

    #[test]
    fn live_reader_preserves_complete_long_lines_within_the_activity_budget() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("build.log");
        File::create(&path)?;
        let mut reader = Reader::new(&path, "Compiling")?;
        let mut writer = OpenOptions::new().append(true).open(&path)?;
        let lines: Vec<_> = (0..3)
            .map(|index| format!("Build output {} completion-{index}", "x".repeat(6000)))
            .collect();
        for line in &lines {
            writeln!(writer, "{line}")?;
        }
        let report = reader.poll(seconds(1))?;
        assert_eq!(report.activity, format!("{}\n{}", lines[2], lines[1]));
        Ok(())
    }

    #[test]
    fn live_reader_bounds_verbose_output_and_reports_omissions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("build.log");
        File::create(&path)?;
        let mut reader = Reader::new(&path, "Compiling")?;
        let mut writer = OpenOptions::new().append(true).open(&path)?;
        writer.write_all(&vec![b'x'; MAX_LINE + 100])?;
        writer.write_all(b"\n@@progress 0.25\n")?;
        let report = reader.poll(seconds(1))?;
        assert_eq!(report.fraction, Some(0.25));
        assert!(report.activity.contains("verbose line was omitted"));
        writer.write_all(&vec![b'x'; LIVE_BYTES * 3])?;
        writer.write_all(b"\n@@progress 0.5\n")?;
        let report = reader.poll(seconds(2))?;
        assert_eq!(report.fraction, Some(0.5));
        assert!(report.activity.contains("Older verbose output omitted"));
        assert_eq!(report.stage, "Processing latest build output");
        assert!(report.activity.len() <= ACTIVITY_BYTES);
        assert!(reader.pending.len() <= MAX_LINE);
        Ok(())
    }
}

//! Timelapse selection, naming and the on-disk job queue in `DATA_DIR/out/`.
//!
//! A `TimelapseSpec` says which frames to encode; its name encodes every input, so the same
//! selection always maps to the same files. Status is derived from which files exist:
//! `{name}.mp4` done, `{name}.err` failed, `{name}.job` plus `{name}.mp4.part` running,
//! `{name}.job` alone queued.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, TimeDelta, Timelike, Utc};

use crate::store::{Frame, MINUTE_FORMAT, Store, parse_minute, part_path, remove_if_exists};

/// Extension of a queued job: an ffconcat list of frame paths.
pub const JOB: &str = "job";
/// Extension of a finished timelapse.
pub const MP4: &str = "mp4";
/// Extension of a failure report.
pub const ERR: &str = "err";

/// The newest minute whose frame should already be on disk. The capture ticker is not
/// aligned to the minute, so the current minute's frame may still be missing.
pub fn last_complete_minute(now: DateTime<Utc>) -> DateTime<Utc> {
    let minute = now
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .expect("second 0 is always valid");
    minute - TimeDelta::minutes(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelapseSpec {
    /// First minute, inclusive.
    pub from: DateTime<Utc>,
    /// Last minute, inclusive.
    pub to: DateTime<Utc>,
    /// Keep every Nth frame after the hour filter.
    pub step: u32,
    /// UTC hours `(start, end)` to keep, start inclusive, end exclusive. Wraps when
    /// `start > end`.
    pub hours: Option<(u32, u32)>,
    /// x264 `-crf` for this timelapse; `None` uses the configured default.
    pub crf: Option<u8>,
    /// `hqdn3d` luma strength for this timelapse; `None` uses the configured default.
    pub denoise: Option<u32>,
}

impl TimelapseSpec {
    /// `{from}_{to}_s{step}[_h{start}-{end}][_c{crf}][_d{denoise}]`, e.g.
    /// `20261011-0000_20261011-2359_s1_h06-20_c22`.
    pub fn name(&self) -> String {
        let mut name = format!(
            "{}_{}_s{}",
            self.from.format(MINUTE_FORMAT),
            self.to.format(MINUTE_FORMAT),
            self.step
        );
        if let Some((start, end)) = self.hours {
            name.push_str(&format!("_h{start:02}-{end:02}"));
        }
        if let Some(crf) = self.crf {
            name.push_str(&format!("_c{crf}"));
        }
        if let Some(denoise) = self.denoise {
            name.push_str(&format!("_d{denoise}"));
        }
        name
    }

    /// Exact inverse of `name()`. Rejects anything `name()` would not produce and any spec
    /// that is invalid regardless of the current time.
    pub fn parse_name(name: &str) -> Option<Self> {
        let mut parts = name.split('_').peekable();
        let from = parse_minute(parts.next()?)?;
        let to = parse_minute(parts.next()?)?;
        let step = parts.next()?.strip_prefix('s')?.parse().ok()?;
        let hours = match parts.next_if(|p| p.starts_with('h')) {
            None => None,
            Some(h) => {
                let (start, end) = h[1..].split_once('-')?;
                Some((start.parse().ok()?, end.parse().ok()?))
            }
        };
        let crf = parts
            .next_if(|p| p.starts_with('c'))
            .map(|c| c[1..].parse())
            .transpose()
            .ok()?;
        let denoise = parts
            .next_if(|p| p.starts_with('d'))
            .map(|d| d[1..].parse())
            .transpose()
            .ok()?;
        if parts.next().is_some() {
            return None;
        }
        let spec = Self {
            from,
            to,
            step,
            hours,
            crf,
            denoise,
        };
        // The round trip rejects non-canonical spellings such as `s01` or `s+1`.
        (spec.shape_error().is_none() && spec.name() == name).then_some(spec)
    }

    /// Check the spec against `now`. The message is shown to the user as is.
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), String> {
        if let Some(e) = self.shape_error() {
            return Err(e.to_owned());
        }
        if self.to > last_complete_minute(now) {
            return Err("to must be before the current minute".to_owned());
        }
        Ok(())
    }

    /// Frames this spec keeps, from `frames` sorted oldest first.
    pub fn select(&self, frames: Vec<Frame>) -> Vec<Frame> {
        frames
            .into_iter()
            .filter(|f| self.keeps_hour(f.time.hour()))
            .step_by(self.step as usize)
            .collect()
    }

    /// Problems that do not depend on the current time.
    fn shape_error(&self) -> Option<&'static str> {
        if self.step == 0 {
            return Some("step must be at least 1");
        }
        if let Some((start, end)) = self.hours {
            if start > 23 || end > 23 {
                return Some("hours must be between 0 and 23");
            }
            if start == end {
                return Some("hour range must not be empty");
            }
        }
        if self.crf.is_some_and(|crf| crf > 51) {
            return Some("crf must be between 0 and 51");
        }
        if self.denoise.is_some_and(|denoise| denoise > 30) {
            return Some("denoise must be between 0 and 30");
        }
        if self.from > self.to {
            return Some("from must not be after to");
        }
        None
    }

    fn keeps_hour(&self, hour: u32) -> bool {
        match self.hours {
            None => true,
            Some((start, end)) if start < end => start <= hour && hour < end,
            Some((start, end)) => hour >= start || hour < end,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Queued,
    Running,
    Done,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Submitted {
    /// The MP4 already exists.
    Done,
    /// A job exists, either written now or earlier.
    Queued,
    /// The spec selects no frames; nothing was written.
    NoFrames,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub spec: TimelapseSpec,
    pub status: Status,
    /// Size of the MP4 in bytes; 0 unless done.
    pub size: u64,
    /// Newest modification time among the entry's files.
    pub modified: SystemTime,
    /// The `.err` contents when failed.
    pub error: Option<String>,
}

/// Queue `spec` unless its MP4 exists or it is already queued. `spec` must be validated.
pub fn submit(store: &Store, spec: &TimelapseSpec) -> io::Result<Submitted> {
    let name = spec.name();
    let mp4 = store.out_file(&name, MP4);
    if mp4.try_exists()? {
        // Retention counts from the last request, so a video asked for again is kept.
        fs::File::options()
            .write(true)
            .open(&mp4)?
            .set_modified(SystemTime::now())?;
        return Ok(Submitted::Done);
    }
    if store.out_file(&name, JOB).try_exists()? {
        return Ok(Submitted::Queued);
    }
    let frames = spec.select(store.frames_between(spec.from, spec.to)?);
    if frames.is_empty() {
        return Ok(Submitted::NoFrames);
    }
    fs::create_dir_all(store.out_dir())?;
    // Resubmitting retries a failure.
    remove_if_exists(&store.out_file(&name, ERR))?;
    write_job(&store.out_file(&name, JOB), &ffconcat(store, &frames)?)
}

/// Write `<job>.part`, then rename it to `job`. The part file is created exclusively: if it
/// already exists, a concurrent submit of the same selection is writing it, so this one is
/// already queued. Startup cleanup removes a part left behind by a crash.
fn write_job(job: &Path, contents: &str) -> io::Result<Submitted> {
    let part = part_path(job);
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(Submitted::Queued),
        Err(e) => return Err(e),
    };
    let written = file
        .write_all(contents.as_bytes())
        .and_then(|()| fs::rename(&part, job));
    if written.is_err() {
        let _ = fs::remove_file(&part);
    }
    written.map(|()| Submitted::Queued)
}

/// Every timelapse in `out/`, newest first.
pub fn list(store: &Store) -> io::Result<Vec<Entry>> {
    let mut by_name: BTreeMap<String, (TimelapseSpec, Files)> = BTreeMap::new();
    for file in scan(store)? {
        let (_, files) = by_name
            .entry(file.name)
            .or_insert_with(|| (file.spec, Files::default()));
        match file.kind {
            Kind::Job => files.job = true,
            Kind::Mp4Part => files.running = true,
            Kind::Mp4 => files.size = Some(file.size),
            Kind::Err => files.failed = true,
        }
        files.modified = files.modified.max(Some(file.modified));
    }
    let mut entries = Vec::new();
    for (name, (spec, files)) in by_name {
        let Some(status) = files.status() else {
            continue;
        };
        let error = match status {
            Status::Failed => read_if_exists(&store.out_file(&name, ERR))?,
            _ => None,
        };
        entries.push(Entry {
            name,
            spec,
            status,
            size: files.size.unwrap_or(0),
            modified: files.modified.unwrap_or(UNIX_EPOCH),
            error,
        });
    }
    entries.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    Ok(entries)
}

/// Names of queued and running jobs, oldest `.job` first, ties broken by name.
pub fn pending_jobs(store: &Store) -> io::Result<Vec<String>> {
    let mut jobs: Vec<_> = scan(store)?
        .into_iter()
        .filter(|f| f.kind == Kind::Job)
        .map(|f| (f.modified, f.name))
        .collect();
    jobs.sort();
    Ok(jobs.into_iter().map(|(_, name)| name).collect())
}

/// Delete MP4 and `.err` files last modified before `cutoff`. Jobs are never pruned.
/// Returns how many files were removed.
pub fn prune(store: &Store, cutoff: SystemTime) -> io::Result<usize> {
    let mut removed = 0;
    for file in scan(store)? {
        if matches!(file.kind, Kind::Mp4 | Kind::Err) && file.modified < cutoff {
            remove_if_exists(&file.path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// An ffconcat list of absolute frame paths. Absolute because ffmpeg resolves relative
/// entries against the list file's directory.
fn ffconcat(store: &Store, frames: &[Frame]) -> io::Result<String> {
    let mut out = String::from("ffconcat version 1.0\n");
    for frame in frames {
        let path = std::path::absolute(store.frame_path(frame.time))?;
        // Inside single quotes nothing is special, so a quote closes the string, adds an
        // escaped quote and reopens it.
        let quoted = path.display().to_string().replace('\'', r"'\''");
        out.push_str(&format!("file '{quoted}'\n"));
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Job,
    Mp4Part,
    Mp4,
    Err,
}

impl Kind {
    /// Split `{name}.{ext}` into the name and the kind of file.
    fn split(file_name: &str) -> Option<(&str, Self)> {
        [
            (".job", Self::Job),
            (".mp4.part", Self::Mp4Part),
            (".mp4", Self::Mp4),
            (".err", Self::Err),
        ]
        .into_iter()
        .find_map(|(suffix, kind)| Some((file_name.strip_suffix(suffix)?, kind)))
    }
}

struct OutFile {
    name: String,
    spec: TimelapseSpec,
    kind: Kind,
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

#[derive(Default)]
struct Files {
    job: bool,
    running: bool,
    failed: bool,
    size: Option<u64>,
    modified: Option<SystemTime>,
}

impl Files {
    fn status(&self) -> Option<Status> {
        if self.size.is_some() {
            Some(Status::Done)
        } else if self.failed {
            Some(Status::Failed)
        } else if self.job && self.running {
            Some(Status::Running)
        } else if self.job {
            Some(Status::Queued)
        } else {
            None
        }
    }
}

/// Timelapse files in `out/`. Other files are ignored, as are files that vanish while being
/// read (the worker deletes jobs and prunes outputs concurrently).
fn scan(store: &Store) -> io::Result<Vec<OutFile>> {
    let entries = match fs::read_dir(store.out_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some((name, kind)) = file_name.to_str().and_then(Kind::split) else {
            continue;
        };
        let Some(spec) = TimelapseSpec::parse_name(name) else {
            continue;
        };
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        files.push(OutFile {
            name: name.to_owned(),
            spec,
            kind,
            path: entry.path(),
            size: meta.len(),
            modified: meta.modified()?,
        });
    }
    Ok(files)
}

fn read_if_exists(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use pretty_assertions::assert_eq;

    use super::*;

    fn at(d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, d, h, m, 0).unwrap()
    }

    fn spec(step: u32, hours: Option<(u32, u32)>) -> TimelapseSpec {
        TimelapseSpec {
            from: at(11, 0, 0),
            to: at(11, 23, 59),
            step,
            hours,
            crf: None,
            denoise: None,
        }
    }

    fn frame(d: u32, h: u32, m: u32) -> Frame {
        let time = at(d, h, m);
        Frame {
            name: time.format("%Y%m%d-%H%M.jpg").to_string(),
            time,
        }
    }

    fn hours_of(frames: Vec<Frame>) -> Vec<u32> {
        frames.into_iter().map(|f| f.time.hour()).collect()
    }

    #[test]
    fn name_round_trips() {
        for (s, name) in [
            (spec(1, None), "20261011-0000_20261011-2359_s1"),
            (
                spec(12, Some((6, 20))),
                "20261011-0000_20261011-2359_s12_h06-20",
            ),
            (
                spec(1, Some((20, 6))),
                "20261011-0000_20261011-2359_s1_h20-06",
            ),
            (
                TimelapseSpec {
                    crf: Some(22),
                    ..spec(1, None)
                },
                "20261011-0000_20261011-2359_s1_c22",
            ),
            (
                TimelapseSpec {
                    denoise: Some(0),
                    ..spec(1, Some((20, 6)))
                },
                "20261011-0000_20261011-2359_s1_h20-06_d0",
            ),
            (
                TimelapseSpec {
                    crf: Some(18),
                    denoise: Some(16),
                    ..spec(2, Some((20, 6)))
                },
                "20261011-0000_20261011-2359_s2_h20-06_c18_d16",
            ),
        ] {
            assert_eq!(s.name(), name);
            assert_eq!(TimelapseSpec::parse_name(name), Some(s), "{name}");
        }
    }

    #[test]
    fn parse_name_rejects_junk() {
        for name in [
            "",
            "notes",
            "20261011-0000_20261011-2359",
            "20261011-0000_20261011-2359_s0",
            "20261011-0000_20261011-2359_s01",
            "20261011-0000_20261011-2359_s+1",
            "20261011-0000_20261011-2359_s1_h6-20",
            "20261011-0000_20261011-2359_s1_h06-24",
            "20261011-0000_20261011-2359_s1_h06-06",
            "20261011-0000_20261011-2359_s1_h06-20_x",
            "20261012-0000_20261011-2359_s1",
            "20261011-0000_20261011-2359_s1.mp4",
            "20261011-0000_20261011-2359_s1_c52",
            "20261011-0000_20261011-2359_s1_c022",
            "20261011-0000_20261011-2359_s1_c",
            "20261011-0000_20261011-2359_s1_d31",
            "20261011-0000_20261011-2359_s1_d8_c22",
            "20261011-0000_20261011-2359_s1_c22_h06-20",
            "20261011-0000_20261011-2359_s1_c22_c22",
        ] {
            assert_eq!(TimelapseSpec::parse_name(name), None, "{name}");
        }
    }

    #[test]
    fn validate_cases() {
        let now = at(12, 0, 0);
        assert_eq!(spec(1, None).validate(now), Ok(()));
        assert_eq!(spec(1, Some((20, 6))).validate(now), Ok(()));
        let err = |s: TimelapseSpec| s.validate(now).unwrap_err();
        assert_eq!(err(spec(0, None)), "step must be at least 1");
        assert_eq!(
            err(spec(1, Some((6, 24)))),
            "hours must be between 0 and 23"
        );
        assert_eq!(err(spec(1, Some((6, 6)))), "hour range must not be empty");
        assert_eq!(
            err(TimelapseSpec {
                crf: Some(52),
                ..spec(1, None)
            }),
            "crf must be between 0 and 51"
        );
        assert_eq!(
            err(TimelapseSpec {
                denoise: Some(31),
                ..spec(1, None)
            }),
            "denoise must be between 0 and 30"
        );
        let mut backwards = spec(1, None);
        backwards.from = at(12, 0, 0);
        assert_eq!(err(backwards), "from must not be after to");
        // The current minute's frame may not have been captured yet.
        let mid_last_minute = at(11, 23, 59) + chrono::TimeDelta::seconds(30);
        assert_eq!(
            spec(1, None).validate(mid_last_minute).unwrap_err(),
            "to must be before the current minute"
        );
        assert_eq!(
            spec(1, None).validate(at(11, 23, 58)).unwrap_err(),
            "to must be before the current minute"
        );
    }

    #[test]
    fn last_complete_minute_is_the_minute_before_now() {
        let now = at(11, 7, 5) + chrono::TimeDelta::seconds(42);
        assert_eq!(last_complete_minute(now), at(11, 7, 4));
    }

    #[test]
    fn select_applies_hour_filter_then_step() {
        let frames: Vec<_> = (0..24).map(|h| frame(11, h, 0)).collect();
        assert_eq!(hours_of(spec(1, None).select(frames.clone())).len(), 24);
        assert_eq!(
            hours_of(spec(1, Some((6, 9))).select(frames.clone())),
            [6, 7, 8]
        );
        assert_eq!(
            hours_of(spec(1, Some((22, 2))).select(frames.clone())),
            [0, 1, 22, 23]
        );
        assert_eq!(
            hours_of(spec(2, Some((6, 11))).select(frames.clone())),
            [6, 8, 10]
        );
        assert_eq!(hours_of(spec(10, None).select(frames)), [0, 10, 20]);
    }

    #[test]
    fn wrapping_hours_span_midnight_across_days() {
        let frames = vec![
            frame(11, 21, 0),
            frame(11, 23, 0),
            frame(12, 1, 0),
            frame(12, 2, 0),
            frame(12, 12, 0),
        ];
        let kept: Vec<_> = spec(1, Some((22, 2)))
            .select(frames)
            .into_iter()
            .map(|f| f.time)
            .collect();
        assert_eq!(kept, [at(11, 23, 0), at(12, 1, 0)]);
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path());
            Self { _dir: dir, store }
        }

        fn add_frames(&self, times: &[DateTime<Utc>]) {
            for t in times {
                let path = self.store.frame_path(*t);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"jpeg").unwrap();
            }
        }

        /// Write `out/{name}.{ext}` with modification time `mtime(mtime_secs)`.
        fn put(&self, name: &str, ext: &str, contents: &str, mtime_secs: u64) -> PathBuf {
            fs::create_dir_all(self.store.out_dir()).unwrap();
            let path = self.store.out_file(name, ext);
            fs::write(&path, contents).unwrap();
            let file = fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(mtime(mtime_secs)).unwrap();
            path
        }

        fn statuses(&self) -> Vec<(String, Status)> {
            list(&self.store)
                .unwrap()
                .into_iter()
                .map(|e| (e.name, e.status))
                .collect()
        }
    }

    fn mtime(secs: u64) -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000 + secs)
    }

    fn named(d: u32) -> String {
        TimelapseSpec {
            from: at(d, 0, 0),
            to: at(d, 23, 59),
            step: 1,
            hours: None,
            crf: None,
            denoise: None,
        }
        .name()
    }

    #[test]
    fn submit_writes_absolute_ffconcat_job() {
        let f = Fixture::new();
        f.add_frames(&[at(11, 0, 0), at(11, 0, 1), at(11, 0, 2), at(12, 0, 0)]);

        assert_eq!(submit(&f.store, &spec(2, None)).unwrap(), Submitted::Queued);

        let job = fs::read_to_string(f.store.out_file(&spec(2, None).name(), JOB)).unwrap();
        let frames = std::path::absolute(f.store.frames_dir()).unwrap();
        let frames = frames.display();
        assert_eq!(
            job,
            format!(
                "ffconcat version 1.0\n\
                 file '{frames}/2026-10-11/20261011-0000.jpg'\n\
                 file '{frames}/2026-10-11/20261011-0002.jpg'\n"
            )
        );
        assert_eq!(f.statuses(), [(spec(2, None).name(), Status::Queued)]);
    }

    #[test]
    fn job_escapes_quotes_in_the_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("ben's data"));
        let frame = store.frame_path(at(11, 0, 0));
        fs::create_dir_all(frame.parent().unwrap()).unwrap();
        fs::write(&frame, b"jpeg").unwrap();

        assert_eq!(submit(&store, &spec(1, None)).unwrap(), Submitted::Queued);

        let job = fs::read_to_string(store.out_file(&spec(1, None).name(), JOB)).unwrap();
        let root = std::path::absolute(dir.path()).unwrap();
        assert_eq!(
            job,
            format!(
                "ffconcat version 1.0\n\
                 file '{}/ben'\\''s data/frames/2026-10-11/20261011-0000.jpg'\n",
                root.display()
            )
        );
    }

    #[test]
    fn submit_is_idempotent() {
        let f = Fixture::new();
        f.add_frames(&[at(11, 0, 0)]);
        let s = spec(1, None);
        let job = f.put(&s.name(), JOB, "sentinel", 0);

        assert_eq!(submit(&f.store, &s).unwrap(), Submitted::Queued);
        assert_eq!(fs::read_to_string(&job).unwrap(), "sentinel");

        f.put(&s.name(), MP4, "mp4", 1);
        assert_eq!(submit(&f.store, &s).unwrap(), Submitted::Done);
    }

    #[test]
    fn resubmitting_a_finished_timelapse_restarts_its_retention() {
        let f = Fixture::new();
        f.add_frames(&[at(11, 0, 0)]);
        let s = spec(1, None);
        let mp4 = f.put(&s.name(), MP4, "mp4", 0);

        assert_eq!(submit(&f.store, &s).unwrap(), Submitted::Done);

        assert_eq!(prune(&f.store, mtime(100)).unwrap(), 0);
        assert!(mp4.exists());
    }

    #[test]
    fn submit_without_frames_writes_nothing() {
        let f = Fixture::new();
        f.add_frames(&[at(11, 12, 0)]);
        assert_eq!(
            submit(&f.store, &spec(1, Some((0, 6)))).unwrap(),
            Submitted::NoFrames
        );
        assert_eq!(f.statuses(), []);
    }

    #[test]
    fn submit_leaves_a_concurrent_submit_of_the_same_selection_alone() {
        let f = Fixture::new();
        f.add_frames(&[at(11, 0, 0)]);
        let s = spec(1, None);
        // Another request for the same selection is still writing its job.
        let part = f.put(&s.name(), "job.part", "in progress", 0);

        assert_eq!(submit(&f.store, &s).unwrap(), Submitted::Queued);
        assert_eq!(fs::read_to_string(&part).unwrap(), "in progress");
        assert!(!f.store.out_file(&s.name(), JOB).exists());
    }

    #[test]
    fn resubmit_after_failure_retries() {
        let f = Fixture::new();
        f.add_frames(&[at(11, 0, 0)]);
        let s = spec(1, None);
        f.put(&s.name(), ERR, "boom", 0);
        assert_eq!(f.statuses(), [(s.name(), Status::Failed)]);

        assert_eq!(submit(&f.store, &s).unwrap(), Submitted::Queued);
        assert!(!f.store.out_file(&s.name(), ERR).exists());
        assert_eq!(f.statuses(), [(s.name(), Status::Queued)]);
    }

    #[test]
    fn list_derives_status_newest_first_and_ignores_junk() {
        let f = Fixture::new();
        f.put(&named(1), MP4, "mp4!", 10);
        f.put(&named(1), JOB, "", 0); // Left behind by a crash after the rename.
        f.put(&named(2), ERR, "boom\nmore", 20);
        f.put(&named(3), JOB, "", 30);
        f.put(&named(3), "mp4.part", "", 40);
        f.put(&named(4), JOB, "", 50);
        f.put(&named(5), "mp4.part", "", 60); // Orphan part: no status.
        f.put(&named(6), "job.part", "", 70);
        f.put("notes", "txt", "", 80);
        f.put("junk", MP4, "", 80);

        let entries = list(&f.store).unwrap();
        let summary: Vec<_> = entries
            .iter()
            .map(|e| (e.name.clone(), e.status, e.size, e.error.clone()))
            .collect();
        assert_eq!(
            summary,
            [
                (named(4), Status::Queued, 0, None),
                (named(3), Status::Running, 0, None),
                (named(2), Status::Failed, 0, Some("boom\nmore".to_owned())),
                (named(1), Status::Done, 4, None),
            ]
        );
        assert_eq!(entries[0].modified, mtime(50));
        assert_eq!(
            entries[3].spec,
            TimelapseSpec::parse_name(&named(1)).unwrap()
        );
    }

    #[test]
    fn list_without_out_dir_is_empty() {
        let f = Fixture::new();
        assert_eq!(list(&f.store).unwrap(), []);
    }

    #[test]
    fn pending_jobs_are_oldest_first() {
        let f = Fixture::new();
        f.put(&named(3), JOB, "", 5);
        f.put(&named(2), JOB, "", 5);
        f.put(&named(1), JOB, "", 9);
        f.put(&named(4), MP4, "", 0);
        assert_eq!(
            pending_jobs(&f.store).unwrap(),
            [named(2), named(3), named(1)]
        );
    }

    #[test]
    fn prune_removes_only_old_outputs_and_failures() {
        let f = Fixture::new();
        let old_mp4 = f.put(&named(1), MP4, "", 0);
        let old_err = f.put(&named(2), ERR, "", 0);
        let old_job = f.put(&named(3), JOB, "", 0);
        let new_mp4 = f.put(&named(4), MP4, "", 100);
        let junk = f.put("junk", MP4, "", 0);

        assert_eq!(prune(&f.store, mtime(50)).unwrap(), 2);
        assert!(!old_mp4.exists());
        assert!(!old_err.exists());
        assert!(old_job.exists());
        assert!(new_mp4.exists());
        assert!(junk.exists());
    }
}

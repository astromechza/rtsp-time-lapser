# rtsp-time-lapser M2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship M2: assemble any UTC range of captured JPEGs into an MP4 from the HTML UI, through an on-disk job queue that survives restarts and that M3 can reuse.

**Architecture:** `POST /timelapses` resolves the selection and writes `DATA_DIR/out/{name}.job`, an ffconcat list of absolute frame paths. The deterministic `{name}` encodes every selection input. One `Encoder` task drains `.job` files oldest first through `ffmpeg -f concat` (`.mp4.part`, then rename), writes `.err` on failure, and prunes old outputs. Status comes only from which files exist.

**Tech Stack:** Rust 1.99 (edition 2024), tokio, axum 0.8 (`Form`), tower 0.5 (`ServiceExt::oneshot`, now a runtime dependency), tower-http 0.7 (`ServeFile`), askama 0.16, serde 1 (new, derive), prometheus-client 0.25, clap 4.6, chrono. ffmpeg with libx264 (already in the Alpine image).

**Spec:** `docs/superpowers/specs/2026-10-14-m2-timelapse-assembly-design.md` (parent: `docs/superpowers/specs/2026-10-12-rtsp-time-lapser-design.md`)

All code in this plan was compiled, linted (`clippy -D warnings`) and tested on Rust 1.99.0 during planning, at every intermediate task state. `real_ffmpeg_encode` passed against ffmpeg 8.0.1. An end-to-end run of the binary with real ffmpeg (60 generated frames, step 2, giving a 30-frame 640×360 MP4) passed with both an absolute and a relative `DATA_DIR`. The container image was not rebuilt.

## Global Constraints

- Rust `1.99`, edition `2024`. Single binary crate. All tests are unit tests inside their modules (`#[cfg(test)] mod tests`).
- `make lint` (`cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`) and `make test` pass at the end of every task.
- `src/main.rs` keeps `mod` lines alphabetical and carries `#![allow(dead_code)]` from Task 1 until Task 5 wires everything in.
- Crash-only: filenames are the state, no job state in memory. Atomic writes: `*.part`, then rename. Startup `cleanup_parts()` already deletes every `*.part` under `DATA_DIR`.
- UTC everywhere. Timelapse names: `{from:%Y%m%d-%H%M}_{to:%Y%m%d-%H%M}_s{step}[_h{start:02}-{end:02}]`.
- `Store` owns paths: `out_dir()` is `DATA_DIR/out`, `out_file(name, ext)` is `out/{name}.{ext}`. File extensions: `job`, `mp4.part`, `mp4`, `err`.
- Encode command (verified):
  `ffmpeg -nostdin -loglevel error -f concat -safe 0 -i <job> -vf "setpts=N/<fps>/TB,scale='<width>':-2" -r <fps> -c:v libx264 -preset <preset> -crf <crf> -pix_fmt yuv420p -movflags +faststart -f mp4 -y <out>.part`.
  `<width>` is `trunc(min(iw,<max>)/2)*2`, or `trunc(iw/2)*2` without a cap. No `nice`, no `-threads`: the 16-core host has headroom.
- Error text from ffmpeg goes through `ffmpeg::describe_error` (redact, then truncate) before it is logged or written to `.err`.
- HTML UI only, no JSON API, no JavaScript, no inline styles. New classes go in `assets/style.css` using the existing CSS variables.
- New config, each with a clap `env` fallback: `TIMELAPSE_FPS` (30), `TIMELAPSE_CRF` (20, 0–51), `TIMELAPSE_PRESET` (`veryfast`, x264 preset names only), `TIMELAPSE_MAX_WIDTH` (unset), `ENCODE_TIMEOUT_SECS` (7200), `OUT_RETENTION_DAYS` (7, 1–36500).
- New metrics: `timelapse_encodes_total{result="ok"|"error"}`, `timelapse_encode_duration_seconds` (buckets 10, 30, 60, 120, 300, 600, 1800, 3600, 7200).
- Commit messages use `feat:`, `fix:` or `chore:` and end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Commits are signed through 1Password: if signing fails, stop and ask the user. Never pass `--no-gpg-sign`.

## Review Focus

- **Retrying a failed range:** a user who submits the same range again after a failure expects a fresh attempt, not a stuck "failed" row. Pinned by `timelapse::tests::resubmit_after_failure_retries` (Task 2).
- **Night-only ranges crossing midnight:** `hour_from=22, hour_to=2` over two days must keep 22:00–01:59 from both days. Pinned by `timelapse::tests::wrapping_hours_span_midnight_across_days` (Task 1).
- **Pod killed mid-encode:** the job must finish after the restart without being resubmitted. Pinned by `encoder::tests::interrupted_encode_is_redone_after_restart` (Task 3).
- **Crash between the MP4 rename and the job deletion:** the leftover `.job` must not re-encode or overwrite a good MP4. Pinned by `encoder::tests::job_left_next_to_finished_mp4_is_removed_without_encoding` (Task 3).
- **Fetching arbitrary files through `/timelapses/{file}`:** only `{valid name}.mp4` may be served; `.job`, junk names and `..` get 404. Pinned by `web::tests::timelapse_files_other_than_videos_are_not_served` (Task 4).

---

### Task 1: Selection and naming

Adds `TimelapseSpec`: the deterministic name and its inverse, validation, and frame selection (hour filter, then step). Also adds the store queries it needs.

**Files:**
- Modify: `src/main.rs` (dead-code allow, `mod timelapse;`)
- Modify: `src/store.rs` (rename `FRAME_FORMAT` to `pub MINUTE_FORMAT`, add `out_dir`, `out_file`, `frames_between`, `parse_minute`)
- Create: `src/timelapse.rs`

**Interfaces:**
- Produces: `store::MINUTE_FORMAT: &str` (`"%Y%m%d-%H%M"`), `store::parse_minute(&str) -> Option<DateTime<Utc>>`.
- Produces: `Store::out_dir(&self) -> PathBuf`, `Store::out_file(&self, name: &str, ext: &str) -> PathBuf`, `Store::frames_between(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> io::Result<Vec<Frame>>` (inclusive, oldest first).
- Produces: `timelapse::TimelapseSpec { pub from: DateTime<Utc>, pub to: DateTime<Utc>, pub step: u32, pub hours: Option<(u32, u32)> }` (`Debug, Clone, Copy, PartialEq, Eq`), with `name(&self) -> String`, `parse_name(&str) -> Option<Self>`, `validate(&self, now: DateTime<Utc>) -> Result<(), String>` and `select(&self, frames: Vec<Frame>) -> Vec<Frame>`.

- [ ] **Step 1: Write the failing tests**

Add the `frames_between` test to `src/store.rs`:

```diff
--- a/src/store.rs
+++ b/src/store.rs
@@ -291,6 +319,29 @@ mod tests {
         assert_eq!(newest.url(), "/frames/2026-10-10/20261010-2359.jpg");
     }
 
+    #[test]
+    fn frames_between_is_inclusive_across_days_and_skips_missing_days() {
+        let dir = tempfile::tempdir().unwrap();
+        let store = Store::new(dir.path());
+        for t in [
+            at(2026, 10, 9, 23, 59),
+            at(2026, 10, 10, 23, 58),
+            at(2026, 10, 10, 23, 59),
+            at(2026, 10, 12, 0, 0),
+            at(2026, 10, 12, 0, 1),
+        ] {
+            touch(&store.frame_path(t));
+        }
+
+        let times: Vec<_> = store
+            .frames_between(at(2026, 10, 10, 23, 59), at(2026, 10, 12, 0, 0))
+            .unwrap()
+            .into_iter()
+            .map(|f| f.time)
+            .collect();
+        assert_eq!(times, [at(2026, 10, 10, 23, 59), at(2026, 10, 12, 0, 0)]);
+    }
+
     #[test]
     fn newest_frame_none_when_empty() {
         let dir = tempfile::tempdir().unwrap();
```

Create `src/timelapse.rs` containing only this test module. The implementation goes above it in Step 3.

```rust
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
        let mut backwards = spec(1, None);
        backwards.from = at(12, 0, 0);
        assert_eq!(err(backwards), "from must not be after to");
        assert_eq!(
            spec(1, None).validate(at(11, 23, 58)).unwrap_err(),
            "to must not be in the future"
        );
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
}
```

Register the module and allow dead code until Task 5:

```diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,8 +1,10 @@
+#![allow(dead_code)] // Removed in Task 5, once the timelapse code is wired into main.
 mod capture;
 mod config;
 mod ffmpeg;
 mod metrics;
 mod store;
+mod timelapse;
 mod web;
 
 use std::{io::IsTerminal, sync::Arc};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test`
Expected: compile errors: `TimelapseSpec`, `frames_between` and `Frame` imports are unresolved.

- [ ] **Step 3: Implement**

`src/store.rs`:

```diff
--- a/src/store.rs
+++ b/src/store.rs
@@ -10,7 +10,8 @@ use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
 use tracing::warn;
 
 const DAY_FORMAT: &str = "%Y-%m-%d";
-const FRAME_FORMAT: &str = "%Y%m%d-%H%M";
+/// UTC minute format shared by frame names and timelapse names.
+pub const MINUTE_FORMAT: &str = "%Y%m%d-%H%M";
 const PART_SUFFIX: &str = ".part";
 
 #[derive(Debug, Clone, PartialEq, Eq)]
@@ -48,13 +49,23 @@ impl Store {
         self.data_dir.join("frames")
     }
 
+    /// Timelapse jobs and outputs.
+    pub fn out_dir(&self) -> PathBuf {
+        self.data_dir.join("out")
+    }
+
+    /// `out/{name}.{ext}`.
+    pub fn out_file(&self, name: &str, ext: &str) -> PathBuf {
+        self.out_dir().join(format!("{name}.{ext}"))
+    }
+
     pub fn day_dir(&self, date: NaiveDate) -> PathBuf {
         self.frames_dir().join(date.format(DAY_FORMAT).to_string())
     }
 
     pub fn frame_path(&self, time: DateTime<Utc>) -> PathBuf {
         self.day_dir(time.date_naive())
-            .join(format!("{}.jpg", time.format(FRAME_FORMAT)))
+            .join(format!("{}.jpg", time.format(MINUTE_FORMAT)))
     }
 
     /// Days that contain a valid day directory, newest first.
@@ -85,6 +96,19 @@ impl Store {
         Ok(frames)
     }
 
+    /// Valid frames with `from <= time <= to`, oldest first. Only days in the range are read.
+    pub fn frames_between(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> io::Result<Vec<Frame>> {
+        let mut frames = Vec::new();
+        for date in self.day_dates()? {
+            if date < from.date_naive() || date > to.date_naive() {
+                continue;
+            }
+            let day = self.list_frames(date)?;
+            frames.extend(day.into_iter().filter(|f| f.time >= from && f.time <= to));
+        }
+        Ok(frames)
+    }
+
     /// The newest valid frame, searching backwards through days until one has frames.
     pub fn newest_frame(&self) -> io::Result<Option<Frame>> {
         for date in self.day_dates()?.into_iter().rev() {
@@ -139,7 +163,11 @@ pub fn parse_day_name(name: &str) -> Option<NaiveDate> {
 
 /// Parse `YYYYMMDD-HHMM.jpg` as a UTC time.
 pub fn parse_frame_name(name: &str) -> Option<DateTime<Utc>> {
-    let stem = name.strip_suffix(".jpg")?;
+    parse_minute(name.strip_suffix(".jpg")?)
+}
+
+/// Parse exactly `YYYYMMDD-HHMM` as a UTC time.
+pub fn parse_minute(stem: &str) -> Option<DateTime<Utc>> {
     let all_digits = stem.bytes().enumerate().all(|(i, b)| match i {
         8 => b == b'-',
         _ => b.is_ascii_digit(),
@@ -147,7 +175,7 @@ pub fn parse_frame_name(name: &str) -> Option<DateTime<Utc>> {
     if stem.len() != 13 || !all_digits {
         return None;
     }
-    NaiveDateTime::parse_from_str(stem, FRAME_FORMAT)
+    NaiveDateTime::parse_from_str(stem, MINUTE_FORMAT)
         .ok()
         .map(|t| t.and_utc())
 }
```

Top of `src/timelapse.rs`, above the test module:

```rust
//! Timelapse selection and naming. A `TimelapseSpec` says which frames to encode; its name
//! encodes every input, so the same selection always maps to the same output file.

use chrono::{DateTime, Timelike, Utc};

use crate::store::{Frame, MINUTE_FORMAT, parse_minute};

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
}

impl TimelapseSpec {
    /// `{from}_{to}_s{step}[_h{start}-{end}]`, e.g. `20261011-0000_20261011-2359_s1_h06-20`.
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
        name
    }

    /// Exact inverse of `name()`. Rejects anything `name()` would not produce and any spec
    /// that is invalid regardless of the current time.
    pub fn parse_name(name: &str) -> Option<Self> {
        let mut parts = name.split('_');
        let from = parse_minute(parts.next()?)?;
        let to = parse_minute(parts.next()?)?;
        let step = parts.next()?.strip_prefix('s')?.parse().ok()?;
        let hours = match parts.next() {
            None => None,
            Some(h) => {
                let (start, end) = h.strip_prefix('h')?.split_once('-')?;
                Some((start.parse().ok()?, end.parse().ok()?))
            }
        };
        if parts.next().is_some() {
            return None;
        }
        let spec = Self {
            from,
            to,
            step,
            hours,
        };
        // The round trip rejects non-canonical spellings such as `s01` or `s+1`.
        (spec.shape_error().is_none() && spec.name() == name).then_some(spec)
    }

    /// Check the spec against `now`. The message is shown to the user as is.
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), String> {
        if let Some(e) = self.shape_error() {
            return Err(e.to_owned());
        }
        if self.to > now {
            return Err("to must not be in the future".to_owned());
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
```

- [ ] **Step 4: Run lint and tests**

Run: `cargo fmt && make lint && make test`
Expected: clean lint; `test result: ok. 50 passed; 0 failed; 1 ignored`.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/store.rs src/timelapse.rs
git commit -m "feat: add timelapse selection and deterministic naming

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: On-disk job queue

Adds submit, status listing, pending-job ordering and pruning over `DATA_DIR/out/`. All status is derived from which files exist.

**Files:**
- Modify: `src/store.rs` (add `write_atomic`, `remove_if_exists`)
- Modify: `src/timelapse.rs`

**Interfaces:**
- Consumes: Task 1's `TimelapseSpec`, `Store::out_dir`, `Store::out_file`, `Store::frames_between`.
- Produces: `store::write_atomic(path: &Path, contents: &[u8]) -> io::Result<()>`, `store::remove_if_exists(path: &Path) -> io::Result<()>`.
- Produces: `timelapse::{JOB, MP4, ERR}: &str` (`"job"`, `"mp4"`, `"err"`).
- Produces: `timelapse::Status { Queued, Running, Done, Failed }` (`Debug, Clone, Copy, PartialEq, Eq`), `timelapse::Submitted { Done, Queued, NoFrames }`, `timelapse::Entry { pub name: String, pub spec: TimelapseSpec, pub status: Status, pub size: u64, pub modified: SystemTime, pub error: Option<String> }`.
- Produces: `timelapse::submit(store: &Store, spec: &TimelapseSpec) -> io::Result<Submitted>` (expects a validated spec), `timelapse::list(store: &Store) -> io::Result<Vec<Entry>>` (newest first), `timelapse::pending_jobs(store: &Store) -> io::Result<Vec<String>>` (oldest `.job` first), `timelapse::prune(store: &Store, cutoff: SystemTime) -> io::Result<usize>`.

- [ ] **Step 1: Write the failing tests**

Append to the test module in `src/timelapse.rs`:

```diff
--- a/src/timelapse.rs
+++ b/src/timelapse.rs
@@ -234,4 +477,187 @@ mod tests {
             .collect();
         assert_eq!(kept, [at(11, 23, 0), at(12, 1, 0)]);
     }
+
+    struct Fixture {
+        _dir: tempfile::TempDir,
+        store: Store,
+    }
+
+    impl Fixture {
+        fn new() -> Self {
+            let dir = tempfile::tempdir().unwrap();
+            let store = Store::new(dir.path());
+            Self { _dir: dir, store }
+        }
+
+        fn add_frames(&self, times: &[DateTime<Utc>]) {
+            for t in times {
+                let path = self.store.frame_path(*t);
+                fs::create_dir_all(path.parent().unwrap()).unwrap();
+                fs::write(path, b"jpeg").unwrap();
+            }
+        }
+
+        /// Write `out/{name}.{ext}` with modification time `mtime(mtime_secs)`.
+        fn put(&self, name: &str, ext: &str, contents: &str, mtime_secs: u64) -> PathBuf {
+            fs::create_dir_all(self.store.out_dir()).unwrap();
+            let path = self.store.out_file(name, ext);
+            fs::write(&path, contents).unwrap();
+            let file = fs::File::options().write(true).open(&path).unwrap();
+            file.set_modified(mtime(mtime_secs)).unwrap();
+            path
+        }
+
+        fn statuses(&self) -> Vec<(String, Status)> {
+            list(&self.store)
+                .unwrap()
+                .into_iter()
+                .map(|e| (e.name, e.status))
+                .collect()
+        }
+    }
+
+    fn mtime(secs: u64) -> SystemTime {
+        UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000 + secs)
+    }
+
+    fn named(d: u32) -> String {
+        TimelapseSpec {
+            from: at(d, 0, 0),
+            to: at(d, 23, 59),
+            step: 1,
+            hours: None,
+        }
+        .name()
+    }
+
+    #[test]
+    fn submit_writes_absolute_ffconcat_job() {
+        let f = Fixture::new();
+        f.add_frames(&[at(11, 0, 0), at(11, 0, 1), at(11, 0, 2), at(12, 0, 0)]);
+
+        assert_eq!(submit(&f.store, &spec(2, None)).unwrap(), Submitted::Queued);
+
+        let job = fs::read_to_string(f.store.out_file(&spec(2, None).name(), JOB)).unwrap();
+        let frames = std::path::absolute(f.store.frames_dir()).unwrap();
+        let frames = frames.display();
+        assert_eq!(
+            job,
+            format!(
+                "ffconcat version 1.0\n\
+                 file '{frames}/2026-10-11/20261011-0000.jpg'\n\
+                 file '{frames}/2026-10-11/20261011-0002.jpg'\n"
+            )
+        );
+        assert_eq!(f.statuses(), [(spec(2, None).name(), Status::Queued)]);
+    }
+
+    #[test]
+    fn submit_is_idempotent() {
+        let f = Fixture::new();
+        f.add_frames(&[at(11, 0, 0)]);
+        let s = spec(1, None);
+        let job = f.put(&s.name(), JOB, "sentinel", 0);
+
+        assert_eq!(submit(&f.store, &s).unwrap(), Submitted::Queued);
+        assert_eq!(fs::read_to_string(&job).unwrap(), "sentinel");
+
+        f.put(&s.name(), MP4, "mp4", 1);
+        assert_eq!(submit(&f.store, &s).unwrap(), Submitted::Done);
+    }
+
+    #[test]
+    fn submit_without_frames_writes_nothing() {
+        let f = Fixture::new();
+        f.add_frames(&[at(11, 12, 0)]);
+        assert_eq!(
+            submit(&f.store, &spec(1, Some((0, 6)))).unwrap(),
+            Submitted::NoFrames
+        );
+        assert_eq!(f.statuses(), []);
+    }
+
+    #[test]
+    fn resubmit_after_failure_retries() {
+        let f = Fixture::new();
+        f.add_frames(&[at(11, 0, 0)]);
+        let s = spec(1, None);
+        f.put(&s.name(), ERR, "boom", 0);
+        assert_eq!(f.statuses(), [(s.name(), Status::Failed)]);
+
+        assert_eq!(submit(&f.store, &s).unwrap(), Submitted::Queued);
+        assert!(!f.store.out_file(&s.name(), ERR).exists());
+        assert_eq!(f.statuses(), [(s.name(), Status::Queued)]);
+    }
+
+    #[test]
+    fn list_derives_status_newest_first_and_ignores_junk() {
+        let f = Fixture::new();
+        f.put(&named(1), MP4, "mp4!", 10);
+        f.put(&named(1), JOB, "", 0); // Left behind by a crash after the rename.
+        f.put(&named(2), ERR, "boom\nmore", 20);
+        f.put(&named(3), JOB, "", 30);
+        f.put(&named(3), "mp4.part", "", 40);
+        f.put(&named(4), JOB, "", 50);
+        f.put(&named(5), "mp4.part", "", 60); // Orphan part: no status.
+        f.put(&named(6), "job.part", "", 70);
+        f.put("notes", "txt", "", 80);
+        f.put("junk", MP4, "", 80);
+
+        let entries = list(&f.store).unwrap();
+        let summary: Vec<_> = entries
+            .iter()
+            .map(|e| (e.name.clone(), e.status, e.size, e.error.clone()))
+            .collect();
+        assert_eq!(
+            summary,
+            [
+                (named(4), Status::Queued, 0, None),
+                (named(3), Status::Running, 0, None),
+                (named(2), Status::Failed, 0, Some("boom\nmore".to_owned())),
+                (named(1), Status::Done, 4, None),
+            ]
+        );
+        assert_eq!(entries[0].modified, mtime(50));
+        assert_eq!(
+            entries[3].spec,
+            TimelapseSpec::parse_name(&named(1)).unwrap()
+        );
+    }
+
+    #[test]
+    fn list_without_out_dir_is_empty() {
+        let f = Fixture::new();
+        assert_eq!(list(&f.store).unwrap(), []);
+    }
+
+    #[test]
+    fn pending_jobs_are_oldest_first() {
+        let f = Fixture::new();
+        f.put(&named(3), JOB, "", 5);
+        f.put(&named(2), JOB, "", 5);
+        f.put(&named(1), JOB, "", 9);
+        f.put(&named(4), MP4, "", 0);
+        assert_eq!(
+            pending_jobs(&f.store).unwrap(),
+            [named(2), named(3), named(1)]
+        );
+    }
+
+    #[test]
+    fn prune_removes_only_old_outputs_and_failures() {
+        let f = Fixture::new();
+        let old_mp4 = f.put(&named(1), MP4, "", 0);
+        let old_err = f.put(&named(2), ERR, "", 0);
+        let old_job = f.put(&named(3), JOB, "", 0);
+        let new_mp4 = f.put(&named(4), MP4, "", 100);
+        let junk = f.put("junk", MP4, "", 0);
+
+        assert_eq!(prune(&f.store, mtime(50)).unwrap(), 2);
+        assert!(!old_mp4.exists());
+        assert!(!old_err.exists());
+        assert!(old_job.exists());
+        assert!(new_mp4.exists());
+        assert!(junk.exists());
+    }
 }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test timelapse`
Expected: compile errors: `submit`, `list`, `Status`, `JOB` and the other new items are not found.

- [ ] **Step 3: Implement**

`src/store.rs`:

```diff
--- a/src/store.rs
+++ b/src/store.rs
@@ -153,6 +153,21 @@ pub fn part_path(path: &Path) -> PathBuf {
     PathBuf::from(s)
 }
 
+/// Write `path` atomically: write `<path>.part`, then rename it into place.
+pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
+    let part = part_path(path);
+    fs::write(&part, contents)?;
+    fs::rename(&part, path)
+}
+
+/// Delete `path`, treating "already gone" as success.
+pub fn remove_if_exists(path: &Path) -> io::Result<()> {
+    match fs::remove_file(path) {
+        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
+        res => res,
+    }
+}
+
 /// Parse `YYYY-MM-DD`.
 pub fn parse_day_name(name: &str) -> Option<NaiveDate> {
     if name.len() != 10 {
```

`src/timelapse.rs`: replace the module header and imports, and insert the queue code between `impl TimelapseSpec` and the test module:

```diff
--- a/src/timelapse.rs
+++ b/src/timelapse.rs
@@ -1,9 +1,27 @@
-//! Timelapse selection and naming. A `TimelapseSpec` says which frames to encode; its name
-//! encodes every input, so the same selection always maps to the same output file.
+//! Timelapse selection, naming and the on-disk job queue in `DATA_DIR/out/`.
+//!
+//! A `TimelapseSpec` says which frames to encode; its name encodes every input, so the same
+//! selection always maps to the same files. Status is derived from which files exist:
+//! `{name}.mp4` done, `{name}.err` failed, `{name}.job` plus `{name}.mp4.part` running,
+//! `{name}.job` alone queued.
+
+use std::{
+    collections::BTreeMap,
+    fs, io,
+    path::{Path, PathBuf},
+    time::{SystemTime, UNIX_EPOCH},
+};
 
 use chrono::{DateTime, Timelike, Utc};
 
-use crate::store::{Frame, MINUTE_FORMAT, parse_minute};
+use crate::store::{Frame, MINUTE_FORMAT, Store, parse_minute, remove_if_exists, write_atomic};
+
+/// Extension of a queued job: an ffconcat list of frame paths.
+pub const JOB: &str = "job";
+/// Extension of a finished timelapse.
+pub const MP4: &str = "mp4";
+/// Extension of a failure report.
+pub const ERR: &str = "err";
 
 #[derive(Debug, Clone, Copy, PartialEq, Eq)]
 pub struct TimelapseSpec {
@@ -108,6 +126,231 @@ impl TimelapseSpec {
     }
 }
 
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+pub enum Status {
+    Queued,
+    Running,
+    Done,
+    Failed,
+}
+
+#[derive(Debug, PartialEq, Eq)]
+pub enum Submitted {
+    /// The MP4 already exists.
+    Done,
+    /// A job exists, either written now or earlier.
+    Queued,
+    /// The spec selects no frames; nothing was written.
+    NoFrames,
+}
+
+#[derive(Debug, Clone, PartialEq, Eq)]
+pub struct Entry {
+    pub name: String,
+    pub spec: TimelapseSpec,
+    pub status: Status,
+    /// Size of the MP4 in bytes; 0 unless done.
+    pub size: u64,
+    /// Newest modification time among the entry's files.
+    pub modified: SystemTime,
+    /// The `.err` contents when failed.
+    pub error: Option<String>,
+}
+
+/// Queue `spec` unless its MP4 exists or it is already queued. `spec` must be validated.
+pub fn submit(store: &Store, spec: &TimelapseSpec) -> io::Result<Submitted> {
+    let name = spec.name();
+    if store.out_file(&name, MP4).try_exists()? {
+        return Ok(Submitted::Done);
+    }
+    if store.out_file(&name, JOB).try_exists()? {
+        return Ok(Submitted::Queued);
+    }
+    let frames = spec.select(store.frames_between(spec.from, spec.to)?);
+    if frames.is_empty() {
+        return Ok(Submitted::NoFrames);
+    }
+    fs::create_dir_all(store.out_dir())?;
+    // Resubmitting retries a failure.
+    remove_if_exists(&store.out_file(&name, ERR))?;
+    write_atomic(
+        &store.out_file(&name, JOB),
+        ffconcat(store, &frames)?.as_bytes(),
+    )?;
+    Ok(Submitted::Queued)
+}
+
+/// Every timelapse in `out/`, newest first.
+pub fn list(store: &Store) -> io::Result<Vec<Entry>> {
+    let mut by_name: BTreeMap<String, (TimelapseSpec, Files)> = BTreeMap::new();
+    for file in scan(store)? {
+        let (_, files) = by_name
+            .entry(file.name)
+            .or_insert_with(|| (file.spec, Files::default()));
+        match file.kind {
+            Kind::Job => files.job = true,
+            Kind::Mp4Part => files.running = true,
+            Kind::Mp4 => files.size = Some(file.size),
+            Kind::Err => files.failed = true,
+        }
+        files.modified = files.modified.max(Some(file.modified));
+    }
+    let mut entries = Vec::new();
+    for (name, (spec, files)) in by_name {
+        let Some(status) = files.status() else {
+            continue;
+        };
+        let error = match status {
+            Status::Failed => read_if_exists(&store.out_file(&name, ERR))?,
+            _ => None,
+        };
+        entries.push(Entry {
+            name,
+            spec,
+            status,
+            size: files.size.unwrap_or(0),
+            modified: files.modified.unwrap_or(UNIX_EPOCH),
+            error,
+        });
+    }
+    entries.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
+    Ok(entries)
+}
+
+/// Names of queued and running jobs, oldest `.job` first, ties broken by name.
+pub fn pending_jobs(store: &Store) -> io::Result<Vec<String>> {
+    let mut jobs: Vec<_> = scan(store)?
+        .into_iter()
+        .filter(|f| f.kind == Kind::Job)
+        .map(|f| (f.modified, f.name))
+        .collect();
+    jobs.sort();
+    Ok(jobs.into_iter().map(|(_, name)| name).collect())
+}
+
+/// Delete MP4 and `.err` files last modified before `cutoff`. Jobs are never pruned.
+/// Returns how many files were removed.
+pub fn prune(store: &Store, cutoff: SystemTime) -> io::Result<usize> {
+    let mut removed = 0;
+    for file in scan(store)? {
+        if matches!(file.kind, Kind::Mp4 | Kind::Err) && file.modified < cutoff {
+            remove_if_exists(&file.path)?;
+            removed += 1;
+        }
+    }
+    Ok(removed)
+}
+
+/// An ffconcat list of absolute frame paths. Absolute because ffmpeg resolves relative
+/// entries against the list file's directory.
+fn ffconcat(store: &Store, frames: &[Frame]) -> io::Result<String> {
+    let mut out = String::from("ffconcat version 1.0\n");
+    for frame in frames {
+        let path = std::path::absolute(store.frame_path(frame.time))?;
+        out.push_str(&format!("file '{}'\n", path.display()));
+    }
+    Ok(out)
+}
+
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+enum Kind {
+    Job,
+    Mp4Part,
+    Mp4,
+    Err,
+}
+
+impl Kind {
+    /// Split `{name}.{ext}` into the name and the kind of file.
+    fn split(file_name: &str) -> Option<(&str, Self)> {
+        [
+            (".job", Self::Job),
+            (".mp4.part", Self::Mp4Part),
+            (".mp4", Self::Mp4),
+            (".err", Self::Err),
+        ]
+        .into_iter()
+        .find_map(|(suffix, kind)| Some((file_name.strip_suffix(suffix)?, kind)))
+    }
+}
+
+struct OutFile {
+    name: String,
+    spec: TimelapseSpec,
+    kind: Kind,
+    path: PathBuf,
+    size: u64,
+    modified: SystemTime,
+}
+
+#[derive(Default)]
+struct Files {
+    job: bool,
+    running: bool,
+    failed: bool,
+    size: Option<u64>,
+    modified: Option<SystemTime>,
+}
+
+impl Files {
+    fn status(&self) -> Option<Status> {
+        if self.size.is_some() {
+            Some(Status::Done)
+        } else if self.failed {
+            Some(Status::Failed)
+        } else if self.job && self.running {
+            Some(Status::Running)
+        } else if self.job {
+            Some(Status::Queued)
+        } else {
+            None
+        }
+    }
+}
+
+/// Timelapse files in `out/`. Other files are ignored, as are files that vanish while being
+/// read (the worker deletes jobs and prunes outputs concurrently).
+fn scan(store: &Store) -> io::Result<Vec<OutFile>> {
+    let entries = match fs::read_dir(store.out_dir()) {
+        Ok(entries) => entries,
+        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
+        Err(e) => return Err(e),
+    };
+    let mut files = Vec::new();
+    for entry in entries {
+        let entry = entry?;
+        let file_name = entry.file_name();
+        let Some((name, kind)) = file_name.to_str().and_then(Kind::split) else {
+            continue;
+        };
+        let Some(spec) = TimelapseSpec::parse_name(name) else {
+            continue;
+        };
+        let meta = match entry.metadata() {
+            Ok(meta) => meta,
+            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
+            Err(e) => return Err(e),
+        };
+        files.push(OutFile {
+            name: name.to_owned(),
+            spec,
+            kind,
+            path: entry.path(),
+            size: meta.len(),
+            modified: meta.modified()?,
+        });
+    }
+    Ok(files)
+}
+
+fn read_if_exists(path: &Path) -> io::Result<Option<String>> {
+    match fs::read_to_string(path) {
+        Ok(s) => Ok(Some(s)),
+        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
+        Err(e) => Err(e),
+    }
+}
+
 #[cfg(test)]
 mod tests {
     use chrono::TimeZone;
```

- [ ] **Step 4: Run lint and tests**

Run: `cargo fmt && make lint && make test`
Expected: clean lint; `test result: ok. 58 passed; 0 failed; 1 ignored`.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs src/timelapse.rs
git commit -m "feat: add on-disk timelapse job queue

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Encoder

Adds the reusable `encode` function and the `Encoder` worker loop, with its config and metrics. `describe_error` and the fake-ffmpeg test scripts move from `capture.rs` into `ffmpeg.rs` so that capture and the encoder share them.

**Files:**
- Modify: `src/ffmpeg.rs` (add `describe_error`; move `fake_ffmpeg` here from `capture.rs`)
- Modify: `src/capture.rs` (use the shared helpers)
- Modify: `src/config.rs` (timelapse settings; `test_config` gains the new fields)
- Modify: `src/metrics.rs` (encode counter and histogram)
- Modify: `src/main.rs` (`mod encoder;`)
- Create: `src/encoder.rs`

**Interfaces:**
- Consumes: Task 2's `timelapse::{pending_jobs, prune, JOB, MP4, ERR}`, `store::{write_atomic, remove_if_exists, part_path}`.
- Produces: `ffmpeg::describe_error(e: &anyhow::Error, url: &RtspUrl, limit: usize) -> String`; test-only `ffmpeg::fake_ffmpeg(name: &str) -> PathBuf` (scripts `ok`, `fail`, `empty`, `long`, `hang`).
- Produces: `Config` fields `timelapse_fps: u32`, `timelapse_crf: u8`, `timelapse_preset: String`, `timelapse_max_width: Option<u32>`, `encode_timeout_secs: u64`, `out_retention_days: u64`; methods `encode_timeout()` and `out_retention()` returning `Duration`.
- Produces: `Metrics::record_encode_ok(&self, took: Duration)`, `Metrics::record_encode_error(&self)`; test-only `Metrics::encodes(&self, result) -> u64`.
- Produces: `encoder::EncodeSettings { pub fps: u32, pub crf: u8, pub preset: String, pub max_width: Option<u32> }` with `from_config(&Config)`; `encoder::encode(ffmpeg_bin: &Path, job: &Path, out: &Path, settings: &EncodeSettings, limit: Duration) -> anyhow::Result<()>` (the M3 entry point); `encoder::Encoder::new(cfg: Arc<Config>, store: Store, metrics: Metrics, wake: Arc<tokio::sync::Notify>) -> Encoder`, `Encoder::run(self)` (never returns), `Encoder::drain(&self) -> io::Result<()>`.

- [ ] **Step 1: Write the failing tests**

`src/capture.rs` tests: drop the local `fake_ffmpeg` and use the shared one.

```diff
--- a/src/capture.rs
+++ b/src/capture.rs
@@ -128,50 +123,11 @@ fn ffmpeg_args(cfg: &Config, part: &Path) -> Vec<OsString> {
 
 #[cfg(test)]
 mod tests {
-    use std::{os::unix::fs::PermissionsExt, sync::OnceLock};
-
     use chrono::TimeZone;
     use pretty_assertions::assert_eq;
 
     use super::*;
-    use crate::config::test_config;
-
-    /// Fake ffmpeg scripts, written once. Writing them once, before any test spawns a
-    /// process, avoids "text file busy" races between writing and executing a script.
-    fn fake_ffmpeg(name: &str) -> PathBuf {
-        static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
-        let dir = DIR.get_or_init(|| {
-            let dir = tempfile::tempdir().unwrap();
-            let scripts = [
-                // The output path is the last argument.
-                (
-                    "ok",
-                    "for a; do out=\"$a\"; done\nprintf 'jpeg' > \"$out\"\n",
-                ),
-                (
-                    "fail",
-                    "echo 'cannot open rtsp://viewer:s3cret@10.0.0.5:554/stream1' >&2\nexit 1\n",
-                ),
-                ("empty", "for a; do out=\"$a\"; done\n: > \"$out\"\n"),
-                // The URL starts at stderr byte 495, so a 512-byte cut lands inside the password.
-                (
-                    "long",
-                    "printf '%s rtsp://viewer:s3cret@10.0.0.5:554/stream1' \"$(printf '%494s' '' | tr ' ' x)\" >&2\nexit 1\n",
-                ),
-                (
-                    "hang",
-                    "for a; do out=\"$a\"; done\nprintf 'partial' > \"$out\"\nexec sleep 30\n",
-                ),
-            ];
-            for (name, body) in scripts {
-                let path = dir.path().join(name);
-                std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
-                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
-            }
-            dir
-        });
-        dir.path().join(name)
-    }
+    use crate::{config::test_config, ffmpeg::fake_ffmpeg};
 
     fn capturer(data: &Path, script: &str) -> Capturer {
         let mut cfg = test_config(data, &fake_ffmpeg(script));
@@ -231,7 +187,7 @@ mod tests {
         let c = capturer(dir.path(), "long");
 
         let err = c.snap(now()).await.unwrap_err();
-        let msg = describe_error(&err, &c.cfg.rtsp_url);
+        let msg = ffmpeg::describe_error(&err, &c.cfg.rtsp_url, ERROR_LOG_LIMIT);
         assert!(!msg.contains("viewer"), "{msg}");
         assert!(!msg.contains("s3c"), "{msg}");
     }
```

`src/config.rs` tests:

```diff
--- a/src/config.rs
+++ b/src/config.rs
@@ -210,6 +255,22 @@ mod tests {
         assert_eq!(cfg.data_dir, PathBuf::from("/data"));
     }
 
+    #[test]
+    fn parses_timelapse_defaults_and_rejects_unknown_preset() {
+        let cfg = Config::try_parse_from(["x", "--rtsp-url", "rtsp://cam/s"]).unwrap();
+        assert_eq!(cfg.timelapse_preset, "veryfast");
+        assert_eq!(cfg.timelapse_max_width, None);
+        assert_eq!(cfg.out_retention(), Duration::from_secs(7 * 86_400));
+        let res = Config::try_parse_from([
+            "x",
+            "--rtsp-url",
+            "rtsp://cam/s",
+            "--timelapse-preset",
+            "quick",
+        ]);
+        assert!(res.is_err());
+    }
+
     #[test]
     fn rejects_out_of_range_quality() {
         let res =
```

`src/metrics.rs` tests:

```diff
--- a/src/metrics.rs
+++ b/src/metrics.rs
@@ -92,6 +124,8 @@ mod tests {
         let m = Metrics::new();
         m.record_ok(1_760_000_000, Duration::from_secs(2));
         m.record_error();
+        m.record_encode_ok(Duration::from_secs(90));
+        m.record_encode_error();
         let text = m.encode();
         assert!(
             text.contains("timelapse_last_frame_timestamp_seconds 1760000000"),
@@ -109,5 +143,17 @@ mod tests {
             text.contains("timelapse_capture_duration_seconds_count 1"),
             "{text}"
         );
+        assert!(
+            text.contains(r#"timelapse_encodes_total{result="ok"} 1"#),
+            "{text}"
+        );
+        assert!(
+            text.contains(r#"timelapse_encodes_total{result="error"} 1"#),
+            "{text}"
+        );
+        assert!(
+            text.contains("timelapse_encode_duration_seconds_count 1"),
+            "{text}"
+        );
     }
 }
```

Create `src/encoder.rs` containing only this test module. The implementation goes above it in Step 3.

```rust
#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{config::test_config, ffmpeg::fake_ffmpeg};

    const NAME: &str = "20261011-0000_20261011-2359_s1";

    struct Fixture {
        dir: tempfile::TempDir,
        encoder: Encoder,
    }

    impl Fixture {
        fn new(script: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cfg = test_config(dir.path(), &fake_ffmpeg(script));
            let store = Store::new(dir.path());
            fs::create_dir_all(store.out_dir()).unwrap();
            let encoder = Encoder::new(
                Arc::new(cfg),
                store,
                Metrics::new(),
                Arc::new(Notify::new()),
            );
            Self { dir, encoder }
        }

        fn file(&self, ext: &str) -> PathBuf {
            self.encoder.store.out_file(NAME, ext)
        }

        fn queue(&self) {
            fs::write(self.file(JOB), "ffconcat version 1.0\n").unwrap();
        }
    }

    fn settings(max_width: Option<u32>) -> EncodeSettings {
        EncodeSettings {
            fps: 30,
            crf: 20,
            preset: "veryfast".to_owned(),
            max_width,
        }
    }

    #[test]
    fn args_set_frame_timing_scale_and_codec() {
        let args = encode_args(
            Path::new("/data/out/x.job"),
            Path::new("/data/out/x.mp4.part"),
            &settings(Some(1280)),
        );
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-nostdin",
                "-loglevel",
                "error",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                "/data/out/x.job",
                "-vf",
                "setpts=N/30/TB,scale='trunc(min(iw,1280)/2)*2':-2",
                "-r",
                "30",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "20",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                "-y",
                "/data/out/x.mp4.part",
            ]
        );

        let args = encode_args(Path::new("j"), Path::new("p"), &settings(None));
        assert!(args.contains(&OsString::from("setpts=N/30/TB,scale='trunc(iw/2)*2':-2")));
    }

    #[tokio::test]
    async fn success_writes_mp4_and_removes_job() {
        let f = Fixture::new("ok");
        f.queue();

        f.encoder.drain().await.unwrap();

        assert!(f.file(MP4).exists());
        assert!(!f.file(JOB).exists());
        assert!(!f.file("mp4.part").exists());
        assert!(!f.file(ERR).exists());
        assert_eq!(f.encoder.metrics.encodes("ok"), 1);
    }

    #[tokio::test]
    async fn failure_writes_redacted_report_and_removes_job() {
        let f = Fixture::new("fail");
        f.queue();

        f.encoder.drain().await.unwrap();

        let report = fs::read_to_string(f.file(ERR)).unwrap();
        assert!(report.contains("exited with"), "{report}");
        assert!(!report.contains("s3cret"), "{report}");
        assert!(!f.file(MP4).exists());
        assert!(!f.file(JOB).exists());
        assert!(!f.file("mp4.part").exists());
        assert_eq!(f.encoder.metrics.encodes("error"), 1);
    }

    #[tokio::test]
    async fn empty_output_is_a_failure() {
        let f = Fixture::new("empty");
        f.queue();

        f.encoder.drain().await.unwrap();

        let report = fs::read_to_string(f.file(ERR)).unwrap();
        assert!(report.contains("empty video"), "{report}");
        assert!(!f.file(MP4).exists());
    }

    #[tokio::test]
    async fn job_left_next_to_finished_mp4_is_removed_without_encoding() {
        let f = Fixture::new("fail");
        fs::write(f.file(MP4), "mp4").unwrap();
        f.queue();

        f.encoder.drain().await.unwrap();

        assert_eq!(fs::read_to_string(f.file(MP4)).unwrap(), "mp4");
        assert!(!f.file(JOB).exists());
        assert!(!f.file(ERR).exists());
    }

    #[tokio::test]
    async fn interrupted_encode_is_redone_after_restart() {
        let f = Fixture::new("ok");
        f.queue();
        fs::write(f.file("mp4.part"), "half").unwrap();

        // Startup cleanup, as in main.
        f.encoder.store.cleanup_parts().unwrap();
        f.encoder.drain().await.unwrap();

        assert!(f.file(MP4).exists());
        assert!(!f.file(JOB).exists());
    }

    #[tokio::test]
    async fn run_wakes_up_for_new_jobs() {
        let f = Fixture::new("ok");
        let wake = f.encoder.wake.clone();
        let mp4 = f.file(MP4);
        let job = f.file(JOB);
        let worker = tokio::spawn(f.encoder.run());

        // Let the worker finish its first, empty pass and start waiting.
        tokio::time::sleep(Duration::from_millis(100)).await;
        fs::write(&job, "ffconcat version 1.0\n").unwrap();
        wake.notify_one();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !mp4.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        worker.abort();
        assert!(mp4.exists());
        drop(f.dir);
    }

    /// Encodes generated JPEGs with real ffmpeg:
    /// `cargo test real_ffmpeg_encode -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs ffmpeg and ffprobe on PATH"]
    async fn real_ffmpeg_encode() {
        let dir = tempfile::tempdir().unwrap();
        let frames = dir.path().join("f");
        fs::create_dir(&frames).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=641x361:rate=1")
            .args(["-frames:v", "7", "-q:v", "4"])
            .arg(frames.join("%02d.jpg"))
            .status()
            .unwrap();
        assert!(status.success());
        let mut list = String::from("ffconcat version 1.0\n");
        for i in 1..=7 {
            list.push_str(&format!(
                "file '{}'\n",
                frames.join(format!("{i:02}.jpg")).display()
            ));
        }
        let job = dir.path().join("x.job");
        fs::write(&job, list).unwrap();
        let out = dir.path().join("x.mp4");

        encode(
            Path::new("ffmpeg"),
            &job,
            &out,
            &settings(None),
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
            .args(["-show_entries", "stream=nb_read_frames,width,height"])
            .args(["-of", "default=nw=1"])
            .arg(&out)
            .output()
            .unwrap();
        let probe = String::from_utf8(probe.stdout).unwrap();
        println!("{probe}");
        assert_eq!(probe, "width=640\nheight=360\nnb_read_frames=7\n");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test`
Expected: compile errors: `ffmpeg::fake_ffmpeg`, `describe_error`, the encoder items and the new config fields are missing.

- [ ] **Step 3: Implement**

`src/ffmpeg.rs`. The `fake_ffmpeg` body is moved verbatim from `capture.rs`:

```diff
--- a/src/ffmpeg.rs
+++ b/src/ffmpeg.rs
@@ -3,6 +3,8 @@ use std::{process::Stdio, time::Duration};
 use anyhow::{Context, Result, bail};
 use tokio::{process::Command, time::timeout};
 
+use crate::config::{RtspUrl, redact};
+
 /// Run `cmd` to completion, killing it after `limit`. Fails on spawn error, timeout or
 /// non-zero exit. The error includes the full stderr; callers must redact, then truncate it,
 /// before logging.
@@ -24,6 +26,12 @@ pub async fn run(mut cmd: Command, limit: Duration) -> Result<()> {
     Ok(())
 }
 
+/// Loggable error text, at most `limit` bytes. Never log the raw error: ffmpeg's stderr can
+/// contain the URL. Redacts before truncating, so a cut can never leave a partial URL behind.
+pub fn describe_error(e: &anyhow::Error, url: &RtspUrl, limit: usize) -> String {
+    truncate(&redact(&format!("{e:#}"), url), limit).to_owned()
+}
+
 /// Cut `s` to at most `max` bytes on a char boundary.
 pub fn truncate(s: &str, max: usize) -> &str {
     if s.len() <= max {
@@ -36,6 +44,46 @@ pub fn truncate(s: &str, max: usize) -> &str {
     &s[..end]
 }
 
+/// Fake ffmpeg scripts for tests, written once. Writing them once, before any test spawns
+/// a process, avoids "text file busy" races between writing and executing a script.
+#[cfg(test)]
+pub fn fake_ffmpeg(name: &str) -> std::path::PathBuf {
+    use std::{os::unix::fs::PermissionsExt, sync::OnceLock};
+
+    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
+    let dir = DIR.get_or_init(|| {
+        let dir = tempfile::tempdir().unwrap();
+        let scripts = [
+            // The output path is the last argument.
+            (
+                "ok",
+                "for a; do out=\"$a\"; done\nprintf 'jpeg' > \"$out\"\n",
+            ),
+            (
+                "fail",
+                "echo 'cannot open rtsp://viewer:s3cret@10.0.0.5:554/stream1' >&2\nexit 1\n",
+            ),
+            ("empty", "for a; do out=\"$a\"; done\n: > \"$out\"\n"),
+            // The URL starts at stderr byte 495, so a 512-byte cut lands inside the password.
+            (
+                "long",
+                "printf '%s rtsp://viewer:s3cret@10.0.0.5:554/stream1' \"$(printf '%494s' '' | tr ' ' x)\" >&2\nexit 1\n",
+            ),
+            (
+                "hang",
+                "for a; do out=\"$a\"; done\nprintf 'partial' > \"$out\"\nexec sleep 30\n",
+            ),
+        ];
+        for (name, body) in scripts {
+            let path = dir.path().join(name);
+            std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
+            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
+        }
+        dir
+    });
+    dir.path().join(name)
+}
+
 #[cfg(test)]
 mod tests {
     use super::*;
```

`src/capture.rs`:

```diff
--- a/src/capture.rs
+++ b/src/capture.rs
@@ -9,7 +9,7 @@ use tokio::{
 use tracing::{debug, warn};
 
 use crate::{
-    config::{Config, RtspUrl, redact},
+    config::Config,
     ffmpeg,
     metrics::Metrics,
     store::{Store, part_path},
@@ -64,7 +64,8 @@ impl Capturer {
             }
             Err(e) => {
                 self.metrics.record_error();
-                warn!(error = %describe_error(&e, &self.cfg.rtsp_url), "capture failed");
+                let error = ffmpeg::describe_error(&e, &self.cfg.rtsp_url, ERROR_LOG_LIMIT);
+                warn!(%error, "capture failed");
             }
         }
     }
@@ -97,12 +98,6 @@ impl Capturer {
     }
 }
 
-/// Loggable error text. Never log the raw error: ffmpeg's stderr can contain the URL.
-/// Redacts before truncating, so a cut can never leave a partial URL behind.
-fn describe_error(e: &anyhow::Error, url: &RtspUrl) -> String {
-    ffmpeg::truncate(&redact(&format!("{e:#}"), url), ERROR_LOG_LIMIT).to_owned()
-}
-
 fn ffmpeg_args(cfg: &Config, part: &Path) -> Vec<OsString> {
     let mut args: Vec<OsString> = [
         "-nostdin",
```

`src/config.rs`:

```diff
--- a/src/config.rs
+++ b/src/config.rs
@@ -45,6 +45,37 @@ pub struct Config {
     /// Path to the ffmpeg binary.
     #[arg(long, env = "FFMPEG_BIN", default_value = "ffmpeg")]
     pub ffmpeg_bin: PathBuf,
+
+    /// Timelapse output frame rate.
+    #[arg(long, env = "TIMELAPSE_FPS", default_value_t = 30,
+          value_parser = clap::value_parser!(u32).range(1..))]
+    pub timelapse_fps: u32,
+
+    /// Timelapse x264 quality (-crf), 0 (lossless) to 51 (worst).
+    #[arg(long, env = "TIMELAPSE_CRF", default_value_t = 20,
+          value_parser = clap::value_parser!(u8).range(0..=51))]
+    pub timelapse_crf: u8,
+
+    /// Timelapse x264 preset: slower presets give smaller files.
+    #[arg(long, env = "TIMELAPSE_PRESET", default_value = "veryfast",
+          value_parser = ["ultrafast", "superfast", "veryfast", "faster", "fast", "medium",
+                          "slow", "slower", "veryslow"])]
+    pub timelapse_preset: String,
+
+    /// Downscale timelapses to at most this width when set.
+    #[arg(long, env = "TIMELAPSE_MAX_WIDTH",
+          value_parser = clap::value_parser!(u32).range(2..))]
+    pub timelapse_max_width: Option<u32>,
+
+    /// Seconds before a running encode is killed.
+    #[arg(long, env = "ENCODE_TIMEOUT_SECS", default_value_t = 7200,
+          value_parser = clap::value_parser!(u64).range(1..))]
+    pub encode_timeout_secs: u64,
+
+    /// Days to keep finished and failed timelapses.
+    #[arg(long, env = "OUT_RETENTION_DAYS", default_value_t = 7,
+          value_parser = clap::value_parser!(u64).range(1..=36_500))]
+    pub out_retention_days: u64,
 }
 
 impl Config {
@@ -59,6 +90,14 @@ impl Config {
     pub fn stale_after(&self) -> Duration {
         Duration::from_secs(self.stale_after_secs)
     }
+
+    pub fn encode_timeout(&self) -> Duration {
+        Duration::from_secs(self.encode_timeout_secs)
+    }
+
+    pub fn out_retention(&self) -> Duration {
+        Duration::from_secs(self.out_retention_days * 86_400)
+    }
 }
 
 /// An RTSP URL that never prints its credentials. Use `expose()` to get the raw value.
@@ -139,6 +178,12 @@ pub fn test_config(data_dir: &std::path::Path, ffmpeg_bin: &std::path::Path) ->
         scale_width: None,
         stale_after_secs: 180,
         ffmpeg_bin: ffmpeg_bin.to_path_buf(),
+        timelapse_fps: 30,
+        timelapse_crf: 20,
+        timelapse_preset: "veryfast".to_owned(),
+        timelapse_max_width: None,
+        encode_timeout_secs: 5,
+        out_retention_days: 7,
     }
 }
 
```

`src/metrics.rs`:

```diff
--- a/src/metrics.rs
+++ b/src/metrics.rs
@@ -15,6 +15,8 @@ pub struct Metrics {
     last_frame: Gauge,
     captures: Family<ResultLabels, Counter>,
     capture_duration: Histogram,
+    encodes: Family<ResultLabels, Counter>,
+    encode_duration: Histogram,
 }
 
 impl Metrics {
@@ -22,6 +24,10 @@ impl Metrics {
         let last_frame = Gauge::default();
         let captures = Family::<ResultLabels, Counter>::default();
         let capture_duration = Histogram::new([0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0]);
+        let encodes = Family::<ResultLabels, Counter>::default();
+        let encode_duration = Histogram::new([
+            10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0,
+        ]);
 
         let mut registry = Registry::default();
         registry.register(
@@ -40,12 +46,24 @@ impl Metrics {
             "Duration of successful captures",
             capture_duration.clone(),
         );
+        registry.register(
+            "timelapse_encodes",
+            "Timelapse encodes by result",
+            encodes.clone(),
+        );
+        registry.register(
+            "timelapse_encode_duration_seconds",
+            "Duration of successful timelapse encodes",
+            encode_duration.clone(),
+        );
 
         Self {
             registry: Arc::new(registry),
             last_frame,
             captures,
             capture_duration,
+            encodes,
+            encode_duration,
         }
     }
 
@@ -67,6 +85,20 @@ impl Metrics {
         self.count("skipped");
     }
 
+    pub fn record_encode_ok(&self, took: Duration) {
+        self.encode_duration.observe(took.as_secs_f64());
+        self.encodes.get_or_create(&vec![("result", "ok")]).inc();
+    }
+
+    pub fn record_encode_error(&self) {
+        self.encodes.get_or_create(&vec![("result", "error")]).inc();
+    }
+
+    #[cfg(test)]
+    pub fn encodes(&self, result: &'static str) -> u64 {
+        self.encodes.get_or_create(&vec![("result", result)]).get()
+    }
+
     #[cfg(test)]
     pub fn captures(&self, result: &'static str) -> u64 {
         self.captures.get_or_create(&vec![("result", result)]).get()
```

`src/main.rs`:

```diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,6 +1,7 @@
 #![allow(dead_code)] // Removed in Task 5, once the timelapse code is wired into main.
 mod capture;
 mod config;
+mod encoder;
 mod ffmpeg;
 mod metrics;
 mod store;
```

Top of `src/encoder.rs`, above the test module:

```rust
//! Encodes queued timelapse jobs into MP4s, one at a time, and prunes old outputs.

use std::{
    ffi::OsString,
    io,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, ensure};
use tokio::{
    process::Command,
    sync::Notify,
    time::{Instant, timeout},
};
use tracing::{info, warn};

use crate::{
    config::Config,
    ffmpeg,
    metrics::Metrics,
    store::{Store, part_path, remove_if_exists, write_atomic},
    timelapse::{self, ERR, JOB, MP4},
};

/// Failure reports in `.err` files are cut to this many bytes.
const ERROR_REPORT_LIMIT: usize = 4096;
/// Logged failures are cut to this many bytes.
const ERROR_LOG_LIMIT: usize = 512;
/// The queue is rescanned at least this often, even without a wake-up.
const IDLE: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeSettings {
    pub fps: u32,
    pub crf: u8,
    pub preset: String,
    /// Downscale to at most this width; `None` keeps the native width.
    pub max_width: Option<u32>,
}

impl EncodeSettings {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            fps: cfg.timelapse_fps,
            crf: cfg.timelapse_crf,
            preset: cfg.timelapse_preset.clone(),
            max_width: cfg.timelapse_max_width,
        }
    }
}

/// Encode the ffconcat list `job` into `out`, writing `<out>.part` then renaming it.
pub async fn encode(
    ffmpeg_bin: &Path,
    job: &Path,
    out: &Path,
    settings: &EncodeSettings,
    limit: Duration,
) -> Result<()> {
    let part = part_path(out);
    let result = write_video(ffmpeg_bin, job, &part, out, settings, limit).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

async fn write_video(
    ffmpeg_bin: &Path,
    job: &Path,
    part: &Path,
    out: &Path,
    settings: &EncodeSettings,
    limit: Duration,
) -> Result<()> {
    let mut cmd = Command::new(ffmpeg_bin);
    cmd.args(encode_args(job, part, settings));
    ffmpeg::run(cmd, limit).await?;
    let len = tokio::fs::metadata(part).await?.len();
    ensure!(len > 0, "ffmpeg produced an empty video");
    tokio::fs::rename(part, out).await?;
    Ok(())
}

/// `setpts` gives every input frame exactly one output frame; the concat demuxer's own
/// per-image timestamps are ignored. `yuv420p` needs even dimensions, so the scale always
/// rounds the width down to even and `-2` keeps the height even.
fn encode_args(job: &Path, part: &Path, settings: &EncodeSettings) -> Vec<OsString> {
    let width = match settings.max_width {
        Some(max) => format!("trunc(min(iw,{max})/2)*2"),
        None => "trunc(iw/2)*2".to_owned(),
    };
    let fps = settings.fps.to_string();
    let crf = settings.crf.to_string();
    let filter = format!("setpts=N/{fps}/TB,scale='{width}':-2");
    let mut args: Vec<OsString> = [
        "-nostdin",
        "-loglevel",
        "error",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
    ]
    .map(OsString::from)
    .into();
    args.push(job.into());
    args.extend(
        [
            "-vf",
            &filter,
            "-r",
            &fps,
            "-c:v",
            "libx264",
            "-preset",
            &settings.preset,
            "-crf",
            &crf,
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
            "-y",
        ]
        .map(OsString::from),
    );
    args.push(part.into());
    args
}

pub struct Encoder {
    cfg: Arc<Config>,
    settings: EncodeSettings,
    store: Store,
    metrics: Metrics,
    wake: Arc<Notify>,
}

impl Encoder {
    pub fn new(cfg: Arc<Config>, store: Store, metrics: Metrics, wake: Arc<Notify>) -> Self {
        Self {
            settings: EncodeSettings::from_config(&cfg),
            cfg,
            store,
            metrics,
            wake,
        }
    }

    /// Encode forever: drain the queue, prune old outputs, then wait for a wake-up or `IDLE`.
    pub async fn run(self) {
        loop {
            if let Err(e) = self.drain().await {
                warn!(error = %e, "processing timelapse queue failed");
            }
            let cutoff = SystemTime::now()
                .checked_sub(self.cfg.out_retention())
                .unwrap_or(UNIX_EPOCH);
            match timelapse::prune(&self.store, cutoff) {
                Ok(0) => {}
                Ok(removed) => info!(removed, "pruned old timelapses"),
                Err(e) => warn!(error = %e, "pruning timelapses failed"),
            }
            let _ = timeout(IDLE, self.wake.notified()).await;
        }
    }

    /// Encode pending jobs, oldest first, until none are left, including jobs queued
    /// meanwhile.
    pub async fn drain(&self) -> io::Result<()> {
        while let Some(name) = timelapse::pending_jobs(&self.store)?.into_iter().next() {
            self.process(&name).await?;
        }
        Ok(())
    }

    /// Encode one job, record the outcome, then remove the job. A failure leaves a `.err`
    /// report instead of the MP4, so the job is not retried until resubmitted.
    async fn process(&self, name: &str) -> io::Result<()> {
        let job = self.store.out_file(name, JOB);
        let out = self.store.out_file(name, MP4);
        // A crash between the MP4 rename and the job removal leaves both behind.
        if !out.try_exists()? {
            let started = Instant::now();
            let limit = self.cfg.encode_timeout();
            match encode(&self.cfg.ffmpeg_bin, &job, &out, &self.settings, limit).await {
                Ok(()) => {
                    let took = started.elapsed();
                    self.metrics.record_encode_ok(took);
                    info!(name, secs = took.as_secs(), "encoded timelapse");
                }
                Err(e) => {
                    self.metrics.record_encode_error();
                    let report = ffmpeg::describe_error(&e, &self.cfg.rtsp_url, ERROR_REPORT_LIMIT);
                    write_atomic(&self.store.out_file(name, ERR), report.as_bytes())?;
                    let error = ffmpeg::truncate(&report, ERROR_LOG_LIMIT);
                    warn!(name, %error, "encoding timelapse failed");
                }
            }
        }
        remove_if_exists(&job)
    }
}
```

- [ ] **Step 4: Run lint and tests**

Run: `cargo fmt && make lint && make test`
Expected: clean lint; `test result: ok. 66 passed; 0 failed; 2 ignored`.

Then, if ffmpeg and ffprobe are installed locally:

Run: `cargo test real_ffmpeg_encode -- --ignored --nocapture`
Expected: prints `width=640`, `height=360`, `nb_read_frames=7`, and passes.

- [ ] **Step 5: Commit**

```bash
git add src/capture.rs src/config.rs src/encoder.rs src/ffmpeg.rs src/main.rs src/metrics.rs
git commit -m "feat: add timelapse encoder worker

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Timelapse UI and routes

Adds `GET/POST /timelapses` and `GET /timelapses/{name}.mp4`, a nav link, and a "Timelapse this day" button on day pages. `AppState` gains the `Notify` that wakes the encoder.

**Files:**
- Modify: `Cargo.toml` (add `serde` with `derive`; move `tower` with `util` from dev-dependencies to dependencies)
- Modify: `src/web/mod.rs` (routes, `AppState::wake`, test helpers and tests)
- Modify: `src/web/pages.rs` (`DayPage::timelapse_to`)
- Modify: `src/main.rs` (construct `AppState::wake`)
- Create: `src/web/timelapses.rs`
- Modify: `templates/base.html` (`head` block, nav link), `templates/day.html` (button)
- Create: `templates/timelapses.html`
- Modify: `assets/style.css` (header flex, `.badge-busy`, `.button`, `.form`, `.form-hint`, `.inline-form`)

**Interfaces:**
- Consumes: Task 2's `timelapse::{submit, list, Status, Submitted, Entry, MP4}`, Task 1's `TimelapseSpec::{validate, parse_name, name}`.
- Produces: `web::AppState::wake: Arc<tokio::sync::Notify>` (`POST /timelapses` calls `notify_one()` after queuing). Task 5 passes the same `Arc` to `Encoder::new`.
- Produces: `web::timelapses::INPUT_FORMAT` (`"%Y-%m-%dT%H:%M"`).
- Produces routes: `GET /timelapses` (form + table, refresh meta while anything is queued or running); `POST /timelapses` (303 to the MP4 when done, otherwise 303 to `/timelapses`; 400 plain text on bad input or an empty selection); `GET /timelapses/{file}` (only `{valid name}.mp4`, range requests supported, otherwise 404).

- [ ] **Step 1: Write the failing tests**

`src/web/mod.rs` tests: generalise the request helper and add the timelapse route tests.

```diff
--- a/src/web/mod.rs
+++ b/src/web/mod.rs
@@ -108,6 +117,7 @@ mod tests {
                 metrics: Metrics::new(),
                 stale_after: Duration::from_secs(180),
                 started_at,
+                wake: Arc::new(Notify::new()),
             }))
         }
 
@@ -117,10 +127,23 @@ mod tests {
     }
 
     async fn get(app: Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
-        let res = app
-            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
-            .await
+        send(app, Request::get(uri).body(Body::empty()).unwrap()).await
+    }
+
+    async fn post(
+        app: Router,
+        uri: &str,
+        form: &str,
+    ) -> (StatusCode, axum::http::HeaderMap, String) {
+        let req = Request::post(uri)
+            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
+            .body(Body::from(form.to_owned()))
             .unwrap();
+        send(app, req).await
+    }
+
+    async fn send(app: Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
+        let res = app.oneshot(req).await.unwrap();
         let status = res.status();
         let headers = res.headers().clone();
         let body = res.into_body().collect().await.unwrap().to_bytes();
@@ -281,6 +304,126 @@ mod tests {
         );
     }
 
+    const DAY_NAME: &str = "20261011-0000_20261011-2359_s1";
+    const DAY_FORM: &str = "from=2026-10-11T00%3A00&to=2026-10-11T23%3A59";
+
+    fn day_frame() -> DateTime<Utc> {
+        DateTime::parse_from_rfc3339("2026-10-11T07:05:00Z")
+            .unwrap()
+            .to_utc()
+    }
+
+    #[tokio::test]
+    async fn post_timelapse_queues_job_and_lists_it() {
+        let f = Fixture::new();
+        f.add_frame(day_frame());
+
+        let (status, headers, body) = post(f.app(long_ago()), "/timelapses", DAY_FORM).await;
+        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
+        assert_eq!(headers[header::LOCATION], "/timelapses");
+        assert!(f.store.out_file(DAY_NAME, "job").exists());
+
+        let (status, _, body) = get(f.app(long_ago()), "/timelapses").await;
+        assert_eq!(status, StatusCode::OK);
+        assert!(body.contains("queued"), "{body}");
+        assert!(body.contains("Daily hours from (UTC)"), "{body}");
+        assert!(body.contains(r#"http-equiv="refresh""#), "{body}");
+    }
+
+    #[tokio::test]
+    async fn post_finished_timelapse_redirects_to_video() {
+        let f = Fixture::new();
+        std::fs::create_dir_all(f.store.out_dir()).unwrap();
+        std::fs::write(f.store.out_file(DAY_NAME, "mp4"), "0123456789").unwrap();
+
+        let (status, headers, _) = post(f.app(long_ago()), "/timelapses", DAY_FORM).await;
+        assert_eq!(status, StatusCode::SEE_OTHER);
+        let location = headers[header::LOCATION].to_str().unwrap().to_owned();
+        assert_eq!(location, format!("/timelapses/{DAY_NAME}.mp4"));
+
+        let (status, _, body) = get(f.app(long_ago()), &location).await;
+        assert_eq!(status, StatusCode::OK);
+        assert_eq!(body, "0123456789");
+
+        let req = Request::get(&location)
+            .header(header::RANGE, "bytes=2-4")
+            .body(Body::empty())
+            .unwrap();
+        let (status, _, body) = send(f.app(long_ago()), req).await;
+        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
+        assert_eq!(body, "234");
+    }
+
+    #[tokio::test]
+    async fn post_timelapse_rejects_bad_input() {
+        let f = Fixture::new();
+        f.add_frame(day_frame());
+        let future = (Utc::now() + TimeDelta::hours(1)).format("%Y-%m-%dT%H%%3A%M");
+        for (form, message) in [
+            ("from=2026-10-11T00%3A00".to_owned(), "to must look like"),
+            (
+                format!("from=2026-10-11T00%3A00&to={future}"),
+                "to must not be in the future",
+            ),
+            (format!("{DAY_FORM}&step=0"), "step must be at least 1"),
+            (
+                format!("{DAY_FORM}&hour_from=0&hour_to=6"),
+                "no frames in that range",
+            ),
+        ] {
+            let (status, _, body) = post(f.app(long_ago()), "/timelapses", &form).await;
+            assert_eq!(status, StatusCode::BAD_REQUEST, "{form}");
+            assert!(body.contains(message), "{form}: {body}");
+        }
+        assert!(!f.store.out_file(DAY_NAME, "job").exists());
+    }
+
+    #[tokio::test]
+    async fn timelapse_list_shows_finished_and_failed_without_refresh() {
+        let f = Fixture::new();
+        std::fs::create_dir_all(f.store.out_dir()).unwrap();
+        std::fs::write(f.store.out_file(DAY_NAME, "mp4"), "x").unwrap();
+        let failed = "20261012-0000_20261012-2359_s1";
+        std::fs::write(f.store.out_file(failed, "err"), "ffmpeg exited\nmore").unwrap();
+
+        let (status, _, body) = get(f.app(long_ago()), "/timelapses").await;
+        assert_eq!(status, StatusCode::OK);
+        assert!(
+            body.contains(&format!("/timelapses/{DAY_NAME}.mp4")),
+            "{body}"
+        );
+        assert!(body.contains("ffmpeg exited"), "{body}");
+        assert!(!body.contains("more"), "{body}");
+        assert!(!body.contains(r#"http-equiv="refresh""#), "{body}");
+    }
+
+    #[tokio::test]
+    async fn timelapse_files_other_than_videos_are_not_served() {
+        let f = Fixture::new();
+        std::fs::create_dir_all(f.store.out_dir()).unwrap();
+        std::fs::write(f.store.out_file(DAY_NAME, "job"), "list").unwrap();
+        std::fs::write(f.path().join("secret.txt"), "secret").unwrap();
+        for uri in [
+            format!("/timelapses/{DAY_NAME}.job"),
+            "/timelapses/notes.mp4".to_owned(),
+            "/timelapses/..%2Fsecret.txt".to_owned(),
+            format!("/timelapses/{DAY_NAME}.mp4"),
+        ] {
+            let (status, _, _) = get(f.app(long_ago()), &uri).await;
+            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
+        }
+    }
+
+    #[tokio::test]
+    async fn day_page_offers_timelapse_of_the_day() {
+        let f = Fixture::new();
+        f.add_frame(day_frame());
+        let (_, _, body) = get(f.app(long_ago()), "/days/2026-10-11").await;
+        assert!(body.contains(r#"action="/timelapses""#), "{body}");
+        assert!(body.contains(r#"value="2026-10-11T00:00""#), "{body}");
+        assert!(body.contains(r#"value="2026-10-11T23:59""#), "{body}");
+    }
+
     #[tokio::test]
     async fn stylesheet_is_served() {
         let f = Fixture::new();
```

Create `src/web/timelapses.rs` containing only this test module. The implementation goes above it in Step 3.

```rust
#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use pretty_assertions::assert_eq;

    use super::*;

    fn form(from: &str, to: &str, step: &str, hours: (&str, &str)) -> CreateForm {
        CreateForm {
            from: from.to_owned(),
            to: to.to_owned(),
            step: step.to_owned(),
            hour_from: hours.0.to_owned(),
            hour_to: hours.1.to_owned(),
        }
    }

    #[test]
    fn form_parses_into_spec() {
        let spec = form("2026-10-11T00:00", "2026-10-11T23:59:30", "", ("20", "6"))
            .spec()
            .unwrap();
        assert_eq!(
            spec,
            TimelapseSpec {
                from: Utc.with_ymd_and_hms(2026, 10, 11, 0, 0, 0).unwrap(),
                to: Utc.with_ymd_and_hms(2026, 10, 11, 23, 59, 0).unwrap(),
                step: 1,
                hours: Some((20, 6)),
            }
        );
    }

    #[test]
    fn form_errors() {
        let err = |f: CreateForm| f.spec().unwrap_err();
        assert_eq!(
            err(form("", "2026-10-11T23:59", "", ("", ""))),
            "from must look like 2026-10-11T07:05"
        );
        assert_eq!(
            err(form("2026-10-11T00:00", "2026-10-11", "", ("", ""))),
            "to must look like 2026-10-11T07:05"
        );
        assert_eq!(
            err(form("2026-10-11T00:00", "2026-10-11T23:59", "x", ("", ""))),
            "step must be a whole number"
        );
        assert_eq!(
            err(form("2026-10-11T00:00", "2026-10-11T23:59", "", ("6", ""))),
            "set both hour fields or neither"
        );
        assert_eq!(
            err(form(
                "2026-10-11T00:00",
                "2026-10-11T23:59",
                "",
                ("6", "-1")
            )),
            "hours must be between 0 and 23"
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(1), "1 KB");
        assert_eq!(format_size(512_000), "512 KB");
        assert_eq!(format_size(12_345_678), "12.3 MB");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test web`
Expected: compile errors: `AppState` has no field `wake` and `Notify` is unresolved. `src/web/timelapses.rs` is not compiled until Step 3 declares `mod timelapses;`.

- [ ] **Step 3: Implement**

`Cargo.toml` (`Cargo.lock` updates on the next build):

```diff
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -12,7 +12,9 @@ axum = "0.8.9"
 chrono = { version = "0.4.45", default-features = false, features = ["clock", "std"] }
 clap = { version = "4.6", features = ["derive", "env"] }
 prometheus-client = "0.25"
+serde = { version = "1.0.229", features = ["derive"] }
 tokio = { version = "1.53", features = ["fs", "macros", "net", "process", "rt-multi-thread", "signal", "time"] }
+tower = { version = "0.5", features = ["util"] }
 tower-http = { version = "0.7", features = ["fs", "trace"] }
 tracing = "0.1.44"
 tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
@@ -21,4 +23,3 @@ tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
 http-body-util = "0.1"
 pretty_assertions = "1.4"
 tempfile = "3.27"
-tower = { version = "0.5", features = ["util"] }
```

`src/web/mod.rs`:

```diff
--- a/src/web/mod.rs
+++ b/src/web/mod.rs
@@ -1,5 +1,6 @@
 mod health;
 mod pages;
+mod timelapses;
 
 use std::{fmt::Display, sync::Arc, time::Duration};
 
@@ -12,6 +13,7 @@ use axum::{
     routing::get,
 };
 use chrono::{DateTime, Utc};
+use tokio::sync::Notify;
 use tower_http::{services::ServeDir, trace::TraceLayer};
 use tracing::error;
 
@@ -22,6 +24,8 @@ pub struct AppState {
     pub metrics: Metrics,
     pub stale_after: Duration,
     pub started_at: DateTime<Utc>,
+    /// Wakes the encoder after a job is queued.
+    pub wake: Arc<Notify>,
 }
 
 pub fn router(state: Arc<AppState>) -> Router {
@@ -33,6 +37,11 @@ pub fn router(state: Arc<AppState>) -> Router {
         .route("/", get(pages::index))
         .route("/days/{date}", get(pages::day))
         .route("/latest.jpg", get(health::latest))
+        .route(
+            "/timelapses",
+            get(timelapses::list).post(timelapses::create),
+        )
+        .route("/timelapses/{file}", get(timelapses::video))
         .route("/style.css", get(stylesheet))
         .nest_service("/frames", frames)
         .layer(TraceLayer::new_for_http())
```

Top of `src/web/timelapses.rs`, above the test module:

```rust
use std::sync::Arc;

use askama::Template;
use axum::{
    Form,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use chrono::{DateTime, NaiveDateTime, Timelike, Utc};
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::{AppState, internal_error, render};
use crate::timelapse::{self, Entry, MP4, Status, Submitted, TimelapseSpec};

/// `<input type="datetime-local">` value format, read and written as UTC.
pub const INPUT_FORMAT: &str = "%Y-%m-%dT%H:%M";

struct Row {
    name: String,
    range: String,
    filter: String,
    status: &'static str,
    badge: &'static str,
    url: Option<String>,
    detail: String,
}

impl From<&Entry> for Row {
    fn from(e: &Entry) -> Self {
        let (status, badge) = match e.status {
            Status::Queued => ("queued", "badge-busy"),
            Status::Running => ("running", "badge-busy"),
            Status::Done => ("done", "badge-ok"),
            Status::Failed => ("failed", "badge-bad"),
        };
        let mut filter = format!("step {}", e.spec.step);
        if let Some((start, end)) = e.spec.hours {
            filter.push_str(&format!(", {start:02}:00\u{2013}{end:02}:00"));
        }
        let detail = match e.status {
            Status::Done => format_size(e.size),
            Status::Failed => e
                .error
                .as_deref()
                .and_then(|err| err.lines().next())
                .unwrap_or_default()
                .to_owned(),
            Status::Queued | Status::Running => String::new(),
        };
        Self {
            name: e.name.clone(),
            range: format!(
                "{} \u{2192} {}",
                e.spec.from.format("%Y-%m-%d %H:%M"),
                e.spec.to.format("%Y-%m-%d %H:%M")
            ),
            filter,
            status,
            badge,
            url: (e.status == Status::Done).then(|| video_url(&e.name)),
            detail,
        }
    }
}

#[derive(Template)]
#[template(path = "timelapses.html")]
struct TimelapsesPage {
    rows: Vec<Row>,
    pending: bool,
    default_from: String,
    default_to: String,
}

/// Every field defaults to empty so missing fields get the same 400 as malformed ones.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CreateForm {
    from: String,
    to: String,
    step: String,
    hour_from: String,
    hour_to: String,
}

impl CreateForm {
    /// Parse the fields. The result still needs `TimelapseSpec::validate`.
    fn spec(&self) -> Result<TimelapseSpec, String> {
        let from = parse_input_time("from", &self.from)?;
        let to = parse_input_time("to", &self.to)?;
        let step = match self.step.trim() {
            "" => 1,
            s => s
                .parse()
                .map_err(|_| "step must be a whole number".to_owned())?,
        };
        let hours = match (self.hour_from.trim(), self.hour_to.trim()) {
            ("", "") => None,
            ("", _) | (_, "") => return Err("set both hour fields or neither".to_owned()),
            (start, end) => Some((parse_hour(start)?, parse_hour(end)?)),
        };
        Ok(TimelapseSpec {
            from,
            to,
            step,
            hours,
        })
    }
}

pub async fn list(State(state): State<Arc<AppState>>) -> Response {
    let entries = match timelapse::list(&state.store) {
        Ok(entries) => entries,
        Err(e) => return internal_error(e),
    };
    let now = Utc::now();
    render(TimelapsesPage {
        pending: entries
            .iter()
            .any(|e| matches!(e.status, Status::Queued | Status::Running)),
        rows: entries.iter().map(Row::from).collect(),
        default_from: now.format("%Y-%m-%dT00:00").to_string(),
        default_to: now.format(INPUT_FORMAT).to_string(),
    })
}

pub async fn create(State(state): State<Arc<AppState>>, Form(form): Form<CreateForm>) -> Response {
    let spec = match form.spec().and_then(|s| s.validate(Utc::now()).map(|()| s)) {
        Ok(spec) => spec,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    match timelapse::submit(&state.store, &spec) {
        Ok(Submitted::Done) => Redirect::to(&video_url(&spec.name())).into_response(),
        Ok(Submitted::Queued) => {
            state.wake.notify_one();
            Redirect::to("/timelapses").into_response()
        }
        Ok(Submitted::NoFrames) => {
            (StatusCode::BAD_REQUEST, "no frames in that range").into_response()
        }
        Err(e) => internal_error(e),
    }
}

/// Serves only `{name}.mp4` for names that parse, so nothing else in `out/` is reachable.
pub async fn video(
    State(state): State<Arc<AppState>>,
    Path(file): Path<String>,
    req: Request,
) -> Response {
    let Some(name) = file
        .strip_suffix(".mp4")
        .filter(|name| TimelapseSpec::parse_name(name).is_some())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match ServeFile::new(state.store.out_file(name, MP4))
        .oneshot(req)
        .await
    {
        Ok(res) => res.into_response(),
        Err(never) => match never {},
    }
}

fn video_url(name: &str) -> String {
    format!("/timelapses/{name}.mp4")
}

/// Accepts `YYYY-MM-DDTHH:MM`, plus seconds which are dropped, as UTC.
fn parse_input_time(field: &str, value: &str) -> Result<DateTime<Utc>, String> {
    let value = value.trim();
    NaiveDateTime::parse_from_str(value, INPUT_FORMAT)
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S"))
        .ok()
        .and_then(|t| t.with_second(0))
        .map(|t| t.and_utc())
        .ok_or_else(|| format!("{field} must look like 2026-10-11T07:05"))
}

fn parse_hour(value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| "hours must be between 0 and 23".to_owned())
}

/// `512 KB`, `12.3 MB`.
fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{} KB", bytes.div_ceil(1_000))
    }
}
```

`src/web/pages.rs`:

```diff
--- a/src/web/pages.rs
+++ b/src/web/pages.rs
@@ -8,7 +8,7 @@ use axum::{
 };
 use chrono::{NaiveDate, Timelike, Utc};
 
-use super::{AppState, health, internal_error, render};
+use super::{AppState, health, internal_error, render, timelapses::INPUT_FORMAT};
 use crate::store::{DaySummary, Frame, parse_day_name};
 
 struct Latest {
@@ -32,6 +32,8 @@ struct DayPage {
     next: Option<NaiveDate>,
     samples: Vec<Frame>,
     frames: Vec<Frame>,
+    /// End of the "Timelapse this day" range: 23:59, or the current minute for today.
+    timelapse_to: String,
 }
 
 pub async fn index(State(state): State<Arc<AppState>>) -> Response {
@@ -76,7 +78,9 @@ pub async fn day(State(state): State<Arc<AppState>>, Path(date): Path<String>) -
     // `days` is newest first.
     let next = days.iter().rev().map(|d| d.date).find(|d| *d > date);
     let prev = days.iter().map(|d| d.date).find(|d| *d < date);
+    let end_of_day = date.and_hms_opt(23, 59, 0).expect("valid time").and_utc();
     render(DayPage {
+        timelapse_to: end_of_day.min(Utc::now()).format(INPUT_FORMAT).to_string(),
         date,
         prev,
         next,
```

`src/main.rs` (temporary: Task 5 shares this `Notify` with the encoder):

```diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -13,7 +13,7 @@ use std::{io::IsTerminal, sync::Arc};
 use anyhow::{Context, Result, bail};
 use chrono::Utc;
 use clap::Parser;
-use tokio::{net::TcpListener, signal};
+use tokio::{net::TcpListener, signal, sync::Notify};
 use tracing::info;
 use tracing_subscriber::EnvFilter;
 
@@ -49,6 +49,7 @@ async fn main() -> Result<()> {
         metrics,
         stale_after: cfg.stale_after(),
         started_at: Utc::now(),
+        wake: Arc::new(Notify::new()),
     });
     let listener = TcpListener::bind(cfg.listen_addr)
         .await
```

`templates/base.html`:

```diff
--- a/templates/base.html
+++ b/templates/base.html
@@ -5,10 +5,12 @@
   <meta name="viewport" content="width=device-width, initial-scale=1">
   <title>{% block title %}Window timelapse{% endblock %}</title>
   <link rel="stylesheet" href="/style.css">
+  {% block head %}{% endblock %}
 </head>
 <body>
   <header class="site-header">
     <a class="site-title" href="/">Window timelapse</a>
+    <nav class="site-nav"><a href="/timelapses">Timelapses</a></nav>
   </header>
   <main class="page">
     {% block content %}{% endblock %}
```

`templates/day.html`:

```diff
--- a/templates/day.html
+++ b/templates/day.html
@@ -11,6 +11,12 @@
 
 <p class="meta">{{ frames.len() }} frames</p>
 
+<form class="inline-form" method="post" action="/timelapses">
+  <input type="hidden" name="from" value="{{ date }}T00:00">
+  <input type="hidden" name="to" value="{{ timelapse_to }}">
+  <button class="button" type="submit">Timelapse this day</button>
+</form>
+
 <div class="grid">
   {% for frame in samples %}
   <figure class="tile">
```

`templates/timelapses.html`:

```html
{% extends "base.html" %}

{% block title %}Timelapses &middot; Window timelapse{% endblock %}

{% block head %}{% if pending %}<meta http-equiv="refresh" content="10">{% endif %}{% endblock %}

{% block content %}
<h1>Timelapses</h1>

<form class="form" method="post" action="/timelapses">
  <label>From (UTC) <input type="datetime-local" name="from" value="{{ default_from }}" required></label>
  <label>To (UTC) <input type="datetime-local" name="to" value="{{ default_to }}" required></label>
  <label>Every Nth frame <input type="number" name="step" value="1" min="1"></label>
  <label>Daily hours from (UTC) <input type="number" name="hour_from" min="0" max="23" placeholder="any"></label>
  <label>Daily hours to (UTC) <input type="number" name="hour_to" min="0" max="23" placeholder="any"></label>
  <button class="button" type="submit">Create</button>
  <p class="meta form-hint">Daily hours apply to every day in the range, e.g. 6 to 20 for daylight or 20 to 6 for nights. Leave both blank for all hours.</p>
</form>

{% if rows.is_empty() %}
<p class="empty">No timelapses yet.</p>
{% else %}
<table class="table">
  <thead><tr><th>Range (UTC)</th><th>Frames</th><th>Status</th><th></th></tr></thead>
  <tbody>
    {% for row in rows %}
    <tr>
      <td>{% if let Some(url) = row.url %}<a href="{{ url }}">{{ row.range }}</a>{% else %}{{ row.range }}{% endif %}</td>
      <td>{{ row.filter }}</td>
      <td><span class="badge {{ row.badge }}" title="{{ row.name }}">{{ row.status }}</span></td>
      <td class="meta">{{ row.detail }}</td>
    </tr>
    {% endfor %}
  </tbody>
</table>
{% endif %}
{% endblock %}
```

`assets/style.css`:

```diff
--- a/assets/style.css
+++ b/assets/style.css
@@ -37,6 +37,9 @@ a { color: var(--accent); text-decoration: none; }
 a:hover { text-decoration: underline; }
 
 .site-header {
+  display: flex;
+  align-items: center;
+  gap: var(--gap);
   padding: 0.75rem var(--gap);
   border-bottom: 1px solid var(--border);
 }
@@ -72,6 +75,42 @@ a:hover { text-decoration: underline; }
 
 .badge-ok { background: var(--ok); }
 .badge-bad { background: var(--bad); }
+.badge-busy { background: var(--muted); }
+
+.button {
+  padding: 0.25rem 0.75rem;
+  border: 1px solid var(--border);
+  border-radius: var(--radius);
+  background: var(--bg);
+  color: var(--accent);
+  font: inherit;
+  cursor: pointer;
+}
+
+.button:hover { border-color: var(--accent); }
+
+.form {
+  display: flex;
+  flex-wrap: wrap;
+  align-items: end;
+  gap: var(--gap);
+  margin-bottom: calc(var(--gap) * 2);
+}
+
+.form label { display: flex; flex-direction: column; gap: 0.25rem; color: var(--muted); }
+
+.form input {
+  padding: 0.25rem 0.5rem;
+  border: 1px solid var(--border);
+  border-radius: var(--radius);
+  background: var(--bg);
+  color: var(--fg);
+  font: inherit;
+}
+
+.form-hint { flex-basis: 100%; margin: 0; }
+
+.inline-form { margin-bottom: var(--gap); }
 
 .table { border-collapse: collapse; min-width: 20rem; }
 .table th, .table td { padding: 0.25rem 0.75rem; border-bottom: 1px solid var(--border); text-align: left; }
```

- [ ] **Step 4: Run lint and tests**

Run: `cargo fmt && make lint && make test`
Expected: clean lint; `test result: ok. 75 passed; 0 failed; 2 ignored`.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock assets/style.css src/main.rs src/web templates
git commit -m "feat: add timelapse UI and routes

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Wire the encoder into main and document M2

Spawns the encoder next to capture, sharing the `Notify` with the web state, and exits the process if the encoder task ends. Creates `out/` at startup, removes the dead-code allow, and documents M2 in the README.

**Files:**
- Modify: `src/main.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: `Encoder::new(cfg, store, metrics, wake).run()` (Task 3), `AppState::wake` (Task 4), `Store::out_dir` (Task 1).

- [ ] **Step 1: Wire main**

```diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,3 @@
-#![allow(dead_code)] // Removed in Task 5, once the timelapse code is wired into main.
 mod capture;
 mod config;
 mod encoder;
@@ -17,7 +16,10 @@ use tokio::{net::TcpListener, signal, sync::Notify};
 use tracing::info;
 use tracing_subscriber::EnvFilter;
 
-use crate::{capture::Capturer, config::Config, metrics::Metrics, store::Store, web::AppState};
+use crate::{
+    capture::Capturer, config::Config, encoder::Encoder, metrics::Metrics, store::Store,
+    web::AppState,
+};
 
 #[tokio::main]
 async fn main() -> Result<()> {
@@ -30,8 +32,9 @@ async fn main() -> Result<()> {
 
     let cfg = Arc::new(Config::parse());
     let store = Store::new(&cfg.data_dir);
-    std::fs::create_dir_all(store.frames_dir())
-        .with_context(|| format!("creating {}", store.frames_dir().display()))?;
+    for dir in [store.frames_dir(), store.out_dir()] {
+        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
+    }
     let removed = store
         .cleanup_parts()
         .context("removing stale .part files")?;
@@ -43,13 +46,16 @@ async fn main() -> Result<()> {
     }
 
     let capture = tokio::spawn(Capturer::new(cfg.clone(), store.clone(), metrics.clone()).run());
+    let wake = Arc::new(Notify::new());
+    let encoder =
+        tokio::spawn(Encoder::new(cfg.clone(), store.clone(), metrics.clone(), wake.clone()).run());
 
     let state = Arc::new(AppState {
         store,
         metrics,
         stale_after: cfg.stale_after(),
         started_at: Utc::now(),
-        wake: Arc::new(Notify::new()),
+        wake,
     });
     let listener = TcpListener::bind(cfg.listen_addr)
         .await
@@ -61,6 +67,7 @@ async fn main() -> Result<()> {
     tokio::select! {
         res = async { server.await } => res.context("http server failed")?,
         res = capture => bail!("capture task exited unexpectedly: {res:?}"),
+        res = encoder => bail!("encoder task exited unexpectedly: {res:?}"),
     }
     info!("shut down");
     Ok(())
```

- [ ] **Step 2: Run lint and tests**

Run: `cargo fmt && make lint && make test`
Expected: clean lint with no dead-code warnings now that the allow is gone; `test result: ok. 75 passed; 0 failed; 2 ignored`.

- [ ] **Step 3: Smoke-test the binary with real ffmpeg**

Generate an hour of frames for a past day, start the service with an unreachable camera (capture errors in the log are expected), submit, and check the result:

```bash
mkdir -p data/frames/2026-10-13
ffmpeg -loglevel error -f lavfi -i testsrc=size=640x360:rate=1 -frames:v 60 -q:v 4 data/frames/2026-10-13/f%02d.jpg
i=0; for f in data/frames/2026-10-13/f*.jpg; do mv "$f" "data/frames/2026-10-13/20261013-12$(printf %02d $i).jpg"; i=$((i+1)); done
cargo build
RTSP_URL='rtsp://u:pw@127.0.0.1:1/x' DATA_DIR=./data LISTEN_ADDR=127.0.0.1:18080 ./target/debug/rtsp-time-lapser &
sleep 1
curl -s -o /dev/null -w '%{http_code} %{redirect_url}\n' -d 'from=2026-10-13T12:00&to=2026-10-13T12:59&step=2' http://127.0.0.1:18080/timelapses
sleep 3
ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames,width,height -of default=nw=1 http://127.0.0.1:18080/timelapses/20261013-1200_20261013-1259_s2.mp4
curl -s -o /dev/null -w '%{http_code} %{redirect_url}\n' -d 'from=2026-10-13T12:00&to=2026-10-13T12:59&step=2' http://127.0.0.1:18080/timelapses
kill %1; rm -rf data
```

Expected: first POST `303 …/timelapses`; ffprobe `width=640`, `height=360`, `nb_read_frames=30`; second POST `303 …/timelapses/20261013-1200_20261013-1259_s2.mp4`; the log has `encoded timelapse name="20261013-1200_20261013-1259_s2"`. Optionally open `http://127.0.0.1:18080/timelapses` in a browser to check the page while it runs.

- [ ] **Step 4: Document M2**

````diff
--- a/README.md
+++ b/README.md
@@ -1,6 +1,6 @@
 # rtsp-time-lapser
 
-Captures one frame per minute from an RTSP camera to disk and serves a small HTML UI to browse them. Timelapse assembly (M2) and daily archiving (M3) come later. See [the design spec](docs/superpowers/specs/2026-10-12-rtsp-time-lapser-design.md).
+Captures one frame per minute from an RTSP camera to disk, serves a small HTML UI to browse them, and assembles any time range into an MP4 timelapse from the UI. Daily archiving (M3) comes later. See [the design spec](docs/superpowers/specs/2026-10-12-rtsp-time-lapser-design.md) and [the M2 timelapse spec](docs/superpowers/specs/2026-10-14-m2-timelapse-assembly-design.md).
 
 ## Configuration
 
@@ -17,10 +17,29 @@ Every setting is an env var (or the matching `--flag`):
 | `SCALE_WIDTH` | unset | Scale frames to this width |
 | `STALE_AFTER_SECS` | `180` | `/healthz` fails when the newest frame is older |
 | `FFMPEG_BIN` | `ffmpeg` | Path to ffmpeg |
+| `TIMELAPSE_FPS` | `30` | Timelapse frame rate |
+| `TIMELAPSE_CRF` | `20` | x264 quality, 0 (lossless) to 51 |
+| `TIMELAPSE_PRESET` | `veryfast` | x264 preset, `ultrafast` to `veryslow` |
+| `TIMELAPSE_MAX_WIDTH` | unset | Downscale timelapses to at most this width |
+| `ENCODE_TIMEOUT_SECS` | `7200` | Kill an encode after this long |
+| `OUT_RETENTION_DAYS` | `7` | Delete finished and failed timelapses after this many days |
 | `RUST_LOG` | `info` | Log filter |
 
 Frames land in `DATA_DIR/frames/YYYY-MM-DD/YYYYMMDD-HHMM.jpg` (UTC).
 
+## Timelapses
+
+`/timelapses` takes a UTC range, a step (keep every Nth frame) and an optional UTC hour window (e.g. 6 to 20, or 20 to 6 for nights only). Each day page has a "Timelapse this day" button. Jobs are encoded one at a time and survive restarts; the files live in `DATA_DIR/out/`:
+
+| File | Meaning |
+|---|---|
+| `{name}.job` | Queued: an ffconcat list of the selected frames |
+| `{name}.mp4.part` | Encoding |
+| `{name}.mp4` | Done |
+| `{name}.err` | Failed: ffmpeg's error. Submit the same range again to retry |
+
+`{name}` encodes the selection, e.g. `20261011-0000_20261011-2359_s1_h06-20`, so submitting the same selection twice never encodes twice.
+
 ## Development
 
 ```bash
@@ -34,6 +53,12 @@ Try a capture against the real camera (needs `ffmpeg` locally):
 TEST_RTSP_URL='rtsp://viewer:pass@10.0.0.5:554/stream1' cargo test real_camera -- --ignored --nocapture
 ```
 
+Encode generated frames with real ffmpeg (needs `ffmpeg` and `ffprobe` locally):
+
+```bash
+cargo test real_ffmpeg_encode -- --ignored --nocapture
+```
+
 Run locally:
 
 ```bash
@@ -107,7 +132,7 @@ spec:
 
 ## Monitoring
 
-`/metrics` exposes `timelapse_last_frame_timestamp_seconds`, `timelapse_captures_total{result}` and `timelapse_capture_duration_seconds`. Suggested alert:
+`/metrics` exposes `timelapse_last_frame_timestamp_seconds`, `timelapse_captures_total{result}`, `timelapse_capture_duration_seconds`, `timelapse_encodes_total{result}` and `timelapse_encode_duration_seconds`. Suggested alert:
 
 ```promql
 time() - timelapse_last_frame_timestamp_seconds > 300
````

- [ ] **Step 5: Commit**

```bash
git add src/main.rs README.md
git commit -m "feat: run the timelapse encoder alongside capture

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

- [ ] **Step 6: After deploy (manual, user)**

Build and push the image (`make image TAG=…`), roll it out, then encode one full day at default settings and note the encode time from `timelapse_encode_duration_seconds` or the log. That number sizes M3's archive encodes. No manifest changes are needed: every new setting has a default, and `out/` lives on the existing PVC.

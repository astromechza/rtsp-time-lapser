# rtsp-time-lapser M1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship M1: a Rust service that captures one RTSP frame per minute to disk, serves an HTML browse UI, `/healthz` and `/metrics`, packaged as a container with Kustomize manifests and CI.

**Architecture:** Single binary crate. A tokio interval loop shells out to `ffmpeg` per frame (`.part` then rename). An axum router serves askama-rendered pages and raw frames via `ServeDir`. All state lives in filenames under `DATA_DIR/frames/YYYY-MM-DD/YYYYMMDD-HHMM.jpg` (UTC).

**Tech Stack:** Rust 1.99 (edition 2024), tokio, axum 0.8, tower-http 0.7, askama 0.16, prometheus-client 0.25, clap 4.6, chrono, anyhow, tracing. Alpine 3.24 image with ffmpeg and tini. Kustomize. GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-10-12-rtsp-time-lapser-design.md`

All code in this plan was compiled, linted (`clippy -D warnings`) and tested on Rust 1.99.0 during planning, including every intermediate task state. The Dockerfile was **not** built (no Docker on the planning machine); Task 7 is its first real check.

## Global Constraints

- Rust `1.99`, edition `2024`; `rust-toolchain.toml` pins `channel = "1.99"` with `rustfmt` and `clippy`; `Cargo.toml` sets `rust-version = "1.99"`.
- Single binary crate `rtsp-time-lapser`, no workspace. All tests are unit tests inside their modules (`#[cfg(test)] mod tests`), because a binary crate cannot be imported from `tests/`.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` must pass at the end of every task (`make lint`).
- Never log or print the raw RTSP URL. Use `RtspUrl`'s `Display`/`Debug` (which redact) and pass ffmpeg error text through `config::redact`.
- UTC everywhere for frame and day names.
- Atomic writes: write `*.part`, then rename. Startup deletes stale `*.part` files.
- HTML UI only, no JSON API. No inline styles; CSS variables and reusable classes in `assets/style.css`; no JavaScript.
- Durations in config are integer seconds (`*_SECS`).
- Image: builder `rust:1.99.0-alpine3.24`, runtime `alpine:3.24`, UID/GID `10001`. `make image` defaults to `PLATFORMS=linux/amd64`.
- No Ingress in `deploy/`; the README documents an example overlay with an auth warning.
- Commit messages use `feat:`, `fix:` or `chore:` prefixes and end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` when made by an agent.
- `src/main.rs` keeps `mod` lines alphabetical (`cargo fmt` reorders them) and carries `#![allow(dead_code)]` until Task 6.

## Review Focus

- **Credentials echoed by ffmpeg:** ffmpeg's stderr often repeats the input URL. Every logged capture error must show `***`, never the password. Pinned by `config::tests::redact_scrubs_url_userinfo_and_password` and `Capturer::tick`'s use of `redact`, plus the Task 6 smoke check of the startup log.
- **Password containing an unencoded `@`:** a user who skips percent-encoding should still not leak the password via `Display` or logs. Pinned by `config::tests::unencoded_at_sign_in_password_is_still_hidden` (Task 1).
- **Pod restart within the same minute:** a second capture for an existing minute must leave the existing frame untouched. Pinned by `capture::tests::existing_frame_is_skipped` (Task 4).
- **Foreign files in the data volume** (`lost+found`, `.DS_Store`, notes, half-written `.part` files): they must never appear in the UI or be counted, and must never be deleted, except `.part`. Pinned by `store::tests::list_days_is_newest_first_with_counts_and_ignores_junk` and `cleanup_parts_removes_only_part_files` (Task 2).
- **Path traversal via `/frames`:** `..`, encoded or not, must not escape the frames dir. Pinned by `web::tests::frames_block_path_traversal` (Task 5).

---

### Task 1: Scaffold, tooling, CI and config

Creates the crate, toolchain pin, Makefile, CI, and the `config` module (CLI/env parsing, `RtspUrl` redaction).

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`, `Makefile`, `.github/workflows/ci.yml`
- Create: `src/main.rs` (temporary stub)
- Create: `src/config.rs`

**Interfaces:**
- Produces: `config::Config` (pub fields `rtsp_url: RtspUrl`, `data_dir: PathBuf`, `listen_addr: SocketAddr`, `capture_interval_secs: u64`, `capture_timeout_secs: u64`, `jpeg_quality: u8`, `scale_width: Option<u32>`, `stale_after_secs: u64`, `ffmpeg_bin: PathBuf`; methods `capture_interval()`, `capture_timeout()`, `stale_after()` returning `std::time::Duration`).
- Produces: `config::RtspUrl` (`new(impl Into<String>)`, `expose() -> &str`, redacting `Display`/`Debug`), `config::redact(text: &str, url: &RtspUrl) -> String`.
- Produces (test-only): `config::test_config(data_dir: &Path, ffmpeg_bin: &Path) -> Config`, URL `rtsp://viewer:s3cret@10.0.0.5:554/stream1`, `capture_timeout_secs: 5`.

- [ ] **Step 1: Create project files**

`Cargo.toml` (all dependencies for M1 are declared up front so later tasks don't touch it):

```toml
[package]
name = "rtsp-time-lapser"
version = "0.1.0"
edition = "2024"
rust-version = "1.99"
publish = false

[dependencies]
anyhow = "1.0.104"
askama = "0.16"
axum = "0.8.9"
chrono = { version = "0.4.45", default-features = false, features = ["clock", "std"] }
clap = { version = "4.6", features = ["derive", "env"] }
prometheus-client = "0.25"
tokio = { version = "1.53", features = ["fs", "macros", "net", "process", "rt-multi-thread", "signal", "time"] }
tower-http = { version = "0.7", features = ["fs", "trace"] }
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }

[dev-dependencies]
http-body-util = "0.1"
pretty_assertions = "1.4"
tempfile = "3.27"
tower = { version = "0.5", features = ["util"] }
```

`rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.99"
components = ["rustfmt", "clippy"]
```

`.gitignore`:

```
/target
```

`Makefile` (recipe lines are indented with a TAB):

```make
IMAGE ?= ghcr.io/astromechza/rtsp-time-lapser
TAG ?= $(shell git describe --tags --always --dirty)
PLATFORMS ?= linux/amd64

.PHONY: fmt lint test image

fmt:
	cargo fmt

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

image:
	docker buildx build --platform $(PLATFORMS) -t $(IMAGE):$(TAG) --push .
```

`.github/workflows/ci.yml`:

```yaml
name: ci

on:
  push:
    branches: [main]
  pull_request:

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: "1.99"
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
```

`src/main.rs`:

```rust
// Modules are added task by task. `dead_code` is allowed until Task 6 wires everything into main.
#![allow(dead_code)]

mod config;

use clap::Parser;

fn main() {
    let cfg = config::Config::parse();
    println!("{cfg:?}");
}
```

- [ ] **Step 2: Write the failing config tests**

Create `src/config.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn display_and_debug_hide_credentials() {
        let url = RtspUrl::new("rtsp://viewer:s3cret@10.0.0.5:554/stream1");
        assert_eq!(url.to_string(), "rtsp://***@10.0.0.5:554/stream1");
        assert_eq!(
            format!("{url:?}"),
            "RtspUrl(rtsp://***@10.0.0.5:554/stream1)"
        );
    }

    #[test]
    fn display_without_credentials_is_unchanged() {
        let url = RtspUrl::new("rtsp://10.0.0.5:554/stream1");
        assert_eq!(url.to_string(), "rtsp://10.0.0.5:554/stream1");
    }

    #[test]
    fn at_sign_in_path_is_not_userinfo() {
        let url = RtspUrl::new("rtsp://10.0.0.5/live@main");
        assert_eq!(url.to_string(), "rtsp://10.0.0.5/live@main");
    }

    #[test]
    fn unencoded_at_sign_in_password_is_still_hidden() {
        let url = RtspUrl::new("rtsp://viewer:p@ss@10.0.0.5/stream1");
        assert_eq!(url.to_string(), "rtsp://***@10.0.0.5/stream1");
        assert_eq!(redact("auth failed for p@ss", &url), "auth failed for ***");
    }

    #[test]
    fn redact_scrubs_url_userinfo_and_password() {
        let url = RtspUrl::new("rtsp://viewer:s3cret@10.0.0.5:554/stream1");
        let stderr = "rtsp://viewer:s3cret@10.0.0.5:554/stream1: 401 for viewer:s3cret, pw s3cret";
        assert_eq!(redact(stderr, &url), "***: 401 for ***, pw ***");
    }

    #[test]
    fn parses_from_args_with_defaults() {
        let cfg = Config::try_parse_from(["x", "--rtsp-url", "rtsp://cam/s"]).unwrap();
        assert_eq!(cfg.capture_interval(), Duration::from_secs(60));
        assert_eq!(cfg.jpeg_quality, 4);
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
    }

    #[test]
    fn rejects_out_of_range_quality() {
        let res =
            Config::try_parse_from(["x", "--rtsp-url", "rtsp://cam/s", "--jpeg-quality", "1"]);
        assert!(res.is_err());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test config`
Expected: compile errors (`Config`, `RtspUrl`, `redact`, `test_config` not found). The first run also downloads dependencies and writes `Cargo.lock`.

- [ ] **Step 4: Implement config**

Insert above the test module in `src/config.rs`:

```rust
use std::{convert::Infallible, fmt, net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};

use clap::Parser;

/// Runtime configuration. Every flag can also be set through its environment variable.
#[derive(Parser, Debug, Clone)]
#[command(version, about)]
pub struct Config {
    /// Camera stream URL, including credentials: rtsp://user:pass@host:554/path
    #[arg(long, env = "RTSP_URL", hide_env_values = true)]
    pub rtsp_url: RtspUrl,

    /// Root directory for all on-disk state.
    #[arg(long, env = "DATA_DIR", default_value = "/data")]
    pub data_dir: PathBuf,

    /// HTTP bind address.
    #[arg(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    pub listen_addr: SocketAddr,

    /// Seconds between capture attempts.
    #[arg(long, env = "CAPTURE_INTERVAL_SECS", default_value_t = 60,
          value_parser = clap::value_parser!(u64).range(1..))]
    pub capture_interval_secs: u64,

    /// Seconds before a hanging ffmpeg is killed.
    #[arg(long, env = "CAPTURE_TIMEOUT_SECS", default_value_t = 30,
          value_parser = clap::value_parser!(u64).range(1..))]
    pub capture_timeout_secs: u64,

    /// ffmpeg JPEG quality (-q:v), 2 (best) to 31 (worst).
    #[arg(long, env = "JPEG_QUALITY", default_value_t = 4,
          value_parser = clap::value_parser!(u8).range(2..=31))]
    pub jpeg_quality: u8,

    /// Scale frames to this width (keeping aspect ratio) when set.
    #[arg(long, env = "SCALE_WIDTH")]
    pub scale_width: Option<u32>,

    /// /healthz fails when the newest frame is older than this many seconds.
    #[arg(long, env = "STALE_AFTER_SECS", default_value_t = 180,
          value_parser = clap::value_parser!(u64).range(1..))]
    pub stale_after_secs: u64,

    /// Path to the ffmpeg binary.
    #[arg(long, env = "FFMPEG_BIN", default_value = "ffmpeg")]
    pub ffmpeg_bin: PathBuf,
}

impl Config {
    pub fn capture_interval(&self) -> Duration {
        Duration::from_secs(self.capture_interval_secs)
    }

    pub fn capture_timeout(&self) -> Duration {
        Duration::from_secs(self.capture_timeout_secs)
    }

    pub fn stale_after(&self) -> Duration {
        Duration::from_secs(self.stale_after_secs)
    }
}

/// An RTSP URL that never prints its credentials. Use `expose()` to get the raw value.
#[derive(Clone, PartialEq, Eq)]
pub struct RtspUrl(String);

impl RtspUrl {
    pub fn new(url: impl Into<String>) -> Self {
        Self(url.into())
    }

    /// The raw URL, credentials included. Only pass this to ffmpeg.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The `user:pass` part of the authority, if any.
    fn userinfo(&self) -> Option<&str> {
        let (_, rest) = self.0.split_once("://")?;
        let authority = rest.split('/').next().unwrap_or(rest);
        authority.rsplit_once('@').map(|(userinfo, _)| userinfo)
    }
}

impl FromStr for RtspUrl {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

impl fmt::Display for RtspUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.userinfo() {
            Some(userinfo) => f.write_str(&self.0.replacen(userinfo, "***", 1)),
            None => f.write_str(&self.0),
        }
    }
}

impl fmt::Debug for RtspUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RtspUrl({self})")
    }
}

/// Replace the URL, its userinfo and its password with `***` anywhere in `text`.
pub fn redact(text: &str, url: &RtspUrl) -> String {
    let mut out = text.replace(url.expose(), "***");
    if let Some(userinfo) = url.userinfo() {
        out = out.replace(userinfo, "***");
        if let Some((_, password)) = userinfo.split_once(':')
            && !password.is_empty()
        {
            out = out.replace(password, "***");
        }
    }
    out
}

#[cfg(test)]
pub fn test_config(data_dir: &std::path::Path, ffmpeg_bin: &std::path::Path) -> Config {
    Config {
        rtsp_url: RtspUrl::new("rtsp://viewer:s3cret@10.0.0.5:554/stream1"),
        data_dir: data_dir.to_path_buf(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        capture_interval_secs: 60,
        capture_timeout_secs: 5,
        jpeg_quality: 4,
        scale_width: None,
        stale_after_secs: 180,
        ffmpeg_bin: ffmpeg_bin.to_path_buf(),
    }
}
```

Notes: `hide_env_values = true` stops `--help` printing the credentials. `Display` replaces only the authority's userinfo (`rsplit_once('@')` on the authority), so `@` in a path or an unencoded `@` in the password are both handled.

- [ ] **Step 5: Run the tests and lints to verify they pass**

Run: `make lint test`
Expected: 8 tests pass, no fmt diff, no clippy warnings.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: scaffold crate with config parsing and credential redaction"
```


### Task 2: On-disk store

All filesystem layout and queries: frame paths, name parsing, day/frame listing, newest frame, `.part` cleanup. Sync `std::fs` is fine: a day directory holds at most 1440 entries.

**Files:**
- Create: `src/store.rs` (implementation and its `#[cfg(test)] mod tests`)
- Modify: `src/main.rs` (module list)

**Interfaces:**
- Produces: `store::Store` (`Clone`; `new(impl Into<PathBuf>)`, `frames_dir() -> PathBuf`, `day_dir(NaiveDate) -> PathBuf`, `frame_path(DateTime<Utc>) -> PathBuf`, `list_days() -> io::Result<Vec<DaySummary>>` newest first, `list_frames(NaiveDate) -> io::Result<Vec<Frame>>` oldest first and `NotFound` for a missing day, `newest_frame() -> io::Result<Option<Frame>>`, `cleanup_parts() -> io::Result<usize>`).
- Produces: `store::Frame { pub name: String, pub time: DateTime<Utc> }` with `url() -> String` (`/frames/YYYY-MM-DD/NAME`); `store::DaySummary { pub date: NaiveDate, pub frame_count: usize }`.
- Produces: `store::part_path(&Path) -> PathBuf` (appends `.part`), `store::parse_day_name(&str) -> Option<NaiveDate>`, `store::parse_frame_name(&str) -> Option<DateTime<Utc>>`.

- [ ] **Step 1: Write the failing tests**

Create `src/store.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use pretty_assertions::assert_eq;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn frame_path_round_trips_through_parse() {
        let store = Store::new("/data");
        let t = at(2026, 10, 11, 7, 5);
        let path = store.frame_path(t);
        assert_eq!(
            path,
            PathBuf::from("/data/frames/2026-10-11/20261011-0705.jpg")
        );
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(parse_frame_name(name), Some(t));
    }

    #[test]
    fn parse_frame_name_rejects_junk() {
        for name in [
            "20261011-0705.jpg.part",
            "20261011-0705.png",
            "20261011-2460.jpg",
            "2026101-07055.jpg",
            "+2026101-0705.jpg",
            "notes.txt",
            ".jpg",
        ] {
            assert_eq!(parse_frame_name(name), None, "{name}");
        }
    }

    #[test]
    fn parse_day_name_rejects_junk() {
        assert_eq!(
            parse_day_name("2026-10-11"),
            NaiveDate::from_ymd_opt(2026, 10, 11)
        );
        assert_eq!(parse_day_name("2026-13-01"), None);
        assert_eq!(parse_day_name("lost+found"), None);
        assert_eq!(parse_day_name("2026-1-011"), None);
    }

    #[test]
    fn list_days_is_newest_first_with_counts_and_ignores_junk() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        touch(&store.frame_path(at(2026, 10, 10, 0, 0)));
        touch(&store.frame_path(at(2026, 10, 11, 0, 0)));
        touch(&store.frame_path(at(2026, 10, 11, 0, 1)));
        touch(
            &store
                .day_dir(NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())
                .join("notes.txt"),
        );
        fs::create_dir_all(store.frames_dir().join("lost+found")).unwrap();

        let days = store.list_days().unwrap();
        assert_eq!(
            days,
            vec![
                DaySummary {
                    date: NaiveDate::from_ymd_opt(2026, 10, 11).unwrap(),
                    frame_count: 2
                },
                DaySummary {
                    date: NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
                    frame_count: 1
                },
            ]
        );
    }

    #[test]
    fn list_days_without_frames_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Store::new(dir.path()).list_days().unwrap(), vec![]);
    }

    #[test]
    fn list_frames_missing_day_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = Store::new(dir.path())
            .list_frames(NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn newest_frame_skips_empty_newest_day() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        touch(&store.frame_path(at(2026, 10, 10, 23, 58)));
        touch(&store.frame_path(at(2026, 10, 10, 23, 59)));
        fs::create_dir_all(store.day_dir(NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())).unwrap();

        let newest = store.newest_frame().unwrap().unwrap();
        assert_eq!(newest.time, at(2026, 10, 10, 23, 59));
        assert_eq!(newest.url(), "/frames/2026-10-10/20261010-2359.jpg");
    }

    #[test]
    fn newest_frame_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Store::new(dir.path()).newest_frame().unwrap(), None);
    }

    #[test]
    fn cleanup_parts_removes_only_part_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let frame = store.frame_path(at(2026, 10, 11, 0, 0));
        touch(&frame);
        touch(&part_path(&store.frame_path(at(2026, 10, 11, 0, 1))));
        touch(&dir.path().join("out/x.mp4.part"));
        touch(&dir.path().join("keep.txt"));

        assert_eq!(store.cleanup_parts().unwrap(), 2);
        assert!(frame.exists());
        assert!(dir.path().join("keep.txt").exists());
        assert!(!dir.path().join("out/x.mp4.part").exists());
    }
}
```

Replace `src/main.rs` with (module lines stay alphabetical; `cargo fmt` sorts them):

```rust
// Modules are added task by task. `dead_code` is allowed until Task 6 wires everything into main.
#![allow(dead_code)]

mod config;
mod store;

use clap::Parser;

fn main() {
    let cfg = config::Config::parse();
    println!("{cfg:?}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test store`
Expected: compile errors, `Store`, `parse_frame_name`, `part_path` etc. not found.

- [ ] **Step 3: Implement**

Insert above the test module in `src/store.rs`:

```rust
//! On-disk layout: `DATA_DIR/frames/YYYY-MM-DD/YYYYMMDD-HHMM.jpg`, all UTC.
//! Filenames are the only state. Anything not matching the patterns is ignored.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

const DAY_FORMAT: &str = "%Y-%m-%d";
const FRAME_FORMAT: &str = "%Y%m%d-%H%M";
const PART_SUFFIX: &str = ".part";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub name: String,
    pub time: DateTime<Utc>,
}

impl Frame {
    /// URL path of the raw file, served by `ServeDir` under `/frames`.
    pub fn url(&self) -> String {
        format!("/frames/{}/{}", self.time.format(DAY_FORMAT), self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaySummary {
    pub date: NaiveDate,
    pub frame_count: usize,
}

#[derive(Debug, Clone)]
pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn frames_dir(&self) -> PathBuf {
        self.data_dir.join("frames")
    }

    pub fn day_dir(&self, date: NaiveDate) -> PathBuf {
        self.frames_dir().join(date.format(DAY_FORMAT).to_string())
    }

    pub fn frame_path(&self, time: DateTime<Utc>) -> PathBuf {
        self.day_dir(time.date_naive())
            .join(format!("{}.jpg", time.format(FRAME_FORMAT)))
    }

    /// Days that contain a valid day directory, newest first.
    pub fn list_days(&self) -> io::Result<Vec<DaySummary>> {
        let mut days = Vec::new();
        for date in self.day_dates()? {
            let frame_count = self.list_frames(date)?.len();
            days.push(DaySummary { date, frame_count });
        }
        days.reverse();
        Ok(days)
    }

    /// Valid frames for one day, oldest first. `NotFound` if the day directory is missing.
    pub fn list_frames(&self, date: NaiveDate) -> io::Result<Vec<Frame>> {
        let mut frames = Vec::new();
        for entry in fs::read_dir(self.day_dir(date))? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(time) = parse_frame_name(name) {
                frames.push(Frame {
                    name: name.to_owned(),
                    time,
                });
            }
        }
        frames.sort_by_key(|f| f.time);
        Ok(frames)
    }

    /// The newest valid frame, searching backwards through days until one has frames.
    pub fn newest_frame(&self) -> io::Result<Option<Frame>> {
        for date in self.day_dates()?.into_iter().rev() {
            if let Some(frame) = self.list_frames(date)?.pop() {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }

    /// Delete every `*.part` file under the data dir. Returns how many were removed.
    pub fn cleanup_parts(&self) -> io::Result<usize> {
        remove_parts(&self.data_dir)
    }

    /// Valid day directory dates, oldest first. Empty if the frames dir does not exist.
    fn day_dates(&self) -> io::Result<Vec<NaiveDate>> {
        let entries = match fs::read_dir(self.frames_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut dates = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(date) = entry.file_name().to_str().and_then(parse_day_name) {
                dates.push(date);
            }
        }
        dates.sort();
        Ok(dates)
    }
}

/// Path of the temporary file ffmpeg writes before it is renamed into place.
pub fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(PART_SUFFIX);
    PathBuf::from(s)
}

/// Parse `YYYY-MM-DD`.
pub fn parse_day_name(name: &str) -> Option<NaiveDate> {
    if name.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(name, DAY_FORMAT).ok()
}

/// Parse `YYYYMMDD-HHMM.jpg` as a UTC time.
pub fn parse_frame_name(name: &str) -> Option<DateTime<Utc>> {
    let stem = name.strip_suffix(".jpg")?;
    let all_digits = stem.bytes().enumerate().all(|(i, b)| match i {
        8 => b == b'-',
        _ => b.is_ascii_digit(),
    });
    if stem.len() != 13 || !all_digits {
        return None;
    }
    NaiveDateTime::parse_from_str(stem, FRAME_FORMAT)
        .ok()
        .map(|t| t.and_utc())
}

fn remove_parts(dir: &Path) -> io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            removed += remove_parts(&path)?;
        } else if path.as_os_str().to_string_lossy().ends_with(PART_SUFFIX) {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}
```

- [ ] **Step 4: Run the tests and lints to verify they pass**

Run: `make lint test`
Expected: no fmt diff, no clippy warnings, all tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: add on-disk frame store"
```


### Task 3: Metrics

Prometheus registry with the three M1 metrics. Labels use `Vec<(&str, &str)>`, which `prometheus-client` encodes without a derive.

**Files:**
- Create: `src/metrics.rs` (implementation and its `#[cfg(test)] mod tests`)
- Modify: `src/main.rs` (module list)

**Interfaces:**
- Produces: `metrics::Metrics` (`Clone`, shared values; `new()`, `set_last_frame(i64)`, `record_ok(unix_secs: i64, took: Duration)`, `record_error()`, `record_skipped()`, `encode() -> String`; test-only `captures(&'static str) -> u64`).
- Metric names: `timelapse_last_frame_timestamp_seconds`, `timelapse_captures_total{result}` (registered as `timelapse_captures`; the encoder appends `_total`), `timelapse_capture_duration_seconds`.

- [ ] **Step 1: Write the failing tests**

Create `src/metrics.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_expected_metric_names() {
        let m = Metrics::new();
        m.record_ok(1_760_000_000, Duration::from_secs(2));
        m.record_error();
        let text = m.encode();
        assert!(
            text.contains("timelapse_last_frame_timestamp_seconds 1760000000"),
            "{text}"
        );
        assert!(
            text.contains(r#"timelapse_captures_total{result="ok"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"timelapse_captures_total{result="error"} 1"#),
            "{text}"
        );
        assert!(
            text.contains("timelapse_capture_duration_seconds_count 1"),
            "{text}"
        );
    }
}
```

Replace `src/main.rs` with (module lines stay alphabetical; `cargo fmt` sorts them):

```rust
// Modules are added task by task. `dead_code` is allowed until Task 6 wires everything into main.
#![allow(dead_code)]

mod config;
mod metrics;
mod store;

use clap::Parser;

fn main() {
    let cfg = config::Config::parse();
    println!("{cfg:?}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test metrics`
Expected: compile errors, `Metrics` not found.

- [ ] **Step 3: Implement**

Insert above the test module in `src/metrics.rs`:

```rust
use std::{sync::Arc, time::Duration};

use prometheus_client::{
    encoding::text::encode,
    metrics::{counter::Counter, family::Family, gauge::Gauge, histogram::Histogram},
    registry::Registry,
};

type ResultLabels = Vec<(&'static str, &'static str)>;

/// Prometheus metrics. Cheap to clone; clones share the same values.
#[derive(Clone)]
pub struct Metrics {
    registry: Arc<Registry>,
    last_frame: Gauge,
    captures: Family<ResultLabels, Counter>,
    capture_duration: Histogram,
}

impl Metrics {
    pub fn new() -> Self {
        let last_frame = Gauge::default();
        let captures = Family::<ResultLabels, Counter>::default();
        let capture_duration = Histogram::new([0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0]);

        let mut registry = Registry::default();
        registry.register(
            "timelapse_last_frame_timestamp_seconds",
            "Unix time of the newest captured frame",
            last_frame.clone(),
        );
        // The text encoder appends `_total` to counters.
        registry.register(
            "timelapse_captures",
            "Capture attempts by result",
            captures.clone(),
        );
        registry.register(
            "timelapse_capture_duration_seconds",
            "Duration of successful captures",
            capture_duration.clone(),
        );

        Self {
            registry: Arc::new(registry),
            last_frame,
            captures,
            capture_duration,
        }
    }

    pub fn set_last_frame(&self, unix_secs: i64) {
        self.last_frame.set(unix_secs);
    }

    pub fn record_ok(&self, unix_secs: i64, took: Duration) {
        self.set_last_frame(unix_secs);
        self.capture_duration.observe(took.as_secs_f64());
        self.count("ok");
    }

    pub fn record_error(&self) {
        self.count("error");
    }

    pub fn record_skipped(&self) {
        self.count("skipped");
    }

    #[cfg(test)]
    pub fn captures(&self, result: &'static str) -> u64 {
        self.captures.get_or_create(&vec![("result", result)]).get()
    }

    pub fn encode(&self) -> String {
        let mut out = String::new();
        encode(&mut out, &self.registry).expect("writing to a String cannot fail");
        out
    }

    fn count(&self, result: &'static str) {
        self.captures.get_or_create(&vec![("result", result)]).inc();
    }
}
```

- [ ] **Step 4: Run the tests and lints to verify they pass**

Run: `make lint test`
Expected: no fmt diff, no clippy warnings, all tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: add prometheus metrics"
```


### Task 4: ffmpeg runner and capture loop

`ffmpeg::run` spawns with a timeout and `kill_on_drop`. `Capturer` writes `<frame>.jpg.part`, checks it is non-empty, and renames it into place. Tests use fake ffmpeg shell scripts, so no camera or ffmpeg install is needed.

**Files:**
- Create: `src/ffmpeg.rs`
- Create: `src/capture.rs`
- Modify: `src/main.rs` (module list)

**Interfaces:**
- Consumes: `Config`, `config::redact`, `config::test_config` (Task 1); `Store`, `store::part_path` (Task 2); `Metrics` (Task 3).
- Produces: `ffmpeg::run(cmd: tokio::process::Command, limit: Duration) -> anyhow::Result<()>` (error text includes stderr truncated to 512 bytes; callers redact).
- Produces: `capture::Capturer` (`Clone`; `new(Arc<Config>, Store, Metrics)`, `async run(self)` loops forever, `async tick(&self, DateTime<Utc>)` never fails, `async snap(&self, DateTime<Utc>) -> anyhow::Result<Outcome>`), `capture::Outcome { Captured(PathBuf), Skipped }`.

- [ ] **Step 1: Write the failing tests**

Create `src/ffmpeg.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("héllo", 3), "hé");
    }
}
```

Create `src/capture.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, sync::OnceLock};

    use chrono::TimeZone;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::config::test_config;

    /// Fake ffmpeg scripts, written once. Writing them once, before any test spawns a
    /// process, avoids "text file busy" races between writing and executing a script.
    fn fake_ffmpeg(name: &str) -> PathBuf {
        static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
        let dir = DIR.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let scripts = [
                // The output path is the last argument.
                (
                    "ok",
                    "for a; do out=\"$a\"; done\nprintf 'jpeg' > \"$out\"\n",
                ),
                (
                    "fail",
                    "echo 'cannot open rtsp://viewer:s3cret@10.0.0.5:554/stream1' >&2\nexit 1\n",
                ),
                ("empty", "for a; do out=\"$a\"; done\n: > \"$out\"\n"),
                (
                    "hang",
                    "for a; do out=\"$a\"; done\nprintf 'partial' > \"$out\"\nexec sleep 30\n",
                ),
            ];
            for (name, body) in scripts {
                let path = dir.path().join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            dir
        });
        dir.path().join(name)
    }

    fn capturer(data: &Path, script: &str) -> Capturer {
        let mut cfg = test_config(data, &fake_ffmpeg(script));
        cfg.capture_timeout_secs = 1;
        Capturer::new(Arc::new(cfg), Store::new(data), Metrics::new())
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 7, 5, 42).unwrap()
    }

    fn part_files(data: &Path) -> usize {
        Store::new(data).cleanup_parts().unwrap()
    }

    #[tokio::test]
    async fn success_writes_frame_and_no_part() {
        let dir = tempfile::tempdir().unwrap();
        let c = capturer(dir.path(), "ok");
        c.tick(now()).await;

        let frame = dir.path().join("frames/2026-10-11/20261011-0705.jpg");
        assert_eq!(std::fs::read(&frame).unwrap(), b"jpeg");
        assert_eq!(part_files(dir.path()), 0);
        assert_eq!(c.metrics.captures("ok"), 1);
    }

    #[tokio::test]
    async fn existing_frame_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let c = capturer(dir.path(), "fail");
        let frame = c.store.frame_path(now());
        std::fs::create_dir_all(frame.parent().unwrap()).unwrap();
        std::fs::write(&frame, b"old").unwrap();

        assert_eq!(c.snap(now()).await.unwrap(), Outcome::Skipped);
        assert_eq!(std::fs::read(&frame).unwrap(), b"old");
    }

    #[tokio::test]
    async fn non_zero_exit_leaves_nothing_and_counts_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = capturer(dir.path(), "fail");

        let err = c.snap(now()).await.unwrap_err();
        assert!(format!("{err:#}").contains("exited with"), "{err:#}");
        assert!(!c.store.frame_path(now()).exists());
        assert_eq!(part_files(dir.path()), 0);

        c.tick(now()).await;
        assert_eq!(c.metrics.captures("error"), 1);
    }

    #[tokio::test]
    async fn empty_output_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = capturer(dir.path(), "empty");

        let err = c.snap(now()).await.unwrap_err();
        assert!(format!("{err:#}").contains("empty frame"), "{err:#}");
        assert!(!c.store.frame_path(now()).exists());
        assert_eq!(part_files(dir.path()), 0);
    }

    #[tokio::test]
    async fn hanging_ffmpeg_is_killed_after_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let c = capturer(dir.path(), "hang");

        let started = Instant::now();
        let err = c.snap(now()).await.unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!c.store.frame_path(now()).exists());
        assert_eq!(part_files(dir.path()), 0);
    }

    #[test]
    fn args_include_url_quality_and_optional_scale() {
        let mut cfg = test_config(Path::new("/data"), Path::new("ffmpeg"));
        cfg.scale_width = Some(1920);
        let args = ffmpeg_args(&cfg, Path::new("/data/frames/x.jpg.part"));
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-nostdin",
                "-loglevel",
                "error",
                "-rtsp_transport",
                "tcp",
                "-i",
                "rtsp://viewer:s3cret@10.0.0.5:554/stream1",
                "-frames:v",
                "1",
                "-q:v",
                "4",
                "-vf",
                "scale=1920:-2",
                "-f",
                "image2",
                "-y",
                "/data/frames/x.jpg.part",
            ]
        );
    }

    /// Manual check against a real camera:
    /// `TEST_RTSP_URL=rtsp://... cargo test real_camera -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs ffmpeg and a real camera in TEST_RTSP_URL"]
    async fn real_camera() {
        let url = std::env::var("TEST_RTSP_URL").expect("TEST_RTSP_URL");
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(dir.path(), Path::new("ffmpeg"));
        cfg.rtsp_url = crate::config::RtspUrl::new(url);
        cfg.capture_timeout_secs = 30;
        let c = Capturer::new(Arc::new(cfg), Store::new(dir.path()), Metrics::new());

        let outcome = c.snap(Utc::now()).await.unwrap();
        let Outcome::Captured(path) = outcome else {
            panic!("skipped")
        };
        println!("{} bytes", std::fs::metadata(&path).unwrap().len());
    }
}
```

Replace `src/main.rs` with:

```rust
// Modules are added task by task. `dead_code` is allowed until Task 6 wires everything into main.
#![allow(dead_code)]

mod capture;
mod config;
mod ffmpeg;
mod metrics;
mod store;

use clap::Parser;

fn main() {
    let cfg = config::Config::parse();
    println!("{cfg:?}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test capture ffmpeg`
Expected: compile errors (`truncate`, `Capturer`, `Outcome`, `ffmpeg_args` not found).

- [ ] **Step 3: Implement the ffmpeg runner**

Insert above the test module in `src/ffmpeg.rs`:

```rust
use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{process::Command, time::timeout};

const STDERR_LIMIT: usize = 512;

/// Run `cmd` to completion, killing it after `limit`. Fails on spawn error, timeout or
/// non-zero exit. The error includes stderr (truncated); callers must redact it before logging.
pub async fn run(mut cmd: Command, limit: Duration) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().context("spawning ffmpeg")?;
    // On timeout the future is dropped, which drops the child and kills it.
    let output = match timeout(limit, child.wait_with_output()).await {
        Ok(output) => output.context("waiting for ffmpeg")?,
        Err(_) => bail!("ffmpeg timed out after {}s", limit.as_secs()),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ffmpeg exited with {}: {}",
            output.status,
            truncate(stderr.trim(), STDERR_LIMIT)
        );
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
```

- [ ] **Step 4: Implement the capturer**

Insert above the test module in `src/capture.rs`:

```rust
use std::{ffi::OsString, path::Path, path::PathBuf, sync::Arc};

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use tokio::{
    process::Command,
    time::{Instant, MissedTickBehavior, interval},
};
use tracing::{debug, warn};

use crate::{
    config::{Config, redact},
    ffmpeg,
    metrics::Metrics,
    store::{Store, part_path},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Captured(PathBuf),
    Skipped,
}

#[derive(Clone)]
pub struct Capturer {
    cfg: Arc<Config>,
    store: Store,
    metrics: Metrics,
}

impl Capturer {
    pub fn new(cfg: Arc<Config>, store: Store, metrics: Metrics) -> Self {
        Self {
            cfg,
            store,
            metrics,
        }
    }

    /// Capture forever. The first tick fires immediately; missed ticks are skipped.
    pub async fn run(self) {
        let mut ticker = interval(self.cfg.capture_interval());
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            self.tick(Utc::now()).await;
        }
    }

    /// One capture attempt with metrics and logging. Never fails.
    pub async fn tick(&self, now: DateTime<Utc>) {
        let started = Instant::now();
        match self.snap(now).await {
            Ok(Outcome::Captured(path)) => {
                self.metrics.record_ok(now.timestamp(), started.elapsed());
                debug!(path = %path.display(), "captured frame");
            }
            Ok(Outcome::Skipped) => {
                self.metrics.record_skipped();
                debug!("frame for this minute already exists");
            }
            Err(e) => {
                self.metrics.record_error();
                // Never log the raw error: ffmpeg's stderr can contain the URL.
                warn!(error = %redact(&format!("{e:#}"), &self.cfg.rtsp_url), "capture failed");
            }
        }
    }

    /// Capture the frame for `now`, writing `<frame>.part` then renaming it into place.
    pub async fn snap(&self, now: DateTime<Utc>) -> Result<Outcome> {
        let out = self.store.frame_path(now);
        if tokio::fs::try_exists(&out).await? {
            return Ok(Outcome::Skipped);
        }
        if let Some(dir) = out.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let part = part_path(&out);
        let result = self.write_frame(&part, &out).await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&part).await;
        }
        result.map(|()| Outcome::Captured(out))
    }

    async fn write_frame(&self, part: &Path, out: &Path) -> Result<()> {
        let mut cmd = Command::new(&self.cfg.ffmpeg_bin);
        cmd.args(ffmpeg_args(&self.cfg, part));
        ffmpeg::run(cmd, self.cfg.capture_timeout()).await?;
        let len = tokio::fs::metadata(part).await?.len();
        ensure!(len > 0, "ffmpeg produced an empty frame");
        tokio::fs::rename(part, out).await?;
        Ok(())
    }
}

fn ffmpeg_args(cfg: &Config, part: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-nostdin",
        "-loglevel",
        "error",
        "-rtsp_transport",
        "tcp",
        "-i",
    ]
    .map(OsString::from)
    .into();
    args.push(cfg.rtsp_url.expose().into());
    args.extend(["-frames:v", "1", "-q:v"].map(OsString::from));
    args.push(cfg.jpeg_quality.to_string().into());
    if let Some(width) = cfg.scale_width {
        args.push("-vf".into());
        args.push(format!("scale={width}:-2").into());
    }
    args.extend(["-f", "image2", "-y"].map(OsString::from));
    args.push(part.into());
    args
}
```

Notes: the `hang` fake script uses `exec sleep` so killing the child kills the sleeper too. Scripts are written once through a `OnceLock` to avoid `ETXTBSY` ("text file busy") races between writing and executing.

- [ ] **Step 5: Run the tests and lints to verify they pass**

Run: `make lint test`
Expected: all tests pass (`real_camera` reports ignored), no warnings. The hang test takes about 1 s.

- [ ] **Step 6 (optional, manual): Capture one real frame**

With `ffmpeg` installed locally and a view-only camera account:

```bash
TEST_RTSP_URL='rtsp://viewer:pass@CAMERA:554/PATH' cargo test real_camera -- --ignored --nocapture
```

Expected: PASS with a byte count printed. This also covers the Notion task "test one-frame ffmpeg capture by hand".

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: add ffmpeg runner and capture loop"
```


### Task 5: HTTP UI, health and metrics endpoints

axum router: HTML pages (askama), `/latest.jpg`, `/frames` via `ServeDir` with immutable caching, `/style.css`, `/healthz`, `/metrics`. Probes sit outside the `TraceLayer`.

**Files:**
- Create: `src/web/mod.rs`, `src/web/health.rs`, `src/web/pages.rs`
- Create: `templates/base.html`, `templates/index.html`, `templates/day.html`
- Create: `assets/style.css`
- Modify: `src/main.rs` (module list)

**Interfaces:**
- Consumes: `Store`, `Frame`, `DaySummary`, `parse_day_name` (Task 2); `Metrics` (Task 3).
- Produces: `web::AppState { pub store: Store, pub metrics: Metrics, pub stale_after: std::time::Duration, pub started_at: DateTime<Utc> }`, `web::router(Arc<AppState>) -> axum::Router`.
- Internal: `health::check(newest: Option<DateTime<Utc>>, now, started_at, stale_after: Duration) -> Result<(), String>`, shared by `/healthz` and the index badge.

- [ ] **Step 1: Write the failing tests**

Create `src/web/health.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    const STALE: Duration = Duration::from_secs(180);

    #[test]
    fn check_cases() {
        let now = Utc::now();
        let old_start = now - TimeDelta::hours(1);
        assert!(check(Some(now - TimeDelta::seconds(60)), now, old_start, STALE).is_ok());
        assert!(check(Some(now - TimeDelta::seconds(181)), now, old_start, STALE).is_err());
        assert!(check(None, now, old_start, STALE).is_err());
        assert!(check(None, now, now - TimeDelta::seconds(10), STALE).is_ok());
        assert!(check(Some(now - TimeDelta::days(2)), now, now, STALE).is_ok());
        // A frame from the future (clock step) counts as fresh.
        assert!(check(Some(now + TimeDelta::seconds(30)), now, old_start, STALE).is_ok());
    }
}
```

Create `src/web/pages.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone};
    use pretty_assertions::assert_eq;

    use super::*;

    fn frame(h: u32, m: u32) -> Frame {
        let time: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 10, 11, h, m, 0).unwrap();
        Frame {
            name: time.format("%Y%m%d-%H%M.jpg").to_string(),
            time,
        }
    }

    #[test]
    fn hourly_samples_take_first_frame_per_hour() {
        let frames = vec![
            frame(0, 3),
            frame(0, 4),
            frame(2, 0),
            frame(2, 59),
            frame(23, 59),
        ];
        let names: Vec<_> = hourly_samples(&frames)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(
            names,
            [
                "20261011-0003.jpg",
                "20261011-0200.jpg",
                "20261011-2359.jpg"
            ]
        );
    }

    #[test]
    fn format_age_units() {
        assert_eq!(format_age(-5), "0s");
        assert_eq!(format_age(42), "42s");
        assert_eq!(format_age(300), "5m");
        assert_eq!(format_age(3 * 3600 + 12 * 60), "3h 12m");
        assert_eq!(format_age(2 * 86_400 + 3 * 3600 + 59), "2d 3h");
    }
}
```

Create `src/web/mod.rs` containing only the module declarations and the router tests:

```rust
mod health;
mod pages;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::{body::Body, http::Request};
    use chrono::TimeDelta;
    use http_body_util::BodyExt;
    use pretty_assertions::assert_eq;
    use tower::ServiceExt;

    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path());
            Self { dir, store }
        }

        fn add_frame(&self, time: DateTime<Utc>) {
            let path = self.store.frame_path(time);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"jpeg").unwrap();
        }

        fn app(&self, started_at: DateTime<Utc>) -> Router {
            router(Arc::new(AppState {
                store: self.store.clone(),
                metrics: Metrics::new(),
                stale_after: Duration::from_secs(180),
                started_at,
            }))
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let res = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    fn long_ago() -> DateTime<Utc> {
        Utc::now() - TimeDelta::hours(1)
    }

    #[tokio::test]
    async fn healthz_ok_with_fresh_frame() {
        let f = Fixture::new();
        f.add_frame(Utc::now());
        let (status, _, body) = get(f.app(long_ago()), "/healthz").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn healthz_fails_with_stale_frame() {
        let f = Fixture::new();
        f.add_frame(Utc::now() - TimeDelta::minutes(10));
        let (status, _, body) = get(f.app(long_ago()), "/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("stale"), "{body}");
    }

    #[tokio::test]
    async fn healthz_grace_period_after_start() {
        let f = Fixture::new();
        let (status, _, _) = get(f.app(Utc::now()), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = get(f.app(long_ago()), "/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn latest_redirects_to_newest_frame() {
        let f = Fixture::new();
        let t = DateTime::parse_from_rfc3339("2026-10-11T07:05:00Z")
            .unwrap()
            .to_utc();
        f.add_frame(t - TimeDelta::minutes(1));
        f.add_frame(t);
        let (status, headers, _) = get(f.app(long_ago()), "/latest.jpg").await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            headers[header::LOCATION],
            "/frames/2026-10-11/20261011-0705.jpg"
        );
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn latest_404_without_frames() {
        let f = Fixture::new();
        let (status, _, _) = get(f.app(long_ago()), "/latest.jpg").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn frames_served_with_immutable_cache() {
        let f = Fixture::new();
        let t = DateTime::parse_from_rfc3339("2026-10-11T07:05:00Z")
            .unwrap()
            .to_utc();
        f.add_frame(t);
        let (status, headers, body) =
            get(f.app(long_ago()), "/frames/2026-10-11/20261011-0705.jpg").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "jpeg");
        assert_eq!(
            headers[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );

        let (status, headers, _) = get(f.app(long_ago()), "/frames/2026-10-11/missing.jpg").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(headers.get(header::CACHE_CONTROL).is_none());
    }

    #[tokio::test]
    async fn frames_block_path_traversal() {
        let f = Fixture::new();
        std::fs::write(f.path().join("secret.txt"), "secret").unwrap();
        std::fs::create_dir_all(f.store.frames_dir()).unwrap();
        for uri in ["/frames/../secret.txt", "/frames/%2e%2e/secret.txt"] {
            let (status, _, body) = get(f.app(long_ago()), uri).await;
            assert_ne!(status, StatusCode::OK, "{uri}");
            assert!(!body.contains("secret"), "{uri}");
        }
    }

    #[tokio::test]
    async fn index_lists_days() {
        let f = Fixture::new();
        f.add_frame(Utc::now());
        let (status, _, body) = get(f.app(long_ago()), "/").await;
        assert_eq!(status, StatusCode::OK);
        let today = Utc::now().format("%Y-%m-%d").to_string();
        assert!(body.contains(&format!("/days/{today}")), "{body}");
        assert!(body.contains("healthy"), "{body}");
    }

    #[tokio::test]
    async fn index_without_frames() {
        let f = Fixture::new();
        let (status, _, body) = get(f.app(long_ago()), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No frames captured yet."), "{body}");
    }

    #[tokio::test]
    async fn day_page_shows_samples_and_navigation() {
        let f = Fixture::new();
        let d10 = DateTime::parse_from_rfc3339("2026-10-10T23:59:00Z")
            .unwrap()
            .to_utc();
        let d11 = DateTime::parse_from_rfc3339("2026-10-11T07:05:00Z")
            .unwrap()
            .to_utc();
        f.add_frame(d10);
        f.add_frame(d11);
        f.add_frame(d11 + TimeDelta::minutes(1));
        let (status, _, body) = get(f.app(long_ago()), "/days/2026-10-11").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("/days/2026-10-10"), "{body}");
        assert!(body.contains("2 frames"), "{body}");
        assert!(
            body.contains("/frames/2026-10-11/20261011-0706.jpg"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn day_page_rejects_bad_and_missing_dates() {
        let f = Fixture::new();
        let (status, _, _) = get(f.app(long_ago()), "/days/bad").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = get(f.app(long_ago()), "/days/2026-10-11").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn metrics_endpoint_exposes_gauge() {
        let f = Fixture::new();
        let (status, headers, body) = get(f.app(long_ago()), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/openmetrics-text")
        );
        assert!(
            body.contains("timelapse_last_frame_timestamp_seconds"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn stylesheet_is_served() {
        let f = Fixture::new();
        let (status, headers, body) = get(f.app(long_ago()), "/style.css").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
        assert!(body.contains(":root"));
    }
}
```

Replace `src/main.rs` with:

```rust
// Modules are added task by task. `dead_code` is allowed until Task 6 wires everything into main.
#![allow(dead_code)]

mod capture;
mod config;
mod ffmpeg;
mod metrics;
mod store;
mod web;

use clap::Parser;

fn main() {
    let cfg = config::Config::parse();
    println!("{cfg:?}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test web`
Expected: compile errors (`router`, `AppState`, `check`, `hourly_samples`, `format_age` not found).

- [ ] **Step 3: Add the stylesheet and templates**

`assets/style.css` (CSS variables, reusable classes, light/dark via `prefers-color-scheme`):

```css
:root {
  --bg: #fafafa;
  --fg: #1f2328;
  --muted: #656d76;
  --border: #d0d7de;
  --accent: #0969da;
  --ok: #1a7f37;
  --bad: #cf222e;
  --radius: 6px;
  --gap: 1rem;
  --font: system-ui, -apple-system, "Segoe UI", sans-serif;
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d1117;
    --fg: #e6edf3;
    --muted: #8d96a0;
    --border: #30363d;
    --accent: #4493f8;
    --ok: #3fb950;
    --bad: #f85149;
  }
}

* { box-sizing: border-box; }

body {
  margin: 0;
  background: var(--bg);
  color: var(--fg);
  font-family: var(--font);
  line-height: 1.5;
}

a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }

.site-header {
  padding: 0.75rem var(--gap);
  border-bottom: 1px solid var(--border);
}

.site-title { font-weight: 600; color: var(--fg); }

.page {
  max-width: 1200px;
  margin: 0 auto;
  padding: var(--gap);
}

.frame {
  display: block;
  width: 100%;
  height: auto;
  border-radius: var(--radius);
  border: 1px solid var(--border);
}

.frame-large { max-height: 70vh; object-fit: contain; }

.meta, .empty { color: var(--muted); }

.badge {
  display: inline-block;
  margin-left: 0.5rem;
  padding: 0 0.5rem;
  border-radius: var(--radius);
  color: var(--bg);
  font-size: 0.85em;
}

.badge-ok { background: var(--ok); }
.badge-bad { background: var(--bad); }

.table { border-collapse: collapse; min-width: 20rem; }
.table th, .table td { padding: 0.25rem 0.75rem; border-bottom: 1px solid var(--border); text-align: left; }
.table .num { text-align: right; }

.day-nav {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--gap);
}

.day-nav h1 { margin: 0; font-size: 1.5rem; }

.grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
  gap: var(--gap);
}

.tile { margin: 0; }
.tile figcaption { color: var(--muted); font-size: 0.9em; }

.all-frames { margin-top: calc(var(--gap) * 2); }

.frame-list {
  columns: 12rem;
  font-family: ui-monospace, monospace;
  font-size: 0.9em;
}
```

`templates/base.html`:

```html
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{% block title %}Window timelapse{% endblock %}</title>
  <link rel="stylesheet" href="/style.css">
</head>
<body>
  <header class="site-header">
    <a class="site-title" href="/">Window timelapse</a>
  </header>
  <main class="page">
    {% block content %}{% endblock %}
  </main>
</body>
</html>
```

`templates/index.html`:

```html
{% extends "base.html" %}

{% block content %}
<section class="latest">
  {% if let Some(latest) = latest %}
  <img class="frame frame-large" src="/latest.jpg" alt="Latest frame">
  <p class="meta">
    {{ latest.time }} UTC &middot; {{ latest.age }} ago
    {% if healthy %}<span class="badge badge-ok">healthy</span>{% else %}<span class="badge badge-bad">stale</span>{% endif %}
  </p>
  {% else %}
  <p class="empty">No frames captured yet.</p>
  {% endif %}
</section>

<section>
  <h2>Days</h2>
  {% if days.is_empty() %}
  <p class="empty">No days yet.</p>
  {% else %}
  <table class="table">
    <thead><tr><th>Date (UTC)</th><th class="num">Frames</th></tr></thead>
    <tbody>
      {% for day in days %}
      <tr><td><a href="/days/{{ day.date }}">{{ day.date }}</a></td><td class="num">{{ day.frame_count }}</td></tr>
      {% endfor %}
    </tbody>
  </table>
  {% endif %}
</section>
{% endblock %}
```

`templates/day.html`:

```html
{% extends "base.html" %}

{% block title %}{{ date }} &middot; Window timelapse{% endblock %}

{% block content %}
<nav class="day-nav">
  {% if let Some(prev) = prev %}<a href="/days/{{ prev }}">&larr; {{ prev }}</a>{% else %}<span></span>{% endif %}
  <h1>{{ date }}</h1>
  {% if let Some(next) = next %}<a href="/days/{{ next }}">{{ next }} &rarr;</a>{% else %}<span></span>{% endif %}
</nav>

<p class="meta">{{ frames.len() }} frames</p>

<div class="grid">
  {% for frame in samples %}
  <figure class="tile">
    <a href="{{ frame.url() }}"><img class="frame" src="{{ frame.url() }}" alt="{{ frame.name }}" loading="lazy"></a>
    <figcaption>{{ frame.time.format("%H:%M") }}</figcaption>
  </figure>
  {% endfor %}
</div>

<details class="all-frames">
  <summary>All frames</summary>
  <ul class="frame-list">
    {% for frame in frames %}
    <li><a href="{{ frame.url() }}">{{ frame.name }}</a></li>
    {% endfor %}
  </ul>
</details>
{% endblock %}
```

- [ ] **Step 4: Implement the router**

Replace the two `mod` lines at the top of `src/web/mod.rs` with the following (it starts with the same two lines):

```rust
mod health;
mod pages;

use std::{fmt::Display, sync::Arc, time::Duration};

use askama::Template;
use axum::{
    Router,
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::error;

use crate::{metrics::Metrics, store::Store};

pub struct AppState {
    pub store: Store,
    pub metrics: Metrics,
    pub stale_after: Duration,
    pub started_at: DateTime<Utc>,
}

pub fn router(state: Arc<AppState>) -> Router {
    let frames = Router::new()
        .fallback_service(ServeDir::new(state.store.frames_dir()))
        .layer(middleware::map_response(immutable_cache));

    Router::new()
        .route("/", get(pages::index))
        .route("/days/{date}", get(pages::day))
        .route("/latest.jpg", get(health::latest))
        .route("/style.css", get(stylesheet))
        .nest_service("/frames", frames)
        .layer(TraceLayer::new_for_http())
        // Probes are added after the trace layer so they stay out of the logs.
        .route("/healthz", get(health::healthz))
        .route("/metrics", get(health::metrics))
        .with_state(state)
}

async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../assets/style.css"),
    )
}

/// Frames never change once written, so successful responses can be cached forever.
async fn immutable_cache(mut res: Response) -> Response {
    if res.status().is_success() {
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    }
    res
}

fn render(page: impl Template) -> Response {
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => internal_error(e),
    }
}

fn internal_error(e: impl Display) -> Response {
    error!(error = %e, "request failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
```

Note: `.layer(TraceLayer)` only wraps routes added before it, which is how the probes stay out of the request logs.

- [ ] **Step 5: Implement health, latest and metrics handlers**

Insert above the test module in `src/web/health.rs`:

```rust
use std::{sync::Arc, time::Duration};

use axum::{
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use chrono::{DateTime, Utc};

use super::{AppState, internal_error};

/// Healthy when the newest frame is younger than `stale_after`, or when no frame is that
/// young but the process itself started less than `stale_after` ago.
pub fn check(
    newest: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    started_at: DateTime<Utc>,
    stale_after: Duration,
) -> Result<(), String> {
    let fresh = |t: DateTime<Utc>| (now - t).to_std().unwrap_or_default() < stale_after;
    match newest {
        Some(t) if fresh(t) => Ok(()),
        _ if fresh(started_at) => Ok(()),
        Some(t) => Err(format!(
            "stale: newest frame is {}s old",
            (now - t).num_seconds()
        )),
        None => Err("stale: no frames".to_owned()),
    }
}

pub async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let newest = match state.store.newest_frame() {
        Ok(frame) => frame.map(|f| f.time),
        Err(e) => return internal_error(e),
    };
    match check(newest, Utc::now(), state.started_at, state.stale_after) {
        Ok(()) => "ok\n".into_response(),
        Err(reason) => (StatusCode::SERVICE_UNAVAILABLE, format!("{reason}\n")).into_response(),
    }
}

pub async fn latest(State(state): State<Arc<AppState>>) -> Response {
    match state.store.newest_frame() {
        Ok(Some(frame)) => (
            [(header::CACHE_CONTROL, "no-store")],
            Redirect::temporary(&frame.url()),
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal_error(e),
    }
}

pub async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        state.metrics.encode(),
    )
}
```

- [ ] **Step 6: Implement the pages**

Insert above the test module in `src/web/pages.rs`:

```rust
use std::{io, sync::Arc};

use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{NaiveDate, Timelike, Utc};

use super::{AppState, health, internal_error, render};
use crate::store::{DaySummary, Frame, parse_day_name};

struct Latest {
    time: String,
    age: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    latest: Option<Latest>,
    healthy: bool,
    days: Vec<DaySummary>,
}

#[derive(Template)]
#[template(path = "day.html")]
struct DayPage {
    date: NaiveDate,
    prev: Option<NaiveDate>,
    next: Option<NaiveDate>,
    samples: Vec<Frame>,
    frames: Vec<Frame>,
}

pub async fn index(State(state): State<Arc<AppState>>) -> Response {
    let (newest, days) = match (state.store.newest_frame(), state.store.list_days()) {
        (Ok(newest), Ok(days)) => (newest, days),
        (Err(e), _) | (_, Err(e)) => return internal_error(e),
    };
    let now = Utc::now();
    let healthy = health::check(
        newest.as_ref().map(|f| f.time),
        now,
        state.started_at,
        state.stale_after,
    )
    .is_ok();
    let latest = newest.map(|f| Latest {
        time: f.time.format("%Y-%m-%d %H:%M").to_string(),
        age: format_age((now - f.time).num_seconds()),
    });
    render(IndexPage {
        latest,
        healthy,
        days,
    })
}

pub async fn day(State(state): State<Arc<AppState>>, Path(date): Path<String>) -> Response {
    let Some(date) = parse_day_name(&date) else {
        return (StatusCode::BAD_REQUEST, "date must be YYYY-MM-DD").into_response();
    };
    let frames = match state.store.list_frames(date) {
        Ok(frames) => frames,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => return internal_error(e),
    };
    let days = match state.store.list_days() {
        Ok(days) => days,
        Err(e) => return internal_error(e),
    };
    // `days` is newest first.
    let next = days.iter().rev().map(|d| d.date).find(|d| *d > date);
    let prev = days.iter().map(|d| d.date).find(|d| *d < date);
    render(DayPage {
        date,
        prev,
        next,
        samples: hourly_samples(&frames),
        frames,
    })
}

/// The first frame of each hour that has frames. `frames` must be sorted by time.
fn hourly_samples(frames: &[Frame]) -> Vec<Frame> {
    let mut samples: Vec<Frame> = Vec::new();
    for frame in frames {
        if samples
            .last()
            .is_none_or(|s| s.time.hour() != frame.time.hour())
        {
            samples.push(frame.clone());
        }
    }
    samples
}

/// Human-readable age: `42s`, `5m`, `3h 12m`, `2d 3h`. Negative ages count as zero.
fn format_age(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}
```

- [ ] **Step 7: Run the tests and lints to verify they pass**

Run: `make lint test`
Expected: all tests pass (40 total, 1 ignored), no warnings.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat: add HTML UI, healthz and metrics endpoints"
```


### Task 6: Wire up main and smoke-test

Startup sequence (create frames dir, clean `.part` files, seed the gauge), capture task, HTTP server, graceful shutdown, and exit if the capture task ever ends.

**Files:**
- Modify: `src/main.rs` (full replacement; removes `#![allow(dead_code)]`)

**Interfaces:**
- Consumes: everything from Tasks 1–5.

- [ ] **Step 1: Replace `src/main.rs`**

```rust
mod capture;
mod config;
mod ffmpeg;
mod metrics;
mod store;
mod web;

use std::{io::IsTerminal, sync::Arc};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use tokio::{net::TcpListener, signal};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::{capture::Capturer, config::Config, metrics::Metrics, store::Store, web::AppState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let cfg = Arc::new(Config::parse());
    let store = Store::new(&cfg.data_dir);
    std::fs::create_dir_all(store.frames_dir())
        .with_context(|| format!("creating {}", store.frames_dir().display()))?;
    let removed = store
        .cleanup_parts()
        .context("removing stale .part files")?;
    info!(removed, "removed stale .part files");

    let metrics = Metrics::new();
    if let Some(frame) = store.newest_frame().context("finding newest frame")? {
        metrics.set_last_frame(frame.time.timestamp());
    }

    let capture = tokio::spawn(Capturer::new(cfg.clone(), store.clone(), metrics.clone()).run());

    let state = Arc::new(AppState {
        store,
        metrics,
        stale_after: cfg.stale_after(),
        started_at: Utc::now(),
    });
    let listener = TcpListener::bind(cfg.listen_addr)
        .await
        .with_context(|| format!("binding {}", cfg.listen_addr))?;
    info!(addr = %cfg.listen_addr, camera = %cfg.rtsp_url, "listening");

    let server =
        axum::serve(listener, web::router(state)).with_graceful_shutdown(shutdown_signal());
    tokio::select! {
        res = async { server.await } => res.context("http server failed")?,
        res = capture => bail!("capture task exited unexpectedly: {res:?}"),
    }
    info!("shut down");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("installing Ctrl+C handler");
    };
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
```

- [ ] **Step 2: Lint and test**

Run: `make lint test`
Expected: no dead-code warnings now that everything is used; all tests pass.

- [ ] **Step 3: Smoke-run with a fake camera**

Needs `ffmpeg` installed locally. Create a fake ffmpeg that renders a test pattern:

```bash
mkdir -p /tmp/rtl-smoke
printf '#!/bin/sh\nfor a; do out="$a"; done\nffmpeg -loglevel error -f lavfi -i testsrc=size=640x360 -frames:v 1 -f image2 -y "$out"\n' > /tmp/rtl-smoke/fake-ffmpeg
chmod +x /tmp/rtl-smoke/fake-ffmpeg
```

Run the service:

```bash
RTSP_URL='rtsp://u:pw@cam/s' DATA_DIR=/tmp/rtl-smoke/data LISTEN_ADDR=127.0.0.1:18080 FFMPEG_BIN=/tmp/rtl-smoke/fake-ffmpeg cargo run
```

In another shell:

```bash
curl -s -o /dev/null -w '%{http_code}\n' localhost:18080/healthz
curl -s -o /dev/null -w '%{http_code} %{redirect_url}\n' localhost:18080/latest.jpg
curl -s localhost:18080/metrics | grep -v '^#'
```

Expected: `200`; `307 http://localhost:18080/frames/<today>/<YYYYMMDD-HHMM>.jpg`; `timelapse_captures_total{result="ok"} 1`. Open `http://localhost:18080/` in a browser and check the latest frame, days table and day page render. The startup log line shows `camera=rtsp://***@cam/s`, never `pw`. Press Ctrl+C: the log shows `shut down` and the process exits 0.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: wire capture loop and http server into main"
```


### Task 7: Container image

Multi-stage Alpine build: a static musl binary on `alpine:3.24` with `ffmpeg` and `tini`, running as UID 10001.

**Files:**
- Create: `Dockerfile`, `.dockerignore`

- [ ] **Step 1: Write `.dockerignore`**

```
.git
.github
target
docs
deploy
rust-toolchain.toml
```

- [ ] **Step 2: Write `Dockerfile`**

```dockerfile
# syntax=docker/dockerfile:1
FROM rust:1.99.0-alpine3.24 AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# rust-toolchain.toml is excluded by .dockerignore so the image's own 1.99.0 toolchain is used.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/rtsp-time-lapser /rtsp-time-lapser

FROM alpine:3.24
RUN apk add --no-cache ffmpeg tini \
    && addgroup -S -g 10001 app \
    && adduser -S -D -H -u 10001 -G app app
COPY --from=build /rtsp-time-lapser /usr/local/bin/rtsp-time-lapser
USER 10001:10001
ENV DATA_DIR=/data
EXPOSE 8080
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/rtsp-time-lapser"]
```

- [ ] **Step 3: Build and run it locally**

Needs Docker with buildx (not installed on the planning machine, so this is the first real check of the Dockerfile).

```bash
docker buildx build --platform linux/amd64 -t rtsp-time-lapser:dev --load .
docker run --rm rtsp-time-lapser:dev --help
docker run --rm --entrypoint ffmpeg rtsp-time-lapser:dev -version
```

Expected: the build succeeds; `--help` lists all flags with `[env: RTSP_URL]` and no value shown; ffmpeg prints its version.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: add container image"
```


### Task 8: Kubernetes manifests and README

Kustomize base (namespace, PVC, Deployment, Service) plus an example Secret, and a README covering config, image build, deploy and the ingress overlay.

**Files:**
- Create: `deploy/kustomization.yaml`, `deploy/namespace.yaml`, `deploy/pvc.yaml`, `deploy/deployment.yaml`, `deploy/service.yaml`, `deploy/secret.example.yaml`
- Create: `README.md`

- [ ] **Step 1: Write the manifests**

`deploy/kustomization.yaml`:

```yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
namespace: rtsp-time-lapser
resources:
  - namespace.yaml
  - pvc.yaml
  - deployment.yaml
  - service.yaml
images:
  - name: ghcr.io/astromechza/rtsp-time-lapser
    newTag: latest
```

`deploy/namespace.yaml`:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: rtsp-time-lapser
```

`deploy/pvc.yaml`:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: rtsp-time-lapser-data
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 20Gi
```

`deploy/deployment.yaml`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rtsp-time-lapser
  labels:
    app.kubernetes.io/name: rtsp-time-lapser
spec:
  replicas: 1
  # Never run two capture loops against the same PVC.
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app.kubernetes.io/name: rtsp-time-lapser
  template:
    metadata:
      labels:
        app.kubernetes.io/name: rtsp-time-lapser
      annotations:
        prometheus.io/scrape: "true"
        prometheus.io/port: "8080"
        prometheus.io/path: /metrics
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 10001
        runAsGroup: 10001
        fsGroup: 10001
      containers:
        - name: rtsp-time-lapser
          image: ghcr.io/astromechza/rtsp-time-lapser
          env:
            - name: RTSP_URL
              valueFrom:
                secretKeyRef:
                  name: rtsp-time-lapser
                  key: RTSP_URL
            - name: DATA_DIR
              value: /data
          ports:
            - name: http
              containerPort: 8080
          securityContext:
            readOnlyRootFilesystem: true
            allowPrivilegeEscalation: false
            capabilities:
              drop: [ALL]
          resources:
            requests:
              cpu: 100m
              memory: 64Mi
            limits:
              memory: 512Mi
          startupProbe:
            httpGet: { path: /healthz, port: http }
            periodSeconds: 10
            failureThreshold: 30
          livenessProbe:
            httpGet: { path: /healthz, port: http }
            periodSeconds: 60
            failureThreshold: 10
          readinessProbe:
            httpGet: { path: /healthz, port: http }
            periodSeconds: 30
          volumeMounts:
            - name: data
              mountPath: /data
            - name: tmp
              mountPath: /tmp
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: rtsp-time-lapser-data
        - name: tmp
          emptyDir: {}
```

`deploy/service.yaml`:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: rtsp-time-lapser
spec:
  selector:
    app.kubernetes.io/name: rtsp-time-lapser
  ports:
    - name: http
      port: 80
      targetPort: http
```

`deploy/secret.example.yaml`:

```yaml
# Example only: not referenced by kustomization.yaml. Create the real Secret with:
#   kubectl -n rtsp-time-lapser create secret generic rtsp-time-lapser \
#     --from-literal=RTSP_URL='rtsp://viewer:password@10.0.0.5:554/stream1'
# Percent-encode special characters in the username and password.
apiVersion: v1
kind: Secret
metadata:
  name: rtsp-time-lapser
type: Opaque
stringData:
  RTSP_URL: rtsp://viewer:password@10.0.0.5:554/stream1
```

- [ ] **Step 2: Validate the kustomization renders**

Run: `kubectl kustomize deploy`
Expected: four resources (Namespace, PersistentVolumeClaim, Deployment, Service), all in namespace `rtsp-time-lapser`, image `ghcr.io/astromechza/rtsp-time-lapser:latest`. No Secret and no Ingress.

- [ ] **Step 3: Write `README.md`**

````markdown
# rtsp-time-lapser

Captures one frame per minute from an RTSP camera to disk and serves a small HTML UI to browse them. Timelapse assembly (M2) and daily archiving (M3) come later. See [the design spec](docs/superpowers/specs/2026-10-12-rtsp-time-lapser-design.md).

## Configuration

Every setting is an env var (or the matching `--flag`):

| Env | Default | Purpose |
|---|---|---|
| `RTSP_URL` | required | `rtsp://user:pass@host:554/path`, special characters percent-encoded |
| `DATA_DIR` | `/data` | Root of all on-disk state |
| `LISTEN_ADDR` | `0.0.0.0:8080` | HTTP bind address |
| `CAPTURE_INTERVAL_SECS` | `60` | Seconds between captures |
| `CAPTURE_TIMEOUT_SECS` | `30` | Kill a hanging ffmpeg after this long |
| `JPEG_QUALITY` | `4` | ffmpeg `-q:v`, 2 (best) to 31 |
| `SCALE_WIDTH` | unset | Scale frames to this width |
| `STALE_AFTER_SECS` | `180` | `/healthz` fails when the newest frame is older |
| `FFMPEG_BIN` | `ffmpeg` | Path to ffmpeg |
| `RUST_LOG` | `info` | Log filter |

Frames land in `DATA_DIR/frames/YYYY-MM-DD/YYYYMMDD-HHMM.jpg` (UTC).

## Development

```bash
make lint
make test
```

Try a capture against the real camera (needs `ffmpeg` locally):

```bash
TEST_RTSP_URL='rtsp://viewer:pass@10.0.0.5:554/stream1' cargo test real_camera -- --ignored --nocapture
```

Run locally:

```bash
RTSP_URL='rtsp://viewer:pass@10.0.0.5:554/stream1' DATA_DIR=./data LISTEN_ADDR=127.0.0.1:8080 cargo run
```

## Image

Built and pushed locally with buildx (CI only runs lint and tests):

```bash
make image
make image PLATFORMS=linux/amd64,linux/arm64 TAG=v0.1.0
```

## Deploy

```bash
kubectl apply -f deploy/namespace.yaml
kubectl -n rtsp-time-lapser create secret generic rtsp-time-lapser \
  --from-literal=RTSP_URL='rtsp://viewer:pass@10.0.0.5:554/stream1'
kubectl apply -k deploy
```

Set the image tag in `deploy/kustomization.yaml` (`images[0].newTag`). Use a view-only camera account.

### Ingress

No Ingress ships with the base. **The UI has no authentication: anyone who can reach it sees the camera frames.** Keep it on the LAN or put auth in front. Example overlay:

```yaml
# kustomization.yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - github.com/astromechza/rtsp-time-lapser/deploy?ref=v0.1.0
  - ingress.yaml
```

```yaml
# ingress.yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: rtsp-time-lapser
  namespace: rtsp-time-lapser
spec:
  rules:
    - host: timelapse.example.lan
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: rtsp-time-lapser
                port:
                  name: http
```

## Monitoring

`/metrics` exposes `timelapse_last_frame_timestamp_seconds`, `timelapse_captures_total{result}` and `timelapse_capture_duration_seconds`. Suggested alert:

```promql
time() - timelapse_last_frame_timestamp_seconds > 300
```
````

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: add kubernetes manifests and README"
```

- [ ] **Step 5 (manual, with the user): Deploy**

1. `make image TAG=v0.1.0`, then set `newTag: v0.1.0` in `deploy/kustomization.yaml`.
2. Follow the README's Deploy section (namespace, Secret, `kubectl apply -k deploy`).
3. `kubectl -n rtsp-time-lapser port-forward svc/rtsp-time-lapser 8080:80` and check `/` shows a fresh frame.
4. Run the Notion failure-mode checks: unplug the camera (`captures_total{result="error"}` climbs, `/healthz` goes 503 after 3 min) and delete the pod mid-capture (a new pod starts and frames resume).


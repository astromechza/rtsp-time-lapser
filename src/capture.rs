use std::{ffi::OsString, path::Path, path::PathBuf, sync::Arc};

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use tokio::{
    process::Command,
    time::{Instant, MissedTickBehavior, interval},
};
use tracing::{debug, warn};

use crate::{
    config::Config,
    ffmpeg,
    metrics::Metrics,
    store::{Store, part_path},
};

/// Logged capture errors are cut to this many bytes.
const ERROR_LOG_LIMIT: usize = 512;

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
                let error = ffmpeg::describe_error(&e, &self.cfg.rtsp_url, ERROR_LOG_LIMIT);
                warn!(%error, "capture failed");
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

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{config::test_config, ffmpeg::fake_ffmpeg};

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
    async fn long_stderr_is_redacted_before_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let c = capturer(dir.path(), "long");

        let err = c.snap(now()).await.unwrap_err();
        let msg = ffmpeg::describe_error(&err, &c.cfg.rtsp_url, ERROR_LOG_LIMIT);
        assert!(!msg.contains("viewer"), "{msg}");
        assert!(!msg.contains("s3c"), "{msg}");
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

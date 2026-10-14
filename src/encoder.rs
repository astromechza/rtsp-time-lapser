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

use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{process::Command, time::timeout};

use crate::config::{RtspUrl, redact};

/// Run `cmd` to completion, killing it after `limit`. Fails on spawn error, timeout or
/// non-zero exit. The error includes the full stderr; callers must redact, then truncate it,
/// before logging.
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
        bail!("ffmpeg exited with {}: {}", output.status, stderr.trim());
    }
    Ok(())
}

/// Loggable error text, at most `limit` bytes. Never log the raw error: ffmpeg's stderr can
/// contain the URL. Redacts before truncating, so a cut can never leave a partial URL behind.
pub fn describe_error(e: &anyhow::Error, url: &RtspUrl, limit: usize) -> String {
    truncate(&redact(&format!("{e:#}"), url), limit).to_owned()
}

/// Cut `s` to at most `max` bytes on a char boundary.
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Fake ffmpeg scripts for tests, written once. Writing them once, before any test spawns
/// a process, avoids "text file busy" races between writing and executing a script.
#[cfg(test)]
pub fn fake_ffmpeg(name: &str) -> std::path::PathBuf {
    use std::{os::unix::fs::PermissionsExt, sync::OnceLock};

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
            // The URL starts at stderr byte 495, so a 512-byte cut lands inside the password.
            (
                "long",
                "printf '%s rtsp://viewer:s3cret@10.0.0.5:554/stream1' \"$(printf '%494s' '' | tr ' ' x)\" >&2\nexit 1\n",
            ),
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

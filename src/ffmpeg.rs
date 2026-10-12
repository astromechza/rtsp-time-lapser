use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{process::Command, time::timeout};

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

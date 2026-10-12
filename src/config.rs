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

    /// Split into `(scheme, userinfo, host_and_path)` when the URL has `user:pass@`.
    /// Uses the last `@` so unencoded `@`, `/`, `?` or `#` in the password still count as
    /// userinfo. Errs towards redacting too much: `host:port/path@x` is treated as userinfo.
    fn parts(&self) -> Option<(&str, &str, &str)> {
        let (scheme, rest) = self.0.split_once("://")?;
        let (userinfo, host) = rest.rsplit_once('@')?;
        userinfo.contains(':').then_some((scheme, userinfo, host))
    }

    /// The `user:pass` part of the URL, if any.
    fn userinfo(&self) -> Option<&str> {
        self.parts().map(|(_, userinfo, _)| userinfo)
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
        match self.parts() {
            Some((scheme, _, host)) => write!(f, "{scheme}://***@{host}"),
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
    fn unencoded_slash_query_or_hash_in_password_is_still_hidden() {
        for (raw, password) in [
            ("rtsp://viewer:pa/ss@10.0.0.5/s", "pa/ss"),
            ("rtsp://viewer:pa?ss@10.0.0.5/s", "pa?ss"),
            ("rtsp://viewer:pa#ss@10.0.0.5/s", "pa#ss"),
        ] {
            let url = RtspUrl::new(raw);
            assert_eq!(url.to_string(), "rtsp://***@10.0.0.5/s", "{raw}");
            assert_eq!(redact(&format!("bad {password}"), &url), "bad ***", "{raw}");
        }
    }

    #[test]
    fn empty_password_display_hides_user() {
        let url = RtspUrl::new("rtsp://p:@h/s");
        assert_eq!(url.to_string(), "rtsp://***@h/s");
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

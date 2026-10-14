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
    encodes: Family<ResultLabels, Counter>,
    encode_duration: Histogram,
}

impl Metrics {
    pub fn new() -> Self {
        let last_frame = Gauge::default();
        let captures = Family::<ResultLabels, Counter>::default();
        let capture_duration = Histogram::new([0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0]);
        let encodes = Family::<ResultLabels, Counter>::default();
        let encode_duration = Histogram::new([
            10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0,
        ]);

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
        registry.register(
            "timelapse_encodes",
            "Timelapse encodes by result",
            encodes.clone(),
        );
        registry.register(
            "timelapse_encode_duration_seconds",
            "Duration of successful timelapse encodes",
            encode_duration.clone(),
        );

        Self {
            registry: Arc::new(registry),
            last_frame,
            captures,
            capture_duration,
            encodes,
            encode_duration,
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

    pub fn record_encode_ok(&self, took: Duration) {
        self.encode_duration.observe(took.as_secs_f64());
        self.encodes.get_or_create(&vec![("result", "ok")]).inc();
    }

    pub fn record_encode_error(&self) {
        self.encodes.get_or_create(&vec![("result", "error")]).inc();
    }

    #[cfg(test)]
    pub fn encodes(&self, result: &'static str) -> u64 {
        self.encodes.get_or_create(&vec![("result", result)]).get()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_expected_metric_names() {
        let m = Metrics::new();
        m.record_ok(1_760_000_000, Duration::from_secs(2));
        m.record_error();
        m.record_encode_ok(Duration::from_secs(90));
        m.record_encode_error();
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
        assert!(
            text.contains(r#"timelapse_encodes_total{result="ok"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"timelapse_encodes_total{result="error"} 1"#),
            "{text}"
        );
        assert!(
            text.contains("timelapse_encode_duration_seconds_count 1"),
            "{text}"
        );
    }
}

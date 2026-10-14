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

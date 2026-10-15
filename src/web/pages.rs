use std::{io, sync::Arc};

use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{NaiveDate, Timelike, Utc};

use super::{AppState, health, internal_error, render, timelapses::INPUT_FORMAT};
use crate::{
    store::{DaySummary, Frame, parse_day_name},
    timelapse::last_complete_minute,
};

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
    /// End of the "Timelapse this day" range: 23:59, or the last complete minute for today.
    timelapse_to: String,
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
    let end_of_day = date.and_hms_opt(23, 59, 0).expect("valid time").and_utc();
    render(DayPage {
        timelapse_to: end_of_day
            .min(last_complete_minute(Utc::now()))
            .format(INPUT_FORMAT)
            .to_string(),
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

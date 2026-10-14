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

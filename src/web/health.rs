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

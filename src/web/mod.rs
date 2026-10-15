mod health;
mod pages;
mod timelapses;

use std::{fmt::Display, sync::Arc, time::Duration};

use askama::Template;
use axum::{
    Router,
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use tokio::sync::Notify;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::error;

use crate::{metrics::Metrics, store::Store};

pub struct AppState {
    pub store: Store,
    pub metrics: Metrics,
    pub stale_after: Duration,
    pub started_at: DateTime<Utc>,
    /// Wakes the encoder after a job is queued.
    pub wake: Arc<Notify>,
}

pub fn router(state: Arc<AppState>) -> Router {
    let frames = Router::new()
        .fallback_service(ServeDir::new(state.store.frames_dir()))
        .layer(middleware::map_response(immutable_cache));

    Router::new()
        .route("/", get(pages::index))
        .route("/days/{date}", get(pages::day))
        .route("/latest.jpg", get(health::latest))
        .route(
            "/timelapses",
            get(timelapses::list).post(timelapses::create),
        )
        .route("/timelapses/{file}", get(timelapses::video))
        .route("/style.css", get(stylesheet))
        .nest_service("/frames", frames)
        .layer(TraceLayer::new_for_http())
        // Probes are added after the trace layer so they stay out of the logs.
        .route("/healthz", get(health::healthz))
        .route("/metrics", get(health::metrics))
        .with_state(state)
}

async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../assets/style.css"),
    )
}

/// Frames never change once written, so successful responses can be cached forever.
async fn immutable_cache(mut res: Response) -> Response {
    if res.status().is_success() {
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    }
    res
}

fn render(page: impl Template) -> Response {
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => internal_error(e),
    }
}

fn internal_error(e: impl Display) -> Response {
    error!(error = %e, "request failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::{body::Body, http::Request};
    use chrono::TimeDelta;
    use http_body_util::BodyExt;
    use pretty_assertions::assert_eq;
    use tower::ServiceExt;

    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path());
            Self { dir, store }
        }

        fn add_frame(&self, time: DateTime<Utc>) {
            let path = self.store.frame_path(time);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"jpeg").unwrap();
        }

        fn app(&self, started_at: DateTime<Utc>) -> Router {
            router(Arc::new(AppState {
                store: self.store.clone(),
                metrics: Metrics::new(),
                stale_after: Duration::from_secs(180),
                started_at,
                wake: Arc::new(Notify::new()),
            }))
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        send(app, Request::get(uri).body(Body::empty()).unwrap()).await
    }

    async fn post(
        app: Router,
        uri: &str,
        form: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let req = Request::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form.to_owned()))
            .unwrap();
        send(app, req).await
    }

    async fn send(app: Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    fn long_ago() -> DateTime<Utc> {
        Utc::now() - TimeDelta::hours(1)
    }

    #[tokio::test]
    async fn healthz_ok_with_fresh_frame() {
        let f = Fixture::new();
        f.add_frame(Utc::now());
        let (status, _, body) = get(f.app(long_ago()), "/healthz").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn healthz_fails_with_stale_frame() {
        let f = Fixture::new();
        f.add_frame(Utc::now() - TimeDelta::minutes(10));
        let (status, _, body) = get(f.app(long_ago()), "/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("stale"), "{body}");
    }

    #[tokio::test]
    async fn healthz_grace_period_after_start() {
        let f = Fixture::new();
        let (status, _, _) = get(f.app(Utc::now()), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = get(f.app(long_ago()), "/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn latest_redirects_to_newest_frame() {
        let f = Fixture::new();
        let t = DateTime::parse_from_rfc3339("2026-10-11T07:05:00Z")
            .unwrap()
            .to_utc();
        f.add_frame(t - TimeDelta::minutes(1));
        f.add_frame(t);
        let (status, headers, _) = get(f.app(long_ago()), "/latest.jpg").await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            headers[header::LOCATION],
            "/frames/2026-10-11/20261011-0705.jpg"
        );
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn latest_404_without_frames() {
        let f = Fixture::new();
        let (status, _, _) = get(f.app(long_ago()), "/latest.jpg").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn frames_served_with_immutable_cache() {
        let f = Fixture::new();
        let t = DateTime::parse_from_rfc3339("2026-10-11T07:05:00Z")
            .unwrap()
            .to_utc();
        f.add_frame(t);
        let (status, headers, body) =
            get(f.app(long_ago()), "/frames/2026-10-11/20261011-0705.jpg").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "jpeg");
        assert_eq!(
            headers[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );

        let (status, headers, _) = get(f.app(long_ago()), "/frames/2026-10-11/missing.jpg").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(headers.get(header::CACHE_CONTROL).is_none());
    }

    #[tokio::test]
    async fn frames_block_path_traversal() {
        let f = Fixture::new();
        std::fs::write(f.path().join("secret.txt"), "secret").unwrap();
        std::fs::create_dir_all(f.store.frames_dir()).unwrap();
        for uri in ["/frames/../secret.txt", "/frames/%2e%2e/secret.txt"] {
            let (status, _, body) = get(f.app(long_ago()), uri).await;
            assert_ne!(status, StatusCode::OK, "{uri}");
            assert!(!body.contains("secret"), "{uri}");
        }
    }

    #[tokio::test]
    async fn index_lists_days() {
        let f = Fixture::new();
        f.add_frame(Utc::now());
        let (status, _, body) = get(f.app(long_ago()), "/").await;
        assert_eq!(status, StatusCode::OK);
        let today = Utc::now().format("%Y-%m-%d").to_string();
        assert!(body.contains(&format!("/days/{today}")), "{body}");
        assert!(body.contains("healthy"), "{body}");
    }

    #[tokio::test]
    async fn index_without_frames() {
        let f = Fixture::new();
        let (status, _, body) = get(f.app(long_ago()), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No frames captured yet."), "{body}");
    }

    #[tokio::test]
    async fn day_page_shows_samples_and_navigation() {
        let f = Fixture::new();
        let d10 = DateTime::parse_from_rfc3339("2026-10-10T23:59:00Z")
            .unwrap()
            .to_utc();
        let d11 = DateTime::parse_from_rfc3339("2026-10-11T07:05:00Z")
            .unwrap()
            .to_utc();
        f.add_frame(d10);
        f.add_frame(d11);
        f.add_frame(d11 + TimeDelta::minutes(1));
        let (status, _, body) = get(f.app(long_ago()), "/days/2026-10-11").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("/days/2026-10-10"), "{body}");
        assert!(body.contains("2 frames"), "{body}");
        assert!(
            body.contains("/frames/2026-10-11/20261011-0706.jpg"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn day_page_rejects_bad_and_missing_dates() {
        let f = Fixture::new();
        let (status, _, _) = get(f.app(long_ago()), "/days/bad").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = get(f.app(long_ago()), "/days/2026-10-11").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn metrics_endpoint_exposes_gauge() {
        let f = Fixture::new();
        let (status, headers, body) = get(f.app(long_ago()), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/openmetrics-text")
        );
        assert!(
            body.contains("timelapse_last_frame_timestamp_seconds"),
            "{body}"
        );
    }

    const DAY_NAME: &str = "20261011-0000_20261011-2359_s1";
    const DAY_FORM: &str = "from=2026-10-11T00%3A00&to=2026-10-11T23%3A59";

    fn day_frame() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-11T07:05:00Z")
            .unwrap()
            .to_utc()
    }

    #[tokio::test]
    async fn post_timelapse_queues_job_and_lists_it() {
        let f = Fixture::new();
        f.add_frame(day_frame());

        let (status, headers, body) = post(f.app(long_ago()), "/timelapses", DAY_FORM).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        assert_eq!(headers[header::LOCATION], "/timelapses");
        assert!(f.store.out_file(DAY_NAME, "job").exists());

        let (status, _, body) = get(f.app(long_ago()), "/timelapses").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("queued"), "{body}");
        assert!(body.contains("Daily hours from (UTC)"), "{body}");
        assert!(body.contains(r#"http-equiv="refresh""#), "{body}");
    }

    #[tokio::test]
    async fn post_finished_timelapse_redirects_to_video() {
        let f = Fixture::new();
        std::fs::create_dir_all(f.store.out_dir()).unwrap();
        std::fs::write(f.store.out_file(DAY_NAME, "mp4"), "0123456789").unwrap();

        let (status, headers, _) = post(f.app(long_ago()), "/timelapses", DAY_FORM).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let location = headers[header::LOCATION].to_str().unwrap().to_owned();
        assert_eq!(location, format!("/timelapses/{DAY_NAME}.mp4"));

        let (status, _, body) = get(f.app(long_ago()), &location).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "0123456789");

        let req = Request::get(&location)
            .header(header::RANGE, "bytes=2-4")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(f.app(long_ago()), req).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, "234");
    }

    #[tokio::test]
    async fn post_timelapse_rejects_bad_input() {
        let f = Fixture::new();
        f.add_frame(day_frame());
        let future = (Utc::now() + TimeDelta::hours(1)).format("%Y-%m-%dT%H%%3A%M");
        for (form, message) in [
            ("from=2026-10-11T00%3A00".to_owned(), "to must look like"),
            (
                format!("from=2026-10-11T00%3A00&to={future}"),
                "to must be before the current minute",
            ),
            (format!("{DAY_FORM}&step=0"), "step must be at least 1"),
            (
                format!("{DAY_FORM}&hour_from=0&hour_to=6"),
                "no frames in that range",
            ),
        ] {
            let (status, _, body) = post(f.app(long_ago()), "/timelapses", &form).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{form}");
            assert!(body.contains(message), "{form}: {body}");
        }
        assert!(!f.store.out_file(DAY_NAME, "job").exists());
    }

    #[tokio::test]
    async fn timelapse_list_shows_finished_and_failed_without_refresh() {
        let f = Fixture::new();
        std::fs::create_dir_all(f.store.out_dir()).unwrap();
        std::fs::write(f.store.out_file(DAY_NAME, "mp4"), "x").unwrap();
        let failed = "20261012-0000_20261012-2359_s1";
        std::fs::write(f.store.out_file(failed, "err"), "ffmpeg exited\nmore").unwrap();

        let (status, _, body) = get(f.app(long_ago()), "/timelapses").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(&format!("/timelapses/{DAY_NAME}.mp4")),
            "{body}"
        );
        assert!(body.contains("ffmpeg exited"), "{body}");
        assert!(!body.contains("more"), "{body}");
        assert!(!body.contains(r#"http-equiv="refresh""#), "{body}");
    }

    #[tokio::test]
    async fn timelapse_files_other_than_videos_are_not_served() {
        let f = Fixture::new();
        std::fs::create_dir_all(f.store.out_dir()).unwrap();
        std::fs::write(f.store.out_file(DAY_NAME, "job"), "list").unwrap();
        std::fs::write(f.path().join("secret.txt"), "secret").unwrap();
        for uri in [
            format!("/timelapses/{DAY_NAME}.job"),
            "/timelapses/notes.mp4".to_owned(),
            "/timelapses/..%2Fsecret.txt".to_owned(),
            format!("/timelapses/{DAY_NAME}.mp4"),
        ] {
            let (status, _, _) = get(f.app(long_ago()), &uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn day_page_offers_timelapse_of_the_day() {
        let f = Fixture::new();
        f.add_frame(day_frame());
        let (_, _, body) = get(f.app(long_ago()), "/days/2026-10-11").await;
        assert!(body.contains(r#"action="/timelapses""#), "{body}");
        assert!(body.contains(r#"value="2026-10-11T00:00""#), "{body}");
        assert!(body.contains(r#"value="2026-10-11T23:59""#), "{body}");
    }

    #[tokio::test]
    async fn stylesheet_is_served() {
        let f = Fixture::new();
        let (status, headers, body) = get(f.app(long_ago()), "/style.css").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
        assert!(body.contains(":root"));
    }
}

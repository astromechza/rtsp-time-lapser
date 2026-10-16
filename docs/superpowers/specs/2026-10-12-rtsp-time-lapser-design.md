# rtsp-time-lapser: design

Date: 2026-10-12
Source: Notion "Project: Window camera timelapse" (goal, decision, principles, failure modes).

## Goal

Capture one frame per minute from the window-facing IP camera's RTSP stream to disk, and build a timelapse from any time range of frames on demand.

Success criteria:

- Frames land every minute without supervision.
- A camera reboot, stream drop or pod restart costs at most a few frames.
- Any range assembles into an MP4 (M2/M3).
- Storage stays bounded (M3).

## Scope and milestones

| Milestone | Contents | Status of this spec |
|---|---|---|
| **M1** | Crate scaffold, capture loop, startup cleanup, `/healthz`, `/metrics`, HTML browse UI, container image, Kustomize manifests, CI | Fully specified; first implementation plan |
| **M2** | Timelapse assembly from JPEGs, triggered from the HTML UI | Specified in [M2 design](2026-10-14-m2-timelapse-assembly-design.md) |
| **M3** | Daily H.264 archive, retention, archive-backed assembly | Outline only; designed in detail after M1 is deployed |

The UI is HTML only. There is no JSON API in any milestone.

## Architecture

A single Rust binary runs as a single-replica Kubernetes Deployment with a PVC at `/data`. It shells out to `ffmpeg` (and in M3 `ffprobe`); it does not link against them.

Design principles (from the Notion page, binding):

- **Crash-only, state on disk.** No schedule state in memory. Filenames are the state. Every task is idempotent and resumes by scanning the filesystem.
- **Atomic writes.** Write `*.part`, then rename. Delete stale `*.part` files on startup.
- **UTC filenames** (`YYYYMMDD-HHMM.jpg`): lexically sortable, no DST duplicates or gaps.
- **Health = newest frame age.**
- **Credentials live in the RTSP URL** from a Kubernetes Secret and are never logged.
- ~~Capture is isolated from heavy work~~: dropped in M2. The 16-core host has ample headroom, so encoders run at normal priority with default threading, one job at a time.

### Toolchain and dependencies

- Rust **1.99**, edition **2024**. `rust-toolchain.toml` pins `channel = "1.99"` with `rustfmt` and `clippy`; `Cargo.toml` sets `rust-version = "1.99"`.
- Single binary crate `rtsp-time-lapser` (no workspace).
- Runtime crates: `tokio` 1.53 (rt-multi-thread, macros, process, time, fs, signal), `axum` 0.8, `tower-http` 0.7 (fs, trace), `tower` 0.5, `chrono` 0.4, `anyhow` 1, `clap` 4.6 (derive, env), `tracing` 0.1, `tracing-subscriber` 0.3 (env-filter), `prometheus-client` 0.25, `askama` 0.16.
- Dev crates: `tempfile` 3, `pretty_assertions` 1, `http-body-util` 0.1, `tower` (util) for `ServiceExt::oneshot`.
- Lint with `cargo clippy --all-targets -- -D warnings`; format with `cargo fmt`.

### Modules

```
src/
  main.rs       parse config, init tracing, startup cleanup, spawn capture, serve axum
  config.rs     clap derive + env; RtspUrl newtype with redacting Debug/Display; redact()
  store.rs      on-disk layout and all filesystem queries
  ffmpeg.rs     run an ffmpeg command: timeout, kill_on_drop, capture + redact stderr
  capture.rs    interval loop and single-frame snap
  metrics.rs    prometheus-client registry and metric handles
  web/
    mod.rs      router, AppState, shared middleware
    pages.rs    "/" and "/days/{date}" handlers
    health.rs   "/healthz", "/metrics", "/latest.jpg"
templates/      askama templates (base.html, index.html, day.html)
assets/style.css
```

Time is injected: `store` and `capture` functions take `now: DateTime<Utc>` so tests never depend on the wall clock or sleep for real minutes.

## Configuration

All settings are clap args with `env` fallbacks. Durations are integer seconds.

| Env | Default | Purpose |
|---|---|---|
| `RTSP_URL` | required | `rtsp://user:pass@host:554/path`; userinfo percent-encoded |
| `DATA_DIR` | `/data` | Root of all on-disk state |
| `LISTEN_ADDR` | `0.0.0.0:8080` | HTTP bind address |
| `CAPTURE_INTERVAL_SECS` | `60` | Tick period |
| `CAPTURE_TIMEOUT_SECS` | `30` | Kill ffmpeg after this long |
| `JPEG_QUALITY` | `4` | ffmpeg `-q:v` |
| `SCALE_WIDTH` | unset | When set, adds `-vf scale=W:-2` |
| `STALE_AFTER_SECS` | `180` | `/healthz` fails when the newest frame is older |
| `FFMPEG_BIN` | `ffmpeg` | Path to ffmpeg; tests substitute a fake script |

`RtspUrl` wraps the URL string. Its `Debug` and `Display` print the scheme and host with userinfo replaced by `***` (e.g. `rtsp://***@10.0.0.5:554/stream1`). The raw value is reachable only through an explicit `expose()` method used when building the ffmpeg command.

`config::redact(text, url) -> String` replaces every occurrence of the full URL, and of its userinfo component, with `***`. It is applied to all ffmpeg stderr before logging.

## Storage layout

```
DATA_DIR/
  frames/
    YYYY-MM-DD/
      YYYYMMDD-HHMM.jpg
```

Both directory and file names are UTC. `store` owns all path logic and exposes:

- `frame_path(now) -> PathBuf`
- `parse_frame_name(&str) -> Option<DateTime<Utc>>`: accepts exactly `YYYYMMDD-HHMM.jpg`.
- `list_days() -> Vec<DaySummary { date, frame_count }>`: sorted newest first.
- `list_frames(date) -> Vec<Frame { name, time }>`: sorted ascending.
- `newest_frame() -> Option<Frame>`: scans the newest day directory; if it contains no valid frames, falls back to earlier days.
- `cleanup_parts()`: recursively deletes `*.part` files under `DATA_DIR`.

Files and directories that do not match the naming patterns are ignored by listings and never deleted, except `*.part` by cleanup.

## Capture

`capture_loop` uses `tokio::time::interval(CAPTURE_INTERVAL_SECS)` with `MissedTickBehavior::Skip`. The first tick fires immediately. Ticks run sequentially, so a slow ffmpeg cannot overlap the next tick.

Each tick calls `snap(now)`:

1. Compute `out = frame_path(now)`. If `out` exists (e.g. restart within the same minute), record `skipped` and return.
2. `create_dir_all` the day directory.
3. Run `FFMPEG_BIN -nostdin -loglevel error -rtsp_transport tcp -i <url> -frames:v 1 -q:v <q> [-vf scale=W:-2] -f image2 -y <out>.part` with `kill_on_drop(true)`, stdout discarded and stderr captured, wrapped in `tokio::time::timeout(CAPTURE_TIMEOUT_SECS)`.
4. On success with a non-empty `.part`: rename to `out`, set the last-frame gauge, record `ok` and observe duration.
5. On any failure (spawn error, non-zero exit, timeout, empty output): remove the `.part` if present, record `error`, and log one `warn` line containing the reason and the redacted stderr truncated to 512 bytes.

Note: `interval` is monotonic while filenames use the wall clock; a clock step can rarely skip or duplicate a minute. This is harmless because names are idempotent and assembly tolerates gaps.

## Process lifecycle

Startup:

1. Parse config; init tracing (`RUST_LOG`, default `info`, plain text to stdout).
2. Create `DATA_DIR/frames`.
3. `cleanup_parts()`.
4. Seed the last-frame gauge from `newest_frame()`.
5. Spawn the capture task, then serve HTTP.

`main` waits on `tokio::select!` over the server future and the capture `JoinHandle`. If the capture task ends or panics, the process exits non-zero so Kubernetes restarts it; the web server never keeps running without capture.

On SIGTERM/SIGINT, axum shuts down gracefully and the capture task is dropped. `kill_on_drop` kills any running ffmpeg; its `.part` file is removed on the next startup.

## HTTP

axum 0.8 router with shared `Arc<AppState { config, store, metrics, started_at }>`.

| Route | Behaviour |
|---|---|
| `GET /` | Latest frame (`<img src="/latest.jpg">`), its UTC timestamp and age, health badge, table of days (newest first) with frame counts linking to `/days/{date}` |
| `GET /days/{date}` | `date` must be `YYYY-MM-DD` (else 400); 404 if the directory does not exist. Prev/next day links; grid of up to 24 hourly samples (first frame in each hour present) with `loading="lazy"`; `<details>` list of every frame linking to its raw file |
| `GET /latest.jpg` | 307 to `/frames/{date}/{name}` with `Cache-Control: no-store`; 404 if no frames |
| `GET /frames/*` | `tower_http::services::ServeDir` on `DATA_DIR/frames`; response header `Cache-Control: public, max-age=31536000, immutable` |
| `GET /style.css` | `assets/style.css` embedded with `include_str!` |
| `GET /healthz` | 200 `ok` if the newest frame is younger than `STALE_AFTER_SECS`; also 200 if there are no fresh frames yet but the process started less than `STALE_AFTER_SECS` ago (startup grace). Otherwise 503 with the frame age in the body |
| `GET /metrics` | OpenMetrics text |

UI rules: server-rendered askama templates sharing `base.html`; one stylesheet built on CSS variables and reusable classes; no inline styles; no JavaScript.

A `tower_http::trace::TraceLayer` covers all routes except `/healthz` and `/metrics`.

## Observability

Metrics (`prometheus-client`):

- `timelapse_last_frame_timestamp_seconds` (gauge): Unix time of the newest frame.
- `timelapse_captures_total{result="ok"|"error"|"skipped"}` (counter).
- `timelapse_capture_duration_seconds` (histogram, buckets 0.5, 1, 2, 5, 10, 20, 30).

Suggested Grafana alert: `time() - timelapse_last_frame_timestamp_seconds > 300`.

Logs never contain the RTSP URL or its credentials.

## Container image

Multi-stage `Dockerfile`:

- Builder: `rust:1.99.0-alpine3.24`; static musl release build; BuildKit cache mounts for the cargo registry and `target`.
- Runtime: `alpine:3.24`; `apk add --no-cache ffmpeg tini`; non-root user and group `10001`; `ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/rtsp-time-lapser"]`; `EXPOSE 8080`.

`Makefile` targets:

- `fmt`, `lint`, `test`
- `image`: `podman build --platform $(PLATFORMS) --manifest $(IMAGE):$(TAG) .` then `podman manifest push --all`, with `PLATFORMS ?= linux/amd64`, `IMAGE ?= ghcr.io/astromechza/rtsp-time-lapser`, `TAG ?= $(shell git describe --tags --always --dirty)`.

The image is built and pushed locally with podman (Docker is not used); CI does not build images. The target server is an Intel Atom (amd64).

## Kubernetes manifests

`deploy/` is a Kustomize base:

- `kustomization.yaml`: no `namespace` (the consumer chooses it via `kubectl -n` or an overlay); `images:` entry for the tag.
- `pvc.yaml`: `ReadWriteOnce`, 20Gi, no `storageClassName` (cluster default; overlays patch it, e.g. `topolvm-provisioner` on hensteeth).
- `deployment.yaml`:
  - `replicas: 1`, `strategy: Recreate`.
  - `automountServiceAccountToken: false`.
  - Pod `securityContext`: `runAsNonRoot`, UID/GID/`fsGroup` 10001, `fsGroupChangePolicy: OnRootMismatch`, `seccompProfile: RuntimeDefault`. Container: `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, drop all capabilities.
  - `RTSP_URL` from Secret `rtsp-time-lapser`, key `RTSP_URL`.
  - PVC mounted at `/data`; `emptyDir` at `/tmp` (root filesystem is read-only).
  - Resources: requests `100m` CPU / `64Mi`; limit `2Gi` memory (raised from `512Mi` in M2: a full-day x264 encode of 2304×1296 frames peaked at ~850 MiB on 12 cores and 1047 MiB (ffmpeg VmHWM) on the 16-core Atom, where a 512Mi container was OOM-killed); no CPU limit.
  - `startupProbe`: `/healthz`, period 10s, failureThreshold 30.
  - `livenessProbe`: `/healthz`, period 60s, failureThreshold 10 (~10 min restart cadence when frames are stale).
  - `readinessProbe`: TCP socket on the HTTP port, period 30s. Not `/healthz`: the UI must stay browsable while the camera is down.
  - Pod annotations `prometheus.io/scrape: "true"`, `prometheus.io/port: "8080"`, `prometheus.io/path: /metrics`.
- `service.yaml`: ClusterIP, port 80 → 8080.
- `secret.example.yaml`: documents the Secret shape; **not** referenced by the kustomization.

No Ingress is shipped. The README documents an example overlay (remote base at a pinned `?ref=`, namespace, storage-class patch and an `ingress.yaml`) to adapt at install time, with a warning that the UI has no authentication and must stay LAN-only or sit behind auth.

## CI

`.github/workflows/ci.yml`, on push to `main` and pull requests, `ubuntu-latest`:

1. Checkout.
2. Install toolchain `1.99` with `rustfmt` and `clippy` (`dtolnay/rust-toolchain`).
3. `Swatinem/rust-cache`.
4. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

CI does not install ffmpeg: all automated tests use fake ffmpeg scripts.

## Error handling summary

| Failure | Behaviour |
|---|---|
| Camera reboot / stream drop | ffmpeg fails; `error` recorded; next tick retries |
| ffmpeg hangs | Timeout drops the future; `kill_on_drop` kills the child; `.part` removed |
| Pod killed mid-write | `.part` left behind; deleted on next startup |
| Capture task panics | Process exits non-zero; Kubernetes restarts it |
| Frames silently stop | `/healthz` 503 → liveness restart; Grafana alert on metric |
| Disk full | ffmpeg or rename fails; `error` recorded (bounded storage arrives in M3) |
| Credentials in logs | `RtspUrl` redacting formatters; stderr passed through `redact` |

## Testing (M1)

- `store` unit tests over a `tempfile` directory: frame name round trip, invalid names ignored, `list_days` ordering and counts, `newest_frame` fallback across an empty newest day, `cleanup_parts` removes only `*.part`.
- `config` unit tests: `RtspUrl` `Debug`/`Display` redaction; `redact` scrubs the full URL and the userinfo.
- `capture::snap` integration tests using a fake `FFMPEG_BIN` shell script: success (file present, no `.part`), exit 1 (no file, no `.part`, error counted), hang (killed after a 1 s timeout), existing target (skipped).
- `web` router tests via `tower::ServiceExt::oneshot`: `/healthz` 200, 503 and startup grace; `/latest.jpg` redirect and 404; `/days/bad` 400; missing day 404; `/frames/../` traversal blocked; `/metrics` contains the last-frame gauge.
- An `#[ignore]`d `real_camera` test captures one frame with real ffmpeg from `TEST_RTSP_URL`, run by hand.
- Manual after deploy: the Notion failure-mode checklist (unplug camera, kill pod mid-capture).

## M2: timelapse assembly

See [M2 design](2026-10-14-m2-timelapse-assembly-design.md). M2 replaces the earlier outline: jobs are `.job` ffconcat files in `DATA_DIR/out/`, encoded one at a time by a worker task.

## M3 outline: archive and retention (not yet designed in detail)

- Task at startup and hourly: for completed UTC days older than `JPEG_RETENTION_DAYS` (default 14) without an archive, write `.idx`, encode with H.264 (`libx264`) through M2's `encode` (H.265 was measured at 40–55% smaller but 3–5× slower to encode and not universally playable in browsers; H.264 was chosen for the Atom; `hqdn3d` denoising is still to be evaluated), verify `ffprobe` frame count equals `.idx` line count, rename `.mp4.part` and `.idx.part`, then delete the day directory.
- `ARCHIVE_RETENTION_DAYS` unset by default (keep forever).
- Assembly gains an archive path for archived days; the simplest correct approach is chosen at M3 design time.
- Day page shows a `<video>` for archived days.
- Metrics `timelapse_data_bytes` and `timelapse_archives_total{result}`.

## Open questions (deferred)

- Camera model, stream path and native resolution (decides whether `SCALE_WIDTH` is needed).
- ~~Target output resolution for timelapses (M2)~~: native, capped by `TIMELAPSE_MAX_WIDTH`.
- Retention values N and M (M3).
- ~~Night exclusion default (M2)~~: opt-in UTC hour range, off by default.

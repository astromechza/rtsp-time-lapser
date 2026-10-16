# M2: timelapse assembly from JPEGs

Date: 2026-10-14
Parent: [rtsp-time-lapser design](2026-10-12-rtsp-time-lapser-design.md). M1 (capture, browse UI) is deployed.

## Goal

Assemble any UTC time range of captured frames into an MP4, triggered from the HTML UI. The encode path is built as a reusable unit so that M3 (daily archive and retention) drives it with its own jobs and settings.

Success criteria:

- A range submitted from the UI produces a playable MP4 without further interaction.
- Submitting the same selection twice never encodes twice.
- Queued and running jobs survive a pod restart and complete afterwards.
- Failures are visible in the UI, not only in logs.
- Outputs are pruned after a retention period.

Out of scope: M3 itself (archive encoding, JPEG deletion, `ffprobe` verification), deleting outputs from the UI, cancelling jobs.

## Principles

M1's principles apply unchanged: crash-only, filenames are the state, atomic `*.part` then rename, UTC everywhere, no JSON API, no JavaScript, no inline styles.

The M1 principle "capture is isolated from heavy work (`nice -n 19 -threads 2`)" is dropped. The target host is a 16-core Intel Atom and encodes have ample headroom; ffmpeg uses its default threading and normal priority. Jobs still run one at a time to keep memory and disk I/O predictable.

## Selection

`TimelapseSpec { from, to, step, hours }`:

- `from`, `to`: UTC `DateTime` at minute precision, both inclusive. `from <= to` is required, and `to` must be at or before the last complete minute (the minute before the current one), so a finished output name never refers to frames that could still appear. The capture ticker is not aligned to the minute, so the current minute's frame may not exist yet.
- `step`: integer ≥ 1. Keep every Nth frame, applied after the hour filter, counting from the first kept frame.
- `hours`: optional `(start, end)`, each 0–23, UTC. A frame is kept when its hour `h` satisfies `start <= h < end` if `start < end`, or `h >= start || h < end` if `start > end` (wrapping, e.g. `20-06` keeps 20:00–05:59). `start == end` is rejected.

`Store::frames_between(from, to) -> io::Result<Vec<Frame>>` walks only the day directories whose date lies in `[from.date, to.date]` and returns frames with `from <= time <= to`, oldest first. Missing day directories are skipped. `timelapse::select(&spec, frames) -> Vec<Frame>` applies the hour filter and step.

## Naming

`TimelapseSpec::name()` encodes every selection input:

```
{from:%Y%m%d-%H%M}_{to:%Y%m%d-%H%M}_s{step}[_h{start:02}-{end:02}]
```

Examples: `20261011-0000_20261011-2359_s1`, `20261011-0000_20261013-2359_s2_h06-20`.

`TimelapseSpec::parse_name(&str) -> Option<TimelapseSpec>` is the exact inverse and rejects anything else. Encode settings (fps, CRF, preset, width cap) are deployment-wide and deliberately not in the name; changing them affects only future encodes.

## On-disk queue

`Store` gains `out_dir() -> DATA_DIR/out`, created at startup next to `frames/`.

```
DATA_DIR/out/
  {name}.job        ffconcat list of the selected frames
  {name}.mp4.part   encode in progress
  {name}.mp4        finished output
  {name}.err        failure: redacted ffmpeg stderr, truncated to 4 KiB
```

A `.job` file is a complete, self-contained job: the frame selection frozen at submit time.

```
ffconcat version 1.0
file '/data/frames/2026-10-11/20261011-0000.jpg'
file '/data/frames/2026-10-11/20261011-0001.jpg'
```

Paths are absolute. A `'` in the path (possible through `DATA_DIR`) is written as `'\''`: close the quote, add an escaped quote, reopen.

Status is derived from the files present for a name, in this order of precedence:

| Files present | Status |
|---|---|
| `.mp4` | `Done` |
| `.err` | `Failed` |
| `.job` and `.mp4.part` | `Running` |
| `.job` | `Queued` |

`timelapse::list(&store) -> io::Result<Vec<Entry { name, spec, status, size, modified, error }>>` scans `out/`, ignores names that do not parse, and returns entries newest first by modification time. `error` holds the `.err` contents for failed entries.

### Submit

`timelapse::submit(&store, &spec, now) -> Result<Submitted>`:

1. Validate `spec` against `now` (see Selection).
2. If `{name}.mp4` exists, set its mtime to now (retention counts from the last request) and return `Submitted::Done`.
3. If `{name}.job` exists, return `Submitted::Queued`.
4. Select frames. If none, return a validation error.
5. Remove `{name}.err` if present (resubmitting retries a failure).
6. Write `{name}.job.part`, rename to `{name}.job`, return `Submitted::Queued`.

Step 6 creates `{name}.job.part` exclusively (`create_new`). If it already exists, a concurrent submit of the same selection is writing it, and this submit returns `Submitted::Queued` without touching it. A part left by a crash is removed at startup.

### Restart

The existing startup `cleanup_parts()` already removes `*.part` under `DATA_DIR`, so an interrupted `{name}.mp4.part` disappears, the job reverts to `Queued`, and the worker re-encodes it. A stray `.job.part` from an interrupted submit is removed the same way.

### M3 hook

A job is a frame list plus an output path. M3 writes its own `.job` files (one per completed day) into its own directory and calls the same `encode` with archive settings. The `.job` doubles as M3's `.idx` for the `ffprobe` frame-count check. How M3's queue shares the worker is decided at M3 design time.

## Encoder

`src/encoder.rs`.

### `encode`

`encode(ffmpeg_bin, job: &Path, out: &Path, settings: &EncodeSettings, limit: Duration) -> Result<()>` runs, through the existing `ffmpeg::run` (timeout and `kill_on_drop`):

```
<FFMPEG_BIN> -nostdin -loglevel error
  -f concat -safe 0 -i <job>
  -vf "setpts=N/<fps>/TB,scale='trunc(min(iw,<max_width>)/2)*2':-2"
  -r <fps> -c:v libx264 -preset <preset> -crf <crf> -pix_fmt yuv420p
  -movflags +faststart -f mp4 -y <out>.part
```

Without `TIMELAPSE_MAX_WIDTH` the scale expression is `trunc(iw/2)*2`. The scale is always applied because `yuv420p` requires even dimensions. `setpts=N/<fps>/TB` gives each input frame exactly one output frame regardless of the concat demuxer's per-image timestamps; this was verified with ffmpeg 8.0 (7 JPEGs at 641×361 produced 7 frames at 640×360, 30 fps).

On success with a non-empty `<out>.part`, rename it to `<out>`. On any failure, remove `<out>.part` and return the error.

`EncodeSettings { fps, crf, preset, max_width }` is built from config. M3 constructs its own.

### Worker

`Encoder::run(self)` is spawned from `main`:

```
loop {
    while let Some(job) = oldest .job in out/ (by mtime, then name) {
        process(job)
    }
    prune(out/, OUT_RETENTION_DAYS, now)
    wait until notified or 10 minutes pass
}
```

`process(job)`:

- Success: delete `{name}.job`, record `ok` and the duration, log `info`.
- Failure: write `{name}.err` (via `.err.part` and rename) with `redact`ed stderr truncated to 4 KiB, delete `{name}.job` so it is not retried forever, record `error`, log one `warn`. If the `.err` cannot be written (e.g. disk full), log a second `warn` and still delete the job; the failure is then visible only in logs and metrics.

The RTSP URL never appears in encode commands; `redact` is applied anyway.

A worker I/O error while scanning or pruning is logged as `warn` and retried after the next wait. The wake signal is a `tokio::sync::Notify` shared through `AppState`; `POST /timelapses` calls `notify_one()` after queuing a job.

`main`'s `tokio::select!` gains a third arm: if the worker task ends or panics, the process exits non-zero, matching the capture task.

### Retention

`timelapse::prune` deletes `{name}.mp4` and `{name}.err` files in `out/` whose mtime is older than `OUT_RETENTION_DAYS` days. `.job` files and files with names that do not parse are never deleted.

## HTTP

| Route | Behaviour |
|---|---|
| `GET /timelapses` | Form plus job table. Table columns: name (linked to the MP4 when done), range, status badge, size for done or the first line of the error for failed. When any entry is `Queued` or `Running`, the page includes `<meta http-equiv="refresh" content="10">`. |
| `POST /timelapses` | `application/x-www-form-urlencoded`: `from`, `to` (`YYYY-MM-DDTHH:MM`, interpreted as UTC), `step` (default 1), `hour_from`, `hour_to` (both blank or both 0–23). `Submitted::Done` → 303 to `/timelapses/{name}.mp4`. `Submitted::Queued` → notify the worker, 303 to `/timelapses`. Validation errors → 400 with a plain-text message. |
| `GET /timelapses/{file}` | Serves `out/{name}.mp4` with `tower_http::services::ServeFile`, only when `file` is `{name}.mp4` and `name` parses as a spec; anything else is 404, so `.job`/`.err` files and traversal are unreachable. Supports range requests so browsers can seek. No immutable cache header (outputs are pruned and can be regenerated). |

The form uses `<input type="datetime-local">` labelled "UTC"; the browser does not convert it. The hour fields are labelled "Daily hours from/to (UTC)" with a hint under the form: they apply to every day in the range (e.g. 6 to 20 for daylight, 20 to 6 for nights), blank for all hours. The datetimes set the overall span; the daily hours filter within it. `from` defaults to the start of the current UTC day and `to` to the last complete minute.

UI additions:

- `base.html`: nav link to `/timelapses`.
- `day.html`: a "Timelapse this day" button, a POST form with hidden `from={date}T00:00`, `to={date}T23:59` (clamped to the last complete minute for today).
- New `timelapses.html` template. New styles go into `assets/style.css` as reusable classes built on the existing CSS variables.

## Configuration

New clap args with `env` fallbacks:

| Env | Default | Purpose |
|---|---|---|
| `TIMELAPSE_FPS` | `30` | Output frame rate, ≥ 1 |
| `TIMELAPSE_CRF` | `23` | x264 CRF, 0–51. 23 was picked from a real day (1439 frames at 2304×1296): 190 MB vs 256 MB at CRF 20 (veryfast, measured with ffmpeg 8.0) |
| `TIMELAPSE_PRESET` | `veryfast` | x264 preset |
| `TIMELAPSE_MAX_WIDTH` | unset | Downscale to at most this width; unset keeps native width |
| `ENCODE_TIMEOUT_SECS` | `7200` | Kill an encode after this long, ≥ 1 |
| `OUT_RETENTION_DAYS` | `7` | Prune finished and failed outputs older than this, ≥ 1 |

## Observability

- `timelapse_encodes_total{result="ok"|"error"}` (counter).
- `timelapse_encode_duration_seconds` (histogram, buckets 10, 30, 60, 120, 300, 600, 1800, 3600, 7200).

## Error handling

| Failure | Behaviour |
|---|---|
| Invalid form input or empty selection | 400, nothing written |
| ffmpeg non-zero exit or empty output | `.err` written, `.job` removed, `error` recorded |
| Encode exceeds `ENCODE_TIMEOUT_SECS` | ffmpeg killed, treated as failure |
| Frame deleted after submit (e.g. by M3) | ffmpeg fails, treated as failure |
| Frames of mixed resolution in one range (`SCALE_WIDTH` changed) | ffmpeg fails, treated as failure; documented limitation |
| Pod killed mid-encode | `.mp4.part` removed at startup, job re-encoded |
| Worker scan or prune I/O error | `warn`, retried after the next wait |
| Worker task panics | Process exits non-zero; Kubernetes restarts it |
| Disk full | Encode or rename fails, treated as failure; if the `.err` cannot be written either, the job is still dropped and the failure is logged |

## Module layout

```
src/
  timelapse.rs   TimelapseSpec, naming, select, submit, list, pending_jobs, prune
  encoder.rs     EncodeSettings, encode, Encoder worker loop
  store.rs       + out_dir(), frames_between()
  config.rs      + timelapse settings
  metrics.rs     + encode counter and histogram
  web/
    timelapses.rs  GET/POST /timelapses handlers
    mod.rs         + routes, ServeDir on out/, Notify in AppState
templates/timelapses.html
```

Time is injected (`now` arguments) as in M1.

## Testing

- `timelapse` unit tests: name round trip with and without hours; `parse_name` rejects junk; validation (`from > to`, `to > now`, `step 0`, `start == end`); `select` with step, plain and wrapping hour ranges, and ranges spanning days; status derivation from each file combination; `submit` returns `Done` for an existing MP4, `Queued` without rewriting an existing job, and removes a stale `.err` on resubmit.
- `store` unit tests: `frames_between` across day boundaries, with missing days, inclusive at both ends.
- `encoder` tests with a fake `FFMPEG_BIN` script: success produces `.mp4`, removes `.job` and leaves no `.part`; exit 1 writes `.err` and removes `.job`; a leftover `.job` next to a finished MP4 is removed without encoding; a leftover `.mp4.part` is re-encoded after startup cleanup; `run` wakes up on `notify_one`. `timelapse` tests cover oldest-first `pending_jobs` and `prune` removing only old `.mp4`/`.err`.
- `web` router tests via `oneshot`: POST writes a `.job` and returns 303 to `/timelapses`; POST for an existing MP4 returns 303 to the MP4; 400 cases; the list page shows each status and the refresh meta only while work is pending; the day page contains the timelapse form.
- An `#[ignore]`d `real_ffmpeg_encode` test generates JPEGs with `ffmpeg -f lavfi -i testsrc`, encodes them through `encode`, and checks with `ffprobe` that the output frame count equals the input count and the dimensions are even.
- Manual after deploy: encode one full day at default settings and record the encode duration for M3 sizing.

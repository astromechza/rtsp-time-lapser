# rtsp-time-lapser

Captures one frame per minute from an RTSP camera to disk, serves a small HTML UI to browse them, and assembles any time range into an MP4 timelapse from the UI. Daily archiving (M3) comes later. See [the design spec](docs/superpowers/specs/2026-10-12-rtsp-time-lapser-design.md) and [the M2 timelapse spec](docs/superpowers/specs/2026-10-14-m2-timelapse-assembly-design.md).

## Configuration

Every setting is an env var (or the matching `--flag`):

| Env | Default | Purpose |
|---|---|---|
| `RTSP_URL` | required | `rtsp://user:pass@host:554/path`, special characters percent-encoded |
| `DATA_DIR` | `/data` | Root of all on-disk state |
| `LISTEN_ADDR` | `0.0.0.0:8080` | HTTP bind address |
| `CAPTURE_INTERVAL_SECS` | `60` | Seconds between captures |
| `CAPTURE_TIMEOUT_SECS` | `30` | Kill a hanging ffmpeg after this long |
| `JPEG_QUALITY` | `4` | ffmpeg `-q:v`, 2 (best) to 31 |
| `SCALE_WIDTH` | unset | Scale frames to this width |
| `STALE_AFTER_SECS` | `180` | `/healthz` fails when the newest frame is older |
| `FFMPEG_BIN` | `ffmpeg` | Path to ffmpeg |
| `TIMELAPSE_FPS` | `30` | Timelapse frame rate |
| `TIMELAPSE_CRF` | `26` | x264 quality, 0 (lossless) to 51; lower is bigger and sharper |
| `TIMELAPSE_PRESET` | `veryfast` | x264 preset, `ultrafast` to `veryslow` |
| `TIMELAPSE_DENOISE` | `8` | Spatial denoising strength (`hqdn3d`) before encoding, 0 to 30; 0 turns it off |
| `TIMELAPSE_MAX_WIDTH` | unset | Downscale timelapses to at most this width |
| `ENCODE_TIMEOUT_SECS` | `7200` | Kill an encode after this long |
| `OUT_RETENTION_DAYS` | `7` | Delete finished and failed timelapses after this many days |
| `RUST_LOG` | `info` | Log filter |

Frames land in `DATA_DIR/frames/YYYY-MM-DD/YYYYMMDD-HHMM.jpg` (UTC).

## Timelapses

`/timelapses` takes a UTC range, a step (keep every Nth frame) and an optional UTC hour window (e.g. 6 to 20, or 20 to 6 for nights only). Each day page has a "Timelapse this day" button. Jobs are encoded one at a time and survive restarts; the files live in `DATA_DIR/out/`:

| File | Meaning |
|---|---|
| `{name}.job` | Queued: an ffconcat list of the selected frames |
| `{name}.mp4.part` | Encoding |
| `{name}.mp4` | Done |
| `{name}.err` | Failed: ffmpeg's error. Submit the same range again to retry |

`{name}` encodes the selection, e.g. `20261011-0000_20261011-2359_s1_h06-20`, so submitting the same selection twice never encodes twice.

## Development

```bash
make lint
make test
```

Try a capture against the real camera (needs `ffmpeg` locally):

```bash
TEST_RTSP_URL='rtsp://viewer:pass@10.0.0.5:554/stream1' cargo test real_camera -- --ignored --nocapture
```

Encode generated frames with real ffmpeg (needs `ffmpeg` and `ffprobe` locally):

```bash
cargo test real_ffmpeg_encode -- --ignored --nocapture
```

Run locally:

```bash
RTSP_URL='rtsp://viewer:pass@10.0.0.5:554/stream1' DATA_DIR=./data LISTEN_ADDR=127.0.0.1:8080 cargo run
```

## Image

Built and pushed locally with podman (CI only runs lint and tests). Log in once with `podman login ghcr.io`.

```bash
make image
make image PLATFORMS=linux/amd64,linux/arm64 TAG=v0.1.0
```

## Deploy

`deploy/` is a Kustomize base with no namespace and no Ingress. The simplest install:

```bash
kubectl create namespace rtsp-time-lapser
kubectl -n rtsp-time-lapser create secret generic rtsp-time-lapser \
  --from-literal=RTSP_URL='rtsp://viewer:pass@10.0.0.5:554/stream1'
kubectl -n rtsp-time-lapser apply -k deploy
```

Use a view-only camera account.

### From another repo (overlay)

Reference the base at a pinned tag and add site-specific bits (namespace, storage class, ingress). **The UI has no authentication: anyone who can reach it sees the camera frames.** Keep it on an internal network or put auth in front.

```yaml
# kustomization.yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
namespace: production
resources:
  - github.com/astromechza/rtsp-time-lapser/deploy?ref=v0.1.1
  - ingress.yaml
images:
  - name: ghcr.io/astromechza/rtsp-time-lapser
    newTag: v0.1.1
patches:
  - target: { kind: PersistentVolumeClaim, name: rtsp-time-lapser-data }
    patch: |
      - op: add
        path: /spec/storageClassName
        value: topolvm-provisioner
```

```yaml
# ingress.yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: rtsp-time-lapser
spec:
  rules:
    - host: timelapse.example.lan
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: rtsp-time-lapser
                port:
                  name: http
```

## Monitoring

`/metrics` exposes `timelapse_last_frame_timestamp_seconds`, `timelapse_captures_total{result}`, `timelapse_capture_duration_seconds`, `timelapse_encodes_total{result}` and `timelapse_encode_duration_seconds`. Suggested alert:

```promql
time() - timelapse_last_frame_timestamp_seconds > 300
```

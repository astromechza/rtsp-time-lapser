# rtsp-time-lapser

Captures one frame per minute from an RTSP camera to disk and serves a small HTML UI to browse them. Timelapse assembly (M2) and daily archiving (M3) come later. See [the design spec](docs/superpowers/specs/2026-10-12-rtsp-time-lapser-design.md).

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
| `RUST_LOG` | `info` | Log filter |

Frames land in `DATA_DIR/frames/YYYY-MM-DD/YYYYMMDD-HHMM.jpg` (UTC).

## Development

```bash
make lint
make test
```

Try a capture against the real camera (needs `ffmpeg` locally):

```bash
TEST_RTSP_URL='rtsp://viewer:pass@10.0.0.5:554/stream1' cargo test real_camera -- --ignored --nocapture
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

```bash
kubectl apply -f deploy/namespace.yaml
kubectl -n rtsp-time-lapser create secret generic rtsp-time-lapser \
  --from-literal=RTSP_URL='rtsp://viewer:pass@10.0.0.5:554/stream1'
kubectl apply -k deploy
```

Set the image tag in `deploy/kustomization.yaml` (`images[0].newTag`). Use a view-only camera account.

### Ingress

No Ingress ships with the base. **The UI has no authentication: anyone who can reach it sees the camera frames.** Keep it on the LAN or put auth in front. Example overlay:

```yaml
# kustomization.yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - github.com/astromechza/rtsp-time-lapser/deploy?ref=v0.1.0
  - ingress.yaml
```

```yaml
# ingress.yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: rtsp-time-lapser
  namespace: rtsp-time-lapser
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

`/metrics` exposes `timelapse_last_frame_timestamp_seconds`, `timelapse_captures_total{result}` and `timelapse_capture_duration_seconds`. Suggested alert:

```promql
time() - timelapse_last_frame_timestamp_seconds > 300
```

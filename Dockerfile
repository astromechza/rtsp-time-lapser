# syntax=docker/dockerfile:1
FROM rust:1.99.0-alpine3.24 AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# rust-toolchain.toml is excluded by .dockerignore so the image's own 1.99.0 toolchain is used.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/rtsp-time-lapser /rtsp-time-lapser

FROM alpine:3.24
RUN apk add --no-cache ffmpeg tini \
    && addgroup -S -g 10001 app \
    && adduser -S -D -H -u 10001 -G app app
COPY --from=build /rtsp-time-lapser /usr/local/bin/rtsp-time-lapser
USER 10001:10001
ENV DATA_DIR=/data
EXPOSE 8080
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/rtsp-time-lapser"]

IMAGE ?= ghcr.io/astromechza/rtsp-time-lapser
TAG ?= $(shell git describe --tags --always --dirty)
PLATFORMS ?= linux/amd64

.PHONY: fmt lint test image

fmt:
	cargo fmt

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

image:
	docker buildx build --platform $(PLATFORMS) -t $(IMAGE):$(TAG) --push .

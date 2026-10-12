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

# Builds a (multi-arch) manifest list with podman and pushes it. Any stale local
# manifest with the same name is removed first so platforms don't accumulate.
image:
	-podman manifest rm $(IMAGE):$(TAG) 2>/dev/null
	podman build --platform $(PLATFORMS) --manifest $(IMAGE):$(TAG) .
	podman manifest push --all $(IMAGE):$(TAG) docker://$(IMAGE):$(TAG)

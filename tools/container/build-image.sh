#!/usr/bin/env bash
# Build the isolated test image.  Idempotent: re-run after editing the Dockerfile.
#
# Build-time downloads (apt, rustup components) go through the caller's proxy, which
# is passed as a build argument and never stored in the image.  --network host makes
# the caller's loopback proxy reachable from the build.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE="${MUXTERM_TEST_IMAGE:-muxterm-test:latest}"
# Any image that has (or can install) Rust, tmux and python3 will do; the default is
# the canonical public one.  Override it to reuse an image that is already local.
BASE_IMAGE="${MUXTERM_BASE_IMAGE:-rust:1-slim-bookworm}"

proxy_args=()
if [ -n "${https_proxy:-}" ]; then
  proxy_args+=(--build-arg "http_proxy=${http_proxy:-$https_proxy}")
  proxy_args+=(--build-arg "https_proxy=${https_proxy}")
  proxy_args+=(--build-arg "HTTP_PROXY=${HTTP_PROXY:-$https_proxy}")
  proxy_args+=(--build-arg "HTTPS_PROXY=${HTTPS_PROXY:-$https_proxy}")
fi

docker build \
  --network host \
  --build-arg "BASE_IMAGE=${BASE_IMAGE}" \
  "${proxy_args[@]}" \
  --file "$ROOT/docker/test-image.Dockerfile" \
  --tag "$IMAGE" \
  "$ROOT/docker"

echo "built $IMAGE"
docker run --rm --entrypoint /bin/sh "$IMAGE" -c 'cargo --version; rustc --version; tmux -V; python3 --version'
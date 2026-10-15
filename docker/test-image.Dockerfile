# Test/dev image for Muxterm.
#
# Everything in this repository is built and tested *inside* this image, so that no
# tmux server, PTY or cargo artefact ever touches the developer's desktop session.
#
# BASE_IMAGE defaults to the canonical public Rust image.  Any Debian/Ubuntu based
# image that provides (or can install) Rust, tmux and python3 works; pointing the
# build at an image that is already present locally avoids a large download:
#
#   MUXTERM_BASE_IMAGE=my-local-image tools/container/build-image.sh
#
# See docs/dev-environment.md.
ARG BASE_IMAGE=rust:1-slim-bookworm
FROM ${BASE_IMAGE}

# Proxy for build-time downloads only.  Never baked into the image.
ARG http_proxy=""
ARG https_proxy=""
ARG HTTP_PROXY=""
ARG HTTPS_PROXY=""

# Some base images carry an entrypoint for their own runtime; the test runner needs a
# container that simply runs the command it is given.
ENTRYPOINT []
SHELL ["/bin/bash", "-lc"]

# The test dependency set:
#   tmux            the system under test (Debian bookworm ships 3.3a, the oldest
#                   entry of the compatibility matrix)
#   python3         runs tools/probe/probe_control_mode.py
#   toolchain, libs needed to build further tmux versions from source
#   openssh-client  exercises the SSH transport
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends \
      tmux \
      python3 \
      curl \
      ca-certificates \
      xz-utils \
      git \
      openssh-client \
      build-essential \
      pkg-config \
      locales \
      procps \
      less \
      libevent-dev \
      libncurses-dev \
      bison \
 && rm -rf /var/lib/apt/lists/*

# Lint components are optional: without network access the image still builds and the
# lint stage of tools/container/test.sh reports that clippy is unavailable.
RUN rustup component add clippy rustfmt \
 && (cargo clippy --version || true) \
 && (cargo fmt --version || true)

# Extra tmux versions from the compatibility matrix are built into
# /opt/tmux/<version>/bin/tmux by tools/container/build-tmux-versions.sh and selected
# with PATH.

# Deterministic locale so byte/width tests do not depend on the host.
ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    TERM=xterm-256color \
    TMUX_TMPDIR=/tmp/tmux-runtime \
    CARGO_TERM_COLOR=always \
    RUST_BACKTRACE=1 \
    MUXTERM_IN_CONTAINER=1

# tmux must never see the host's server.
RUN mkdir -p /tmp/tmux-runtime && chmod 700 /tmp/tmux-runtime

WORKDIR /work
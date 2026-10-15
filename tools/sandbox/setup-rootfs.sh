#!/usr/bin/env bash
# Provision the sandbox rootfs (once): tmux, python3, a C toolchain, ssh and a
# Rust toolchain.
#
# Downloads go through the caller's proxy settings ($http_proxy/$https_proxy),
# which are forwarded into the sandbox.
#
# Rootfs flavours supported: Alpine (apk) and Debian/Ubuntu (apt).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ROOTFS="${MUXTERM_SANDBOX_ROOTFS:-$ROOT/.sandbox/rootfs}"

# absolute symlinks (/bin/sh -> /bin/busybox) do not resolve outside the chroot
if [ ! -e "$ROOTFS/bin/busybox" ] && [ ! -e "$ROOTFS/bin/bash" ] && [ ! -e "$ROOTFS/bin/dash" ]; then
  cat >&2 <<EOF
sandbox rootfs missing at $ROOTFS

Alpine (small, fast):
  mkdir -p "$ROOT/.sandbox" && cd "$ROOT/.sandbox"
  curl -sSL -o alpine.tar.gz https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz
  mkdir -p rootfs && tar -xzf alpine.tar.gz -C rootfs

Ubuntu (bigger, glibc):
  curl -sSL -o rootfs.tar.xz https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-amd64-root.tar.xz
  mkdir -p rootfs && tar -xJf rootfs.tar.xz -C rootfs --numeric-owner
EOF
  exit 2
fi

echo "== provisioning $ROOTFS =="
"$ROOT/tools/sandbox/run.sh" '
set -e
# rustup downloads into TMPDIR and then renames into RUSTUP_HOME; the rootfs lives
# on a network filesystem where a cross-device rename fails, so keep TMPDIR on the
# same filesystem as the toolchain.
export TMPDIR=/var/tmp
mkdir -p /var/tmp /usr/local/cargo /usr/local/rustup

if command -v apk >/dev/null 2>&1; then
  echo "--- apk"
  apk update
  apk add --no-cache tmux python3 build-base curl ca-certificates openssh-client \
      bash ncurses-terminfo git procps vim
else
  echo "--- apt"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends \
      tmux openssh-client ca-certificates curl xz-utils \
      build-essential pkg-config python3 locales procps less vim-tiny
  sed -i "s/^# *en_US.UTF-8/en_US.UTF-8/" /etc/locale.gen 2>/dev/null || true
  locale-gen en_US.UTF-8 >/dev/null 2>&1 || true
fi

if [ ! -x /usr/local/cargo/bin/cargo ]; then
  echo "--- rustup"
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs -o /var/tmp/rustup-init.sh
  RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
    sh /var/tmp/rustup-init.sh -y --profile minimal --default-toolchain stable --no-modify-path
fi

echo "--- versions"
/usr/local/cargo/bin/cargo --version
/usr/local/cargo/bin/rustc --version
tmux -V
python3 --version
'
echo "== done =="
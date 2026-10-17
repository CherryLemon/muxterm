#!/usr/bin/env bash
# Build tmux releases from source into /opt/tmux/<version>/bin.
#
# The compatibility matrix names three releases besides the Debian package; this
# is the script that puts them on disk.  It is deliberately a plain configure &&
# make build: no source patches, no vendored tree, no attempt to "fix" upstream.
#
# Usage: tools/container/build-tmux-versions.sh [version ...]
#        (default: 3.4)
#
# The build dependencies are installed by docker/test-image.Dockerfile:
#   build-essential pkg-config libevent-dev libncurses-dev bison curl
#
# Select a version afterwards with:
#   PATH=/opt/tmux/3.4/bin:$PATH tmux -V
set -euo pipefail

PREFIX="${TMUX_PREFIX:-/opt/tmux}"
versions=("$@")
if [ "${#versions[@]}" -eq 0 ]; then
    versions=(3.4)
fi

work="${TMPDIR:-/tmp}/muxterm-tmux-build.$$"
mkdir -p "$work"
trap 'rm -rf "$work"' EXIT

for version in "${versions[@]}"; do
    echo "== building tmux $version into $PREFIX/$version =="
    tarball="tmux-$version.tar.gz"
    url="https://github.com/tmux/tmux/releases/download/$version/$tarball"
    curl -fsSL --retry 3 --retry-delay 2 -o "$work/$tarball" "$url"
    tar -C "$work" -xzf "$work/$tarball"
    (
        cd "$work/tmux-$version"
        ./configure --prefix="$PREFIX/$version"
        make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)"
        make install
    )
    "$PREFIX/$version/bin/tmux" -V
done
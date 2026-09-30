#!/usr/bin/env bash
# Runs scripts/deps/<name>.sh inside Ubuntu 22.04 (podman or docker), like CI: its output
# (build/deps) then carries that glibc (2.35), not the build machine's.
# Usage: scripts/deps/container.sh ffmpeg [mesa]
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
RUN=$(command -v podman || command -v docker)
"$RUN" run --rm -v "$ROOT:/src:z" -w /src docker.io/library/ubuntu:22.04 bash -c \
  "bash scripts/deps/ubuntu-packages.sh && for s in $*; do PREFIX=${PREFIX:-/src/build/deps} SRC=/src/build/deps-src-jammy bash scripts/deps/\$s.sh || exit 1; done"

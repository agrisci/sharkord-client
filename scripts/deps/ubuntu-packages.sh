#!/usr/bin/env bash
# What scripts/deps/*.sh need on Ubuntu 22.04, the release build machine (CI) and
# scripts/deps/container.sh: compilers, meson from pip (Mesa needs >= 1.4), and for Mesa's
# build-time shader compilers LLVM 15 with libclc and the SPIR-V translator. SPIRV-Tools and
# glslang are too old here; mesa.sh and ffmpeg.sh build their own. libva2 lets the helper's
# self-check run.
set -euo pipefail
SUDO=$([ "$(id -u)" = 0 ] || echo sudo)
$SUDO apt-get update -qq
$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
  build-essential git curl ca-certificates cmake ninja-build pkg-config python3-pip bison flex \
  libpipewire-0.3-dev libva2 libva-drm2 libudev-dev libzstd-dev zlib1g-dev libelf-dev libclang-dev \
  llvm-15-dev libclang-15-dev libclang-common-15-dev clang-15 libclc-15-dev libclc-15 \
  libllvmspirvlib-15-dev llvm-spirv-15 >/dev/null
$SUDO pip3 install -q 'meson>=1.4' mako pyyaml packaging

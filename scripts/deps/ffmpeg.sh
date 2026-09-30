#!/usr/bin/env bash
# Builds the static, LGPL-only FFmpeg the Linux helper links: hardware encoders only (Vulkan and
# VA-API), the few filters that move a PipeWire DMA-BUF into an encoder frame, no programs.
# Everything comes from pinned sources into $PREFIX (default build/deps), so the result doesn't
# depend on what the build machine's distro strips (Fedora's FFmpeg has no H.264) or how old it is
# (Ubuntu 22.04's Vulkan headers predate Vulkan video encode). libva, libdrm and the Vulkan loader
# are linked dynamically: every desktop that can encode has them, and a static libva would carry
# one distro's driver path.
#
# Usage: scripts/deps/ffmpeg.sh            (needs gcc/g++, cmake, meson, ninja, pkg-config, git)
#        scripts/deps/container.sh ffmpeg  (the same inside the release build container)
set -euo pipefail

FFMPEG=n8.1.3
VULKAN_HEADERS=v1.4.364     # >= 1.4.317 for av1_vulkan
GLSLANG=16.6.0              # scale_vulkan: its shaders compiled at build time (glslang) and run time (libglslang)
LIBVA=2.24.1                # headers for the VA-API 1.15+ AV1 encode structs
LIBDRM=libdrm-2.4.134
SPIRV_HEADERS=vulkan-sdk-1.4.321.0
SPIRV_TOOLS=v2025.3

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=${PREFIX:-$ROOT/build/deps}
SRC=${SRC:-$ROOT/build/deps-src}
JOBS=${JOBS:-$(nproc)}
mkdir -p "$PREFIX" "$SRC"
export PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig:$PREFIX/lib64/pkgconfig:$PREFIX/share/pkgconfig
export PKG_CONFIG_LIBDIR=$PKG_CONFIG_PATH   # only ours: the host's libva/libdrm must not win
export PATH=$PREFIX/bin:$PATH

fetch () {  # fetch <dir> <git url> <tag>
  [ -d "$SRC/$1" ] || git -c advice.detachedHead=false clone -q --depth 1 --branch "$3" "$2" "$SRC/$1"
}

fetch vulkan-headers https://github.com/KhronosGroup/Vulkan-Headers.git $VULKAN_HEADERS
cmake -S "$SRC/vulkan-headers" -B "$SRC/vulkan-headers/build" -G Ninja -DCMAKE_INSTALL_PREFIX="$PREFIX" \
  -DVULKAN_HEADERS_ENABLE_MODULE=OFF -DVULKAN_HEADERS_ENABLE_TESTS=OFF >/dev/null
cmake --build "$SRC/vulkan-headers/build" --target install >/dev/null

# SPIRV-Tools, static: FFmpeg's configure links glslang with it (glslang itself is built without its
# optimizer, so none of it ends up in the helper), and Mesa's shader compilers need >= 2024.1
fetch spirv-headers https://github.com/KhronosGroup/SPIRV-Headers.git $SPIRV_HEADERS
fetch spirv-tools https://github.com/KhronosGroup/SPIRV-Tools.git $SPIRV_TOOLS
cmake -S "$SRC/spirv-headers" -B "$SRC/spirv-headers/build" -G Ninja -DCMAKE_INSTALL_PREFIX="$PREFIX" \
  -DSPIRV_HEADERS_ENABLE_TESTS=OFF >/dev/null
cmake --build "$SRC/spirv-headers/build" --target install >/dev/null
cmake -S "$SRC/spirv-tools" -B "$SRC/spirv-tools/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="$PREFIX" -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
  -DSPIRV-Headers_SOURCE_DIR="$SRC/spirv-headers" -DSPIRV_SKIP_TESTS=ON -DSPIRV_WERROR=OFF \
  -DSPIRV_SKIP_EXECUTABLES=ON -DBUILD_SHARED_LIBS=OFF >/dev/null
cmake --build "$SRC/spirv-tools/build" -j "$JOBS" --target install >/dev/null

# glslang without SPIRV-Tools' optimizer: FFmpeg only compiles a few small compute shaders
fetch glslang https://github.com/KhronosGroup/glslang.git $GLSLANG
cmake -S "$SRC/glslang" -B "$SRC/glslang/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="$PREFIX" -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
  -DBUILD_SHARED_LIBS=OFF -DENABLE_OPT=OFF -DENABLE_GLSLANG_BINARIES=ON -DENABLE_HLSL=OFF \
  -DGLSLANG_TESTS=OFF >/dev/null
cmake --build "$SRC/glslang/build" -j "$JOBS" --target install >/dev/null

# libdrm and libva: shared, only to link against (the system's copies are loaded at run time)
fetch libdrm https://gitlab.freedesktop.org/mesa/drm.git $LIBDRM
meson setup --reconfigure "$SRC/libdrm/build" "$SRC/libdrm" --prefix="$PREFIX" --libdir=lib \
  --buildtype=release -Dintel=disabled -Dradeon=disabled -Damdgpu=disabled -Dnouveau=disabled \
  -Dvmwgfx=disabled -Dtests=false -Dman-pages=disabled -Dvalgrind=disabled -Dcairo-tests=disabled >/dev/null
ninja -C "$SRC/libdrm/build" install >/dev/null

fetch libva https://github.com/intel/libva.git $LIBVA
meson setup --reconfigure "$SRC/libva/build" "$SRC/libva" --prefix="$PREFIX" --libdir=lib \
  --buildtype=release -Dwith_x11=no -Dwith_glx=no -Dwith_wayland=no -Dwith_win32=no >/dev/null
ninja -C "$SRC/libva/build" install >/dev/null

# Our patches on a clean tree: runtime bitrate changes without an IDR (Vulkan, VA-API)
fetch ffmpeg https://git.ffmpeg.org/ffmpeg.git $FFMPEG
git -C "$SRC/ffmpeg" checkout -q -- .
for p in "$ROOT"/scripts/deps/ffmpeg-*.patch; do
  git -C "$SRC/ffmpeg" apply "$p"
done
mkdir -p "$SRC/ffmpeg/build"
cd "$SRC/ffmpeg/build"
../configure --prefix="$PREFIX" --pkg-config-flags=--static \
  --extra-cflags="-I$PREFIX/include" --extra-ldflags="-L$PREFIX/lib" --extra-libs="-lstdc++" \
  --disable-everything --disable-autodetect --disable-programs --disable-doc --disable-network \
  --disable-avformat --disable-avdevice --disable-swresample --disable-swscale \
  --disable-x86asm --disable-debug --enable-static --disable-shared --enable-pic \
  --enable-avcodec --enable-avutil --enable-avfilter --enable-pthreads \
  --enable-libdrm --enable-vaapi --enable-vulkan --enable-libglslang \
  --enable-encoder=h264_vulkan,av1_vulkan,h264_vaapi,av1_vaapi \
  --enable-filter=buffer,buffersink,format,hwmap,hwupload,scale_vulkan,scale_vaapi \
  >configure.log
grep -q 'License: LGPL version 2.1 or later' configure.log || { echo 'FFmpeg is not LGPL-2.1 only' >&2; exit 1; }
make -j "$JOBS" >/dev/null
make install >/dev/null
cp configure.log "$PREFIX/ffmpeg-configure.log"
# Licences of what ends up in the helper, for scripts/stage-native.js
mkdir -p "$PREFIX/licenses"
cp "$SRC/ffmpeg/COPYING.LGPLv2.1" "$PREFIX/licenses/FFmpeg-LGPL-2.1.txt"
cp "$SRC/glslang/LICENSE.txt" "$PREFIX/licenses/glslang.txt"
# The static libraries are bundled into ffmpeg-sys-next's rlib when that crate compiles: a
# rebuilt FFmpeg only reaches the helper once the crate is rebuilt
[ -d "$ROOT/native/target" ] && cargo clean -q --manifest-path "$ROOT/native/Cargo.toml" --release -p ffmpeg-sys-next || true
echo "FFmpeg $FFMPEG in $PREFIX"

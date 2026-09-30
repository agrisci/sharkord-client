#!/usr/bin/env bash
# Builds the static, LGPL-only FFmpeg the Windows helper links: DXGI desktop duplication (ddagrab),
# the D3D11 scaler, and the hardware encoders of the three GPU vendors (AMD AMF, NVIDIA NVENC,
# Intel Quick Sync through oneVPL), no programs. Built with MSVC (Rust's MSVC target can't link
# MinGW archives) against the static CRT, like the helper, so the helper is one self-contained exe.
# AMF and NVENC are headers only (the drivers' DLLs load at run time); libvpl, the Quick Sync
# dispatcher, is linked in. Everything from pinned sources into $PREFIX (default build/deps).
#
# Usage, from MSYS2's bash (make, diffutils, git, pkgconf from pacman) with Visual Studio 2022's C++
# tools installed (their environment is imported here when `cl` isn't on PATH):
#   C:\msys64\usr\bin\bash.exe scripts/deps/ffmpeg-windows.sh
set -euo pipefail

FFMPEG=n9.0.2               # >= 9.0: AMF without a frame of delay (AV_CODEC_FLAG_LOW_DELAY)
AMF=v1.5.3                  # headers only; FFmpeg needs >= 1.5.2
NV_CODEC_HEADERS=n12.1.14.0 # headers only; the oldest FFmpeg 9 takes, so NVIDIA drivers >= 531 work
LIBVPL=v2.17.0

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=${PREFIX:-$ROOT/build/deps}
SRC=${SRC:-$ROOT/build/deps-src}
JOBS=${JOBS:-$(nproc)}
export PATH=/usr/bin:$PATH

# The MSVC environment, from vcvars64.bat, unless the caller already has it. MSVC's directories go
# first: MSYS2's coreutils have a `link` of their own
if ! command -v cl >/dev/null; then
  VS=$("/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe" -latest -products '*' \
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath | tr -d '\r')
  [ -n "$VS" ] || { echo 'no Visual Studio with the C++ tools' >&2; exit 1; }
  # Through a batch file: MSYS2 would re-quote the path on a cmd command line
  BAT=$(mktemp --suffix=.bat)
  printf '@call "%s\\VC\\Auxiliary\\Build\\vcvars64.bat" >nul\r\n@set\r\n' "$(cygpath -w "$VS")" >"$BAT"
  VSENV=$(cmd //c "$(cygpath -w "$BAT")" | tr -d '\r')
  rm -f "$BAT"
  VSPATH=
  while IFS='=' read -r k v; do
    case "$k" in
      Path|PATH) VSPATH=$v ;;
      INCLUDE|LIB|LIBPATH|VCToolsInstallDir|VCINSTALLDIR|WindowsSdkDir|WindowsSDKVersion|UCRTVersion|UniversalCRTSdkDir|VSINSTALLDIR)
        export "$k=$v" ;;
    esac
  done <<<"$VSENV"
  [ -n "$VSPATH" ] || { echo 'vcvars64.bat gave no environment' >&2; exit 1; }
  export PATH="$(cygpath -p "$VSPATH"):$PATH"
fi
CL_DIR=$(dirname "$(command -v cl)")
export PATH="$CL_DIR:/usr/bin:$PATH"

mkdir -p "$PREFIX" "$SRC"
WPREFIX=$(cygpath -m "$PREFIX")
export PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig
export PKG_CONFIG_LIBDIR=$PKG_CONFIG_PATH

fetch () {  # fetch <dir> <git url> <tag>
  [ -d "$SRC/$1" ] || git -c advice.detachedHead=false clone -q --depth 1 --branch "$3" "$2" "$SRC/$1"
}

# AMF: only its public headers, as FFmpeg includes them (<AMF/core/Factory.h>)
if [ ! -d "$SRC/amf" ]; then
  git -c advice.detachedHead=false clone -q --depth 1 --branch $AMF --filter=blob:none --sparse \
    https://github.com/GPUOpen-LibrariesAndSDKs/AMF.git "$SRC/amf"
  git -C "$SRC/amf" sparse-checkout set amf/public/include
fi
rm -rf "$PREFIX/include/AMF" && mkdir -p "$PREFIX/include"
cp -r "$SRC/amf/amf/public/include" "$PREFIX/include/AMF"

fetch nv-codec-headers https://github.com/FFmpeg/nv-codec-headers.git $NV_CODEC_HEADERS
make -C "$SRC/nv-codec-headers" install PREFIX="$PREFIX" >/dev/null

# libvpl: the Quick Sync dispatcher (it finds Intel's runtime in the driver), static with the static CRT
fetch libvpl https://github.com/intel/libvpl.git $LIBVPL
cmake -S "$SRC/libvpl" -B "$SRC/libvpl/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="$WPREFIX" -DUSE_MSVC_STATIC_RUNTIME=ON -DCMAKE_POLICY_DEFAULT_CMP0091=NEW \
  -DBUILD_SHARED_LIBS=OFF -DBUILD_TESTS=OFF -DBUILD_EXAMPLES=OFF \
  -DINSTALL_EXAMPLES=OFF -DINSTALL_DEV=ON >/dev/null
cmake --build "$SRC/libvpl/build" -j "$JOBS" --target install >/dev/null
# Its pkg-config file leaves out what the static dispatcher needs from Windows (the registry, COM)
sed -i 's|\r||; s|^Libs:.*|Libs: -L${libdir} -lvpl -ladvapi32 -lole32|' "$PREFIX/lib/pkgconfig/vpl.pc"

# Our patches on a clean tree: runtime bitrate changes without an IDR (AMF, NVENC, Quick Sync), and
# the D3D11 scaler's colour space and output pool
fetch ffmpeg https://git.ffmpeg.org/ffmpeg.git $FFMPEG
git -C "$SRC/ffmpeg" checkout -q -- .
for p in "$ROOT"/scripts/deps/ffmpeg-windows-*.patch; do
  git -C "$SRC/ffmpeg" apply "$p"
done
# In the source tree: MSYS2 has no symbolic link for an out-of-tree build's `src`
cd "$SRC/ffmpeg"
git clean -qfdx
./configure --toolchain=msvc --prefix="$PREFIX" --pkg-config-flags=--static \
  --extra-cflags="-MT -I$WPREFIX/include" --extra-ldflags="-LIBPATH:$WPREFIX/lib" \
  --disable-everything --disable-autodetect --disable-programs --disable-doc --disable-network \
  --disable-avformat --disable-avdevice --disable-swresample --disable-swscale \
  --disable-x86asm --disable-debug --enable-static --disable-shared \
  --enable-avcodec --enable-avutil --enable-avfilter --enable-w32threads \
  --enable-d3d11va --enable-amf --enable-ffnvcodec --enable-nvenc --enable-libvpl \
  --enable-encoder=h264_amf,av1_amf,h264_nvenc,av1_nvenc,h264_qsv,av1_qsv \
  --enable-filter=buffer,buffersink,format,hwmap,hwupload,ddagrab,scale_d3d11 \
  >configure.log
grep -q 'License: LGPL version 2.1 or later' configure.log || { echo 'FFmpeg is not LGPL-2.1 only' >&2; exit 1; }
for e in h264_amf av1_amf h264_nvenc av1_nvenc h264_qsv av1_qsv; do
  grep -q "^$e" <(sed -n '/Enabled encoders:/,/^$/p' configure.log | tr -s ' ' '\n') ||
    { echo "configure left out $e (see $SRC/ffmpeg/configure.log)" >&2; exit 1; }
done
make -j "$JOBS" >/dev/null
make install >/dev/null
cp configure.log "$PREFIX/ffmpeg-configure.log"
# Licences of what ends up in the helper, for scripts/stage-native.js
mkdir -p "$PREFIX/licenses"
cp "$SRC/ffmpeg/COPYING.LGPLv2.1" "$PREFIX/licenses/FFmpeg-LGPL-2.1.txt"
cp "$SRC/libvpl/LICENSE" "$PREFIX/licenses/libvpl.txt"
cp "$SRC/amf/LICENSE.txt" "$PREFIX/licenses/AMF.txt" 2>/dev/null ||
  git -C "$SRC/amf" show HEAD:LICENSE.txt >"$PREFIX/licenses/AMF.txt"
sed -n '/^ \*/,/\*\//p' "$SRC/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h" | head -30 >"$PREFIX/licenses/nv-codec-headers.txt"
# The static libraries are bundled into ffmpeg-sys-next's rlib when that crate compiles: a
# rebuilt FFmpeg only reaches the helper once the crate is rebuilt
[ -d "$ROOT/native/target" ] && cargo clean -q --manifest-path "$ROOT/native/Cargo.toml" --release -p ffmpeg-sys-next || true
echo "FFmpeg $FFMPEG in $PREFIX"

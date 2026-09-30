#!/usr/bin/env bash
# Builds the Vulkan drivers the Linux helper falls back to when the system's can't encode H.264
# (Fedora and openSUSE build Mesa without it): RADV (AMD) and ANV (Intel), video encode on
# (H.264, H.265, AV1), nothing else -- no OpenGL, no window system (the helper only encodes).
# The helper points the Vulkan loader at them (VK_DRIVER_FILES) only after the system's driver
# failed its encode probe; the app itself never loads them.
#
# RADV compiles shaders with ACO and ANV with its precompiled kernels, so neither needs LLVM at run
# time; building ANV's kernels does (mesa-clc: LLVM, clang, libclc), so Mesa's compilers are built
# first on their own and the drivers after, without LLVM. libdrm is built in statically (RADV needs
# a newer one than old distros ship).
#
# Usage: scripts/deps/mesa.sh            (after ffmpeg.sh, for its glslang; needs meson >= 1.4, ninja,
#                                         gcc/g++, python3-mako, pyyaml, llvm/clang/libclc/
#                                         spirv-llvm-translator dev files: ubuntu-packages.sh)
#        scripts/deps/container.sh mesa  (the same inside the release build container)
set -euo pipefail

MESA=mesa-26.2.3

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=${PREFIX:-$ROOT/build/deps}
SRC=${SRC:-$ROOT/build/deps-src}
mkdir -p "$PREFIX/mesa" "$SRC"

[ -d "$SRC/mesa" ] || git -c advice.detachedHead=false clone -q --depth 1 --branch $MESA https://gitlab.freedesktop.org/mesa/mesa.git "$SRC/mesa"
common=(--buildtype=release -Db_ndebug=true -Dgallium-drivers= -Dplatforms= -Dopengl=false
  -Dglx=disabled -Degl=disabled -Dgles1=disabled -Dgles2=disabled -Dgbm=disabled -Dvulkan-layers=
  -Dtools= -Dbuild-tests=false -Dintel-rt=disabled -Dintel-elk=false -Dxmlconfig=disabled
  -Dexpat=disabled -Dvalgrind=disabled -Dlibunwind=disabled)

# From ffmpeg.sh (run it first): glslangValidator (Mesa needs >= 12.2) and SPIRV-Tools >= 2024.1
# for mesa-clc; Ubuntu 22.04's are older
TOOLS=$SRC/mesa/tools
export PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig:$PREFIX/share/pkgconfig:${PKG_CONFIG_PATH:-}
export PATH=$PREFIX/bin:$TOOLS/bin:$PATH

# 1. Mesa's own shader compilers (mesa_clc, vtn_bindgen2), with LLVM: build tools only
meson setup --wipe "$SRC/mesa/build-tools" "$SRC/mesa" --prefix="$TOOLS" "${common[@]}" \
  -Dvulkan-drivers=intel -Dllvm=enabled -Dshared-llvm=enabled -Dmesa-clc=enabled \
  -Dprecomp-compiler=enabled -Dinstall-mesa-clc=true -Dinstall-precomp-compiler=true >/dev/null
ninja -C "$SRC/mesa/build-tools" install >/dev/null

# 2. The drivers, with those tools and no LLVM or SPIRV-Tools at run time
meson setup --wipe "$SRC/mesa/build" "$SRC/mesa" --prefix="$SRC/mesa/install" \
  --libdir=lib "${common[@]}" -Dstrip=true -Dvulkan-drivers=amd,intel \
  -Dvideo-codecs=h264enc,h265enc,av1enc -Dllvm=disabled -Damd-use-llvm=false -Dspirv-tools=disabled \
  -Dmesa-clc=system -Dprecomp-compiler=system \
  -Dallow-fallback-for=libdrm --force-fallback-for=libdrm -Dlibdrm:default_library=static >/dev/null
ninja -C "$SRC/mesa/build" install >/dev/null

# The drivers and loader manifests pointing next to themselves (paths relative to the manifest)
for d in radeon intel; do
  cp "$SRC/mesa/install/lib/libvulkan_$d.so" "$PREFIX/mesa/"
  api=$(python3 -c "import json;print(json.load(open('$SRC/mesa/install/share/vulkan/icd.d/${d}_icd.x86_64.json'))['ICD']['api_version'])")
  printf '{"file_format_version":"1.0.1","ICD":{"library_path":"./libvulkan_%s.so","api_version":"%s"}}\n' "$d" "$api" >"$PREFIX/mesa/${d}_icd.json"
done
mkdir -p "$PREFIX/licenses"
cp "$SRC/mesa/docs/license.rst" "$PREFIX/licenses/Mesa.rst"
echo "$MESA Vulkan drivers in $PREFIX/mesa"

// Stages the native screen share helper (native/) into build/native/, which electron-builder
// ships as resources/native/.
//
// Linux: the helper in bin/, statically linked with our FFmpeg (scripts/deps/ffmpeg.sh), and in
// mesa/ the Mesa Vulkan drivers it falls back to where the system's can't encode H.264
// (scripts/deps/mesa.sh), with their licences in LICENSES/. Both deps scripts run here when their
// output is missing (CI fetches it prebuilt instead). The app probes the helper at startup with
// `--check`.
//
// Windows: the helper in bin/, one exe statically linked with our FFmpeg and the static C runtime
// (scripts/deps/ffmpeg-windows.sh, run here through MSYS2's bash when its output is missing), with
// the licences in LICENSES/. The GPU vendors' encoder runtimes come with their drivers.
//
// Needs Rust, and on Windows MSYS2 (make, diffutils, git, pkgconf) and Visual Studio's C++ tools;
// on Linux what the deps scripts list. Without them it warns and stages nothing (Linux: no Mesa
// fallback if only that fails), so `npm run dist:*` still builds; SHARKORD_REQUIRE_NATIVE=1 (CI)
// makes that an error instead.
const fs = require('fs')
const path = require('path')
const { spawnSync } = require('child_process')

const ROOT = path.join(__dirname, '..')
const OUT = path.join(ROOT, 'build', 'native')
const EXE = path.join(ROOT, 'native', 'target', 'release', process.platform === 'win32' ? 'sharkord-share.exe' : 'sharkord-share')
const DEPS = path.join(ROOT, 'build', 'deps')
const MSYS2 = process.env.MSYS2_ROOT || 'C:\\msys64'
const REQUIRE = process.env.SHARKORD_REQUIRE_NATIVE === '1'

const log = (...a) => console.log('[stage-native]', ...a)
function skip (why) {
  if (REQUIRE) { console.error('[stage-native] error:', why); process.exit(1) }
  log('skipped:', why, '-- the installer will have no native screen share')
  fs.rmSync(OUT, { recursive: true, force: true })
  fs.mkdirSync(OUT, { recursive: true })   // electron-builder's extraResources wants it to exist
  process.exit(0)
}

function copy (from, to) { fs.mkdirSync(path.dirname(to), { recursive: true }); fs.copyFileSync(from, to) }

if (process.platform === 'linux') {
  const script = name => spawnSync('bash', [path.join(ROOT, 'scripts', 'deps', name)], { stdio: 'inherit' })
  if (!fs.existsSync(path.join(DEPS, 'lib', 'libavcodec.a'))) {
    const r = script('ffmpeg.sh')
    if (r.error || r.status !== 0) skip('scripts/deps/ffmpeg.sh failed (see its header for what it needs)')
  }
  if (!fs.existsSync(path.join(DEPS, 'mesa', 'radeon_icd.json'))) {
    const r = script('mesa.sh')
    if (r.error || r.status !== 0) {
      if (REQUIRE) { console.error('[stage-native] error: scripts/deps/mesa.sh failed'); process.exit(1) }
      log('warning: scripts/deps/mesa.sh failed -- no bundled Vulkan drivers (clean Fedora/openSUSE get no H.264)')
    }
  }
  const cargo = spawnSync('cargo', ['build', '--release'], {
    cwd: path.join(ROOT, 'native'), stdio: 'inherit',
    env: { ...process.env, FFMPEG_DIR: DEPS, PKG_CONFIG_PATH: path.join(DEPS, 'lib', 'pkgconfig') },
  })
  if (cargo.error || cargo.status !== 0) skip('cargo build failed')
  fs.rmSync(OUT, { recursive: true, force: true })
  const exe = path.join(OUT, 'bin', 'sharkord-share')
  copy(EXE, exe)
  spawnSync('strip', ['--strip-debug', exe])   // 43 -> 10 MB; the build keeps line tables for backtraces
  for (const f of fs.existsSync(path.join(DEPS, 'mesa')) ? fs.readdirSync(path.join(DEPS, 'mesa')) : []) {
    copy(path.join(DEPS, 'mesa', f), path.join(OUT, 'mesa', f))
  }
  // Licences of what ships inside: FFmpeg (LGPL, static in the helper, with how to rebuild it),
  // glslang (static, for FFmpeg's scale_vulkan), Mesa (the bundled drivers)
  const lic = path.join(OUT, 'LICENSES')
  for (const f of fs.readdirSync(path.join(DEPS, 'licenses'))) copy(path.join(DEPS, 'licenses', f), path.join(lic, f))
  const ffmpeg = fs.readFileSync(path.join(ROOT, 'scripts', 'deps', 'ffmpeg.sh'), 'utf8').match(/^FFMPEG=(\S+)/m)?.[1]
  fs.writeFileSync(path.join(lic, 'SOURCES.md'), [
    '# Third-party code in this folder\'s helper and drivers', '',
    `- FFmpeg ${ffmpeg} (LGPL-2.1), https://git.ffmpeg.org/ffmpeg.git, statically linked into bin/sharkord-share.`,
    '  Configure line and patches: scripts/deps/ffmpeg.sh and scripts/deps/ffmpeg-*.patch in',
    '  https://github.com/agrisci/sharkord-client; relink with your own FFmpeg by building native/ with FFMPEG_DIR.',
    '- glslang (BSD-3-Clause and others, see glslang.txt), statically linked for FFmpeg\'s scale_vulkan.',
    '- Mesa (MIT, see Mesa.rst): mesa/libvulkan_radeon.so and mesa/libvulkan_intel.so, built by scripts/deps/mesa.sh.', '',
  ].join('\n'))
  // Proves it runs: any answer will do. What this machine can encode is the app's business at
  // runtime -- CI has no GPU
  const check = spawnSync(exe, ['--check'], { encoding: 'utf8' })
  try { JSON.parse(check.stdout) } catch { throw new Error(`self-check failed: ${check.stdout}${check.stderr}`) }
  log(`staged ${exe}; self-check ${check.stdout.trim()}`)
  process.exit(0)
}
if (process.platform !== 'win32') skip('the native share is Windows and Linux only')

// Windows. Our FFmpeg, unless already built (CI restores it from its cache)
if (!fs.existsSync(path.join(DEPS, 'lib', 'avcodec.lib'))) {
  const bash = path.join(MSYS2, 'usr', 'bin', 'bash.exe')
  if (!fs.existsSync(bash)) skip(`no MSYS2 at ${MSYS2} (set MSYS2_ROOT)`)
  const r = spawnSync(bash, [path.join(ROOT, 'scripts', 'deps', 'ffmpeg-windows.sh').replace(/\\/g, '/')], {
    stdio: 'inherit', env: { ...process.env, MSYSTEM: 'MSYS', CHERE_INVOKING: '1', MSYS2_PATH_TYPE: 'inherit' },
  })
  if (r.error || r.status !== 0) skip('scripts/deps/ffmpeg-windows.sh failed (see its header for what it needs)')
}
const cargo = spawnSync('cargo', ['build', '--release'], {
  cwd: path.join(ROOT, 'native'), stdio: 'inherit', env: { ...process.env, FFMPEG_DIR: DEPS },
})
if (cargo.error || cargo.status !== 0) skip('cargo build failed (is Rust installed?)')

fs.rmSync(OUT, { recursive: true, force: true })
const exe = path.join(OUT, 'bin', 'sharkord-share.exe')
copy(EXE, exe)
// Licences of what is linked in: FFmpeg (LGPL, with how to rebuild it), libvpl, the AMF and NVENC headers
const lic = path.join(OUT, 'LICENSES')
for (const f of fs.readdirSync(path.join(DEPS, 'licenses'))) copy(path.join(DEPS, 'licenses', f), path.join(lic, f))
const ffmpeg = fs.readFileSync(path.join(ROOT, 'scripts', 'deps', 'ffmpeg-windows.sh'), 'utf8').match(/^FFMPEG=(\S+)/m)?.[1]
fs.writeFileSync(path.join(lic, 'SOURCES.md'), [
  '# Third-party code in this folder\'s helper', '',
  `- FFmpeg ${ffmpeg} (LGPL-2.1), https://git.ffmpeg.org/ffmpeg.git, statically linked into bin/sharkord-share.exe.`,
  '  Configure line and patches: scripts/deps/ffmpeg-windows.sh and scripts/deps/ffmpeg-windows-*.patch in',
  '  https://github.com/agrisci/sharkord-client; relink with your own FFmpeg by building native/ with FFMPEG_DIR.',
  '- libvpl (MIT, see libvpl.txt), the Intel Quick Sync dispatcher, statically linked.',
  '- AMD AMF headers (MIT, see AMF.txt) and NVIDIA nv-codec-headers (MIT, see nv-codec-headers.txt).', '',
].join('\n'))
// Proves it runs: any answer will do. What this machine can encode is the app's business at
// runtime -- CI has no GPU
const check = spawnSync(exe, ['--check'], { encoding: 'utf8' })
try { JSON.parse(check.stdout) } catch { throw new Error(`self-check failed: ${check.stdout}${check.stderr}`) }
log(`staged ${exe} (${(fs.statSync(exe).size / 1048576).toFixed(1)} MiB); self-check ${check.stdout.trim()}`)

// Stages the native screen share helper (native/) with the GStreamer runtime it needs into
// build/native/, which electron-builder ships as resources/native/ (Windows only).
//
// GStreamer's own layout -- bin/ (the helper and every DLL it pulls in), lib/gstreamer-1.0/ (six
// plugins), libexec/gstreamer-1.0/ (the plugin scanner) -- because GStreamer finds its plugins
// relative to the directory libgstreamer-1.0-0.dll loads from: no configuration needed. The DLL
// set is the import closure of the helper, the plugins and the scanner, walked from their PE
// import tables. Same approach as the native client's scripts/package_windows.py.
//
// Needs Rust and GStreamer MSVC (devel). Without them it warns and stages nothing, so
// `npm run dist:win` still builds an installer, just without the native share;
// SHARKORD_REQUIRE_NATIVE=1 (CI) makes that an error instead.
const fs = require('fs')
const path = require('path')
const os = require('os')
const { spawnSync } = require('child_process')

const ROOT = path.join(__dirname, '..')
const OUT = path.join(ROOT, 'build', 'native')
const EXE = path.join(ROOT, 'native', 'target', 'release', 'sharkord-share.exe')
const GST = process.env.GSTREAMER_1_0_ROOT_MSVC_X86_64 || 'C:\\Program Files\\gstreamer\\1.0\\msvc_x86_64'
const SYSTEM32 = path.join(process.env.SystemRoot || 'C:\\Windows', 'System32')
const REQUIRE = process.env.SHARKORD_REQUIRE_NATIVE === '1'

// capsfilter/queue, appsink, videorate, the d3d11 capture/convert/device provider, AMF, the parsers
const PLUGINS = ['coreelements', 'app', 'videorate', 'd3d11', 'amfcodec', 'videoparsersbad']
// The VC++ runtime ships app-locally: a machine without the redistributable must still run it
const CRT = /^(vcruntime140(_1)?|msvcp140(_\d+)?|concrt140)\.dll$/
// Shipped DLL → the share/licenses folder(s) that cover it
const LICENSES = [
  [/^gst/, ['gstreamer-1.0', 'gst-plugins-base-1.0', 'gst-plugins-bad-1.0']],
  [/^(glib|gobject|gmodule|gio)-2\.0/, ['glib']],
  [/^ffi-/, ['libffi']], [/^intl-/, ['proxy-libintl']], [/^pcre2-/, ['pcre2']],
  [/^orc-/, ['orc']], [/^z(lib)?[-.]/, ['zlib']],
]

const log = (...a) => console.log('[stage-native]', ...a)
function skip (why) {
  if (REQUIRE) { console.error('[stage-native] error:', why); process.exit(1) }
  log('skipped:', why, '-- the installer will have no native screen share')
  fs.rmSync(OUT, { recursive: true, force: true })
  fs.mkdirSync(OUT, { recursive: true })   // electron-builder's extraResources wants it to exist
  process.exit(0)
}

// DLL names a PE image imports, normal and delay-load (64-bit images only)
function peImports (file) {
  const b = fs.readFileSync(file)
  const pe = b.readUInt32LE(0x3c)
  if (b.toString('latin1', pe, pe + 4) !== 'PE\0\0') throw new Error(`${file}: not a PE image`)
  const sections = b.readUInt16LE(pe + 6), optSize = b.readUInt16LE(pe + 20), opt = pe + 24
  if (b.readUInt16LE(opt) !== 0x20b) throw new Error(`${file}: not a 64-bit image`)
  const secTable = opt + optSize
  const offset = rva => {
    for (let i = 0; i < sections; i++) {
      const s = secTable + i * 40, va = b.readUInt32LE(s + 12), size = Math.max(b.readUInt32LE(s + 8), b.readUInt32LE(s + 16))
      if (rva >= va && rva < va + size) return rva - va + b.readUInt32LE(s + 20)
    }
    return -1
  }
  const cstr = at => b.toString('latin1', at, b.indexOf(0, at))
  const names = []
  const dir = i => b.readUInt32LE(opt + 112 + i * 8)
  for (let at = offset(dir(1)); dir(1) && at > 0; at += 20) {        // import descriptors
    const name = b.readUInt32LE(at + 12); if (!name) break
    names.push(cstr(offset(name)))
  }
  for (let at = offset(dir(13)); dir(13) && at > 0; at += 32) {      // delay-load descriptors
    const name = b.readUInt32LE(at + 4); if (!name) break
    names.push(cstr(offset(name)))
  }
  return names.map(n => n.toLowerCase())
}

function copy (from, to) { fs.mkdirSync(path.dirname(to), { recursive: true }); fs.copyFileSync(from, to) }

if (process.platform !== 'win32') skip('the native share is Windows-only')
if (!fs.existsSync(path.join(GST, 'bin', 'gstreamer-1.0-0.dll'))) skip(`no GStreamer MSVC at ${GST}`)

// Build the helper
const cargo = spawnSync('cargo', ['build', '--release'], {
  cwd: path.join(ROOT, 'native'), stdio: 'inherit',
  env: { ...process.env, PKG_CONFIG_PATH: path.join(GST, 'lib', 'pkgconfig') },
})
if (cargo.error || cargo.status !== 0) skip('cargo build failed (is Rust installed?)')

fs.rmSync(OUT, { recursive: true, force: true })
const bin = path.join(OUT, 'bin')
copy(EXE, path.join(bin, 'sharkord-share.exe'))
for (const p of PLUGINS) copy(path.join(GST, 'lib', 'gstreamer-1.0', `gst${p}.dll`), path.join(OUT, 'lib', 'gstreamer-1.0', `gst${p}.dll`))
copy(path.join(GST, 'libexec', 'gstreamer-1.0', 'gst-plugin-scanner.exe'), path.join(OUT, 'libexec', 'gstreamer-1.0', 'gst-plugin-scanner.exe'))

// Import closure: from GStreamer's bin, the CRT from System32; anything else in System32 is the OS's
const shipped = new Set()
const queue = [path.join(bin, 'sharkord-share.exe'), ...PLUGINS.map(p => path.join(OUT, 'lib', 'gstreamer-1.0', `gst${p}.dll`)),
  path.join(OUT, 'libexec', 'gstreamer-1.0', 'gst-plugin-scanner.exe')]
while (queue.length) {
  for (const dll of peImports(queue.shift())) {
    if (shipped.has(dll) || /^(api|ext)-ms-/.test(dll)) continue
    const fromGst = path.join(GST, 'bin', dll), fromSys = path.join(SYSTEM32, dll)
    let from = null
    if (fs.existsSync(fromGst)) from = fromGst
    else if (CRT.test(dll) && fs.existsSync(fromSys)) from = fromSys
    else if (fs.existsSync(fromSys)) continue
    else throw new Error(`${dll} is imported but found neither in GStreamer's bin nor in System32`)
    shipped.add(dll)
    copy(from, path.join(bin, dll))
    queue.push(path.join(bin, dll))
  }
}

// Licences for everything shipped from GStreamer
const folders = new Set()
for (const dll of [...shipped, ...PLUGINS.map(p => `gst${p}.dll`)]) {
  if (CRT.test(dll)) continue
  const row = LICENSES.find(([re]) => re.test(dll))
  if (!row) throw new Error(`no licence folder mapped for ${dll} -- add it to LICENSES`)
  row[1].forEach(f => folders.add(f))
}
for (const f of folders) fs.cpSync(path.join(GST, 'share', 'licenses', f), path.join(OUT, 'licenses', f), { recursive: true })

// Self-check: the staged tree alone, with no GStreamer in the environment, must load every element
const registry = path.join(os.tmpdir(), `sharkord-stage-registry-${process.pid}.bin`)
const env = { SystemRoot: process.env.SystemRoot, PATH: `${bin};${SYSTEM32}`, GST_REGISTRY_1_0: registry }
const check = spawnSync(path.join(bin, 'sharkord-share.exe'), ['--check'], { env, encoding: 'utf8' })
fs.rmSync(registry, { force: true })
if (check.status !== 0) throw new Error(`self-check failed: ${check.stdout}${check.stderr}`)

const files = fs.readdirSync(OUT, { recursive: true }).filter(f => fs.statSync(path.join(OUT, f)).isFile())
const mib = files.reduce((n, f) => n + fs.statSync(path.join(OUT, f)).size, 0) / 1048576
log(`staged ${files.length} files, ${mib.toFixed(1)} MiB; self-check ${check.stdout.trim()}`)

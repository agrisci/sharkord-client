// ═══════════════════════════════════════════════════════════════════════════
//  Sharkord Desktop Client — main process
// ═══════════════════════════════════════════════════════════════════════════
const {
  app, BrowserWindow, Menu, Tray, nativeImage, ipcMain, net, shell, desktopCapturer, session,
  screen, MessageChannelMain, dialog, powerMonitor,
} = require('electron')
const { spawn, execFile } = require('child_process')
const path = require('path')
const fs   = require('fs')
const os   = require('os')

const log = (...a) => console.log(new Date().toISOString().slice(11,23), '|', ...a)

// ── Settings ──────────────────────────────────────────────────────────────
const settingsPath     = path.join(app.getPath('userData'), 'settings.json')
// A BOM (Windows tools add one) would make JSON.parse throw and every setting read as unset
const loadUserSettings = () => { try { return JSON.parse(fs.readFileSync(settingsPath,'utf8').replace(/^﻿/,'')) } catch { return {} } }
// Written to a temp file and renamed over, so a crash mid-write can't leave an empty file (and
// every setting, the server URL included, gone)
const saveUserSettings = s  => {
  try { fs.writeFileSync(settingsPath + '.tmp', JSON.stringify(s,null,2)); fs.renameSync(settingsPath + '.tmp', settingsPath) }
  catch (e) { log('[settings] save failed:', e.message) }
}
// Without a trailing slash, like the setup form saves it (older saves may have one)
const savedServerUrl   = () => loadUserSettings().serverUrl?.replace(/\/+$/, '') || ''

// ── Chromium flags ────────────────────────────────────────────────────────
// Hardware video encode/decode for WebRTC screen share (VA-API on Linux, Media
// Foundation on Windows). Only one enable-features switch may be set — a
// second call overrides the first.
if (process.platform === 'linux') {
  app.commandLine.appendSwitch('enable-features', [
    'AcceleratedVideoEncoder',               // VA-API encode (was VaapiVideoEncoder before Chromium 131)
    'AcceleratedVideoDecodeLinuxGL',
    'AcceleratedVideoDecodeLinuxZeroCopyGL',
    'VaapiIgnoreDriverChecks',               // AMD radeonsi isn't on Chromium's driver allow list
  ].join(','))
  app.commandLine.appendSwitch('ignore-gpu-blocklist')
  app.commandLine.appendSwitch('enable-gpu-rasterization')
  app.commandLine.appendSwitch('enable-zero-copy')
} else if (process.platform === 'win32') {
  app.commandLine.appendSwitch('enable-features', [
    // Media Foundation encodes Baseline H.264 in hardware, but WebRTC's usual
    // Constrained Baseline (42e01f, what Sharkord negotiates) falls back to
    // OpenH264 unless this is on (off by default in Chromium, pending rollout)
    'PlatformH264CbpEncoding',
    // Screen capture (WGC on Win11 24H2+) copies every frame to CPU memory and
    // is throttled to half the CPU time; at 4K that caps it near 36 fps. GPU
    // textures keep it at 60.
    'WebRtcAllowWgcUsingTexture',
    // AV1 hardware encoding for WebRTC (libaom otherwise)
    'WebRtcAV1HWEncode',
  ].join(','))
  // Chromium asks each encoder for its max resolutions and drops the ones that
  // don't answer, like AMD's AV1 encoder. Without the query every hardware
  // encoder is limited to 1080p (above that WebRTC falls back to software).
  app.commandLine.appendSwitch('disable-features', 'ExpandMediaFoundationEncodingResolutions')
}

// ── venmic (Linux per-app share audio via PipeWire) ──────────────────────
//   Links selected apps' audio outputs into a virtual mic named
//   "vencord-screen-share"; the preload's getDisplayMedia hook swaps it into
//   the share stream.
const isWayland = process.platform === 'linux' &&
  (process.env.XDG_SESSION_TYPE === 'wayland' || !!process.env.WAYLAND_DISPLAY)
// Windows 10 22H2+ supports loopback capture that excludes our own audio
const supportsLoopbackWithoutChrome = process.platform === 'win32' &&
  Number(os.release().split('.').pop()) >= 19045

let _venmicPatchBay = null, _venmicInstance = null, _venmicLoaded = false
let _venmicHasPipeWire = false, _venmicGlibcOutdated = false, _venmicLinked = false

function obtainVenmic () {
  if (process.platform !== 'linux') return null
  if (!_venmicLoaded) {
    _venmicLoaded = true
    try {
      _venmicPatchBay    = require('@vencord/venmic').PatchBay
      _venmicHasPipeWire = _venmicPatchBay.hasPipeWire()
      _venmicInstance    = new _venmicPatchBay()
    } catch (e) {
      log('[venmic] unavailable:', e.message)
      _venmicGlibcOutdated = (e.stack || e.message || '').toLowerCase().includes('glibc')
    }
  }
  return _venmicInstance
}

const AUDIO_DEFAULTS = {
  mute: true, workaround: false, onlySpeakers: true, onlyDefaultSpeakers: true,
  ignoreInputMedia: true, ignoreVirtual: false, ignoreDevices: true,
  granularSelect: false, deviceSelect: false,
}
const audioSettings = () => ({ ...AUDIO_DEFAULTS, ...(loadUserSettings().audio || {}) })

// Chromium's "Audio Service" process plays the call audio — never share it
const audioServicePid = () =>
  app.getAppMetrics().find(p => p.name === 'Audio Service')?.pid?.toString() ?? 'none'

function buildLinkData ({ include, exclude }) {
  const s   = audioSettings()
  const pid = audioServicePid()
  const data = {
    mute: s.mute,
    include: include || [],
    exclude: exclude || [],
    only_speakers: s.onlySpeakers,
    ignore_devices: s.ignoreDevices,
    only_default_speakers: s.onlyDefaultSpeakers,
  }
  data.exclude.push({ 'application.process.id': pid })
  if (s.ignoreInputMedia) data.exclude.push({ 'media.class': 'Stream/Input/Audio' })
  if (s.ignoreVirtual)    data.exclude.push({ 'node.virtual': 'true' })
  if (s.workaround)       data.workaround = [{ 'application.process.id': pid, 'media.name': 'RecordStream' }]
  return data
}

function venmicUnlink () {
  if (!_venmicLinked) return
  _venmicLinked = false
  try { obtainVenmic()?.unlink(); log('[venmic] unlinked') } catch (e) { log('[venmic] unlink error:', e.message) }
}

// Picker → audio source list
ipcMain.handle('virtmic-list', () => {
  const pb = obtainVenmic()
  if (!pb) return { ok: false, isGlibCxxOutdated: _venmicGlibcOutdated }
  const pid = audioServicePid()
  const props = audioSettings().granularSelect ? [] : ['application.name', 'node.name']
  const targets = pb.list(props).filter(t => t['application.process.id'] !== pid)
  return { ok: true, targets, hasPipewirePulse: _venmicHasPipeWire }
})
ipcMain.handle('audio-settings-get', () => audioSettings())
ipcMain.handle('audio-settings-set', (_e, s) => {
  saveUserSettings({ ...loadUserSettings(), audio: { ...audioSettings(), ...s } })
  return audioSettings()
})

// Page → virtual mic lifecycle
ipcMain.handle('virtmic-active', () => _venmicLinked)
ipcMain.handle('virtmic-unmute', () => { try { obtainVenmic()?.unmute() } catch (_) {} })
ipcMain.handle('virtmic-stop',   () => venmicUnlink())

// Page → "Change server" item added to Sharkord's server menu (preload.js)
ipcMain.on('change-server', e => { if (e.sender === win?.webContents) changeServer() })

// Page → Sharkord showed a notification / it was clicked (preload.js)
ipcMain.on('notification-shown',   e => { if (e.sender === win?.webContents && !win.isFocused()) win.flashFrame(true) })
ipcMain.on('notification-clicked', e => { if (e.sender === win?.webContents) showWindow() })

// Page → "Desktop Client" tab added to Sharkord's user settings (preload.js)
ipcMain.handle('desktop-settings-get', e => e.sender === win?.webContents ? desktopSettings() : null)
ipcMain.handle('desktop-settings-set', (e, s) => e.sender === win?.webContents ? setDesktopSettings(s) : null)

// ── Screen picker ─────────────────────────────────────────────────────────
//   On Wayland the portal has already chosen the source, so the picker opens
//   straight on the audio step (skipPicker) instead of a one-item grid.
let _pickerWin = null, _displayCallback = null, _pickerSources = []

// streams === null denies the request; Electron 44 throws on callback({})
function finishPick (streams) {
  const cb = _displayCallback; _displayCallback = null
  if (cb) cb(streams)
  if (_pickerWin && !_pickerWin.isDestroyed()) _pickerWin.close()
}

ipcMain.handle('picker-go-live', (_e, { id, audio, include, exclude }) => {
  const src = _pickerSources.find(s => s.id === id)
  if (!src) return finishPick(null)

  const streams = { video: src }
  _nativeTarget = nativeTargetFor(src)
  if (process.platform === 'linux' && include && include !== 'None') {
    const pb = obtainVenmic()
    try {
      const ok = include === 'Entire System'
        ? pb?.link(buildLinkData({ exclude: Array.isArray(exclude) ? exclude : [] }))
        : pb?.link(buildLinkData({ include }))
      _venmicLinked = !!ok
      log('[venmic] link', include === 'Entire System' ? 'system' : JSON.stringify(include), '→', !!ok)
    } catch (e) { log('[venmic] link error:', e.message) }
  } else if (process.platform === 'win32' && audio) {
    streams.audio = supportsLoopbackWithoutChrome ? 'loopbackWithoutChrome' : 'loopback'
  }
  finishPick(streams)
})
ipcMain.on('picker-cancelled', () => finishPick(null))

// Opens the picker on `sources`; `done` gets the streams, or null when it is cancelled or closed
async function openPicker (sources, done) {
  _displayCallback = done
  _pickerSources   = sources
  const theme = await pageTheme()
  _pickerWin = new BrowserWindow({
    width:720, height:660, parent:win, modal:false,
    title:'Share Screen', backgroundColor: theme === 'light' ? '#ffffff' : '#0a0a0a', resizable:false,
    ...(fs.existsSync(APP_ICON) ? { icon:APP_ICON } : {}),
    webPreferences:{ preload:path.join(__dirname,'picker-preload.js'), contextIsolation:true, nodeIntegration:false },
  })
  _pickerWin.setMenuBarVisibility(false)
  _pickerWin.loadFile(path.join(__dirname,'picker.html'))
  _pickerWin.webContents.on('did-finish-load', () => {
    _pickerWin.webContents.send('init', {
      sources: sources.map(s=>({id:s.id,name:s.name,thumbnail:s.thumbnail?.toDataURL() || ''})),
      skipPicker: isWayland,
      platform: process.platform,
      theme,
    })
  })
  _pickerWin.on('closed', () => {
    const cb = _displayCallback; _displayCallback = null
    if (cb) cb(null)
    _pickerWin = null
  })
}

async function handleDisplayMediaRequest (_req, callback) {
  try {
    venmicUnlink()   // a previous share's link must not leak into this one
    stopNativeShare(); _nativeTarget = null
    // On Wayland this call shows the portal dialog and returns only its pick
    const width   = isWayland ? 1920 : 320
    const sources = await desktopCapturer.getSources({
      types:['screen','window'], thumbnailSize:{ width, height:Math.round(width*9/16) },
    })
    if (!sources.length) { callback(null); return }   // portal cancelled
    await openPicker(sources, callback)
  } catch (e) { log('[screen-share] error:', e.message); _displayCallback = null; callback(null) }
}

// ── Native screen share (Windows; Linux on Wayland; experimental) ─────────
//   A helper (native/, Rust + GStreamer) captures the picked monitor (Windows) or
//   portal pick (Linux: a screen or a window) and encodes
//   it with the GPU; the preload swaps its frames into Sharkord's own share.
//   Frames go straight to the page over a MessagePort; the page sends keyframe
//   and bitrate requests back the same way. The `nativeShare` setting turns it
//   on (SHARKORD_NATIVE_SHARE=1 forces it, for testing).
//   Linux: the helper owns the pick. The page asks for it (native-share-pick)
//   instead of calling Chromium's getDisplayMedia, so the portal asks once: the
//   helper shows it, then the picker's audio step, and the page builds the
//   share's stream (its preview decoded from the helper's frames).
let _nativeTarget = null, _nativeShare = null
let _nativeProbe = null   // the helper's --check ({ missing, h264, av1 } or { error }), once per run

const EXE_NAME = process.platform === 'win32' ? 'sharkord-share.exe' : 'sharkord-share'
const nativeShareExe = () => ['win32', 'linux'].includes(process.platform) && [
  path.join(process.resourcesPath || '', 'native', 'bin', EXE_NAME),   // installed (scripts/stage-native.js)
  path.join(__dirname, '..', 'native', 'target', 'release', EXE_NAME),
].find(p => fs.existsSync(p))
// Only where the startup probe opened an encoder (an AMD GPU on Windows; on Linux a VA-API one,
// which Fedora's own Mesa lacks for H.264), and on Linux only on Wayland (X11 stays Chromium's)
const canEncode = codec => !_nativeProbe?.missing?.length && !!_nativeProbe?.[codec]
const nativeShareSupported = () => !!nativeShareExe() &&
  (process.platform !== 'linux' || isWayland) && (canEncode('h264') || canEncode('av1'))

// What the startup probe found the GPU encodes, once it ran cleanly: a list under the switch
const nativeShareCodecs = () => {
  const p = _nativeProbe
  return nativeShareExe() && p && !p.error && !p.missing?.length ? { h264: !!p.h264, av1: !!p.av1 } : null
}

// Why the native share is off on this machine, or how to get H.264: shown under its switch in
// Settings next to that list, so a missing driver or plugin doesn't just make the option vanish.
// null: nothing to say (or the probe hasn't answered yet).
function nativeShareNote () {
  const linux = process.platform === 'linux', p = _nativeProbe
  if (!nativeShareExe()) return null
  if (linux && !isWayland) return 'Needs a Wayland session; on X11 shares use the browser\'s capture.'
  if (!p) return null
  if (p.error) return linux
    ? 'The helper could not run: GStreamer 1.22+ (gstreamer1, gstreamer1-plugins-base) is needed.'
    : 'The helper could not run; reinstalling Sharkord should fix it.'
  if (p.missing?.includes('pipewiresrc')) return 'GStreamer\'s PipeWire plugin is missing: install pipewire-gstreamer (Fedora) or gstreamer1.0-pipewire (Debian/Ubuntu).'
  if (p.missing?.length && linux) return 'No VA-API GPU found, or GStreamer\'s va plugin is missing: gstreamer1-plugins-bad-free (Fedora) or gstreamer1.0-plugins-bad (Debian/Ubuntu).'
  if (p.missing?.length) return 'The helper is incomplete; reinstalling Sharkord should fix it.'
  const freeworld = linux ? ' On Fedora with an AMD GPU, H.264 needs mesa-va-drivers-freeworld from RPM Fusion.' : ''
  if (!p.h264 && !p.av1) return linux ? freeworld.trim() || null : 'Only AMD GPUs are supported.'
  if (!p.h264) return freeworld.trim() || null
  return null
}
const nativeShareOn = () => nativeShareSupported() &&
  (process.env.SHARKORD_NATIVE_SHARE === '1' || !!loadUserSettings().nativeShare)

// Windows: its own GStreamer only -- no GST_* from an installed GStreamer, and a registry of its
// own. Linux uses the system's GStreamer, environment included.
function helperEnv () {
  if (process.platform !== 'win32') return process.env
  const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^GST(REAMER)?_/i.test(k)))
  env.GST_REGISTRY_1_0 = path.join(app.getPath('userData'), 'gstreamer-registry.bin')
  return env
}

// Which codecs this machine really encodes: the helper opens each encoder (`--check`). Once per
// run; a share only goes native with a codec it found (native-share-pick / native-share-target).
// The first run on Windows also builds the helper's plugin registry, hence the long timeout.
function probeNativeShare () {
  const exe = nativeShareExe()
  if (!exe || (process.platform === 'linux' && !isWayland)) return
  execFile(exe, ['--check'], { timeout: 30000, windowsHide: true, env: helperEnv() }, (err, stdout) => {
    try { _nativeProbe = JSON.parse(stdout) } catch { _nativeProbe = { error: err?.message || 'no answer' } }
    log('[native-share] probe', stdout.trim() || _nativeProbe.error)
    updateTrayMenu()
  })
}

// Windows: only whole screens, the helper captures monitors. Linux goes native through
// native-share-pick instead, never through Chromium's pick.
function nativeTargetFor (src) {
  if (process.platform !== 'win32' || !src.id.startsWith('screen:') || !nativeShareOn()) return null
  const display = screen.getAllDisplays().find(d => String(d.id) === src.display_id)
  return display ? { label: display.label, primary: display.id === screen.getPrimaryDisplay().id } : { primary: true }
}

function stopNativeShare () {
  const s = _nativeShare; _nativeShare = null
  if (!s) return
  try { s.proc.stdin.end() } catch {}
  setTimeout(() => { if (s.proc.exitCode === null) s.proc.kill() }, 2000)
  s.port.close()
  s.settle('error')
}

// Spawns the helper and wires its records to a MessagePort that native-share-start hands the page.
// `ready` settles on its first answer on Linux, where it opens the portal at once: 'selected',
// 'cancelled', or 'error' (also when it exits or is stopped first).
function spawnHelper () {
  stopNativeShare()
  const proc = spawn(nativeShareExe(), [], { stdio: ['pipe', 'pipe', 'ignore'], windowsHide: true, env: helperEnv() })
  const { port1: port, port2 } = new MessageChannelMain()
  let settle
  const ready = new Promise(resolve => { settle = resolve })
  const share = _nativeShare = { proc, port, port2, ready, settle, started: false }
  // Records: u8 kind, u8 flags, u16 reserved, u32 LE length, u64 LE pts, payload
  let buf = Buffer.alloc(0)
  proc.stdout.on('data', chunk => {
    buf = buf.length ? Buffer.concat([buf, chunk]) : chunk
    while (buf.length >= 16) {
      const len = buf.readUInt32LE(4)
      if (buf.length < 16 + len) break
      const kind = buf[0], flags = buf[1], pts = Number(buf.readBigUInt64LE(8))
      const payload = Buffer.from(buf.subarray(16, 16 + len))
      buf = buf.subarray(16 + len)
      if (kind === 1) port.postMessage({ type: 'frame', key: !!(flags & 1), pts, data: payload })
      else if (kind === 2) {
        try {
          const ev = JSON.parse(payload.toString())
          if (ev.type !== 'stats') log('[native-share]', JSON.stringify(ev))
          if (['selected', 'cancelled', 'error'].includes(ev.type)) settle(ev.type)
          port.postMessage({ ...ev, type: 'event', event: ev.type })
        } catch {}
      }
    }
  })
  // The helper ending on its own (a crash, or an error it couldn't report) must still reach the
  // page, or the share stays on its placeholder and viewers see black
  const gone = why => {
    log('[native-share] helper', why)
    settle('error')
    if (_nativeShare !== share) return   // stopped by the page
    port.postMessage({ type: 'event', event: 'error', message: 'helper ' + why })
    stopNativeShare()
  }
  proc.on('exit', code => gone('exited (' + code + ')'))
  proc.on('error', err => gone('failed: ' + err.message))
  // A write to a helper that has already exited fails asynchronously (EPIPE), past any try
  proc.stdin.on('error', err => log('[native-share] helper stdin:', err.message))
  port.on('message', ({ data }) => { try { proc.stdin.write(JSON.stringify(data) + '\n') } catch {} })
  port.on('close', () => { if (_nativeShare === share) stopNativeShare() })
  port.start()
  return share
}

// Page → Linux: a share picked by the helper instead of Chromium. 'chromium' when it doesn't
// apply here (the page calls Chromium's getDisplayMedia), including a `codec` the startup probe
// found no encoder for (AV1 on older GPUs), 'cancelled' when the user cancelled a dialog, 'ok'
// when the helper holds the screen and the audio step is done.
ipcMain.handle('native-share-pick', async (e, codec) => {
  if (e.sender !== win?.webContents || process.platform !== 'linux' || !nativeShareOn()) return 'chromium'
  if (!canEncode(codec)) { log('[native-share] no', codec, 'encoder here: Chromium\'s share'); return 'chromium' }
  venmicUnlink(); _nativeTarget = null
  if (_pickerWin && !_pickerWin.isDestroyed()) _pickerWin.close()
  const answer = await spawnHelper().ready
  log('[native-share] pick', answer)
  if (answer === 'cancelled') return 'cancelled'
  if (answer !== 'selected') return 'chromium'   // the helper failed before a pick: Chromium's own
  // The audio step only: the helper already has the screen
  const streams = await new Promise(resolve => openPicker([{ id: 'native', name: 'Screen' }], resolve))
  if (!streams) { stopNativeShare(); return 'cancelled' }
  _nativeTarget = {}
  return 'ok'
})
// Windows: the picked monitor, or null (Chromium's share) when the probe found no `codec` encoder
ipcMain.handle('native-share-target', (e, codec) =>
  e.sender === win?.webContents && canEncode(codec) ? _nativeTarget : null)
ipcMain.on('native-share-start', (e, opts) => {
  if (e.sender !== win?.webContents || !_nativeTarget) return
  // Linux: the helper from the pick, already holding the screen; Windows: spawned now
  const share = process.platform !== 'linux' ? spawnHelper() : _nativeShare?.started === false ? _nativeShare : null
  if (!share) {   // it exited after the pick: the error makes the page end the share
    const { port1, port2 } = new MessageChannelMain()
    port1.postMessage({ type: 'event', event: 'error', message: 'helper exited before start' })
    return e.sender.postMessage('native-share-port', { id: opts.id }, [port2])
  }
  share.started = true
  share.proc.stdin.write(JSON.stringify({ cmd: 'start', ...opts, ..._nativeTarget }) + '\n')
  // The id lets the page hand the port to the share that asked for it
  e.sender.postMessage('native-share-port', { id: opts.id }, [share.port2])
  log('[native-share] helper started', JSON.stringify({ ...opts, ..._nativeTarget }))
})
// Page → a share ended before native-share-start (Linux: the helper from the pick still holds
// the portal session, and the desktop's "sharing" indicator)
ipcMain.on('native-share-stop', e => { if (e.sender === win?.webContents) stopNativeShare() })
// A suspend loses the capture (the portal's PipeWire stream doesn't come back on resume): the
// page still ends the share, but doesn't count it as the helper failing
function watchSuspend () {
  powerMonitor.on('suspend', () => _nativeShare?.port.postMessage({ type: 'event', event: 'suspend' }))
}

// ── Main window ───────────────────────────────────────────────────────────
const APP_ICON = path.join(__dirname,'..','build', process.platform==='win32'?'icon.ico':'icon.png')
const BG = { dark:'#0a0a0a', light:'#ffffff' }   // Sharkord's --background

let win

// Sharkord's current theme (its ThemeProvider sets a class on <html>).
// Remembered so the client's own pages match it even when the server is down.
const savedTheme = () => loadUserSettings().theme === 'light' ? 'light' : 'dark'
async function pageTheme () {
  if (!win || win.isDestroyed() || win.webContents.getURL().startsWith('file:')) return savedTheme()
  const theme = await win.webContents.executeJavaScript(
    "document.documentElement.classList.contains('light') ? 'light' : 'dark'"
  ).catch(() => savedTheme())
  if (theme !== loadUserSettings().theme) saveUserSettings({ ...loadUserSettings(), theme })
  return theme
}

function createWindow () {
  win = new BrowserWindow({
    width:1280, height:800, minWidth:880, minHeight:600,
    title: 'Sharkord',   // Sharkord sets document.title to the server name
    backgroundColor:BG[savedTheme()],
    webPreferences:{
      preload: path.join(__dirname,'preload.js'),
      contextIsolation:true, nodeIntegration:false,
      backgroundThrottling:false,   // keep voice/timers running while minimised
    },
    ...(fs.existsSync(APP_ICON) ? { icon:APP_ICON } : {}),
    show:false,
  })

  loadServer()

  // The page's native share log next to the helper's, so one log shows both ends of a share
  win.webContents.on('console-message', e => { if (e.message?.startsWith('[native-share]')) log('[page]', e.message) })
  // Load failed anyway (e.g. server went down between check and load)
  win.webContents.on('did-fail-load', (_e, code, desc, url, isMainFrame) => {
    if (!isMainFrame || code === -3) return   // -3: aborted by a newer navigation
    log('[load] failed:', url, desc)
    setImmediate(showUnreachable)
  })
  // Sharkord retitles the window once it has loaded — a good moment to note its theme
  win.on('page-title-updated', () => { pageTheme() })

  // Buttons on the unreachable page
  win.webContents.on('will-navigate', (e, url) => {
    if (url === RETRY_URL)         { e.preventDefault(); loadServer() }
    if (url === CHANGE_SERVER_URL) { e.preventDefault(); changeServer() }
  })

  win.webContents.setWindowOpenHandler(({ url }) => { shell.openExternal(url); return { action:'deny' } })
  win.webContents.on('before-input-event', (_e, inp) => {
    if (inp.control && inp.shift && inp.key.toLowerCase()==='i') {
      win.webContents.isDevToolsOpened() ? win.webContents.closeDevTools() : win.webContents.openDevTools()
    }
    if (inp.type==='keyDown' && inp.control && inp.shift && inp.key.toLowerCase()==='o') changeServer()
  })
  win.on('focus', () => win.flashFrame(false))   // stop the notification flash
  // Sharkord blocks unload while in a voice channel; a browser asks, Electron silently cancels
  // the close (tray Quit did nothing). Quit always wins; closing the window asks like a browser.
  win.webContents.on('will-prevent-unload', e => {
    if (quitting || dialog.showMessageBoxSync(win, { type:'question', buttons:['Leave','Stay'], defaultId:0, cancelId:1,
      message:'Leave the voice channel?', detail:'Closing Sharkord disconnects you from the call.' }) === 0) e.preventDefault()
  })
  // Minimize to tray: X hides the window (quitting from the tray still closes it)
  win.on('close', e => {
    if (quitting || !tray || !loadUserSettings().minimizeToTray) return
    e.preventDefault()
    win.hide()
  })
  // Launched at login with Start minimized on: start in the tray (only if there is one to get back)
  win.once('ready-to-show', () => {
    if (!(startHidden && tray && loadUserSettings().startMinimized)) win.show()
    startHidden = false   // only the first window (not one re-created by Change server)
  })
}
let startHidden = process.argv.includes('--hidden')   // launched at login (AUTOSTART_ARGS)

function showWindow () {
  if (!win || win.isDestroyed()) return
  if (win.isMinimized()) win.restore()
  win.show()
  win.focus()
}

// A Sharkord server answers /info with its id and name (Sharkord's own client
// fetches it the same way). Returns 'ok' | 'not-sharkord' | 'unreachable'.
async function checkServer (serverUrl) {
  try {
    const res  = await net.fetch(`${serverUrl.replace(/\/+$/, '')}/info`, { signal:AbortSignal.timeout(10000) })
    const info = res.ok ? await res.json().catch(() => null) : null
    if (typeof info?.serverId === 'string' && typeof info?.name === 'string') return 'ok'
    log('[check] not a Sharkord server:', serverUrl, res.status)
    return 'not-sharkord'
  } catch (e) { log('[check] unreachable:', serverUrl, e.message); return 'unreachable' }
}

// Check the server before loading it: navigating the window to an
// unreachable URL and then to a local page can leave it unable to paint.
async function loadServer () {
  const serverUrl = savedServerUrl()
  const status = await checkServer(serverUrl)
  if (win.isDestroyed()) return
  status === 'ok' ? win.loadURL(serverUrl).catch(() => {}) : showUnreachable(status)
}

function showUnreachable (reason = 'unreachable') {
  if (win.isDestroyed()) return
  win.loadFile(path.join(__dirname,'unreachable.html'),
    { query:{ url:savedServerUrl(), theme:savedTheme(), reason } })
}

// ── First launch ──────────────────────────────────────────────────────────
//   Asks for the server URL (modeled on Vesktop's first-launch tour). The
//   view reports back through console messages: "cancel" or "form:{json}".
const CHANGE_SERVER_URL = 'sharkord://change-server'
const RETRY_URL         = 'sharkord://retry'
const SETUP_ERRORS = {
  'not-sharkord': "That address isn't a Sharkord server.",
  'unreachable':  "Couldn't reach that server. Check the address and your connection.",
}

function createFirstLaunch (current = '', theme = savedTheme()) {
  const setup = new BrowserWindow({
    width:440, height:500, useContentSize:true, resizable:false,
    title:'Sharkord Setup', backgroundColor:BG[theme],
    ...(fs.existsSync(APP_ICON) ? { icon:APP_ICON } : {}),
    webPreferences:{ contextIsolation:true, nodeIntegration:false },
  })
  setup.loadFile(path.join(__dirname,'first-launch.html'), { query:{ current, theme } })

  let done = false, checking = false
  setup.webContents.on('console-message', async e => {
    const msg = e.message
    if (msg === 'cancel') return setup.close()
    if (!msg?.startsWith('form:')) return
    let serverUrl
    try { ({ serverUrl } = JSON.parse(msg.slice(5))) } catch { return }
    if (checking) return
    // Only save addresses that really are Sharkord servers
    checking = true
    const status = await checkServer(serverUrl)
    checking = false
    if (setup.isDestroyed()) return
    if (status !== 'ok') {
      return setup.webContents.executeJavaScript(`setError(${JSON.stringify(SETUP_ERRORS[status])})`)
    }
    saveUserSettings({ ...loadUserSettings(), serverUrl })
    done = true
    createWindow()
    setup.close()
  })
  // Cancelled: back to the current server if there is one, else quit
  setup.on('closed', () => { if (!done && !quitting) current ? createWindow() : app.quit() })
}

// Swap the main window for the setup window (setup opens first so
// window-all-closed doesn't quit the app)
async function changeServer () {
  if (!win || win.isDestroyed() || changeServer.busy) return
  changeServer.busy = true
  const theme = await pageTheme()
  if (_pickerWin && !_pickerWin.isDestroyed()) _pickerWin.close()
  venmicUnlink()
  stopNativeShare()
  createFirstLaunch(savedServerUrl(), theme)
  win.destroy()
  changeServer.busy = false
}

function openApp () {
  // Rewrite older saves that still end in a slash
  const s = loadUserSettings()
  if (s.serverUrl && s.serverUrl !== savedServerUrl()) saveUserSettings({ ...s, serverUrl:savedServerUrl() })
  savedServerUrl() ? createWindow() : createFirstLaunch()
}

// ── Desktop integration (open at login, tray) ─────────────────────────────
//   Open at login lives in the OS (login item / autostart file), not in
//   settings.json, so it stays right if the user removes it there. Its
//   --hidden only marks a login launch; Start minimized (settings.json)
//   decides whether that launch stays in the tray, so the entry never has to
//   change (and Windows only finds its login item by the same args).
const AUTOSTART_ARGS = ['--hidden']
const autostartFile  = path.join(process.env.XDG_CONFIG_HOME || path.join(os.homedir(),'.config'), 'autostart', 'sharkord.desktop')

function openAtLogin () {
  if (process.platform === 'linux') return fs.existsSync(autostartFile)
  // Windows only reports openAtLogin when queried with the same args it was set with
  return app.getLoginItemSettings({ args:AUTOSTART_ARGS }).openAtLogin
}

function setOpenAtLogin (on) {
  if (process.platform !== 'linux') return app.setLoginItemSettings({ openAtLogin:on, args:AUTOSTART_ARGS })
  if (!on) return fs.rmSync(autostartFile, { force:true })
  // Inside an AppImage execPath is a temporary mount; unpackaged (npm start) needs the app path
  const cmd = [process.env.APPIMAGE || process.execPath, ...(app.isPackaged ? [] : [app.getAppPath()]), ...AUTOSTART_ARGS]
  fs.mkdirSync(path.dirname(autostartFile), { recursive:true })
  fs.writeFileSync(autostartFile, [
    '[Desktop Entry]', 'Type=Application', 'Name=Sharkord',
    `Exec=${cmd.map(a => `"${a.replace(/(["`$\\])/g, '\\$1')}"`).join(' ')}`,
    'Terminal=false', 'StartupNotify=false', '',
  ].join('\n'))
}

// nativeShare is left out where the helper can't run, which hides its tray item and greys out its
// switch; nativeShareNote says why (or which codecs it covers)
const desktopSettings = () => ({
  openAtLogin:openAtLogin(), startMinimized:!!loadUserSettings().startMinimized,
  minimizeToTray:!!loadUserSettings().minimizeToTray,
  ...(nativeShareSupported() ? { nativeShare:nativeShareOn() } : {}),
  ...(nativeShareNote() ? { nativeShareNote:nativeShareNote() } : {}),
  ...(nativeShareCodecs() ? { nativeShareCodecs:nativeShareCodecs() } : {}),
})
function setDesktopSettings (s) {
  try {
    if (typeof s?.openAtLogin === 'boolean') setOpenAtLogin(s.openAtLogin)
    if (typeof s?.startMinimized === 'boolean') saveUserSettings({ ...loadUserSettings(), startMinimized:s.startMinimized })
    if (typeof s?.minimizeToTray === 'boolean') saveUserSettings({ ...loadUserSettings(), minimizeToTray:s.minimizeToTray })
    if (typeof s?.nativeShare === 'boolean') saveUserSettings({ ...loadUserSettings(), nativeShare:s.nativeShare })
  } catch (e) { log('[desktop] settings error:', e.message) }
  updateTrayMenu()
  return desktopSettings()
}

let tray = null
function createTray () {
  if (tray || !fs.existsSync(APP_ICON)) return
  const icon = nativeImage.createFromPath(APP_ICON)
  tray = new Tray(process.platform === 'win32' ? icon : icon.resize({ width:32, height:32 }))
  tray.setToolTip('Sharkord')
  tray.on('click', showWindow)
  updateTrayMenu()
}

// Rebuilt on every change: Linux trays don't update a checkbox in place
function updateTrayMenu () {
  if (!tray) return
  const s = desktopSettings()
  tray.setContextMenu(Menu.buildFromTemplate([
    { label:'Open Sharkord',  click:showWindow },
    { label:'Change Server…', click:changeServer },
    { type:'separator' },
    { label:'Open at login',    type:'checkbox', checked:s.openAtLogin,    click:i => setDesktopSettings({ openAtLogin:i.checked }) },
    { label:'Start minimized',  type:'checkbox', checked:s.startMinimized, enabled:s.openAtLogin, click:i => setDesktopSettings({ startMinimized:i.checked }) },
    { label:'Minimize to tray', type:'checkbox', checked:s.minimizeToTray, click:i => setDesktopSettings({ minimizeToTray:i.checked }) },
    ...('nativeShare' in s ? [{ label:'Native screen share', type:'checkbox', checked:s.nativeShare, click:i => setDesktopSettings({ nativeShare:i.checked }) }] : []),
    { type:'separator' },
    { label:'Quit', click:() => app.quit() },
  ]))
}

// ── App startup ───────────────────────────────────────────────────────────
// One instance only: a second launch (or autostart) brings the existing window back
const primaryInstance = app.requestSingleInstanceLock()
if (!primaryInstance) app.quit()
app.on('second-instance', showWindow)

app.whenReady().then(() => {
  if (!primaryInstance) return
  // Windows toasts need an AppUserModelID (matches build.appId on the installer's shortcut)
  if (process.platform === 'win32') app.setAppUserModelId('com.sharkord.client')
  // Delayed: the GPU process isn't initialised yet at whenReady
  setTimeout(() => {
    // Per-codec encode profiles aren't exposed here — see chrome://gpu for those
    app.getGPUInfo('basic').then(info => {
      log('[gpu] feature status:', JSON.stringify(app.getGPUFeatureStatus()))
      log('[gpu] devices:', JSON.stringify(info.gpuDevice || []))
    }).catch(e => log('[gpu] info error:', e.message))
  }, 5000)

  session.defaultSession.setDisplayMediaRequestHandler(handleDisplayMediaRequest)
  Menu.setApplicationMenu(null)   // no menu bar; shortcuts live in before-input-event
  createTray()
  probeNativeShare()
  watchSuspend()
  openApp()
})

// ── Lifecycle ─────────────────────────────────────────────────────────────
let quitting = false
app.on('before-quit', () => { quitting = true; tray?.destroy(); tray = null })
app.on('window-all-closed', () => {
  venmicUnlink()
  stopNativeShare()
  if (process.platform !== 'darwin') app.quit()
})
app.on('activate', () => { if (BrowserWindow.getAllWindows().length === 0) openApp() })

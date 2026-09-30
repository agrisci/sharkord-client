const { contextBridge, ipcRenderer } = require('electron')

contextBridge.exposeInMainWorld('electronAPI', {
  // Screen share audio (venmic virtual mic)
  virtmicActive: () => ipcRenderer.invoke('virtmic-active'),
  virtmicUnmute: () => ipcRenderer.invoke('virtmic-unmute'),
  virtmicStop:   () => ipcRenderer.invoke('virtmic-stop'),
  // Desktop notifications
  notificationShown:   () => ipcRenderer.send('notification-shown'),
  notificationClicked: () => ipcRenderer.send('notification-clicked'),
  // Native screen share (the nativeShare setting): the picked monitor, or null when the
  // share should stay Chromium's own; start hands the page a MessagePort (below)
  // Linux: pick asks the helper to pick the screen (instead of Chromium): 'ok', 'cancelled', or
  // 'chromium' to use Chromium's getDisplayMedia; stop is for a share that ends before start
  nativeShareTarget: codec => ipcRenderer.invoke('native-share-target', codec),
  nativeShareStart:  opts => ipcRenderer.send('native-share-start', opts),
  nativeSharePick:   codec => ipcRenderer.invoke('native-share-pick', codec),
  nativeShareStop:   () => ipcRenderer.send('native-share-stop'),
})

// The helper's frame port can't cross contextBridge; the page world takes it from a
// window message instead (installNativeShare)
ipcRenderer.on('native-share-port', (e, msg) => window.postMessage({ sharkordNativeSharePort: true, id: msg?.id }, '*', e.ports))

// Runs in the page's own world before any page script, so Sharkord's calls
// to getDisplayMedia / enumerateDevices go through these wrappers.
function installShareAudioHooks (VIRTMIC) {
  const md = navigator.mediaDevices
  if (!md?.getDisplayMedia) return

  const enumerate = md.enumerateDevices.bind(md)
  const gum       = md.getUserMedia.bind(md)
  const gdm       = md.getDisplayMedia.bind(md)

  // Hide the virtual mic from Sharkord's microphone list
  md.enumerateDevices = async () => (await enumerate()).filter(d => d.label !== VIRTMIC)

  const findVirtmic = async () => {
    // the node can take a moment to appear after link()
    for (let i = 0; i < 10; i++) {
      const dev = (await enumerate()).find(d => d.kind === 'audioinput' && d.label === VIRTMIC)
      if (dev) return dev
      await new Promise(r => setTimeout(r, 100))
    }
    return null
  }

  // Also used on the stream the native share builds itself (Linux), which never calls Chromium's
  const withShareAudio = async (stream) => {
    const api    = window.electronAPI

    if (await api.virtmicActive()) {
      try {
        const dev = await findVirtmic()
        if (dev) {
          const mic = await gum({ audio: {
            deviceId: { exact: dev.deviceId },
            autoGainControl: false, echoCancellation: false, noiseSuppression: false,
            channelCount: 2, sampleRate: 48000, sampleSize: 16,
          } })
          stream.getAudioTracks().forEach(t => { stream.removeTrack(t); t.stop() })
          stream.addTrack(mic.getAudioTracks()[0])
          api.virtmicUnmute()
        } else console.warn('[share] virtual mic not found')
      } catch (e) { console.warn('[share] virtual mic:', e.message) }
    }

    // Sharkord ends a share with track.stop(), which fires no 'ended' — hook both
    let done = false
    const onEnd = () => { if (!done) { done = true; api.virtmicStop() } }
    stream.getTracks().forEach(t => {
      const stop = t.stop.bind(t)
      t.stop = () => { onEnd(); stop() }
      t.addEventListener('ended', onEnd)
    })
    return stream
  }
  md.getDisplayMedia = async (constraints) => withShareAudio(await gdm(constraints))
  md.getDisplayMedia.withShareAudio = withShareAudio
}

contextBridge.executeInMainWorld({ func: installShareAudioHooks, args: ['vencord-screen-share'] })

// ── Native screen share (the nativeShare setting, or SHARKORD_NATIVE_SHARE=1) ──
// Sharkord keeps the real capture track (its preview); the connection gets a placeholder whose
// every outgoing encoded frame is swapped, by an encoded transform, for a frame the native helper
// (native/) captured and encoded with the GPU. Chromium keeps the connection, packetization and
// bandwidth estimate; nothing here depends on Sharkord's or mediasoup-client's internals.
// Each placeholder is announced to the worker with its timestamp, and Chromium derives the RTP
// timestamp from it (plus a random offset, and smoothed against the system clock), so once one
// frame is identified the worker pairs each frame with the nearest placeholder of the same size --
// frames are a whole frame interval apart, and the size changes exactly on native keyframes -- and
// follows the smoothing's drift. Chromium drops placeholders it isn't ready to encode, so the first one is found
// by its size: until the worker has anchored, each placeholder's width carries a small counter.
// Keyframes must line up: receivers (and mediasoup, for AV1) trust Chromium's frame type, not our
// payload. Chromium 152 has no generateKeyFrame() on the transformer, but its encoder always emits
// a keyframe when the input size changes, so a native keyframe's placeholder switches size. Until
// a Chromium keyframe carries a native keyframe nothing is sent -- at start, after a lost frame,
// and when a viewer's PLI makes Chromium key on its own.
const NATIVE_SHARE_WORKER = `
const queue = [], stats = { swapped: 0, bootstrap: 0, noMatch: 0, lost: 0, chromeKeys: 0, resync: 0, nativeKeys: 0, ckNoNk: 0, nkNoCk: 0 }
let offset = null, codec = null, synced = false, askedKey = false, lostAt = 0, lostWhy = '', unsent = 0
// A loss of sync and how long it took to get back, reported once sync is back
const unsync = why => { if (synced || !lostAt) { lostAt = performance.now(); lostWhy = why; unsent = 0 } synced = false }
const needKey = () => { if (!askedKey) { askedKey = true; self.postMessage({ type: 'need-key' }) } }
self.onmessage = e => {
  const m = e.data
  if (m.type !== 'frame') return
  queue.push(m); if (m.key) { askedKey = false; stats.nativeKeys++ }
  // Before anchoring only the newest placeholders can be the one Chromium encodes next, and the
  // width counter is unique among 16
  if (offset === null && queue.length > 16) queue.splice(0, queue.length - 16)
}
const rtpOf = ts => (offset + Math.round(ts * 0.09)) >>> 0
self.onrtctransform = e => {
  const out = e.transformer.writable.getWriter(), reader = e.transformer.readable.getReader()
  ;(async () => { for (;;) {
    const { value: frame, done } = await reader.read(); if (done) return
    const meta = frame.getMetadata(), ckey = frame.type === 'key'
    if (!codec) { codec = meta.mimeType; self.postMessage({ type: 'codec', codec }) }
    if (ckey) stats.chromeKeys++
    if (offset === null) {
      const j = queue.findLastIndex(q => q.w === meta.width)
      if (j < 0) { stats.noMatch++; continue }
      offset = (meta.rtpTimestamp - Math.round(queue[j].ts * 0.09)) >>> 0
      self.postMessage({ type: 'anchored' })
    }
    let i = -1, err = 0
    queue.forEach((q, k) => { if (q.w !== meta.width || q.h !== meta.height) return   // the size pins keyframes to the right frame
      const d = (rtpOf(q.ts) - meta.rtpTimestamp) | 0; if (Math.abs(d) <= 720 && (i < 0 || Math.abs(d) < Math.abs(err))) { i = k; err = d } })
    if (i >= 0) offset = (offset - Math.round(err * 0.2)) >>> 0   // follow Chromium's timestamp smoothing
    if (i < 0) { stats.noMatch++; if (ckey) { unsync('keyframe without a match'); needKey() } continue }
    if (i > 0) { if (queue.slice(0, i).some(q => q.data)) { stats.lost += i; unsync('frames dropped by Chromium') } queue.splice(0, i) }
    const f = queue.shift()
    if (!f.data) { stats.bootstrap++; continue }          // placeholder sent before the helper's frames
    if (synced && ckey && !f.key) unsync('Chromium keyframe (a request)')   // a viewer asked for a keyframe
    if (!synced) {
      if (!(ckey && f.key)) { stats.resync++; unsent++; if (ckey) stats.ckNoNk++; if (f.key) stats.nkNoCk++; needKey(); continue }
      synced = true
      if (lostAt) { self.postMessage({ type: 'resynced', why: lostWhy, ms: Math.round(performance.now() - lostAt), unsent }); lostAt = 0 }
    }
    frame.data = f.data; stats.swapped++
    await out.write(frame)
  } })()
  setInterval(() => self.postMessage({ type: 'stats', stats }), 1000)
}`

function installNativeShare (workerSource, helperPicks) {
  const md = navigator.mediaDevices
  const withShareAudio = md.getDisplayMedia.withShareAudio
  const gdm = md.getDisplayMedia.bind(md)
  const api = window.electronAPI
  const log = (...a) => console.log('[native-share]', ...a)
  const shares = new Map()   // Sharkord's track (the capture, or on Linux the preview) → share state
  const CODECS = { 'video/H264': 'h264', 'video/AV1': 'av1' }
  const now = () => Math.round(performance.now() * 1000)   // one clock for every placeholder
  const num = v => typeof v === 'number' ? v : (v?.ideal ?? v?.exact ?? v?.max)
  const PC = RTCPeerConnection.prototype
  const { addTransceiver, addTrack } = PC
  const replaceTrack = RTCRtpSender.prototype.replaceTrack
  const senderGetStats = RTCRtpSender.prototype.getStats, pcGetStats = PC.getStats
  const bySender = new Map()   // sender carrying a placeholder → share state

  // The helper's frame port comes from the preload's isolated world as a window message, tagged
  // with the id of the share that asked for it (a quick restart must not take the old share's)
  const portWaiters = new Map()
  let nextId = 1
  let nativeOff = ''   // why, once a native share failed mid-share
  window.addEventListener('message', e => {
    if (e.source !== window || !e.data?.sharkordNativeSharePort || !e.ports[0]) return
    const waiter = portWaiters.get(e.data.id)
    portWaiters.delete(e.data.id)
    if (waiter) waiter(e.ports[0])
    else { e.ports[0].postMessage({ cmd: 'stop' }); e.ports[0].close() }   // its share is gone
  })

  // Whether the server allows simulcast: one of its public settings, which reach the page only over
  // Sharkord's WebSocket (on joining, and again when an admin changes them). Read from each message
  // as Sharkord receives it, never changed. null until seen
  let serverSimulcast = null
  const WS = window.WebSocket
  window.WebSocket = class WebSocket extends WS {
    constructor (...args) {
      super(...args)
      this.addEventListener('message', e => {
        const m = typeof e.data === 'string' && e.data.match(/"webRtcSimulcastEnabled":(true|false)/)
        if (m && serverSimulcast !== (m[1] === 'true')) { serverSimulcast = m[1] === 'true'; log('server simulcast:', serverSimulcast) }
      })
    }
  }

  // The screen codec picked in Sharkord's Devices settings (its localStorage). The helper encodes
  // only H.264 and AV1, and on Linux it has to know before the pick, so it's used only when one of
  // them is picked explicitly and the share won't be simulcast: Sharkord shares VP8 when the server
  // allows simulcast and the user's switch is on (while the server's setting hasn't been seen, the
  // switch alone decides). Anything else (VP8, VP9, auto, a setting it can't read) is Chromium's
  // share from the start, on both platforms.
  const nativeCodec = () => {
    try {
      const devices = JSON.parse(localStorage.getItem('sharkord-devices-settings'))
      const simulcast = serverSimulcast !== false && devices?.simulcastEnabled !== false
      return simulcast ? null : CODECS[devices?.screenCodec] || null
    } catch { return null }
  }

  md.getDisplayMedia = async (constraints) => {
    // Linux: the helper picks and captures the screen, Chromium doesn't (one portal dialog, not
    // two). Sharkord gets a stream the page builds: the preview is the helper's frames, decoded.
    // Any other share (VP8, VP9, auto, simulcast) stays entirely Chromium's: its capture and encoder.
    const codec = nativeCodec()
    if (nativeOff && codec) log('Chromium\'s share: the native share is off for this session:', nativeOff)
    if (helperPicks && !nativeOff && constraints?.video && codec) {
      const pick = await api.nativeSharePick(codec).catch(() => 'chromium')
      if (pick === 'cancelled') throw new DOMException('Permission denied by user', 'NotAllowedError')
      if (pick === 'ok') {
        const preview = new MediaStreamTrackGenerator({ kind: 'video' })
        startShare(preview, constraints, true)
        return withShareAudio(new MediaStream([preview]))
      }
    }
    const real = await gdm(constraints)
    const video = real.getVideoTracks()[0]
    // A window, no helper, another codec or one this GPU can't encode, or it failed earlier:
    // Chromium's own share
    const target = video && codec && await api.nativeShareTarget(codec).catch(() => null)
    if (!target || nativeOff) return real
    startShare(video, constraints, false)
    return real
  }

  // `owned`: the helper's capture is the only one (Linux), `video` its decoded preview
  const startShare = (video, constraints, owned) => {
    const set = video.getSettings(), c = constraints?.video || {}
    const want = {
      width: num(c.width) || set.width || 1920, height: num(c.height) || set.height || 1080,
      fps: Math.round(num(c.frameRate) || set.frameRate || 60),
    }
    const gen = new MediaStreamTrackGenerator({ kind: 'video' })
    // No contentHint 'detail' here, although Sharkord sets it on the real track: as screen content
    // Chromium probes every 5 s, and each probe result replaced the estimate, often lower -- it
    // sawtoothed at 5-8 Mbps; as camera content it ramped cleanly past 12 Mbps
    const genWriter = gen.writable.getWriter()
    // Tiny black frames: Chromium encodes them in software for next to nothing. Once anchored,
    // two sizes, because a size change makes its encoder emit a keyframe for exactly that frame.
    // Built in memory, not on a canvas: a canvas frame lives on the GPU and has to be read back for
    // the software encoder, and with the GPU busy (4K capture and encode, a video playing) that
    // stalled the track at 15 fps.
    const blanks = new Map()
    const blank = (w, h) => {
      const k = w + 'x' + h
      if (!blanks.has(k)) { const b = new Uint8Array(w * h * 3 / 2); b.fill(16, 0, w * h); b.fill(128, w * h); blanks.set(k, b) }
      return blanks.get(k)
    }
    const worker = new Worker(URL.createObjectURL(new Blob([workerSource], { type: 'text/javascript' })))
    const s = { id: nextId++, owned, want, gen, worker, port: null, sender: null, stopped: false, fell: false, anchored: false, tag: 0, stats: {}, helper: {}, keys: [], size: 0, kbps: 2000, kbpsAt: 0, pending: [], lastPush: 0, pacer: null }
    shares.set(video, s)

    const pushPlaceholder = (key, data) => {
      if (s.stopped || s.fell) return
      const ts = now()
      let w = 320, h = 180
      if (!s.anchored) w = 320 - 2 * (s.tag++ % 16)   // the worker identifies its first frame by width
      else { if (key) s.size ^= 1; h = s.size ? 176 : 180 }
      worker.postMessage({ type: 'frame', key, data, ts, w, h }, data ? [data] : [])
      genWriter.write(new VideoFrame(blank(w, h), { format: 'I420', codedWidth: w, codedHeight: h, timestamp: ts })).catch(() => {})
    }
    // Native frames reach the page in bursts, and Chromium thins out a source that delivers faster
    // than its frame rate. So they are paced: pushed at least 80% of a frame interval apart,
    // at most a frame late.
    const minGap = 800 / want.fps
    const pace = () => {
      if (s.pacer || !s.pending.length) return
      s.pacer = setTimeout(() => {
        s.pacer = null
        const f = s.pending.shift()
        s.lastPush = performance.now()
        pushPlaceholder(f.key, f.data)
        pace()
      }, Math.max(0, s.lastPush + minGap - performance.now()))
    }
    // Until the helper's frames flow: placeholders at the share's rate, so Chromium encodes and
    // the worker learns the negotiated codec
    s.boot = setInterval(() => pushPlaceholder(false, null), 1000 / want.fps)

    // At most one keyframe per 300 ms: each one resizes the placeholder, which makes Chromium
    // reconfigure its encoder, and a burst of them turned one dropped frame into a keyframe storm
    // (4K AV1 on a real server, when keyframes weren't capped). Waiting longer kept a storm going
    // too: a viewer's request makes Chromium key the placeholder on its own, the swap loses sync and
    // sends nothing until a native keyframe lines up (~570 ms at 1 s), and the viewer asks again.
    // Until the helper has started, a second.
    const requestKeyframe = () => {
      if (s.keyTimer) return
      const gap = s.encoder ? 300 : 1000
      const wait = Math.max(0, (s.keyAt || 0) + gap - performance.now())
      s.keyTimer = setTimeout(() => { s.keyTimer = null; s.keyAt = performance.now(); s.keyReqs = (s.keyReqs || 0) + 1; s.port?.postMessage({ cmd: 'keyframe' }) }, wait)
    }
    const closeHelper = () => {
      clearInterval(s.boot)
      portWaiters.delete(s.id)
      if (!s.port) api.nativeShareStop()   // Linux: the helper from the pick is waiting for start
      try { s.port?.postMessage({ cmd: 'stop' }); s.port?.close() } catch {}
      s.port = null
    }
    // Before the placeholder reached a connection, the connection just gets Chromium's own capture
    // -- if there is one: a share the helper picked (Linux) has nothing else, and ends. Once a
    // connection carried it, the share ends instead, as if the capture had stopped: Sharkord cleans
    // up on the track's `ended`, viewers see the share end, and shares for the rest of the session
    // use Chromium's own path (not after a suspend, which loses the capture without the helper
    // being at fault). Swapping Chromium's capture in after that left viewers black even before
    // any native frame was swapped (#87: the helper failing 0.3 s in), and mid-share it restarted
    // the RTP timestamps from its older clock (viewers dropped every frame as stale); the hardware
    // encoder Chromium switches to at <= 1080p stalled after a few frames.
    s.fallback = why => {
      if (s.fell || s.stopped) return
      s.fell = true
      closeHelper()
      if (!s.owned && !s.sender) {
        log('using Chromium capture:', why)
        return
      }
      log('ending the share:', why)
      // A capture lost to a suspend isn't the helper's fault: the next share can use it again
      if (s.suspended) log('native share stays on: the system was suspended')
      else nativeOff = why
      // Again every second until Sharkord stops the track: a fallback right at the start (a codec
      // the helper can't encode) comes before Sharkord listens for the end, and the share was
      // left running on a black preview
      let tries = 0
      const end = () => {
        if (s.trackStopped || tries++ >= 10) return
        video.dispatchEvent(new Event('ended'))
        setTimeout(end, 1000)
      }
      setTimeout(end)
    }

    // The preview of a share the helper picked: its frames, decoded by Chromium (on the GPU where
    // it can). Decoding starts at the first keyframe.
    const previewWriter = owned && video.writable.getWriter()
    const decoder = owned && new VideoDecoder({
      output: f => previewWriter.write(f).catch(() => f.close()),
      error: e => log('preview:', e.message),
    })
    const preview = (codec, msg) => {
      if (!decoder || decoder.state === 'closed') return
      if (decoder.state === 'unconfigured') {
        if (!msg.key) return
        decoder.configure({ codec: codec === 'h264' ? 'avc1.42e034' : 'av01.0.13M.08', optimizeForLatency: true })
      }
      try { decoder.decode(new EncodedVideoChunk({ type: msg.key ? 'key' : 'delta', timestamp: msg.pts, data: msg.data })) }
      catch (e) { log('preview:', e.message) }
    }

    worker.onmessage = e => {
      const m = e.data
      if (m.type === 'stats') s.stats = m.stats
      if (m.type === 'anchored') s.anchored = true
      if (m.type === 'need-key') requestKeyframe()
      if (m.type === 'resynced') log(`resynced after ${m.why}: ${m.ms} ms, ${m.unsent} frames not sent, keyframe asked ${s.keyAt ? Math.round(performance.now() - s.keyAt) + ' ms ago' : 'never'}`)
      if (m.type === 'codec' && !s.port && !s.fell) {
        const codec = CODECS[m.codec]
        if (!codec) return s.fallback(m.codec + ' has no native encoder')
        portWaiters.set(s.id, port => {
          if (s.stopped || s.fell) { port.postMessage({ cmd: 'stop' }); port.close(); return }
          s.port = port; s.portAt = performance.now()
          port.onmessage = ({ data: msg }) => {
            if (msg.type === 'frame') {
              clearInterval(s.boot)
              s.lastFrameAt = performance.now()
              preview(codec, msg)   // before its buffer moves to the worker
              const d = msg.data
              // Native keyframes and why they came (a bitrate change restarts the encoder with
              // one, a request asks for one): at 4K each is a burst the bandwidth estimate feels
              if (msg.key) {
                const t = performance.now(), kb = Math.round(d.byteLength / 1024)
                s.keys.push(`${kb} KB ${t - s.kbpsAt < 1500 ? 'rate' : t - (s.keyAt ?? -Infinity) < 1500 ? 'req' : 'other'}`)
                s.lastKey = { t, kb }
              }
              s.pending.push({ key: msg.key, data: d.byteOffset === 0 && d.byteLength === d.buffer.byteLength ? d.buffer : d.slice().buffer })
              pace()
            } else if (msg.event === 'stats') s.helper = { fps: msg.fps, kbps: msg.kbps, padding: msg.padding, held: msg.held }
            else if (msg.event === 'error') s.fallback('helper failed: ' + msg.message)
            else if (msg.event === 'suspend') { s.suspended = true; log('system suspending') }
            else if (msg.event === 'started') { s.encoder = { name: msg.encoder, width: msg.size?.[0], height: msg.size?.[1] }; log(msg.event, JSON.stringify(msg)) }
            else if (msg.event) log(msg.event, JSON.stringify(msg))
          }
        })
        // Always the resolution the user picked, as Chromium does for a screen share: the estimate
        // can't rise past what a mostly still screen sends (~5 Mbps on a LAN), so following it
        // flipped a 4K share between 1080p and 4K
        s.kbpsAt = performance.now()
        api.nativeShareStart({ id: s.id, codec, ...want, kbps: s.kbps })
        log('starting helper', codec, JSON.stringify(want))
      }
    }

    s.stop = () => {
      if (s.stopped) return
      s.stopped = true; clearInterval(s.timer); clearTimeout(s.pacer); clearTimeout(s.keyTimer); closeHelper()
      worker.terminate(); gen.stop(); shares.delete(video); bySender.delete(s.sender)
      try { if (decoder && decoder.state !== 'closed') decoder.close() } catch {}
      log('stopped', JSON.stringify(s.stats))
    }
    // Sharkord ends a share with track.stop(), which fires no 'ended'
    const stop = video.stop.bind(video)
    video.stop = () => { s.trackStopped = true; s.stop(); stop() }
    video.addEventListener('ended', () => s.stop())
  }

  // Put the placeholder on the sender, attach the transform, and lift the resolution-based
  // bitrate cap (Chromium would budget for a 320x180 stream)
  const capFor = ({ width, height, fps }) => Math.round(width * height * fps * 0.05 / 1000)
  const attach = (sender, s, pc) => {
    if (s.sender) return
    s.sender = sender; s.outIds = new Set(); bySender.set(sender, s)
    sender.transform = new RTCRtpScriptTransform(s.worker, {})
    s.timer = setInterval(async () => {
      // After a fallback: what Chromium's own capture really sends, so a poor fallback is visible
      if (s.fell) {
        let o, m; (await senderGetStats.call(sender)).forEach(x => { if (x.type === 'outbound-rtp') o = x; if (x.type === 'media-source' && x.kind === 'video') m = x })
        if (o) log(`fallback: sent ${o.framesPerSecond ?? 0} fps, ${o.frameWidth}x${o.frameHeight}, ${o.encoderImplementation}, target ${Math.round((o.targetBitrate || 0) / 1000)} kbps, encoded ${o.framesEncoded}, sent ${o.framesSent}, limit ${o.qualityLimitationReason}, source ${m?.framesPerSecond ?? '-'} fps ${m?.width}x${m?.height} frames ${m?.frames}`)
        return
      }
      // Watchdog: a share that stops getting frames through leaves viewers on the black
      // placeholder with no error anywhere (a GPU stall once did, for minutes). Resyncs after a
      // keyframe request take a second or two, so 6 s without a swapped frame means stuck.
      if (s.port) {
        const t = performance.now(), swapped = s.stats.swapped || 0
        if (swapped > (s.lastSwapped ?? -1)) { s.lastSwapped = swapped; s.swappedAt = t }
        if (t - (s.lastFrameAt ?? s.portAt) > 10000) return s.fallback('no frames from the helper for 10 s')
        if (s.lastFrameAt && t - (s.swappedAt ?? s.portAt) > 6000) return s.fallback('stalled: no frame swapped for 6 s')
      }
      let o, r, bwe; (await senderGetStats.call(sender)).forEach(x => {
        if (x.type === 'outbound-rtp') { o = x; s.outIds.add(x.id) }
        if (x.type === 'remote-inbound-rtp' && x.kind === 'video') r = x
        if (x.type === 'candidate-pair' && x.nominated && x.availableOutgoingBitrate) bwe = Math.round(x.availableOutgoingBitrate / 1000)
      })
      // Time packets waited in Chromium's pacer since the last tick: grows when the helper sends
      // faster than the pacing rate (an encoder overshooting on motion)
      const pacerMs = o && o.packetsSent > (s.lastSent || 0) ? Math.round((o.totalPacketSendDelay - (s.lastDelay || 0)) * 1000 / (o.packetsSent - (s.lastSent || 0))) : 0
      if (o) { s.lastSent = o.packetsSent; s.lastDelay = o.totalPacketSendDelay }
      // The helper follows the transport estimate, not outbound-rtp targetBitrate: Chromium counts
      // bytes the transform added as post-encode overhead (packetized size minus the placeholder's
      // encoded size, capped at half) and halves the encoder target. But the estimate itself kept
      // dropping 15-30% every few seconds on a clean LAN (no loss, no NACKs, 1 ms RTT, iperf clean
      // to 100 Mbps), so while the path is clean the helper works up to its cap and the estimate follows
      // what is really sent; on real congestion it drops to 0.85x the estimate until 10 s without.
      // Congestion: loss, the round trip growing, or packets kept waiting in Chromium's pacer. The
      // pacer stays queued when the estimate is really below what goes out: after a quiet stretch
      // it fell 32 -> 5.6 Mbps and stayed, and holding 25 Mbps through that queued 0.8 s of lag.
      // A keyframe queues 50-180 ms on a clean path (over two ticks on Wi-Fi), so only over 250 ms
      // counts: counted, each keyframe lowered the bitrate, and the change itself restarted the
      // encoder with a keyframe (GStreamer's AMF did) -- a keyframe every few seconds. The cap:
      // Sharkord's bitrate setting (x-google-max-bitrate in the answer) and 0.05 bits per pixel per
      // frame (~25 Mbps at 4K60). So down at once, and up in 10% steps at least 2 s apart: the
      // helper's FFmpeg changes the rate in place, without a keyframe.
      // A collapse of the estimate (no loss): Chromium's overuse rule sets it to 0.85x what was
      // acknowledged lately, tiny after a still screen; logged with the keyframe before it
      if (bwe && s.lastBwe && bwe < s.lastBwe * 0.6) log(`estimate fell ${s.lastBwe} -> ${bwe} kbps (${s.lastKey ? `${Math.round(performance.now() - s.lastKey.t)} ms after a ${s.lastKey.kb} KB keyframe` : 'no keyframe yet'}, helper sending ${s.helper.kbps ?? '-'} kbps)`)
      if (bwe) s.lastBwe = bwe
      const rtt = r?.roundTripTime
      if (rtt != null) s.minRtt = Math.min(s.minRtt ?? rtt, rtt)
      if ((r?.fractionLost ?? 0) > 0.02 || (rtt != null && rtt > s.minRtt + 0.02) || pacerMs > 250) s.congestedAt = performance.now()
      if (bwe && s.port && s.encoder) {
        const mid = pc?.getTransceivers().find(t => t.sender === sender)?.mid
        const sec = mid != null && pc.remoteDescription?.sdp.split(/(?=^m=)/m).find(m => m.includes('a=mid:' + mid + '\r'))
        const setting = +(sec?.match(/x-google-max-bitrate=(\d+)/)?.[1]) || Infinity
        const cap = Math.round(Math.min(setting, capFor({ ...s.want, width: s.encoder.width, height: s.encoder.height })))
        const clean = performance.now() - (s.congestedAt ?? -Infinity) > 10000
        // Clean: 0.9x the estimate, never down. It grows ~8%/s whenever at least 2/3 of it is
        // sent, so running ahead doesn't ramp faster -- straight to the cap queued up to 1.3 s in
        // Chromium's pacer, 1.5x the estimate still 0.85 s, while it climbed 5 -> 25 Mbps in ~20 s.
        // And the whole estimate left no room for audio, overhead and keyframes on a link that
        // really is that fast: on a 3 Mbps upload each step up queued 0.5-0.7 s.
        // Except after a still screen: the helper sent under half what it was asked (VBR, a few
        // hundred kbps), Chromium's estimate fell to what was acknowledged (32 -> 5.8 Mbps), and
        // motion then went out at the old 25 Mbps into it: 1.4 s in the pacer for ~6 s. Then down
        // to the estimate, and up with it again
        // (The helper's padding counts: it holds the estimate up while the screen is still)
        const idle = (s.helper.kbps ?? Infinity) + (s.helper.padding ?? 0) < s.kbps * 0.5 && bwe < s.kbps
        const aim = clean && !idle ? Math.min(cap, Math.max(s.kbps, Math.round(bwe * 0.9))) : Math.min(cap, Math.round(bwe * (clean ? 0.9 : 0.85)))
        s.cap = cap
        // The last step may be smaller: the cap (Sharkord's bitrate slider) is often under the step
        const [step, every] = [1.1, 2000]
        if (aim < s.kbps * 0.9 || ((aim >= s.kbps * step || (aim === cap && aim > s.kbps)) && performance.now() - s.kbpsAt > every)) {
          s.kbps = aim; s.kbpsAt = performance.now(); s.port.postMessage({ cmd: 'bitrate', kbps: aim })
        }
      }
      if (o) log(`sent ${o.framesPerSecond ?? 0} fps, estimate ${bwe} kbps, target ${Math.round((o.targetBitrate || 0) / 1000)} kbps, asked ${s.kbps} kbps (cap ${s.cap}), pacer ${pacerMs} ms, rtt ${Math.round((r?.roundTripTime ?? 0) * 1000)} ms, jitter ${Math.round((r?.jitter ?? 0) * 1000)} ms, lost ${r?.packetsLost ?? 0} (${Math.round((r?.fractionLost ?? 0) * 1000) / 10}%), nack ${o?.nackCount ?? 0}, retx ${o?.retransmittedPacketsSent ?? 0}, pli ${o.pliCount ?? 0}, fir ${o.firCount ?? 0}, keys ${o.keyFramesEncoded ?? 0}, keyreq ${s.keyReqs || 0}, native keys [${s.keys.join(', ')}], helper ${JSON.stringify(s.helper)}, worker ${JSON.stringify(s.stats)}`)
      s.keys = []
    }, 2000)
  }
  // Stats describe the helper's encode, not the placeholder's: Sharkord's stats panel would
  // otherwise show Chromium's software encoder at 320x180. Frame rate and bytes stay Chromium's
  // own counts -- they are what really goes out. Until the helper has started, "Native: starting"
  // (not the placeholder's OpenH264); after a fallback, Chromium's own encoder again.
  // "Native: AMF, h264_amf" tells the helper apart from Chromium's encoders, and carries a word
  // Sharkord's stats match to label it GPU (`hardware|vaapi|amf|nvenc...` in its
  // stats-popover.tsx; h264_vulkan alone shows as Unknown).
  const API_LABELS = { vulkan: 'Vulkan (hardware)', vaapi: 'VAAPI', amf: 'AMF', nvenc: 'NVENC', qsv: 'Quick Sync (hardware)' }
  const nativeLabel = ({ name }) => `Native: ${API_LABELS[name.split('_')[1]] ?? 'hardware'}, ${name}`
  const rewrite = (report, sender) => {
    // Through the sender its outbound-rtp is known before the first stats tick records its id
    const own = sender && bySender.get(sender)
    if (own) report.forEach((x, id) => { if (x.type === 'outbound-rtp') own.outIds.add(id) })
    const shares = [...bySender.values()].filter(s => !s.fell && !s.stopped)
    if (!shares.length) return report
    const out = new Map()
    report.forEach((x, id) => {
      const s = x.type === 'outbound-rtp' && shares.find(s => s.outIds.has(id))
      if (!s) return out.set(id, x)
      out.set(id, !s.encoder ? { ...x, encoderImplementation: 'Native: starting' }
        : { ...x, encoderImplementation: nativeLabel(s.encoder), powerEfficientEncoder: true,
            frameWidth: s.encoder.width ?? x.frameWidth, frameHeight: s.encoder.height ?? x.frameHeight })
    })
    return out
  }
  RTCRtpSender.prototype.getStats = async function () { return rewrite(await senderGetStats.call(this), this) }
  PC.getStats = async function (...args) { return rewrite(await pcGetStats.apply(this, args)) }

  PC.addTransceiver = function (trackOrKind, init) {
    const s = shares.get(trackOrKind)
    if (!s || s.fell) return addTransceiver.call(this, trackOrKind, init)
    const encs = init?.sendEncodings
    if (encs?.length > 1) { s.fallback('simulcast'); return addTransceiver.call(this, trackOrKind, init) }
    init = { ...init, sendEncodings: [{ ...(encs?.[0] || {}), maxBitrate: 50e6 }] }
    const t = addTransceiver.call(this, s.gen, init)
    attach(t.sender, s, this)
    return t
  }
  PC.addTrack = function (track, ...streams) {
    const s = shares.get(track)
    if (!s || s.fell) return addTrack.call(this, track, ...streams)
    const sender = addTrack.call(this, s.gen, ...streams)
    attach(sender, s, this)
    return sender
  }
  RTCRtpSender.prototype.replaceTrack = function (track) {
    const s = track && shares.get(track)
    if (!s || s.fell) return replaceTrack.call(this, track)
    attach(this, s)
    return replaceTrack.call(this, s.gen)
  }

  // Watching: which decoder each incoming video uses (GPU or CPU), logged when it first plays
  // and when the codec or decoder changes. Chromium names the decoder only while the page is
  // capturing (the mic in a voice channel counts); otherwise it says so. Every 10 s, what the
  // viewer really got since the last line: stutter shows as freezes and jitter buffer delay.
  const pcs = new Set(), decoders = new WeakMap(), received = new WeakMap(), setRemote = PC.setRemoteDescription
  PC.setRemoteDescription = function (...args) { pcs.add(this); return setRemote.apply(this, args) }
  let tick = 0
  setInterval(async () => {
    tick++
    for (const pc of pcs) {
      if (pc.connectionState === 'closed') { pcs.delete(pc); continue }
      for (const r of pc.getReceivers()) {
        if (r.track?.kind !== 'video' || r.track.readyState !== 'live') continue
        let i; const codecs = new Map()
        try { (await r.getStats()).forEach(x => { if (x.type === 'inbound-rtp') i = x; if (x.type === 'codec') codecs.set(x.id, x.mimeType) }) } catch { continue }
        if (!i?.framesDecoded) continue
        const codec = (codecs.get(i.codecId) || '?').replace('video/', ''), key = codec + '|' + i.decoderImplementation
        if (tick % 2 === 0) {
          const p = received.get(r), d = n => (i[n] ?? 0) - (p?.[n] ?? 0), secs = (i.timestamp - (p?.timestamp ?? i.timestamp)) / 1000
          if (p && secs > 0) log(`watching ${codec} ${i.frameWidth}x${i.frameHeight}: ${Math.round(d('framesDecoded') / secs)} fps, dropped ${d('framesDropped')}, freezes ${d('freezeCount')} (${Math.round(d('totalFreezesDuration') * 1000)} ms), jitter buffer ${Math.round(d('jitterBufferDelay') * 1000 / Math.max(1, d('jitterBufferEmittedCount')))} ms, keys ${d('keyFramesDecoded')}, pli ${d('pliCount')}, lost ${d('packetsLost')}, nack ${d('nackCount')}, ${Math.round(d('bytesReceived') * 8 / 1000 / secs)} kbps`)
          received.set(r, i)
        }
        if (decoders.get(r) === key) continue
        decoders.set(r, key)
        const where = i.powerEfficientDecoder === true ? 'GPU' : i.powerEfficientDecoder === false ? 'CPU' : 'unknown'
        log(`incoming ${codec} ${i.frameWidth}x${i.frameHeight}: decoder ${i.decoderImplementation ?? '(hidden: not capturing)'}, ${where}, ${Math.round(i.framesPerSecond || 0)} fps`)
      }
    }
  }, 5000)
}

// Installed on Windows and Linux always; each share asks the main process whether to go native
if (process.platform === 'win32' || process.platform === 'linux')
  contextBridge.executeInMainWorld({ func: installNativeShare, args: [NATIVE_SHARE_WORKER, process.platform === 'linux'] })

// Sharkord shows plain `new Notification(...)` without an onclick, so hook the
// constructor: flash the taskbar when one is shown, bring the window back on click.
function installNotificationHooks () {
  const N = window.Notification
  if (!N) return

  const items   = id => [...document.querySelectorAll(`[data-testid="${id}"]`)]
  const waitFor = async find => {
    for (let i = 0; i < 20; i++) { const el = find(); if (el) return el; await new Promise(r => setTimeout(r, 100)) }
    return null
  }
  // The notification carries no channel id — only Sharkord's (untranslated) title:
  // "<author> in #<channel>" or "<author> (DM)". A DM notified through "All messages"
  // gets the channel form, with the DM channel's name "DM - <id>:<id>". Open the
  // matching sidebar item; if nothing matches, the window is just shown.
  const openSource = async title => {
    const dm = title.match(/^(.*) \(DM\)$/) || title.match(/^(.*) in #DM - \d+:\d+$/)
    const ch = !dm && title.match(/^.* in #(.*)$/)
    if (!dm && !ch) return
    const matches = dm
      ? () => items('dm-item').filter(el => el.querySelector('span.truncate')?.textContent === dm[1])
      : () => items('channel-item').filter(el => el.querySelector('.lucide-hash') && el.querySelector('span')?.textContent === ch[1])
    // Same name twice: the one the message went to has an unread badge
    const find = () => { const m = matches(); return m.find(el => el.querySelector('[data-testid="unread-count"]')) || m[0] }
    // Channels and DMs share the sidebar: switch it if the other list is showing
    if (!!dm === !!items('channel-item').length) document.querySelector('[data-testid="dm-toggle"]')?.click()
    ;(await waitFor(find))?.click()
  }

  window.Notification = class Notification extends N {
    constructor (...a) {
      super(...a)
      window.electronAPI.notificationShown()
      this.addEventListener('click', () => {
        window.electronAPI.notificationClicked()
        openSource(this.title).catch(() => {})
      })
    }
  }
}

contextBridge.executeInMainWorld({ func: installNotificationHooks })

// ── "Change server" inside Sharkord's UI ────────────────────────────────
//   Added to the ☰ server menu (above Disconnect) and the login screen (under
//   Connect). Each is anchored on a data-testid that Sharkord's own e2e tests
//   rely on, and is a clone of that element so it keeps Sharkord's exact
//   styling. If an anchor ever disappears the control just isn't added
//   (Ctrl+Shift+O and the tray menu still work).
const MARK = 'data-client-change-server'

function cloneAs (anchor, label, place) {
  if (anchor.parentElement.querySelector(`[${MARK}]`)) return null
  const el = anchor.cloneNode(false)
  el.removeAttribute('data-testid')
  el.setAttribute(MARK, '')
  el.textContent = label
  el.addEventListener('click', () => ipcRenderer.send('change-server'))
  anchor[place](el)
  return el
}

function addChangeServerControls () {
  // ☰ server menu: styled like Disconnect minus its red
  const disconnect = document.querySelector('[data-testid="server-menu-disconnect"]')
  const item = disconnect && cloneAs(disconnect, 'Change server', 'before')
  if (item) {
    item.removeAttribute('data-highlighted')
    item.removeAttribute('data-radix-collection-item')   // not one of Radix's own items
    item.classList.remove('text-destructive', 'focus:text-destructive')
    item.classList.add('focus:text-accent-foreground')
    item.addEventListener('pointermove', () => item.focus())   // Radix does this for its items
  }

  // Login screen: styled like Connect (or the SSO button on OIDC-only servers)
  const connect = document.querySelector('[data-testid="connect-button"]') ||
                  document.querySelector('[data-testid="connect-oidc-button"]')
  const button = connect && cloneAs(connect, 'Change server', 'after')
  if (button) {
    button.removeAttribute('disabled')   // Connect is disabled while the fields are empty
    button.type = 'button'
  }
}

// ── "Desktop Client" tab in Sharkord's user settings ──────────────────────────
//   A sidebar entry after "Others" (found by its sliders-horizontal icon, so
//   server settings don't get one), cloned from it so it keeps Sharkord's
//   styling. Selecting it hides Sharkord's content area (its own elements are
//   never changed, only hidden) and shows ours in the same place, built from
//   Sharkord's card / label / switch / save-bar class strings. Changes wait
//   for Save Changes like Sharkord's own; any other entry puts Sharkord's
//   content back and drops an unsaved draft. The tray menu has the same
//   toggles, applied at once, if the anchor ever disappears.
const DESKTOP = 'data-client-desktop-tab'
const SWITCH_CLASS = 'peer data-[state=checked]:bg-primary data-[state=unchecked]:bg-input focus-visible:border-ring focus-visible:ring-ring/50 dark:data-[state=unchecked]:bg-input/80 inline-flex h-[1.15rem] w-8 shrink-0 items-center rounded-full border border-transparent shadow-xs transition-all outline-none focus-visible:ring-[3px] disabled:cursor-not-allowed disabled:opacity-50'
const THUMB_CLASS  = 'bg-background dark:data-[state=unchecked]:bg-foreground dark:data-[state=checked]:bg-primary-foreground pointer-events-none block size-4 rounded-full ring-0 transition-transform data-[state=checked]:translate-x-[calc(100%-2px)] data-[state=unchecked]:translate-x-0'
const BUTTON_CLASS = "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium transition-all disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg:not([class*='size-'])]:size-4 shrink-0 [&_svg]:shrink-0 outline-none focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px] bg-primary text-primary-foreground hover:bg-primary/90 h-9 px-4 py-2"
const ACTIVE_ENTRY = ['bg-accent', 'font-medium']
// lucide "check" and "x", for the codecs the GPU encodes
const CHECK_ICON = '<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="lucide lucide-check h-4 w-4 shrink-0 text-green-500"><path d="M20 6 9 17l-5-5"/></svg>'
const X_ICON = '<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="lucide lucide-x h-4 w-4 shrink-0 text-muted-foreground"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>'
const svg = markup => new DOMParser().parseFromString(markup, 'image/svg+xml').documentElement
// lucide "monitor"
const MONITOR_ICON = '<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="lucide lucide-monitor h-4 w-4 shrink-0"><rect width="20" height="14" x="2" y="3" rx="2"/><line x1="8" x2="16" y1="21" y2="21"/><line x1="12" x2="12" y1="17" y2="21"/></svg>'
const DESKTOP_OPTIONS = [
  { key: 'openAtLogin',    label: 'Open Sharkord when your computer starts up' },
  // Only for launches at login: greyed out while Open at login is off (its value is kept)
  { key: 'startMinimized', label: 'Start minimized', requires: 'openAtLogin',
    description: 'When Sharkord opens at login, it starts in the system tray instead of showing its window.' },
  { key: 'minimizeToTray', label: 'Minimize Sharkord to system tray',
    description: 'Clicking X hides Sharkord to the tray instead of closing it.' },
  { key: 'nativeShare',    label: 'Native screen share',
    description: 'Captures and encodes shares with the GPU outside the browser, for a steady frame rate and sharper picture, when H.264 or AV1 is picked in the Devices tab (with Simulcast off, where the server offers it). ' +
      (process.platform === 'linux' ? 'AMD and Intel GPUs (Vulkan video or VA-API), Wayland. On by default.'
        : 'Screens and windows. On by default with AMD GPUs; NVIDIA and Intel GPUs are supported but not tested yet, turn it on to try.') + ' Takes effect on the next share.' },
  { key: 'chromiumHwEncode', label: 'Hardware encoding for other shares',
    description: 'Lets the browser encode shares that don\'t use the native share on the GPU. Turn it off if those shares look corrupted or never load for viewers. ' +
      (process.platform === 'linux' ? 'Off by default: some drivers encode incorrectly. ' : '') + 'Takes effect after restarting Sharkord.' },
]

function el (tag, className, text) {
  const e = document.createElement(tag)
  if (className) e.className = className
  if (text) e.textContent = text
  return e
}

// The open Desktop tab: { entry, panel, main, was, dirty } -- our entry and content, Sharkord's
// hidden content area and the entry that was selected, and whether there are unsaved changes
let _desktopOpen = null
const OUTLINE_BUTTON_CLASS = "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium transition-all disabled:pointer-events-none disabled:opacity-50 shrink-0 outline-none focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px] border bg-background shadow-xs hover:bg-accent hover:text-accent-foreground dark:bg-input/30 dark:border-input dark:hover:bg-input/50 h-9 px-4 py-2"

function closeDesktopTab () {
  const o = _desktopOpen
  if (!o) return
  _desktopOpen = null
  o.panel.remove()
  o.main.hidden = false
  o.entry.classList.remove(...ACTIVE_ENTRY)
  if (o.was?.isConnected) o.was.classList.add(...ACTIVE_ENTRY)   // Sharkord re-renders it anyway when another is picked
}

// Sharkord's "Discard unsaved changes?" dialog, rebuilt from its AlertDialog class strings and
// words (its own is React state we can't open). true: Discard; false: Cancel. Like Sharkord's, only
// its buttons answer it: Escape and a click on the backdrop do nothing
function confirmDiscard () {
  return new Promise(resolve => {
    const root = el('div')
    root.setAttribute(DESKTOP, 'dialog')
    const overlay = el('div', 'fixed inset-0 z-50 bg-black/50')
    const box = el('div', 'bg-background fixed top-[50%] left-[50%] z-50 grid w-full max-w-[calc(100%-2rem)] translate-x-[-50%] translate-y-[-50%] gap-4 rounded-lg border p-6 shadow-lg sm:max-w-lg')
    box.setAttribute('role', 'alertdialog')
    box.setAttribute('aria-modal', 'true')
    const header = el('div', 'flex flex-col gap-2 text-center sm:text-left')
    header.append(el('h2', 'text-lg font-semibold', 'Discard unsaved changes?'),
      el('p', 'text-muted-foreground text-sm', 'You have unsaved changes. If you leave now, they will be lost.'))
    const footer = el('div', 'flex flex-col-reverse gap-2 sm:flex-row sm:justify-end')
    const cancel = el('button', OUTLINE_BUTTON_CLASS, 'Cancel')
    const discard = el('button', BUTTON_CLASS, 'Discard')
    cancel.type = discard.type = 'button'
    footer.append(cancel, discard)
    box.append(header, footer)
    root.append(overlay, box)
    const done = answer => { root.remove(); document.removeEventListener('keydown', onKey, true); resolve(answer) }
    // Escape stays here: not answering, and not reaching Sharkord (which would close its settings)
    const onKey = e => { if (e.key === 'Escape') { e.preventDefault(); e.stopImmediatePropagation() } }
    cancel.addEventListener('click', () => done(false))
    discard.addEventListener('click', () => done(true))
    document.addEventListener('keydown', onKey, true)
    document.body.append(root)
    discard.focus()
  })
}

const SAVE_BAR = '.sticky.bottom-4'   // Sharkord's SaveBar: its tab has unsaved changes
const CONFIRM_DIALOG = '[data-slot="alert-dialog-content"]'

// Sharkord's own tab has unsaved changes: let Sharkord ask, with its own "discard changes?" dialog,
// by switching to another of its entries, and run `open` in the same mutation that drops them --
// before the browser paints, so that other entry never shows. Cancel keeps the changes and the tab.
// The dialog closes a moment before Sharkord drops the changes, hence the grace period.
function afterSharkordTab (nav, main, open) {
  if (!main.querySelector(SAVE_BAR)) return open()
  const other = [...nav.querySelectorAll('[data-testid="settings-sidebar-entry"]')].find(b => !b.classList.contains('bg-accent'))
  if (!other) return
  let asked = false, timer = null
  const stop = () => { watch.disconnect(); clearTimeout(timer) }
  const watch = new MutationObserver(() => {
    if (!main.isConnected) return stop()
    if (!main.querySelector(SAVE_BAR)) { stop(); return open() }
    if (document.querySelector(CONFIRM_DIALOG)) { asked = true; clearTimeout(timer); timer = null }
    else if (asked && !timer) timer = setTimeout(stop, 1000)   // cancelled: the changes stay
  })
  watch.observe(document.body, { childList: true, subtree: true })
  timer = setTimeout(() => { if (!asked) stop() }, 2000)   // no dialog came
  other.click()
}

function addDesktopTab () {
  // Sharkord re-rendered its content area, or the settings closed: ours goes too
  if (_desktopOpen && (!_desktopOpen.main.isConnected || !_desktopOpen.panel.isConnected || !_desktopOpen.main.hidden)) closeDesktopTab()
  const others = document.querySelector('svg.lucide-sliders-horizontal')?.closest('[data-testid="settings-sidebar-entry"]')
  const nav = others?.closest('nav')
  if (!nav || nav.querySelector(`[${DESKTOP}]`)) return
  // Built fresh (an icon and a truncated label, as Sharkord's SidebarEntry renders) with the
  // entry's own class string, so it looks like Sharkord's whatever Others carries
  const entry = el('button', others.className)
  entry.type = 'button'
  entry.setAttribute(DESKTOP, '')
  entry.classList.remove(...ACTIVE_ENTRY)
  entry.append(svg(MONITOR_ICON), el('span', 'truncate', 'Desktop Client'))
  others.after(entry)

  entry.addEventListener('click', async () => {
    const main = nav.parentElement?.querySelector(':scope > main')
    if (_desktopOpen || !main) return
    // Fetched first, so opening needs no wait once Sharkord lets go of its tab
    const initial = await ipcRenderer.invoke('desktop-settings-get').catch(() => null)
    if (!initial) return
    afterSharkordTab(nav, main, () => {
      if (_desktopOpen || !entry.isConnected || !main.isConnected) return
      const was = nav.querySelector('[data-testid="settings-sidebar-entry"].bg-accent')
      was?.classList.remove(...ACTIVE_ENTRY)
      entry.classList.add(...ACTIVE_ENTRY)
      const { panel, dirty } = buildDesktopPanel(main.className, initial)
      main.hidden = true
      main.after(panel)
      _desktopOpen = { entry, panel, main, was, dirty }
      // On a narrow window the sidebar is a drawer: close it through Sharkord's own backdrop
      nav.parentElement.querySelector(':scope > div.absolute.inset-0')?.click()
    })
  })

  // Another sidebar entry or the back button: Sharkord's content comes back, after asking if there
  // are unsaved changes (capture, before Sharkord's handlers see the click)
  const shell = nav.parentElement.parentElement
  shell.addEventListener('click', e => {
    if (!_desktopOpen || entry.contains(e.target)) return
    // A sidebar entry, or a button in the settings header (back)
    const leaving = e.target.closest('nav button') || (shell.firstElementChild?.contains(e.target) && e.target.closest('button'))
    if (!leaving) return
    if (!_desktopOpen.dirty()) return closeDesktopTab()
    // Unsaved changes: hold the click, ask, and on Discard make it again
    e.preventDefault(); e.stopPropagation()
    confirmDiscard().then(discard => { if (discard && _desktopOpen) { closeDesktopTab(); leaving.click() } })
  }, true)
}

// Escape closes Sharkord's settings: ask first when our tab has unsaved changes
document.addEventListener('keydown', e => {
  if (e.key !== 'Escape' || !_desktopOpen || document.querySelector(`[${DESKTOP}=dialog]`)) return
  if (!_desktopOpen.dirty()) return closeDesktopTab()
  e.preventDefault(); e.stopImmediatePropagation()
  confirmDiscard().then(discard => {
    if (!discard || !_desktopOpen) return
    closeDesktopTab()
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  })
}, true)

// Our content area: one card like Sharkord's, and its save bar while there are unsaved changes
function buildDesktopPanel (mainClass, initial) {
  let saved = initial, draft = { ...initial }
  const panel = el('main', mainClass)
  panel.setAttribute(DESKTOP, 'panel')
  const wrap = el('div', 'mx-auto max-w-4xl space-y-6 p-4 md:p-6')
  const card = el('div', 'bg-card text-card-foreground flex flex-col gap-6 rounded-xl border py-6 shadow-sm')
  const header = el('div', '@container/card-header grid auto-rows-min grid-rows-[auto_auto] items-start gap-1.5 px-6')
  header.append(el('div', 'leading-none font-semibold', 'Desktop Client'),
    el('div', 'text-muted-foreground text-sm', 'Options of this desktop client, kept on this computer.'))
  const content = el('div', 'px-6 space-y-4')
  card.append(header, content)
  wrap.append(card)

  const bar = el('div', 'pointer-events-none sticky bottom-4 z-20 px-4 md:px-6')
  const barInner = el('div', 'pointer-events-auto mx-auto flex max-w-4xl items-center justify-between gap-4 rounded-xl border bg-card px-4 py-3 shadow-lg')
  const save = el('button', BUTTON_CLASS, 'Save Changes')
  save.type = 'button'
  barInner.append(el('span', 'text-sm font-medium', 'You have unsaved changes'), save)
  bar.append(barInner)
  panel.append(wrap, bar)

  const switches = {}
  const render = () => {
    for (const [key, sw] of Object.entries(switches)) {
      const requires = DESKTOP_OPTIONS.find(o => o.key === key)?.requires
      sw.disabled = !(key in initial) || (!!requires && !draft[requires])
      sw.closest(`[${DESKTOP}=row]`)?.classList.toggle('opacity-50', !!requires && !draft[requires])
      const state = draft[key] ? 'checked' : 'unchecked'
      sw.setAttribute('aria-checked', String(!!draft[key]))
      sw.dataset.state = state
      sw.firstChild.dataset.state = state
    }
    bar.hidden = !changed().length
  }
  const changed = () => Object.keys(switches).filter(k => !!draft[k] !== !!saved[k])
  save.addEventListener('click', async () => {
    save.disabled = true; save.textContent = 'Saving...'
    const next = await ipcRenderer.invoke('desktop-settings-set', Object.fromEntries(changed().map(k => [k, !!draft[k]]))).catch(() => null)
    save.disabled = false; save.textContent = 'Save Changes'
    // A failed save keeps the draft, and the bar, to try again
    if (next) { saved = next; draft = { ...draft, ...Object.fromEntries(Object.keys(switches).map(k => [k, next[k]])) } }
    render()
  })

  // An option with only a note (the native share where it can't run here) shows greyed out, with
  // the note saying why. Options the main process leaves out get no row (nativeShare in a build
  // without the helper).
  for (const { key, label, description } of DESKTOP_OPTIONS.filter(({ key }) => key in initial || initial[key + 'Note'])) {
    const group = el('div', 'flex flex-col gap-2')
    group.setAttribute(DESKTOP, 'row')
    const text = el('div', 'flex flex-col')
    text.append(el('label', 'flex items-center gap-2 text-sm leading-none font-medium', label))
    if (description) text.append(el('span', 'text-sm text-muted-foreground', description))
    // What the startup probe found the GPU encodes: a check or a cross per codec
    const codecs = initial[key + 'Codecs']
    if (codecs) {
      const list = el('div', 'flex flex-wrap items-center gap-x-4 gap-y-1 pt-1 text-sm text-muted-foreground')
      list.append(el('span', '', 'This GPU can hardware encode:'))
      for (const [id, name] of [['h264', 'H.264'], ['av1', 'AV1']]) {
        const item = el('span', 'flex items-center gap-1')
        item.append(svg(codecs[id] ? CHECK_ICON : X_ICON), el('span', codecs[id] ? 'text-foreground' : '', name))
        item.title = codecs[id] ? name + ' shares use the GPU encoder' : name + ' shares use the browser\'s encoder'
        list.append(item)
      }
      text.append(list)
    }
    if (initial[key + 'Note']) text.append(el('span', 'text-sm text-muted-foreground', initial[key + 'Note']))
    const sw = el('button', SWITCH_CLASS)
    sw.type = 'button'
    sw.setAttribute('role', 'switch')
    sw.append(el('span', THUMB_CLASS))
    sw.addEventListener('click', () => { draft[key] = !draft[key]; render() })
    switches[key] = sw
    const control = el('div', 'flex flex-col gap-2')
    control.append(sw)
    group.append(text, control)
    content.append(group)
  }
  render()
  return { panel, dirty: () => changed().length > 0 }
}

let _clientControlsQueued = false
new MutationObserver(() => {
  if (_clientControlsQueued) return
  _clientControlsQueued = true
  requestAnimationFrame(() => {
    _clientControlsQueued = false
    addChangeServerControls()
    addDesktopTab()
  })
}).observe(document, { childList: true, subtree: true })

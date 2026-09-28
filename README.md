# Sharkord Desktop Client

A desktop app for your self-hosted [Sharkord](https://github.com/Sharkord/sharkord) server. It
loads the real Sharkord web app, unchanged, and adds what a browser can't do.

## Why use it

- **Hardware-encoded screen sharing up to 4K at 60 fps**, in H.264 and AV1 — smooth, sharp and
  light on your CPU. 120 fps and more GPUs are next on the [roadmap](ROADMAP.md).
- **Screen share with sound** — pick single apps or the whole system on Linux (your own voice is
  never sent back), system audio on Windows.
- **Switch servers on the fly**, without reinstalling or editing files.
- **System notifications** for messages and DMs; click one to jump straight to that channel or DM.
- **Start at login** — Sharkord opens with your computer, straight into the tray if you like.
- **Minimize to tray** — closing the window keeps you connected; the tray icon brings it back.
- **Nothing to break** — everything works exactly as in the browser, because it *is* Sharkord's
  own web app.

**Where this has been tested:** screen sharing at 4K 60 fps (H.264 and AV1) from Windows 11 with an
AMD RX 9060 XT, using the native screen share, watched on a Fedora Linux laptop (AMD Ryzen 4000
graphics). Other GPUs and setups should work but haven't been verified yet — reports welcome.

---

## How to build

### Prerequisites

- **Node.js 22.12+** — https://nodejs.org (the current LTS; Electron 44 needs at least 22.12)
- **For the native screen share (Windows, optional):** Rust (the version pinned in
  `native/rust-toolchain.toml`, installed automatically by `rustup`), the
  [GStreamer 1.28 MSVC](https://gstreamer.freedesktop.org/download/) runtime **and** development
  files, and `pkg-config` (e.g. `choco install pkgconfiglite`). Without them the Windows installer
  still builds, just without the native share.

### 1 — Install dependencies

```sh
npm install
```

### 2 — Build

**Windows installer:**
```sh
npm run dist:win
```

This builds and stages the native share helper first (`npm run stage:native`, see
`scripts/stage-native.js`). CI sets `SHARKORD_REQUIRE_NATIVE=1` so a missing helper fails the
build instead of being skipped.

**Linux packages (AppImage + deb + rpm):**
```sh
npm run dist:linux
```

Output goes to `release/`. Building deb/rpm on Fedora needs `libxcrypt-compat` (for
electron-builder's bundled fpm) and `rpm-build`.

The build isn't tied to a server: on first launch the app asks for your server URL and
saves it in the user's `settings.json` (in Electron's userData folder).

To switch servers later, open Sharkord's **☰** menu (next to the server name) and
choose **Change server**, or use the **Change server** button on the login screen.
Shortcuts: `Ctrl+Shift+O`, or the tray icon → **Change Server…**. If the server
can't be reached, the error page has **Retry** and **Change server** buttons.

### Branches and releases

Work happens on feature branches that are merged into `dev` through pull requests. GitHub
Actions (`.github/workflows/build.yml`) builds the Windows installer and the Linux packages on
native runners for every PR into `dev` or `main` and every push to `dev`, and uploads them as
workflow artifacts. The Windows job installs GStreamer and builds the native helper.

**Versioning:** [SemVer](https://semver.org), independent of the Sharkord server's version (the
client loads whatever web app the server serves). While Sharkord is in alpha the client stays
at `0.x`: **minor** for new features, **patch** for fixes.

**Releasing:**

1. Merge `dev` into `main` (a PR from `dev`, or `git switch main && git merge --ff-only dev && git push`).
2. Actions → **Build** → **Run workflow** on `main`, and pick the bump: `patch`, `minor`,
   `major`, or `none` to release the current `package.json` version.

The run builds everything with the new version and, only if every build passes, commits
`Release vX.Y.Z` to `main`, tags it, publishes the GitHub Release (downloads table + the commit
list since the previous tag) and merges the release commit back into `dev`. For a pre-release,
set a version like `0.2.0-beta.1` in `package.json` and release it with `none`; versions with a
`-` are marked as pre-releases.

Windows builds aren't code-signed, so SmartScreen may warn on first run (**More info → Run anyway**).

---

## How to use it

| Feature | How |
|---------|-----|
| **Hardware screen sharing** | In Sharkord's Devices settings pick **H264** or **AV1** and turn **Simulcast off** (simulcast forces VP8, which most GPUs can't encode). Linux uses VA-API, Windows Media Foundation |
| **Native screen share (Windows, AMD, experimental)** | Turn it on in **Settings → Others → Native screen share (experimental)** or the tray. A small helper (`native/`, Rust + GStreamer) captures the whole screen and encodes it on the GPU (AMD AMF); its frames go through Sharkord's own connection, so Sharkord itself is unchanged. The bitrate follows the network, capped by Sharkord's bitrate slider. Window shares, VP8/VP9 and simulcast use the normal path; if the helper fails mid-share, the share ends cleanly and the next one uses the normal path |
| **Screen share picker** | Choose a screen or window, then the audio. On Wayland the system portal picks the source and the picker opens on the audio step |
| **Share audio** | Linux: per-app or entire-system audio through PipeWire ([venmic](https://github.com/Vencord/venmic)). Windows: system loopback ("Stream With Audio") |
| **Notifications** | Turn them on in Sharkord's **Settings → Notifications**. The taskbar flashes until you come back, and a click also brings the window back from the tray |
| **Change server** | Sharkord's **☰** menu → **Change server**, the button on the login screen, `Ctrl+Shift+O`, or the tray menu |
| **Tray and startup** | At the top of Sharkord's **Settings → Others**, or in the tray menu: **Open at login** (starts minimized in the tray when **Minimize to tray** is on) and **Minimize to tray** (X keeps Sharkord running; quit from the tray, even during a call). Launching Sharkord again brings the running window to the front |
| **DevTools** | `Ctrl+Shift+I` |

---

## Where it's going

[ROADMAP.md](ROADMAP.md) lists every planned improvement with a priority and status. The top of
the list:

- **Never break a share, on any hardware:** a clean fallback for every capture failure, and
  always-on logs plus a "Copy diagnostics" button for bug reports.
- **More GPUs for the native share:** NVIDIA (NVENC) and Intel (Quick Sync), chosen by a startup
  probe of what the machine can really encode.
- **4K at 120 fps:** lift the remaining frame-rate limits (WebRTC's default 60 fps cap per
  sender, capture on high-refresh screens) and test on a 120 Hz display.
- **A safer normal path on Windows:** hardware encoding up to 4K on NVIDIA and Intel, the
  experimental Chromium flags only where they help, and a switch to turn them off.
- **Better quality per bit:** H.264 High profile (the server already offers it), and a faster
  bitrate ramp at the start of a share.
- **Linux:** measure sharing from Linux first, then decide whether it needs a native path too.

---

## Project structure

```
sharkord-client/
├── package.json
├── AGENTS.md              ← how the code is organized (for contributors and AI agents)
├── ROADMAP.md             ← planned improvements, prioritised
├── .github/workflows/
│   └── build.yml          ← CI: build installers; manual run on main publishes a release
├── electron/
│   ├── main.js            ← window, tray, GPU flags, screen-share picker, venmic audio, native share helper
│   ├── preload.js         ← bridge, getDisplayMedia hooks (share audio, native share), injected settings
│   ├── first-launch.html  ← server URL prompt (first run / Change Server)
│   ├── unreachable.html   ← shown when the server can't be reached
│   ├── theme.css          ← Sharkord's design tokens for the two pages above
│   ├── picker.html        ← screen share picker (Sharkord look)
│   └── picker-preload.js  ← bridge for the picker window
├── native/                ← native screen share helper (Rust + GStreamer, Windows)
├── scripts/
│   └── stage-native.js    ← builds the helper and stages it with its GStreamer subset
└── build/
    ├── icon.png           ← Sharkord logo (window icon)
    ├── icon.ico           ← Windows installer/exe icon
    └── icons/             ← Linux icon sizes for packaging
```


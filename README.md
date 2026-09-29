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
graphics); and the native screen share from that laptop (Fedora 44 KDE Plasma, Wayland) at 1080p
60 fps in H.264. Other GPUs and setups should work but haven't been verified yet — reports welcome.

---

## How to build

### Prerequisites

- **Node.js 22.12+** — https://nodejs.org (the current LTS; Electron 44 needs at least 22.12)
- **For the native screen share (Windows, optional):** Rust (the version pinned in
  `native/rust-toolchain.toml`, installed automatically by `rustup`), the
  [GStreamer 1.28 MSVC](https://gstreamer.freedesktop.org/download/) runtime **and** development
  files, and `pkg-config` (e.g. `choco install pkgconfiglite`). Without them the Windows installer
  still builds, just without the native share.
- **For the native screen share (Linux, optional):** the same Rust, plus GStreamer's development
  packages (Fedora: `gstreamer1-devel gstreamer1-plugins-base-devel`; Debian/Ubuntu:
  `libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev pkg-config`). Without them the packages
  still build, just without the helper.

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
electron-builder's bundled fpm) and `rpm-build`. This also builds the native share helper; on
Linux it ships alone and uses the system's GStreamer (see the feature table for what to install).

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
workflow artifacts. Both jobs install GStreamer and build the native helper (the Linux job on
Ubuntu 22.04, so the helper runs on older glibc too).

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
| **Hardware screen sharing** | In Sharkord's Devices settings pick **H264** or **AV1** and turn **Simulcast off** (with simulcast on, and allowed by the server, Sharkord shares VP8, which most GPUs can't encode). Linux uses VA-API, Windows Media Foundation |
| **Native screen share (Windows AMD, Linux Wayland; experimental)** | Turn it on in **Settings → Desktop Client → Native screen share (experimental)** or the tray. A small helper (`native/`, Rust + GStreamer) captures the screen and encodes it on the GPU (AMF on Windows, VA-API on Linux); its frames go through Sharkord's own connection, so Sharkord itself is unchanged. The bitrate follows the network, capped by Sharkord's bitrate slider. It's used only when **H264** or **AV1** is picked in Sharkord's Devices settings with **Simulcast off**, **and** the GPU can encode it (checked at startup; the tab lists what it found, and the switch is greyed out when it can encode neither). Every other share (VP8, VP9, auto, simulcast, a codec the GPU lacks) is the normal Chromium share. On Windows it takes whole screens (window shares stay normal) and a helper failure before the first frame falls back to the normal share; on Linux the helper shows the only portal dialog (screens or windows) and Sharkord's preview shows its frames. A helper failure mid-share ends the share on both, and later shares that session are normal ones. Where it can't be used (or only for one codec) Settings says why under its switch. **Linux needs** a Wayland session (a clean Fedora 44 Workstation or KDE install already has the rest, except H.264 encoding on AMD; the rpm and deb recommend the GStreamer plugins), GStreamer 1.22+ with its `va` and PipeWire plugins (Fedora: `gstreamer1-plugins-bad-free pipewire-gstreamer`; Debian/Ubuntu: `gstreamer1.0-plugins-bad gstreamer1.0-pipewire`) and a VA-API driver that encodes (Fedora's own Mesa has no H.264 encoding: `mesa-va-drivers-freeworld` from RPM Fusion; Intel's driver, `libva-intel-media-driver` on Fedora, is installed by default) |
| **Screen share picker** | Choose a screen or window, then the audio. On Wayland the system portal picks the source and the picker opens on the audio step |
| **Share audio** | Linux: per-app or entire-system audio through PipeWire ([venmic](https://github.com/Vencord/venmic)). Windows: system loopback ("Stream With Audio") |
| **Notifications** | Turn them on in Sharkord's **Settings → Notifications**. The taskbar flashes until you come back, and a click also brings the window back from the tray |
| **Change server** | Sharkord's **☰** menu → **Change server**, the button on the login screen, `Ctrl+Shift+O`, or the tray menu |
| **Tray and startup** | In the **Desktop Client** tab of Sharkord's user settings (saved with **Save Changes**, like Sharkord's own tabs), or in the tray menu: **Open at login**, **Start minimized** (a login launch starts in the tray; only with Open at login) and **Minimize to tray** (X keeps Sharkord running; quit from the tray, even during a call). Launching Sharkord again brings the running window to the front |
| **DevTools** | `Ctrl+Shift+I` |

---

## Where it's going

[ROADMAP.md](ROADMAP.md) lists every planned improvement with a priority and status. The top of
the list:

- **Never break a share, on any hardware:** a clean fallback for every capture failure, and
  always-on logs plus a "Copy diagnostics" button for bug reports.
- **More GPUs for the native share:** NVIDIA (NVENC) and Intel (Quick Sync) on Windows, where
  only AMD is supported today.
- **4K at 120 fps:** lift the remaining frame-rate limits (WebRTC's default 60 fps cap per
  sender, capture on high-refresh screens) and test on a 120 Hz display.
- **A safer normal path on Windows:** hardware encoding up to 4K on NVIDIA and Intel, the
  experimental Chromium flags only where they help, and a switch to turn them off.
- **Better quality per bit:** H.264 High profile (the server already offers it), and a faster
  bitrate ramp at the start of a share.
- **Linux:** the native share on NVIDIA (NVENC) and X11, which use the normal path today.

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
│   ├── preload.js         ← bridge, getDisplayMedia hooks (share audio, native share), Desktop Client settings tab
│   ├── first-launch.html  ← server URL prompt (first run / Change Server)
│   ├── unreachable.html   ← shown when the server can't be reached
│   ├── theme.css          ← Sharkord's design tokens for the two pages above
│   ├── picker.html        ← screen share picker (Sharkord look)
│   └── picker-preload.js  ← bridge for the picker window
├── native/                ← native screen share helper (Rust + GStreamer; Windows, Linux Wayland)
├── scripts/
│   └── stage-native.js    ← builds the helper and stages it (Windows: with its GStreamer subset)
└── build/
    ├── icon.png           ← Sharkord logo (window icon)
    ├── icon.ico           ← Windows installer/exe icon
    └── icons/             ← Linux icon sizes for packaging
```


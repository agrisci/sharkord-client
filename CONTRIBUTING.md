# Contributing

How to build, test and release the Sharkord desktop client. [AGENTS.md](AGENTS.md) explains how the
code is organized, its rules, and the manual test checklist; [ROADMAP.md](ROADMAP.md) what's planned.

## Prerequisites

- **Node.js 22.12+** (the current LTS; Electron 44 needs at least 22.12).
- **For the native screen share (optional):** Rust, the version pinned in
  `native/rust-toolchain.toml` (installed automatically by `rustup`).
  - **Windows:** what the helper's own FFmpeg is built with (`scripts/deps/ffmpeg-windows.sh`):
    Visual Studio 2022 (or its Build Tools) with the C++ workload, [MSYS2](https://www.msys2.org/)
    (`winget install MSYS2.MSYS2`, then in its shell `pacman -S make diffutils git pkgconf`), LLVM
    for bindgen (`winget install LLVM.LLVM`), and `pkg-config` (e.g. `choco install pkgconfiglite`).
  - **Linux:** what the helper's own FFmpeg and Mesa drivers are built with (`scripts/deps/`):
    cmake, meson 1.4+, ninja, clang, PipeWire's development files, and LLVM 15+ with libclc and the
    SPIR-V translator. On Ubuntu 22.04 `scripts/deps/ubuntu-packages.sh` installs them; anywhere
    with podman, `scripts/deps/container.sh ffmpeg mesa` builds them in an Ubuntu 22.04 container.

  Without them the installers still build, just without the native share.

## Build and run

```sh
npm install
npm start              # run the app (DevTools: Ctrl+Shift+I)
npm run dist:win       # Windows installer                  → release/
npm run dist:linux     # AppImage + deb + rpm               → release/
npm run stage:native   # build and stage the native helper only (dist:* does this first)
```

- `dist:*` builds and stages the native share helper first (`scripts/stage-native.js`); that also
  builds its FFmpeg (and on Linux the Mesa drivers) into `build/deps/` the first time. CI sets
  `SHARKORD_REQUIRE_NATIVE=1` so a missing helper fails the build instead of being skipped.
- For `npm start` alone, build the helper with `cd native && cargo build --release`, after
  `scripts/deps/ffmpeg.sh` and `mesa.sh` (Linux) or `C:\msys64\usr\bin\bash.exe
  scripts/deps/ffmpeg-windows.sh` (Windows).
- Building deb/rpm on Fedora needs `libxcrypt-compat` (for electron-builder's bundled fpm) and
  `rpm-build`.
- The build isn't tied to a server: the server's address is asked on first launch and saved in
  `settings.json` in the settings folder.

There are no automated tests besides the helper's (`cd native && cargo test`, and with a GPU
`cargo test -- --ignored`): after a change, go through the relevant parts of the manual test
checklist in AGENTS.md.

## Branches, CI and releases

- Work on a feature branch off `dev`, and open a pull request into `dev` once it's ready to merge.
- GitHub Actions (`.github/workflows/build.yml`) builds the Windows installer and the Linux
  packages for every PR into `dev` or `main` and every push to `dev`, and uploads them as workflow
  artifacts. A newer push to the same PR or to `dev` cancels the run still going for the older
  commit; release runs are never cancelled. The Linux job builds on Ubuntu 22.04, so the app runs on
  glibc 2.35 and newer; the helper's FFmpeg and Mesa are cached between runs.

**Versioning:** [SemVer](https://semver.org), independent of the Sharkord server's version (the
client loads whatever web app the server serves). While Sharkord is in alpha the client stays at
`0.x`: **minor** for new features, **patch** for fixes.

**Releasing:**

1. Merge `dev` into `main` (a pull request from `dev`, merged with a merge commit).
2. Actions → **Build** → **Run workflow** on `main`, and pick the bump: `patch`, `minor`, `major`,
   or `none` to release the current `package.json` version.

The run builds everything with the new version and, only if every build passes, commits
`Release vX.Y.Z` to `main`, tags it, publishes the GitHub Release (a downloads table and the commit
list since the previous tag, so keep commit subjects readable) and merges the release commit back
into `dev`. For a pre-release, set a version like `0.2.0-beta.1` in `package.json` and release it
with `none`; versions with a `-` are marked as pre-releases.

## Project structure

```
sharkord-client/
├── package.json
├── AGENTS.md              ← how the code is organized, rules, manual test checklist
├── ROADMAP.md             ← planned improvements, prioritised
├── .github/workflows/
│   └── build.yml          ← CI: build installers; a manual run on main publishes a release
├── electron/
│   ├── main.js            ← window, tray, Chromium flags, screen-share picker, venmic audio, native share helper
│   ├── preload.js         ← bridge, getDisplayMedia hooks (share audio, native share), Desktop Client settings tab
│   ├── first-launch.html  ← server URL prompt (first run / Change Server)
│   ├── unreachable.html   ← shown when the server can't be reached
│   ├── theme.css          ← Sharkord's design tokens for the two pages above
│   ├── picker.html        ← screen share picker (Sharkord look)
│   └── picker-preload.js  ← bridge for the picker window
├── native/                ← native screen share helper (Rust + FFmpeg; Windows: DXGI, Linux Wayland: PipeWire)
├── scripts/
│   ├── stage-native.js    ← builds the helper and stages it (Linux: with Mesa drivers)
│   └── deps/              ← builds the helper's FFmpeg (and on Linux Mesa drivers)
└── build/
    ├── icon.png           ← Sharkord logo (window icon)
    ├── icon.ico           ← Windows installer/exe icon
    └── icons/             ← Linux icon sizes for packaging
```

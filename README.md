# Sharkord Desktop Client

A desktop app for your self-hosted [Sharkord](https://github.com/Sharkord/sharkord) server. It
loads the real Sharkord web app, unchanged, and adds what a browser can't do.

## Why use it

- **Screen sharing up to 4K at 60 fps**, in H.264 and AV1, encoded by the graphics card, not the CPU.
- **Screen share with sound**: pick single apps or the whole system on Linux (your own voice is
  never sent back), system audio on Windows.
- **Switch servers on the fly**, without reinstalling or editing files.
- **System notifications** for messages and DMs; click one to jump straight to that channel or DM.
- **Start at login**: Sharkord opens with your computer, straight into the tray if you like.
- **Minimize to tray**: closing the window keeps you connected; the tray icon brings it back.
- **Automatic updates**: new versions download in the background; click the green arrow next to
  the ☰ menu to install.
- **Works like the browser**: it runs Sharkord's own web app.

## Download and install

Get the latest version from the [Releases](https://github.com/agrisci/sharkord-client/releases)
page.

| System | Download | Install |
|---|---|---|
| **Windows 10 / 11** | `Sharkord-<version>-x64.exe` | Run it. The installer isn't code-signed yet, so Windows SmartScreen may warn the first time: **More info → Run anyway** |
| **Fedora, openSUSE** | `Sharkord-<version>-x86_64.rpm` | Open it with your software center, or `sudo dnf install ./Sharkord-<version>-x86_64.rpm` |
| **Ubuntu 22.04+, Debian 12+, Linux Mint** | `Sharkord-<version>-amd64.deb` | Open it with your software center, or `sudo apt install ./Sharkord-<version>-amd64.deb` |
| **Any other Linux** | `Sharkord-<version>-x86_64.AppImage` | Make it executable and run it, from a folder you can write to (updates replace the file). Rename it to `Sharkord.AppImage` if you make a shortcut to it: updates then keep the file name |

On first launch Sharkord asks for your server's address (e.g. `https://sharkord.example.com`).

## How to use it

| Feature | How |
|---|---|
| **Change server** | Sharkord's **☰** menu (next to the server name) → **Change server**, the button on the login screen, `Ctrl+Shift+O`, or the tray menu. If the server can't be reached, the error page has **Retry** and **Change server** buttons |
| **Hardware screen sharing** | In Sharkord's **Devices** settings pick **H264** or **AV1** and turn **Simulcast off** (with simulcast on, Sharkord shares VP8, which most graphics cards can't encode) |
| **Native screen share** | On by default with AMD graphics on Windows and with AMD and Intel on Linux (Wayland); with NVIDIA and Intel on Windows (not tested yet) turn it on in **Settings → Desktop Client → Native screen share**, or from the tray. The app then captures and encodes the screen on the graphics card itself, for a steadier frame rate and less load than the browser's share. It's used for H.264 and AV1 shares with simulcast off; any other share works as before. Under the switch, Settings shows which codecs your graphics card can encode, or why the option isn't available |
| **Hardware encoding for other shares** | **Settings → Desktop Client**: shares that don't use the native share are encoded on the graphics card too (after a restart). On by default on Windows, off on Linux, where some drivers produce streams viewers can't play |
| **Screen share picker** | Choose a screen or window, then the audio. On Linux the system's own screen-sharing dialog picks the source first |
| **Share audio** | Linux: single apps or the entire system. Windows: "Stream With Audio" shares the system's sound |
| **Notifications** | Turn them on in Sharkord's **Settings → Notifications**. They come whenever Sharkord isn't the focused window (minimized, in the tray or behind another app), for the channel you have open too, and not while you're using it. The taskbar flashes until you come back, and clicking a notification opens that channel or DM, even from the tray |
| **Tray and startup** | **Settings → Desktop Client** (confirm with **Save Changes**), or the tray menu: **Open at login**, **Start minimized** (starts in the tray at login) and **Minimize to tray** (the X keeps Sharkord running; quit from the tray). Opening Sharkord again brings the running window to the front |
| **Diagnostics** | **Settings → Desktop Client → Save diagnostics…** writes one text file for a bug report, with your server's address and user name left out (see *Reporting a problem*) |

### Native screen share: what's supported

| | Windows | Linux |
|---|---|---|
| **Graphics cards** | AMD; NVIDIA and Intel built in but not tested yet | AMD and Intel; NVIDIA with NVIDIA's own driver |
| **Session** | any | Wayland (on X11 the normal share is used) |
| **What you can share** | screens and windows | screens and windows |
| **Anything to install** | no | no: the app brings what it needs, even where the distribution leaves H.264 out (Fedora, openSUSE) |

While the screen is still, the native share keeps sending at close to its full bitrate, so the
picture is sharp the moment something moves again. A still screen therefore uses as much upload
(and download for each viewer) as a moving one.

### Tested setups

- 4K at 60 fps (H.264 and AV1) from Windows 11 and from Fedora 44 KDE with an AMD Radeon RX 9060 XT,
  watched on a Fedora laptop with AMD Ryzen 4000 graphics: smooth, no freezes.
- 1080p at 60 fps from that laptop (Fedora 44 KDE, Wayland).
- Clean Fedora 44 installs (KDE and GNOME): the app installs and runs with nothing else added.

Other graphics cards and setups should work but haven't been verified yet. Reports are welcome:
please open an issue.

## Reporting a problem

**No notifications?** Check that they are on in Sharkord's **Settings → Notifications**, that your
system isn't in Do Not Disturb, and that your desktop's notification settings allow Sharkord. The app
can't see those last two, so its log shows such a notification as shown. While Sharkord's window is
focused none are shown on purpose: a red dot on the tray icon (and on Windows the taskbar button)
marks one that came while you were away, until you come back.

Open **Settings → Desktop Client** and click **Save diagnostics…** in the Diagnostics card, then
attach the saved `.txt` to your issue. It holds the versions of the client and its browser engine,
your system and graphics hardware, the Desktop Client settings, the state of the window and of
notifications, and the app's own log from this run and the previous one. The log follows what the
app did: notifications shown and clicked, the connection to the server dropping and coming back,
settings changed, the tray, screen shares and errors. Nothing in it identifies you: there are no
usernames, messages, notification texts, channel names or account data, and your server's address, home folder and user name are replaced with `<server>`, `<home>`
and `<user>`. It is plain text, so you can read it before attaching it.

If the app won't start, the raw logs are `logs/main.log` (and `main.old.log`, the previous run) in
the settings folder, `~/.config/sharkord` on Linux or `%APPDATA%\sharkord` on Windows; they never
contain your server's address or paths with your user name either. `Ctrl+Shift+I` opens the
developer tools, if you're asked for the console.

## What's next

[ROADMAP.md](ROADMAP.md) lists every planned improvement. At the top: the native screen share on
NVIDIA and Intel for Windows, and 4K at 120 fps.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) covers building the app and releasing it;
[AGENTS.md](AGENTS.md) explains how the code is organized.

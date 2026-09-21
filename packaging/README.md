# Installing

Two binaries: `dvs` (the CLI and MCP server) and `dvs-studio` (the window). Both need
`ffmpeg` and `ffprobe` at run time; the studio also needs a system webview.

## Runtime dependencies

| | Linux | macOS |
|---|---|---|
| Required | `ffmpeg` | `ffmpeg` |
| Studio window | `webkit2gtk-4.1`, `gtk3`, `libsoup3` | nothing extra — WKWebView ships with the OS |
| Interop tests | `mlt` (`melt`) | `mlt` |
| Optional | `piper` for offline TTS | `piper` |

```bash
# Arch
sudo pacman -S ffmpeg webkit2gtk-4.1 gtk3 libsoup3 mlt
# Debian / Ubuntu
sudo apt install ffmpeg libwebkit2gtk-4.1-0 libgtk-3-0 libsoup-3.0-0 melt
# macOS
brew install ffmpeg mlt
```

Building additionally needs the `-dev`/`-devel` packages of the webkit, gtk and soup
libraries on Linux (`libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev` on Debian;
the Arch packages above already carry their headers).

## From source

```bash
cargo install --path crates/dvs-cli      # `dvs`
cargo install --path crates/dvs-studio   # `dvs-studio`
dvs doctor                               # reports the toolchain and what is missing
```

Both land in `~/.cargo/bin`.

## Desktop entry (Linux)

```bash
install -Dm644 packaging/dvs-studio.desktop ~/.local/share/applications/dvs-studio.desktop
for size in 32 128; do
  install -Dm644 "crates/dvs-studio/icons/${size}x${size}.png" \
    "$HOME/.local/share/icons/hicolor/${size}x${size}/apps/dvs-studio.png"
done
install -Dm644 crates/dvs-studio/icons/icon.svg \
  ~/.local/share/icons/hicolor/scalable/apps/dvs-studio.svg
update-desktop-database ~/.local/share/applications 2>/dev/null || true
```

The entry declares `MimeType=inode/directory`, so a project folder can be dropped on it; it
deliberately does not claim `video/*`, which would put a timeline editor in the "open with"
list for every mp4 on the machine. It validates clean under `desktop-file-validate`.

## wlroots compositors

WebKitGTK's DMABUF renderer can fail on Hyprland and Sway with
`Error 71 (Protocol error) dispatching to Wayland display`:

```bash
WEBKIT_DISABLE_DMABUF_RENDERER=1 dvs-studio
```

Put it in the desktop entry's `Exec=` line, or export it from your session, if your
compositor needs it. It is a WebKitGTK/compositor interaction, not something this
application can fix from the inside.

## Bundles

`bundle.active` is **`false`** in `crates/dvs-studio/tauri.conf.json`, so no `.deb`,
`.AppImage`, `.dmg` or `.msi` is produced today, and none is published. `cargo install` and
the desktop entry above are the supported install path.

Turning bundling on is a deliberate step rather than a flag flip: it pulls in the Tauri CLI,
per-format tooling (`dpkg-deb`, `linuxdeploy`, `create-dmg`), an `.icns`/`.ico` icon set
beyond the PNGs in `crates/dvs-studio/icons/`, and — for anything a stranger should run — a
signing identity and notarisation on macOS. None of that is wired up, and claiming otherwise
in a README is how "just download the release" turns into a Gatekeeper dialog.

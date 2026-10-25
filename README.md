# ScreenMirror

Mirror the laptop screen onto a second screen on KDE Plasma (Wayland). The system-tray menu has three modes:

- **Full** (default). The whole laptop screen is stretched to fill the second screen. Aspect ratio is ignored.
- **Zoom**. Drag out a region on the laptop screen. That region is then stretched to fill the second screen.
- **Black**. The second screen is solid black.

Written in Rust using only the standard library.

## Requirements

Kubuntu 26.04 (Plasma 6 on Wayland) with a second output. KWin has to offer `zwlr_layer_shell_v1`, `wp_viewporter`, and `org.kde.KWin.ScreenShot2`. The session needs `XDG_MENU_PREFIX=plasma-` (startplasma sets this). Without that prefix KWin answers `NoAuthorized` to screen capture.

## Run

```sh
cargo build --release
./target/release/screenmirror --install
./target/release/screenmirror
```

`--install` writes `~/.local/share/applications/screenmirror.desktop` with `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2` and refreshes the KDE service cache. KWin only allows capture from an executable named by that desktop file.

```text
screenmirror [--fps N] [--no-cursor]
screenmirror --install [--autostart]
screenmirror --list-outputs
screenmirror --screenshot OUTPUT FILE.ppm
```

`SCREENMIRROR_SOURCE` and `SCREENMIRROR_TARGET` override output choice. By default the source is the internal panel (eDP, LVDS, or DSI), otherwise the first output, and the target is the other output.

## How it works

Capture is one `org.kde.KWin.ScreenShot2.CaptureScreen` call per frame. KWin writes raw pixels down a pipe; the reply carries width, height, stride, and the QImage format.

The mirror is a layer-shell overlay on the target output, anchored to every edge so it covers that screen, including any panel. Frames live in `wl_shm` buffers. `wp_viewporter` scales the buffer (or, in Zoom, the chosen rectangle) to the output size. Black mode attaches a 1×1 black buffer and stretches it.

The tray icon is an `org.kde.StatusNotifierItem`. Its context menu is `com.canonical.dbusmenu`, with radio items for the three modes. Choosing Zoom opens a dimmed overlay on the laptop screen; drag with the left button to pick the region, or press Esc or the right button to cancel.

## Test VM

`tests/vm/build-rootfs.sh` builds a Kubuntu 26.04 rootfs. `tests/vm/run-vm.sh` boots it under QEMU TCG with a two-head virtio-gpu. KVM on Cloud Agent hosts hits a kernel bug in `KVM_CREATE_VCPU`, and `-cpu max` makes llvmpipe take its AVX2 path, which never finishes drawing under TCG — the script uses `-cpu Nehalem`. Head 1 stays disconnected until `tests/vm/qmp.py resize`. The guest session must export `QT_QUICK_BACKEND=software` and `LIBGL_ALWAYS_SOFTWARE=1`, and the screen locker autolock has to be off; the rootfs build sets both.

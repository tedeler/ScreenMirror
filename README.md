# ScreenMirror

Mirror the screen that holds the KDE panel onto the other screen, on Plasma (Wayland). The source is whichever output the toolbar is on, not a connector name or a fixed output index. Move the panel to the other screen and the mirror follows. The system-tray menu has three modes:

- **Full** (default). The whole primary screen is shown on the secondary screen.
- **Zoom**. Drag out a region on the primary screen. That region is then shown on the secondary screen.
- **Black**. The secondary screen is solid black.

**Seitenverhältnis behalten** is on by default. While it is on, Full and Zoom keep the source aspect ratio: the whole source stays visible and any unused area on the secondary screen is black. While it is off, the image is stretched to fill the secondary screen and aspect ratio is ignored. Black stays solid black either way.

**Keine Spiegelung** removes the mirror and leaves the process and the tray running, the same as pressing Q while the mirror window is focused. **Auf diese Arbeitsfläche** moves the current mirror onto the virtual desktop that is current when the item is chosen, keeping the mode and any zoom region. If mirroring is off, that item starts Full on the current desktop.

Written in Rust using only the standard library.

## Requirements

Kubuntu 26.04 (Plasma 6 on Wayland) with a second output. KWin has to offer `zwlr_layer_shell_v1`, `wp_viewporter`, `wl_subcompositor`, and `org.kde.KWin.ScreenShot2`. The session needs `XDG_MENU_PREFIX=plasma-` (startplasma sets this). Without that prefix KWin answers `NoAuthorized` to screen capture.

## Run

```sh
cargo build --release
./target/release/screenmirror --install
./target/release/screenmirror
```

## Releases

Pushing a version tag (`v*`, for example `v0.1.0`) runs GitHub Actions: it tests, builds a Linux x86_64 release binary, and publishes a GitHub Release with
`screenmirror-<tag>-x86_64-unknown-linux-gnu.tar.gz` attached.

```sh
git tag v0.1.0
git push origin v0.1.0
```

## Debian package

`debian/` is a source package. With `dpkg-dev`, `debhelper`, `rustc`, and `cargo` installed:

```sh
dpkg-buildpackage -us -uc
sudo apt install ../screenmirror_0.1.0_amd64.deb
```

The build runs `cargo build --locked --release`. The `.deb` name follows `debian/changelog` and the host architecture (`screenmirror_0.1.0_amd64.deb` on amd64). It installs:

- `/usr/bin/screenmirror`, the release binary.
- `/usr/share/applications/screenmirror.desktop`, the entry KWin matches to allow `org.kde.KWin.ScreenShot2`. `Exec=/usr/bin/screenmirror` is a normal launch.
- `/etc/xdg/autostart/screenmirror.desktop`, system autostart for Plasma (`OnlyShowIn=KDE`, `Name=ScreenMirror`). `Exec=/usr/bin/screenmirror --no-mirror`, so the tray comes up and no mirror window is mapped until Full, Zoom, or Black is chosen. This file also sets `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2`, which authorizes the autostarted process.

The package Suggests `plasma-workspace`. `screenmirror --install [--autostart]` still writes a user-level desktop file. The package is the system install path.

`--install` writes `~/.local/share/applications/screenmirror.desktop` with `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2` and refreshes the KDE service cache. KWin only allows capture from an executable named by that desktop file.

```text
screenmirror [--fps N] [--no-cursor] [--no-mirror]
screenmirror --install [--autostart]
screenmirror --list-outputs
screenmirror --screenshot OUTPUT FILE.ppm
```

`--no-mirror` starts the tray without opening a mirror. Choose Full, Zoom, or Black to start.

`SCREENMIRROR_SOURCE` and `SCREENMIRROR_TARGET` override output choice. By default the source is the output whose index is the Plasma toolbar's `lastScreen` in `plasma-org.kde.plasma.desktop-appletsrc` (KWin's output order), and the target is the other output.

## How it works

The mirror is a fullscreen window on the target output, on the virtual desktop that is current when mirroring starts. Switch to another virtual desktop and that screen is a normal desktop again; switch back and the mirror returns. It is not placed on every desktop. **Auf diese Arbeitsfläche** closes that window and opens a new one, which KWin pins to the desktop that is current at that moment; the mode, including a zoom region, stays. Press **Q** while that mirror window is focused, or choose **Keine Spiegelung**, to remove it and leave ScreenMirror, including its tray icon, running. Q is not a global shortcut, so it still types in every other window. Choose Full, Zoom, or Black again to start on the desktop that is current then.

Capture is one `org.kde.KWin.ScreenShot2.CaptureScreen` call per frame. KWin writes raw pixels down a pipe; the reply carries width, height, stride, and the QImage format.

Frames live in `wl_shm` buffers. With aspect kept, the toplevel is a 1×1 black buffer stretched to the output, and a centered `wl_subsurface` uses `wp_viewporter` to scale the whole frame (or, in Zoom, the chosen rectangle) so it fits. The compositor scales; the unused area is the black parent, and the source is not cropped. With aspect ignored, `wp_viewporter` stretches that same buffer to the output size. Black mode attaches only the 1×1 black buffer. The window is an `xdg_toplevel` sent `set_fullscreen` for the target output. A layer-shell overlay would be shown on every virtual desktop, so the mirror is not one. Q is a normal key on that window: it stops the mirror only when the window has keyboard focus. An older build registered Q with KGlobalAccel; this one removes that registration on startup so the letter works again in the rest of the session.

The tray icon is an `org.kde.StatusNotifierItem`. Its context menu is `com.canonical.dbusmenu`, with radio items for the three modes and for **Keine Spiegelung**, a checkmark for **Seitenverhältnis behalten**, and an action **Auf diese Arbeitsfläche**. Choosing Zoom opens a dimmed overlay on the primary screen; drag with the left button to pick the region, or press Esc or the right button to cancel. While the mirror is stopped, **Keine Spiegelung** is checked and none of the three modes is.

## Test VM

`tests/vm/build-rootfs.sh` builds a Kubuntu 26.04 rootfs. `tests/vm/run-vm.sh` boots it under QEMU TCG with a two-head virtio-gpu. KVM on Cloud Agent hosts hits a kernel bug in `KVM_CREATE_VCPU`, and `-cpu max` makes llvmpipe take its AVX2 path, which never finishes drawing under TCG — the script uses `-cpu Nehalem`. Head 1 stays disconnected until `tests/vm/qmp.py resize`. The guest session must export `QT_QUICK_BACKEND=software` and `LIBGL_ALWAYS_SOFTWARE=1`, and the screen locker autolock has to be off; the rootfs build sets both.

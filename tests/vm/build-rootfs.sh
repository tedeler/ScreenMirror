#!/bin/bash
# Build the Kubuntu 26.04 (resolute) rootfs and disk image used by run-vm.sh.
#
# Host: Ubuntu with debootstrap, qemu-utils, and passwordless sudo.
# Result: $VM_DIR/kub26.img, vmlinuz, initrd.img.
#
# Confirmed requirements baked into the image (do not "simplify" these away):
#   - plasma-desktop is installed *with* its recommended packages. A
#     --no-install-recommends plasma-desktop leaves plasmashell crashing.
#   - The autologin session exports QT_QUICK_BACKEND=software and
#     LIBGL_ALWAYS_SOFTWARE=1. Without them plasmashell renders black and exits.
#   - kscreenlocker autolock is off. TCG is slow enough that the default
#     idle timeout blanks the session before a test can finish.
#   - XDG_MENU_PREFIX=plasma- is exported as well as left to startplasma.
#     KWin ScreenShot2 answers NoAuthorized when KSycoca has no plasma menu prefix.
set -euo pipefail
ROOT="${ROOT:-/srv/kub26}"
VM_DIR="${VM_DIR:-$HOME/vm}"
IMG="$VM_DIR/kub26.img"
MIRROR="${MIRROR:-http://archive.ubuntu.com/ubuntu}"

if [ "$(id -u)" -ne 0 ]; then
  exec sudo --preserve-env=ROOT,VM_DIR,MIRROR,HOME bash "$0" "$@"
fi

install -d "$VM_DIR"
if [ ! -e /usr/share/debootstrap/scripts/resolute ]; then
  ln -s gutsy /usr/share/debootstrap/scripts/resolute
fi

if [ ! -f "$ROOT/etc/os-release" ]; then
  echo "debootstrap resolute -> $ROOT"
  debootstrap --variant=minbase --components=main,universe resolute "$ROOT" "$MIRROR"
fi

cat > "$ROOT/etc/apt/sources.list" <<EOF
deb $MIRROR resolute main universe
deb $MIRROR resolute-updates main universe
deb http://security.ubuntu.com/ubuntu resolute-security main universe
EOF

# Public resolvers for the chroot. The host's resolv.conf is often 127.0.0.53,
# which is not reachable from a chroot. The image switches to systemd-resolved below.
printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > "$ROOT/etc/resolv.conf"
install -d "$ROOT/etc/dpkg/dpkg.cfg.d"
echo force-unsafe-io > "$ROOT/etc/dpkg/dpkg.cfg.d/force-unsafe-io"

mountpoint -q "$ROOT/proc" || mount --bind /proc "$ROOT/proc"
mountpoint -q "$ROOT/sys" || mount --bind /sys "$ROOT/sys"
mountpoint -q "$ROOT/dev" || mount --bind /dev "$ROOT/dev"
mountpoint -q "$ROOT/dev/pts" || mount --bind /dev/pts "$ROOT/dev/pts"

# Maintainer scripts must not start daemons inside the chroot.
cat > "$ROOT/usr/sbin/policy-rc.d" <<'EOF'
#!/bin/sh
exit 101
EOF
chmod 755 "$ROOT/usr/sbin/policy-rc.d"

chroot "$ROOT" bash -eu <<'CHROOT'
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  kwin-wayland plasma-workspace breeze breeze-icon-theme \
  dbus dbus-user-session libgl1-mesa-dri libegl-mesa0 fonts-dejavu-core \
  kde-spectacle libkf6service-bin qt6-wayland kde-cli-tools wayland-utils \
  xwayland linux-image-generic systemd-sysv udev openssh-server \
  libpam-systemd kmod iproute2 sudo python3 initramfs-tools zstd \
  systemd-resolved locales ca-certificates bash
# Recommends are required: plasmashell crashes without them.
apt-get install -y plasma-desktop
locale-gen en_US.UTF-8
update-locale LANG=en_US.UTF-8
CHROOT

# Drop the host resolver copy and let systemd-resolved own DNS at boot.
rm -f "$ROOT/etc/resolv.conf"
ln -s /run/systemd/resolve/stub-resolv.conf "$ROOT/etc/resolv.conf"

if ! chroot "$ROOT" id ubuntu >/dev/null 2>&1; then
  chroot "$ROOT" useradd -m -u 1000 -s /bin/bash ubuntu
fi
echo 'ubuntu:ubuntu' | chroot "$ROOT" chpasswd
chroot "$ROOT" usermod -aG sudo,video,render,input ubuntu || \
  chroot "$ROOT" usermod -aG sudo,video,input ubuntu
echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' > "$ROOT/etc/sudoers.d/ubuntu"
chmod 440 "$ROOT/etc/sudoers.d/ubuntu"

install -d -m 700 "$ROOT/home/ubuntu/.ssh"
if [ -f /home/ubuntu/.ssh/id_ed25519.pub ]; then
  cp /home/ubuntu/.ssh/id_ed25519.pub "$ROOT/home/ubuntu/.ssh/authorized_keys"
elif [ -n "${SUDO_USER:-}" ] && [ -f "/home/$SUDO_USER/.ssh/id_ed25519.pub" ]; then
  cp "/home/$SUDO_USER/.ssh/id_ed25519.pub" "$ROOT/home/ubuntu/.ssh/authorized_keys"
else
  echo "no id_ed25519.pub for the host user; generating one"
  sudo -u "${SUDO_USER:-ubuntu}" ssh-keygen -t ed25519 -N '' -f "/home/${SUDO_USER:-ubuntu}/.ssh/id_ed25519"
  cp "/home/${SUDO_USER:-ubuntu}/.ssh/id_ed25519.pub" "$ROOT/home/ubuntu/.ssh/authorized_keys"
fi
chmod 600 "$ROOT/home/ubuntu/.ssh/authorized_keys"
chroot "$ROOT" chown -R ubuntu:ubuntu /home/ubuntu

cat > "$ROOT/etc/fstab" <<'EOF'
/dev/vda / ext4 rw 0 1
workspace /workspace 9p trans=virtio,version=9p2000.L,msize=524288,nofail,x-systemd.automount 0 0
EOF
mkdir -p "$ROOT/workspace"

install -d "$ROOT/etc/systemd/system/getty@tty1.service.d"
cat > "$ROOT/etc/systemd/system/getty@tty1.service.d/override.conf" <<'EOF'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin ubuntu --noclear %I $TERM
EOF

install -d "$ROOT/etc/systemd/network"
cat > "$ROOT/etc/systemd/network/20-wired.network" <<'EOF'
[Match]
Name=en*

[Network]
DHCP=yes
EOF

systemctl enable --root="$ROOT" systemd-networkd.service systemd-resolved.service ssh.service
# plasma-desktop's recommends may pull SDDM in. The harness boots through
# getty autologin + startplasma-wayland, so a display manager must not claim the seat.
systemctl disable --root="$ROOT" sddm.service 2>/dev/null || true
systemctl set-default --root="$ROOT" multi-user.target
rm -f "$ROOT/usr/sbin/policy-rc.d"

# Session environment. startplasma sources ~/.config/plasma-workspace/env/*.sh
# and the tty1 login exports the same variables before exec.
install -d "$ROOT/home/ubuntu/.config/plasma-workspace/env"
cat > "$ROOT/home/ubuntu/.config/plasma-workspace/env/screenmirror.sh" <<'EOF'
export QT_QUICK_BACKEND=software
export LIBGL_ALWAYS_SOFTWARE=1
export XDG_MENU_PREFIX=plasma-
EOF
cat > "$ROOT/home/ubuntu/.bash_profile" <<'EOF'
if [ -z "${WAYLAND_DISPLAY:-}" ] && [ "$(tty)" = "/dev/tty1" ]; then
  export QT_QUICK_BACKEND=software
  export LIBGL_ALWAYS_SOFTWARE=1
  export XDG_MENU_PREFIX=plasma-
  echo "===== $(date -Is) startplasma-wayland =====" >> "$HOME/plasma.log"
  exec startplasma-wayland >> "$HOME/plasma.log" 2>&1
fi
EOF

# Screen locker autolock off, system-wide and for the user.
for f in "$ROOT/etc/xdg/kscreenlockerrc" "$ROOT/home/ubuntu/.config/kscreenlockerrc"; do
  install -d "$(dirname "$f")"
  cat > "$f" <<'EOF'
[Daemon]
Autolock=false
LockOnResume=false
Timeout=0
EOF
done

# Don't let PowerDevil DPMS blank the outputs during a long TCG test.
cat > "$ROOT/home/ubuntu/.config/powerdevilrc" <<'EOF'
[AC][Display]
DimDisplayWhenIdle=false
TurnOffDisplayWhenIdle=false
[AC][SuspendAndShutdown]
AutoSuspendAction=0
EOF

echo screenmirror > "$ROOT/etc/hostname"
cat > "$ROOT/etc/hosts" <<'EOF'
127.0.0.1 localhost
127.0.1.1 screenmirror
EOF
[ -s "$ROOT/etc/machine-id" ] || chroot "$ROOT" systemd-machine-id-setup
chroot "$ROOT" chown -R ubuntu:ubuntu /home/ubuntu

# Kernel the host boots with -kernel / -initrd.
KVER="$(chroot "$ROOT" bash -c 'ls -1 /lib/modules | sort | tail -1')"
cp -L "$ROOT/boot/vmlinuz-$KVER" "$VM_DIR/vmlinuz"
cp -L "$ROOT/boot/initrd.img-$KVER" "$VM_DIR/initrd.img"
echo "kernel $KVER"

umount "$ROOT/dev/pts" || true
umount "$ROOT/dev" || true
umount "$ROOT/sys" || true
umount "$ROOT/proc" || true

# Always rewrite the image so a re-run picks up chroot changes.
truncate -s 24G "$IMG"
mkfs.ext4 -F -L kub26 "$IMG"
MNT="$(mktemp -d)"
mount -o loop "$IMG" "$MNT"
tar -C "$ROOT" --one-file-system -cpf - . | tar -C "$MNT" -xpf -
umount "$MNT"
rmdir "$MNT"
echo "image $IMG"
ls -lh "$VM_DIR/vmlinuz" "$VM_DIR/initrd.img" "$IMG"

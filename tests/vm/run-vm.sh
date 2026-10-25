#!/bin/bash
# Boots a Kubuntu-26.04-like guest (Plasma 6 Wayland via startplasma-wayland on
# tty1, autologin) under QEMU with a two-head virtio-gpu, so KWin runs its
# real DRM backend with EGL (llvmpipe) - which KWin's ScreenShot2 needs.
#
# Expects VM_DIR to contain kub26.img (ext4 rootfs), vmlinuz and initrd.img.
# Host /workspace is shared into the guest at /workspace (9p).
#   head 0 ("laptop"):    VNC 127.0.0.1:5905, ${W0}x${H0}
#   head 1 ("projector"): VNC 127.0.0.1:5906, enabled with `qmp.py resize 5906 W H`
#   QMP: $QMP_SOCK, guest ssh: 127.0.0.1:2222
set -eu
VM_DIR="${VM_DIR:-$HOME/vm}"
QMP_SOCK="${QMP_SOCK:-/tmp/sm-qmp.sock}"
W0="${W0:-1920}" H0="${H0:-1200}"
# KVM on Cloud Agent VMs hits a host kernel BUG in KVM_CREATE_VCPU; default to TCG.
ACCEL="${ACCEL:-tcg,thread=multi}"
CPU="${CPU:-max}"

exec qemu-system-x86_64 -accel "$ACCEL" -cpu "$CPU" -smp 4 -m 6G \
    -kernel "$VM_DIR/vmlinuz" -initrd "$VM_DIR/initrd.img" \
    -append "root=/dev/vda rw console=ttyS0 quiet" \
    -drive file="$VM_DIR/kub26.img",format=raw,if=virtio \
    -device virtio-vga,id=video0,max_outputs=2,xres="$W0",yres="$H0" \
    -device virtio-tablet-pci -device virtio-keyboard-pci \
    -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=n0 \
    -virtfs local,path=/workspace,mount_tag=workspace,security_model=none,id=ws \
    -display none \
    -vnc 127.0.0.1:5,id=vnc0,display=video0,head=0 \
    -vnc 127.0.0.1:6,id=vnc1,display=video0,head=1 \
    -qmp unix:"$QMP_SOCK",server,nowait \
    -serial file:"${SERIAL_LOG:-/tmp/sm-serial.log}"

#!/usr/bin/env python3
"""Test helper for the QEMU guest started by run-vm.sh.

  qmp.py resize PORT W H        ask a VNC head for a size (enables QEMU head 1)
  qmp.py screendump HEAD FILE   PNG of what the guest scans out on HEAD
  qmp.py move X Y               absolute pointer position (0..1 of the whole desktop)
  qmp.py click [left|right]     press + release a button
"""
import json
import os
import socket
import struct
import sys
import time

QMP_SOCK = os.environ.get("QMP_SOCK", "/tmp/sm-qmp.sock")


def qmp(*commands):
    s = socket.socket(socket.AF_UNIX)
    s.connect(QMP_SOCK)
    f = s.makefile("rw")
    json.loads(f.readline())
    out = []
    for cmd in ({"execute": "qmp_capabilities"},) + commands:
        f.write(json.dumps(cmd) + "\n")
        f.flush()
        while True:
            r = json.loads(f.readline())
            if "return" in r or "error" in r:
                break
        if "error" in r:
            sys.exit(f"QMP error: {r['error']}")
        out.append(r["return"])
    return out[1:]


def recv_exact(s, n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise ConnectionError("VNC server closed connection")
        b += c
    return b


def resize(port, w, h):
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    recv_exact(s, 12)
    s.sendall(b"RFB 003.008\n")
    types = recv_exact(s, recv_exact(s, 1)[0])
    if 1 not in types:
        sys.exit(f"VNC security type None not offered: {list(types)}")
    s.sendall(b"\x01")
    if struct.unpack(">I", recv_exact(s, 4))[0] != 0:
        sys.exit("VNC security handshake failed")
    s.sendall(b"\x01")
    recv_exact(s, 20)
    recv_exact(s, struct.unpack(">I", recv_exact(s, 4))[0])
    encodings = [0, -308]
    s.sendall(struct.pack(">BxH", 2, len(encodings)) + b"".join(struct.pack(">i", e) for e in encodings))
    s.sendall(struct.pack(">BxHHBx", 251, w, h, 1) + struct.pack(">IHHHHI", 0, 0, 0, w, h, 0))
    time.sleep(1)
    s.close()


def main():
    cmd, args = sys.argv[1], sys.argv[2:]
    if cmd == "resize":
        resize(int(args[0]), int(args[1]), int(args[2]))
    elif cmd == "screendump":
        qmp({"execute": "screendump",
             "arguments": {"filename": os.path.abspath(args[1]), "device": "video0",
                           "head": int(args[0]), "format": "png"}})
    elif cmd == "move":
        x, y = (min(32767, int(float(v) * 32767)) for v in args[:2])
        qmp({"execute": "input-send-event", "arguments": {"events": [
            {"type": "abs", "data": {"axis": "x", "value": x}},
            {"type": "abs", "data": {"axis": "y", "value": y}}]}})
    elif cmd == "click":
        button = args[0] if args else "left"
        for down in (True, False):
            qmp({"execute": "input-send-event", "arguments": {"events": [
                {"type": "btn", "data": {"down": down, "button": button}}]}})
            time.sleep(0.1)
    elif cmd in ("press", "release"):
        qmp({"execute": "input-send-event", "arguments": {"events": [
            {"type": "btn", "data": {"down": cmd == "press", "button": args[0] if args else "left"}}]}})
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()

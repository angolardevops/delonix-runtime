#!/usr/bin/env python3
"""Drive a TrueNAS SCALE unattended install over the installer's own RPC API.

TrueNAS ships no answer-file mechanism, but its installer is a Python module
that already runs as a JSON-RPC server over WebSocket on port 8080 — the
stock `truenas-installer.service` in the live ISO is literally
`python3 -m truenas_installer --server`. So nothing in the ISO needs
repacking: boot it, forward the port, and call `install`.

Methods (read from the module inside the ISO, not guessed):
  system_info() -> {installation_running, installation_completed,
                    installation_error, version, efi}
  list_disks()  -> [{name, size, model, ...}]
  install({disks, set_pmbr, authentication, post_install?})
  reboot() / shutdown()

Progress arrives as `installation_progress` notifications on the same socket.
"""
import asyncio
import json
import sys

import websockets

URL = sys.argv[1] if len(sys.argv) > 1 else "ws://127.0.0.1:18080/ws"
DISK = sys.argv[2] if len(sys.argv) > 2 else "vda"
PASSWORD = sys.argv[3] if len(sys.argv) > 3 else "delonix-admin"


async def call(ws, method, params=None, timeout=120):
    """One JSON-RPC call, tolerating interleaved progress notifications."""
    req = {"jsonrpc": "2.0", "id": 1, "method": method}
    if params is not None:
        req["params"] = [params]
    await ws.send(json.dumps(req))
    while True:
        raw = await asyncio.wait_for(ws.recv(), timeout=timeout)
        msg = json.loads(raw)
        if msg.get("method") == "installation_progress":
            p = msg["params"][0]
            print(f"    [{p['progress']*100:5.1f}%] {p['message']}", flush=True)
            continue
        if "error" in msg:
            raise SystemExit(f"ERROR {method}: {msg['error']}")
        return msg.get("result")


async def connect(deadline_s=900):
    """Retry until the installer's RPC server actually answers a handshake.

    A plain TCP probe is useless here: QEMU's `hostfwd` accepts the connection
    whether or not anything listens inside the guest, so the port looks open
    from the first second of boot. Only a completed WebSocket handshake proves
    the installer is up.
    """
    waited = 0
    while True:
        try:
            return await websockets.connect(
                URL, max_size=None, ping_interval=None, open_timeout=10
            )
        except Exception as e:
            if waited >= deadline_s:
                raise SystemExit(f"installer RPC never answered after {waited}s: {e!r}")
            await asyncio.sleep(10)
            waited += 10
            if waited % 60 == 0:
                print(f"    still waiting for the installer ({waited}s)", flush=True)


async def main():
    ws = await connect()
    async with ws:
        info = await call(ws, "system_info")
        print(f"==> installer {info['version']} (efi={info['efi']})", flush=True)

        disks = await call(ws, "list_disks")
        names = [d["name"] for d in disks]
        print(f"==> disks: {names}", flush=True)
        if DISK not in names:
            raise SystemExit(f"target disk {DISK!r} not offered by the installer: {names}")

        print(f"==> installing onto {DISK}", flush=True)
        await call(
            ws,
            "install",
            {
                "disks": [DISK],
                # No protective MBR: the image is booted by SeaBIOS/OVMF as a
                # plain GPT disk, and set_pmbr is for legacy BIOS-only hosts.
                "set_pmbr": False,
                # `truenas_admin` is the account TrueNAS 24+ expects; `root`
                # is refused as a web login on current releases.
                "authentication": {"username": "truenas_admin", "password": PASSWORD},
            },
            timeout=3600,
        )
        print("==> install finished", flush=True)

        info = await call(ws, "system_info")
        print(f"==> completed={info['installation_completed']} error={info['installation_error']}", flush=True)
        if not info["installation_completed"]:
            raise SystemExit("installer did not report completion")

        # shutdown, not reboot: the qcow2 must be captured with nothing
        # writing to it, and a reboot would boot the ISO again.
        try:
            await call(ws, "shutdown", timeout=30)
        except (asyncio.TimeoutError, websockets.exceptions.ConnectionClosed):
            pass  # the box goes down mid-reply; that IS the success path
        print("==> shutdown requested", flush=True)


asyncio.run(main())

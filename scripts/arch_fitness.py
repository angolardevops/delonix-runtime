#!/usr/bin/env python3
"""Architecture fitness gate: the dependency direction of ADR-0040, enforced.

ADR-0040 fixes four layers and one direction:

    interfaces ─► contexts (domain + app) ◄─ adapters / providers
         │                                        ▲
         └──────────── composes (bins) ───────────┘

This gate exists BEFORE the crates move (phase P0), so the restructuring cannot
regress while it happens. It answers two questions, and they fail differently:

1. **Rules.** A dependency that the layer table forbids fails immediately, unless
   it is a declared EXCEPTION. Every exception names the phase that removes it —
   an exception without a phase is itself a failure, because that is how a
   temporary allowance becomes permanent.
2. **Ratchets.** Numbers that must not grow: how many library crates re-run the
   engine's own binary instead of calling a function, and how many of them print
   to stdout/stderr instead of using `tracing`. Same semantics as
   `lang_ratchet.py` — failing when the number RISES and when it FALLS without
   the baseline being lowered in the same commit. A `<=` would let the debt read
   as green forever.

    python3 scripts/arch_fitness.py            # check (exit 1 when misaligned)
    python3 scripts/arch_fitness.py --list     # show what each number counts
    python3 scripts/arch_fitness.py --update   # lower the baseline
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = Path(__file__).resolve().parent / "arch_baseline.json"

FOUNDATION, CONTEXT, ADAPTER, PROVIDER, INTERFACE, BIN = (
    "foundation",
    "context",
    "adapter",
    "provider",
    "interface",
    "bin",
)

# The layer of every crate, by its name TODAY. ADR-0040's D2 names the crates
# these become; this table is updated in the phase that renames each one, so the
# gate keeps working during the move instead of after it.
LAYERS = {
    "delonix-runtime-core": FOUNDATION,  # → delonix-model + delonix-state + delonix-telemetry (P3)
    "delonix-net-rules": FOUNDATION,  # → delonix-networking/domain (P2)
    "delonix-security-runtime": CONTEXT,  # → delonix-security (P2)
    "delonix-runtime": ADAPTER,  # → delonix-linux (P3)
    "delonix-net": ADAPTER,  # → delonix-sdn (P3)
    "delonix-image": ADAPTER,  # → delonix-oci (P3)
    "delonix-scan": ADAPTER,  # → delonix-scanner (P3)
    "delonix-volume": ADAPTER,  # → delonix-provider-mount (P4)
    "delonix-vm": ADAPTER,  # splits into the VmProvider port (context) + provider crates (P4)
    "delonix-proxmox": PROVIDER,  # → delonix-provider-proxmox (P4)
    "delonix-truenas": PROVIDER,  # → delonix-provider-truenas (P4)
    "delonix-cri": INTERFACE,
    "delonix-mgmt": INTERFACE,  # replaced by delonix-node-api (P5)
    "delonix-mcp": INTERFACE,
    "delonix-runtime-bin": BIN,  # → delonix-cli + bins/delonix (P2)
}

# Which layers each layer may depend on. The direction, in one place.
ALLOWED = {
    FOUNDATION: {FOUNDATION},
    CONTEXT: {FOUNDATION, CONTEXT},
    ADAPTER: {FOUNDATION, CONTEXT},
    PROVIDER: {FOUNDATION, CONTEXT},
    INTERFACE: {FOUNDATION, CONTEXT, ADAPTER, PROVIDER},
    BIN: {FOUNDATION, CONTEXT, ADAPTER, PROVIDER, INTERFACE},
}

# Dependencies that carry a runtime, a server or a CLI. A foundation or context
# crate that takes one of these has stopped being pure, whatever its name says.
HEAVY = {
    "tokio",
    "axum",
    "hyper",
    "hyper-util",
    "tonic",
    "reqwest",
    "clap",
    "ratatui",
    "serde_yaml",
    "rmcp",
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry-otlp",
    "tracing-opentelemetry",
    "prometheus-client",
}

# Each exception names the ADR-0040 phase that removes it. No phase = failure.
EXCEPTIONS = {
    ("dep", "delonix-vm", "delonix-net"): (
        "P3",
        "the VM engine reaches into the SDN holder directly; it moves behind the "
        "NetworkProvider port",
    ),
    ("dep", "delonix-proxmox", "delonix-vm"): (
        "P4",
        "the VmBackend port lives in the same crate as the Cloud Hypervisor and "
        "libvirt adapters; P4 moves the port into the compute context and each "
        "backend into its own provider crate",
    ),
    ("dep", "delonix-scan", "delonix-image"): (
        "P4",
        "the scanner reads layers through the OCI adapter instead of through an "
        "ImageStore port",
    ),
    ("roles", "delonix-runtime-bin", "3 interfaces"): (
        "P3",
        "one executable holds the CLI, the CRI server, the management API and the "
        "MCP server; each becomes its own binary",
    ),
    ("dep", "delonix-mcp", "delonix-mgmt"): (
        "P5",
        "the MCP server reuses the management API's collector; both move onto the "
        "application layer",
    ),
    ("heavy", "delonix-runtime-core", "opentelemetry"): (
        "P3",
        "telemetry leaves the foundation crate for delonix-telemetry",
    ),
    ("heavy", "delonix-runtime-core", "opentelemetry_sdk"): ("P3", "idem"),
    ("heavy", "delonix-runtime-core", "opentelemetry-otlp"): ("P3", "idem"),
    ("heavy", "delonix-runtime-core", "tracing-opentelemetry"): ("P3", "idem"),
    ("heavy", "delonix-runtime-core", "prometheus-client"): (
        "P3",
        "the metric registry leaves the foundation crate for delonix-telemetry",
    ),
}

# `Command::new` is legitimate in an adapter: that is what an adapter is for
# (`ip`, `nft`, `qemu-img`, `ssh`). What this counts is the engine re-running
# ITS OWN binary, which is the hidden cycle ADR-0040 removes — every one of these
# becomes a call into a use case, or a typed spec handed to delonix-launcher.
SELF_EXEC = re.compile(r"current_exe\(\)|\bcli_bin\(\)|\bdelonix_bin\(\)")
PRINTS = re.compile(r"\b(?:e?println!|print!)\s*[(\[]")


def crates() -> dict[str, Path]:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    meta = json.loads(out.stdout)
    return {p["name"]: p for p in meta["packages"]}


def rule_failures(pkgs: dict[str, dict]) -> tuple[list[str], set[tuple]]:
    """Layer and purity violations, and which exceptions were actually used."""
    bad: list[str] = []
    used: set[tuple] = set()
    # One long-running role per executable (ADR-0040 D2.4): a binary composes ONE
    # interface. Linking a second one is how the CLI ends up holding four servers
    # and the servers end up re-running the CLI.
    for name, pkg in sorted(pkgs.items()):
        if LAYERS.get(name) != BIN:
            continue
        ifaces = sorted(
            d["name"]
            for d in pkg["dependencies"]
            if d["kind"] is None and LAYERS.get(d["name"]) == INTERFACE
        )
        if len(ifaces) > 1:
            key = ("roles", name, f"{len(ifaces)} interfaces")
            if key in EXCEPTIONS:
                used.add(key)
            else:
                bad.append(f"{name}: composes {len(ifaces)} interfaces ({', '.join(ifaces)})")
    for name, pkg in sorted(pkgs.items()):
        layer = LAYERS.get(name)
        if layer is None:
            bad.append(f"{name}: crate not in the layer table (scripts/arch_fitness.py)")
            continue
        for dep in pkg["dependencies"]:
            if dep["kind"] is not None:  # dev/build dependencies do not carry the design
                continue
            dname = dep["name"]
            if dname in LAYERS:
                dlayer = LAYERS[dname]
                if dlayer not in ALLOWED[layer]:
                    key = ("dep", name, dname)
                    if key in EXCEPTIONS:
                        used.add(key)
                    else:
                        bad.append(f"{name} ({layer}) → {dname} ({dlayer}): forbidden direction")
            elif dname in HEAVY and layer in (FOUNDATION, CONTEXT):
                key = ("heavy", name, dname)
                if key in EXCEPTIONS:
                    used.add(key)
                else:
                    bad.append(f"{name} ({layer}) → {dname}: a runtime/server/CLI dependency")
    return bad, used


def count(pattern: re.Pattern[str], skip_bin: bool = True) -> tuple[int, list[str]]:
    total = 0
    where: list[str] = []
    for f in sorted((ROOT / "crates").rglob("*.rs")):
        rel = f.relative_to(ROOT).as_posix()
        if skip_bin and rel.startswith("crates/delonix-runtime-bin/"):
            continue
        n = len(pattern.findall(f.read_text(encoding="utf-8", errors="replace")))
        if n:
            total += n
            where.append(f"{rel}: {n}")
    return total, where


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--list", action="store_true", help="show what each number counts")
    ap.add_argument("--update", action="store_true", help="lower the baseline")
    args = ap.parse_args()

    pkgs = crates()
    bad, used = rule_failures(pkgs)

    # An exception with no phase is worse than the violation it covers.
    for key, (phase, reason) in EXCEPTIONS.items():
        if not phase:
            bad.append(f"exception {key} has no phase that removes it")
        if not reason:
            bad.append(f"exception {key} has no reason")
    for key in EXCEPTIONS:
        if key not in used:
            bad.append(f"exception {key} no longer applies — delete it from EXCEPTIONS")

    self_exec, self_where = count(SELF_EXEC)
    prints, print_where = count(PRINTS)
    current = {"self_exec_sites": self_exec, "library_prints": prints}

    if args.list:
        print("self-exec sites (a library crate re-running the engine's own binary):")
        for w in self_where:
            print(f"  {w}")
        print("\nlibrary prints (stdout/stderr instead of tracing):")
        for w in print_where:
            print(f"  {w}")
        print("\nexceptions still standing:")
        for (kind, a, b), (phase, reason) in sorted(EXCEPTIONS.items()):
            print(f"  [{phase}] {kind} {a} → {b}: {reason}")
        return 0

    if args.update:
        BASELINE.write_text(json.dumps(current, indent=2) + "\n", encoding="utf-8")
        print(f"baseline updated: {current}")
        return 0

    base = json.loads(BASELINE.read_text(encoding="utf-8"))
    rc = 0
    for name, value in sorted(current.items()):
        want = base.get(name)
        if want is None:
            print(f"FAIL  {name}: not in the baseline")
            rc = 1
        elif value > want:
            print(f"FAIL  {name}: {value} (baseline {want}) — new debt entered")
            rc = 1
        elif value < want:
            print(
                f"FAIL  {name}: {value} (baseline {want}) — debt was paid; lower the "
                f"baseline in the same commit (--update)"
            )
            rc = 1
        else:
            print(f"ok    {name}: {value}")

    for b in bad:
        print(f"FAIL  {b}")
        rc = 1
    if not bad:
        print(f"ok    layer direction: {len(pkgs)} crates, {len(EXCEPTIONS)} declared exceptions")
    return rc


if __name__ == "__main__":
    sys.exit(main())

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
import tomllib
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
    "delonix-state": ADAPTER,
    "delonix-telemetry": ADAPTER,
    "delonix-model": FOUNDATION,
    "delonix-stack": CONTEXT,
    "delonix-compute": CONTEXT,
    "delonix-node": CONTEXT,
    "delonix-net-rules": FOUNDATION,  # → delonix-networking/domain (P2)
    "delonix-security-runtime": CONTEXT,  # → delonix-security (P2)
    "delonix-linux": ADAPTER,
    "delonix-sdn": ADAPTER,
    "delonix-oci": ADAPTER,
    "delonix-scanner": ADAPTER,
    "delonix-volume": ADAPTER,  # → delonix-provider-mount (P4)
    "delonix-vm": ADAPTER,  # splits into the VmProvider port (context) + provider crates (P4)
    "delonix-proxmox": PROVIDER,  # → delonix-provider-proxmox (P4)
    "delonix-truenas": PROVIDER,  # → delonix-provider-truenas (P4)
    "delonix-opnsense": PROVIDER,  # ADR-0051: GatewayProvider, same P4 shape as delonix-proxmox
    "delonix-cri": INTERFACE,
    "delonix-mgmt": INTERFACE,  # replaced by delonix-node-api (P5)
    "delonix-mcp": INTERFACE,
    "delonix-runtime-bin": BIN,  # → delonix-cli (interfaces) + bins/delonix (P2)
    "delonix-mcp-bin": BIN,  # `delonix-mcp`, run by `delonix mcp` (ADR-0040 D2.4 amended)
    "delonix-mgmt-bin": BIN,  # `delonix-mgmt`, run by `delonix serve api`
    "delonix-node-api": INTERFACE,  # the node contract on the socket (ADR-0040 P5); replaces delonix-mgmt
    "delonix-node-api-bin": BIN,  # `delonix-node-api`, run by `delonix serve node-api`
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
    ("dep", "delonix-linux", "delonix-state"): (
        "P4a",
        "the StateRepository<Container> port landed and covers wait_and_record/stop/"
        "persist_stop/remove (ADR-0044 D6); spawn/create_with still open SecretStore "
        "directly (2 sites) and write_private_temp once for the AppArmor profile — a "
        "SecretVault port closes those, not yet built",
    ),
    ("dep", "delonix-vm", "delonix-state"): (
        "P4",
        "the adapter opens its record store directly; P4 hands it a StateRepository port from the composition root — "
        "AND writes 4 files (set_default_backend's marker, 3 libvirt XML sites including one reached from stop, "
        "not just create) with the state layer's atomic write; a ConfigWriter port closes those, scope mapped but "
        "not yet built (ADR-0044 D6 addendum, 2026-09-19)",
    ),
    ("dep", "delonix-sdn", "delonix-state"): (
        "P4",
        "the adapter writes its own files with the state layer's atomic write; P4 hands it the StateRepository port that owns those files",
    ),
    ("dep", "delonix-oci", "delonix-state"): (
        "P4",
        "the adapter writes its own files with the state layer's atomic write; P4 hands it the StateRepository port that owns those files",
    ),
    ("dep", "delonix-volume", "delonix-state"): (
        "P4",
        "the adapter writes its own files with the state layer's atomic write; P4 hands it the StateRepository port that owns those files",
    ),
    ("dep", "delonix-proxmox", "delonix-vm"): (
        "P4",
        "the VmBackend port lives in the same crate as the Cloud Hypervisor and "
        "libvirt adapters; P4 moves the port into the compute context and each "
        "backend into its own provider crate",
    ),
    ("dep", "delonix-opnsense", "delonix-sdn"): (
        "P4",
        "the GatewayProvider port lives in the same crate as the native nftables "
        "dataplane, by the same reasoning and the same exception as "
        "delonix-proxmox -> delonix-vm above (ADR-0051): P4 moves it into a "
        "context crate alongside VmBackend's own move, not before",
    ),
    ("dep", "delonix-scanner", "delonix-oci"): (
        "P4",
        "the scanner reads layers through the OCI adapter instead of through an "
        "ImageStore port",
    ),
    ("dep", "delonix-mcp", "delonix-mgmt"): (
        "P5",
        "the MCP server reuses the management API's collector; both move onto the "
        "application layer",
    ),
}

# `Command::new` is legitimate in an adapter: that is what an adapter is for
# (`ip`, `nft`, `qemu-img`, `ssh`). What this counts is the engine re-running
# ITS OWN binary, which is the hidden cycle ADR-0040 removes — every one of these
# becomes a call into a use case, or a typed spec handed to delonix-launcher.
SELF_EXEC = re.compile(r"current_exe\(\)|\bcli_bin\(\)|\bdelonix_bin\(\)")
PRINTS = re.compile(r"\b(?:e?println!|print!)\s*[(\[]")
# Writes to the PROCESS environment. Tests run on parallel threads, and a write
# there races every reader of the environment (libc's `getenv` takes no Rust lock);
# in the engine's own code a write is only sound in a single-threaded child. The
# remaining sites are the `delonix-sdn` test guard, the container init, and tests
# not yet moved to injected values — this number only goes down.
ENV_WRITES = re.compile(r"\benv::(?:set_var|remove_var)\s*\(")
# An adapter or provider importing the SHARED error as its own (ADR-0040 P3: errors
# per crate, each converting into the `DX_*` class the foundation owns). A crate
# with its own `Error` still names `delonix_model::Error` — in its `From`, not in a
# `use` that makes the shared type its result type.
SHARED_ERROR = re.compile(
    r"use\s+delonix_model::(?:\{[^}]*\b(?:Error|Result)\b[^}]*\}|(?:Error|Result)\b)"
)
SHARED_ERROR_DIRS = ("crates/adapters/", "crates/providers/")
# A `match`/`matches!` on a variant of the shared error, outside the foundation
# (ADR-0043 D4). A crate's error travels inside the shared class with its code, and
# `Err(Error::NotFound(_))` stops matching it without a word from the compiler.
# Ask `e.is_not_found()`/`e.class()`, or match on `e.root()` for a payload.
_VARIANTS = r"(?:NotFound|VmNotFound|NotRunning|Invalid|Conflict|Unavailable|Timeout|Registry|Runtime|Io|Json)"
RAW_VARIANT_MATCH = re.compile(
    r"Err\(\s*(?:\w+::)*(?:Error|EngineError)::" + _VARIANTS
    + r"\s*(?:\(\s*(?:_|\.\.)\s*\)|\{\s*\.\.\s*\}|\(\s*\w+\s*\)\)\s*(?:=>|if\b))"
    + r"|matches!\(\s*(?:(?!\.(?:into_)?root\(\))[^;])*?,\s*(?:Err\()?\s*(?:\w+::)*(?:Error|EngineError)::"
    + _VARIANTS + r"\b"
)
RAW_VARIANT_SKIP = ("crates/foundation/delonix-model/",)


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


# Where each layer lives on disk (ADR-0040 D2.5). The layer is readable from the
# path, so a crate in the wrong directory is a crate whose layer nobody can trust.
LAYER_DIR = {
    FOUNDATION: "crates/foundation",
    CONTEXT: "crates/contexts",
    ADAPTER: "crates/adapters",
    PROVIDER: "crates/providers",
    INTERFACE: "crates/interfaces",
    BIN: "bins",
}


def misplaced(pkgs: dict[str, dict]) -> list[str]:
    """A crate whose directory is not the one its declared layer lives in."""
    bad: list[str] = []
    for name, pkg in sorted(pkgs.items()):
        layer = LAYERS.get(name)
        if layer is None:
            continue  # already reported by rule_failures
        rel = Path(pkg["manifest_path"]).parent.relative_to(ROOT).as_posix()
        want = f"{LAYER_DIR[layer]}/{name}"
        if rel != want:
            bad.append(f"{name}: lives in {rel}, but its layer ({layer}) lives in {want}")
    return bad

# The engine knows NO consumer (AGENTS.md, «Identidade e fronteira do motor»). It is
# an abstraction over containers and microVMs with its own Kinds and its own provider
# ports; whoever consumes it adapts to its contracts. A consumer's name in the code,
# a comment or the contract is the first step of shaping the engine around that
# consumer — so it fails here, comments included. History that needs the name lives
# in docs/, never in crates/, bins/ or proto/.
CONSUMER_NAMES = re.compile(
    r"delonix[-_](?:paas|api|core|orchestrator|console)\b|ngc[-_](?:agent|api)\b"
    r"|\bdelonixctl\b|\bngolacloud\b|\bpaas\b",
    re.IGNORECASE,
)
BOUNDARY_ROOTS = ("crates", "bins", "proto")
BOUNDARY_FILES = ("Cargo.toml", "Makefile")
TEXT_SUFFIXES = {".rs", ".proto", ".toml", ".md", ".c", ".h", ".sh", ".py", ".json", ".yaml", ".yml"}


def consumer_mentions() -> list[str]:
    """Every place under the engine's code and contract that names a consumer."""
    files = [ROOT / f for f in BOUNDARY_FILES]
    for root in BOUNDARY_ROOTS:
        files += [
            f for f in sorted((ROOT / root).rglob("*"))
            if f.is_file() and f.suffix in TEXT_SUFFIXES and "target" not in f.parts
        ]
    bad: list[str] = []
    for f in files:
        if not f.is_file():
            continue
        for n, line in enumerate(f.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
            m = CONSUMER_NAMES.search(line)
            if m:
                rel = f.relative_to(ROOT).as_posix()
                bad.append(f"{rel}:{n}: names a consumer ({m.group(0)!r}) — the engine knows none")
    return bad

DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def inline_versions(pkgs: dict[str, dict]) -> list[str]:
    """A version written in a member manifest instead of `[workspace.dependencies]`.

    ADR-0040 P0 puts every version in ONE place. Two crates writing their own
    version of the same library is how the same dependency ends up resolved twice,
    and how `default-features = false` in one crate is silently undone by another.
    """
    bad: list[str] = []
    for name, pkg in sorted(pkgs.items()):
        manifest = tomllib.loads(Path(pkg["manifest_path"]).read_text(encoding="utf-8"))
        tables = [(t, manifest.get(t, {})) for t in DEP_TABLES]
        for target, spec in manifest.get("target", {}).items():
            tables += [(f"target.{target}.{t}", spec.get(t, {})) for t in DEP_TABLES]
        for table, deps in tables:
            for dep, value in deps.items():
                if isinstance(value, dict) and (value.get("workspace") or "path" in value):
                    continue
                bad.append(f"{name}: [{table}] {dep} has its own version — move it to [workspace.dependencies]")
    return bad


def count(
    pattern: re.Pattern[str], skip_bin: bool = True, only: tuple[str, ...] = ()
) -> tuple[int, list[str]]:
    total = 0
    where: list[str] = []
    files = sorted((ROOT / "crates").rglob("*.rs")) + sorted((ROOT / "bins").rglob("*.rs"))
    for f in files:
        rel = f.relative_to(ROOT).as_posix()
        if skip_bin and rel.startswith("bins/"):
            continue
        if only and not rel.startswith(only):
            continue
        if pattern is RAW_VARIANT_MATCH and rel.startswith(RAW_VARIANT_SKIP):
            continue
        # A build script speaks to cargo through stdout (`cargo:rerun-if-changed=…`);
        # that is the protocol, not a library writing to a terminal. Measured
        # 2026-09-25: the CRI's `build.rs` was the one false positive in the count,
        # and a second proto-building crate would have read as new debt.
        if pattern is PRINTS and f.name == "build.rs":
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
    bad += inline_versions(pkgs)
    bad += misplaced(pkgs)
    bad += consumer_mentions()

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
    env_writes, env_where = count(ENV_WRITES, skip_bin=False)
    shared_err, shared_err_where = count(SHARED_ERROR, only=SHARED_ERROR_DIRS)
    current = {
        "self_exec_sites": self_exec,
        "library_prints": prints,
        "env_writes": env_writes,
        "shared_error_imports": shared_err,
        "raw_error_variant_matches": count(RAW_VARIANT_MATCH, skip_bin=False)[0],
    }

    if args.list:
        print("self-exec sites (a library crate re-running the engine's own binary):")
        for w in self_where:
            print(f"  {w}")
        print("\nlibrary prints (stdout/stderr instead of tracing):")
        for w in print_where:
            print(f"  {w}")
        print("\nprocess-environment writes (set_var/remove_var):")
        for w in env_where:
            print(f"  {w}")
        print("\nadapters/providers using the shared error as their own:")
        for w in shared_err_where:
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

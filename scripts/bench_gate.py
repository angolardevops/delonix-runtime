#!/usr/bin/env python3
"""Bench gate — fails a performance REGRESSION, and refuses to judge what it cannot.

# What this closes (M11 of docs/roadmap/13-improvements-traceability.md)

`scripts/bench.sh` already measures the three engines on one machine and
already REFUSES to run on a loaded box (`scripts/bancada.sh`). What was missing
was the other half: nothing compared today's number against a recorded one, so
a regression only showed up when a human happened to re-read the table. M11's
last open deliverable is exactly that — "thresholds de regressão em CI".

# The trap this gate is built around

This repository retired a whole latency battery once (`docs/comparacao-medida.md`,
2026-08-10 — the measured-comparison doc every comment below refers back to):
docker 1406 ms, podman 1351 ms, delonix 640 ms, and the next run —
same tools, same distro, same kernel — gave 208 / 268 / 89. Three engines do not
get six times faster at the same time: that was the BENCH, not the engines.

A naive gate would have read that same event as "delonix regressed 7x" and gone
red on a machine problem. So this one never judges a number on its own:

  * a run the bench itself marked NOT publishable is not judged at all;
  * docker and podman are ANCHORS. If they degraded by as much as we did, the
    machine moved and the gate says so instead of accusing the engine;
  * with no anchor present there is no way to tell the two apart, so it refuses
    rather than guessing (exit 3, never a silent green).

Refusing is a THIRD outcome on purpose. A gate that answers "pass" when it did
not measure is indistinguishable from one that measured — the exact failure the
chaos workflow's environment probe was built to avoid.

# The baseline is per MACHINE, and that is not bureaucracy

A median of 80 ms recorded on a 32-thread desktop says nothing about a 2-vCPU
VM. Comparing across boxes would manufacture the very noise this gate exists to
filter, so the baseline is a LIST of entries, each stamped with the machine it
came from, and a run on an unknown machine is refused with the command that
records one.

Usage:
  scripts/bench.sh --json > run.json
  scripts/bench_gate.py --run run.json                 # check
  scripts/bench_gate.py --run run.json --record        # adopt as the baseline
  scripts/bench_gate.py --run run.json --tolerance 0.4 # looser threshold

Exit: 0 ok · 1 regression · 2 bad usage/input · 3 refused to judge.
"""
import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "scripts" / "bench_baseline.json"

# 25% over the recorded median. Not a round number picked for looking tidy: the
# published baseline's own samples span min 69 / max 92 around a median of 80
# (the measured-comparison doc cited above, 2026-08-25), i.e. +-15% of spread on
# a quiet box. A threshold inside that spread would fire on the bench's own noise
# and get muted within a week; one far above it would let a real regression through.
DEFAULT_TOLERANCE = 0.25

# max/min within one line's samples. A line that spans more than this is not a
# measurement of anything — the median hides it, and the load threshold does
# not catch it: measured 2026-09-23 with load 6.44 (well under the limit), a
# ten-sample docker line ran 434 ms to 5407 ms. The published baselines sit at
# 1.1x to 1.33x, so 3x is far above anything a quiet box has ever produced here
# and far below the noise that poisons a baseline.
MAX_SPREAD = 3.0

# Recording is held to a STRICTER bar than judging, and that asymmetry is the
# point: a run used to judge is thrown away, a baseline is reused by every run
# after it. Measured on the same box within minutes of each other: 1.08x when
# quiet, 2.69x while something else was still finishing — under the judging
# ceiling, and it would have recorded 165 ms for an engine that does 88. Every
# baseline ever published here sits between 1.06x and 1.33x, so 1.5x is above
# all of them and still refuses that 2.69x run.
MAX_SPREAD_RECORD = 1.5

# The engine under test. docker/podman are anchors — never gated, because a
# regression in someone else's engine is not ours to fail on.
SUBJECT = "delonix"
ANCHORS = ("docker", "podman")
LINE = "4a"

REFUSED = 3


def refuse(msg: str, detail: list[str] | None = None) -> int:
    print(f"RECUSADO: {msg}", file=sys.stderr)
    for line in detail or []:
        print(f"  {line}", file=sys.stderr)
    print(file=sys.stderr)
    print("  Uma recusa NÃO é um verde: este gate não julgou nada nesta corrida.", file=sys.stderr)
    return REFUSED


def load_run(path: Path | None) -> dict:
    raw = sys.stdin.read() if path is None else path.read_text()
    try:
        run = json.loads(raw)
    except json.JSONDecodeError as e:
        sys.exit(f"corrida ilegível ({e}) — gera com `scripts/bench.sh --json > run.json`")
    if run.get("schema") != 1:
        sys.exit(f"schema {run.get('schema')!r} desconhecido — este gate lê o schema 1")
    return run


def load_baseline() -> dict:
    if not BASELINE.is_file():
        return {"schema": 1, "entries": []}
    data = json.loads(BASELINE.read_text())
    if data.get("schema") != 1:
        sys.exit(f"baseline com schema {data.get('schema')!r} — este gate lê o schema 1")
    return data


def machine_of(run: dict) -> dict:
    b = run["bench"]
    # CPU model and thread count, nothing else. The kernel is deliberately OUT:
    # it changes on every distro update and would invalidate every baseline for
    # a reason that has never once been measured to move these numbers.
    return {"cpu": " ".join(b["cpu"].split()), "threads": b["threads"]}


def find_entry(baseline: dict, machine: dict) -> dict | None:
    for e in baseline["entries"]:
        if e["machine"] == machine:
            return e
    return None


def median_of(run: dict, tool: str) -> float | None:
    cell = run["lines"][LINE].get(tool)
    return None if cell is None else float(cell["median_ms"])


def spread_of(run: dict, tool: str) -> float | None:
    cell = run["lines"][LINE].get(tool)
    if cell is None:
        return None
    # A run produced before `bench.sh` emitted the field: unknown, not stable.
    # Reading a missing number as 1.0 would quietly re-admit exactly the noisy
    # lines this check exists to keep out of a baseline.
    return None if "spread" not in cell else float(cell["spread"])


def unstable(run: dict, tool: str, ceiling: float = MAX_SPREAD) -> bool:
    sp = spread_of(run, tool)
    return sp is None or sp > ceiling


def check(run: dict, tolerance: float) -> int:
    if not run["bench"]["publishable"]:
        return refuse(
            "a corrida foi marcada NÃO PUBLICÁVEL pelo próprio `bench.sh`.",
            [
                f"load {run['bench']['load1']} acima do limiar {run['bench']['threshold']}.",
                "Um número medido numa máquina carregada não acusa nem iliba ninguém.",
            ],
        )

    machine = machine_of(run)
    entry = find_entry(load_baseline(), machine)
    if entry is None:
        return refuse(
            f"não há baseline para esta máquina ({machine['cpu']}, {machine['threads']} threads).",
            [
                "Uma mediana medida noutra máquina não julga esta — comparar as duas",
                "fabricaria exactamente o ruído que este gate existe para filtrar.",
                "Para registar a de hoje: `scripts/bench_gate.py --run run.json --record`.",
            ],
        )

    base = entry["lines"][LINE]
    now = median_of(run, SUBJECT)
    if now is None:
        return refuse(f"a corrida não tem medição de `{SUBJECT}` — nada para julgar.")
    if base.get(SUBJECT) is None:
        return refuse(f"a baseline desta máquina não tem `{SUBJECT}` — nada contra que julgar.")

    if unstable(run, SUBJECT):
        sp = spread_of(run, SUBJECT)
        return refuse(
            f"a linha de `{SUBJECT}` dispersa {'?' if sp is None else f'{sp:.1f}x'}"
            f" (tecto {MAX_SPREAD:.0f}x) — não é uma medição de nada.",
            [
                "A mediana esconde-o e o limiar de load não o apanha: a contenção",
                "que dispersa I/O não aparece no load(1m). Volta a medir numa",
                "máquina quieta antes de julgar o motor por este número.",
            ],
        )

    ratio = now / float(base[SUBJECT])
    # An anchor that scatters stops being an anchor: its ratio no longer says
    # what the machine did. It drops OUT instead of dragging the median.
    anchors = {
        t: median_of(run, t) / float(base[t])
        for t in ANCHORS
        if median_of(run, t) is not None and base.get(t) is not None and not unstable(run, t)
    }

    print(f"máquina:   {machine['cpu']} ({machine['threads']} threads)")
    print(f"baseline:  {entry['recorded']} · commit {entry['commit']} · {entry['delonix_version']}")
    for tool in (SUBJECT, *ANCHORS):
        m, b = median_of(run, tool), base.get(tool)
        if m is None or b is None:
            print(f"  {tool:<8} não medido")
            continue
        sp = spread_of(run, tool)
        flag = "" if not unstable(run, tool) else "   [instável, fora do juízo]"
        sp_txt = "?" if sp is None else f"{sp:.1f}x"
        print(
            f"  {tool:<8} {m:6.0f} ms   baseline {float(b):6.0f} ms   "
            f"×{m / float(b):.2f}   dispersão {sp_txt}{flag}"
        )

    if ratio <= 1 + tolerance:
        print(f"\nok: `{SUBJECT}` ×{ratio:.2f}, dentro da tolerância de {tolerance:.0%}.")
        if ratio <= 1 - tolerance:
            print(
                f"AVISO: {1 / ratio:.2f}× MAIS RÁPIDO que a baseline. Não falha — mas uma\n"
                "  baseline muito melhor que o presente deixa de apanhar a regressão\n"
                "  seguinte. Considera `--record` num commit próprio, com a razão."
            )
        return 0

    if not anchors:
        return refuse(
            f"`{SUBJECT}` ×{ratio:.2f}, acima da tolerância — e não há âncora para atribuir.",
            [
                "Sem docker nem podman medidos, esta corrida não distingue uma regressão",
                "do motor de uma mudança da máquina. Foi essa confusão que fez retirar a",
                "bateria de 2026-08-10. Instala pelo menos uma das duas e volta a medir.",
            ],
        )

    anchor_ratio = statistics.median(anchors.values())
    if anchor_ratio >= 1 + tolerance:
        return refuse(
            f"`{SUBJECT}` ×{ratio:.2f}, mas as âncoras degradaram ×{anchor_ratio:.2f}.",
            [
                "Os outros motores não mudaram nesta árvore: se também estão mais lentos,",
                "o que mudou foi a máquina. O gate não acusa o motor por isso.",
            ],
        )

    print(
        f"\nFALHA: `{SUBJECT}` ×{ratio:.2f} ({now:.0f} ms contra {float(base[SUBJECT]):.0f} ms),\n"
        f"  acima da tolerância de {tolerance:.0%}, e as âncoras ficaram em ×{anchor_ratio:.2f}\n"
        "  — a máquina não explica esta diferença.",
        file=sys.stderr,
    )
    return 1


def record(run: dict) -> int:
    if not run["bench"]["publishable"]:
        return refuse(
            "uma corrida NÃO PUBLICÁVEL não pode virar baseline.",
            ["Seria gravar a contenção desta máquina como se fosse o motor."],
        )
    noisy = [
        f"{t} ({spread_of(run, t) or float('nan'):.2f}x)"
        for t in (SUBJECT, *ANCHORS)
        if median_of(run, t) is not None and unstable(run, t, MAX_SPREAD_RECORD)
    ]
    if noisy:
        return refuse(
            f"linha(s) demasiado dispersa(s) para virarem baseline: {', '.join(noisy)}.",
            [
                f"O tecto para GRAVAR é {MAX_SPREAD_RECORD}x (max/min), mais estrito que",
                f"os {MAX_SPREAD:.0f}x com que se JULGA — uma corrida de juízo deita-se fora,",
                "uma baseline é reusada por todas as que vierem a seguir.",
                "Espera a máquina acalmar e volta a medir.",
            ],
        )
    commit = subprocess.run(
        ["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    machine = machine_of(run)
    entry = {
        "machine": machine,
        "recorded": run["when"][:10],
        "commit": commit,
        "delonix_version": run["tools"][SUBJECT],
        "runs": run["runs"],
        "lines": {
            LINE: {t: median_of(run, t) for t in (SUBJECT, *ANCHORS)},
        },
    }
    baseline = load_baseline()
    entries = [e for e in baseline["entries"] if e["machine"] != machine]
    replaced = len(entries) != len(baseline["entries"])
    entries.append(entry)
    baseline["entries"] = sorted(entries, key=lambda e: (e["machine"]["cpu"], e["machine"]["threads"]))
    BASELINE.write_text(json.dumps(baseline, indent=2, ensure_ascii=False) + "\n")
    verb = "substituída" if replaced else "registada"
    print(f"baseline {verb} para {machine['cpu']} ({machine['threads']} threads) em {BASELINE.name}:")
    print(json.dumps(entry["lines"][LINE], indent=2))
    print("\nCommita-a com a razão da mudança — uma baseline que se move sozinha não é baseline.")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    p.add_argument("--run", type=Path, default=None, help="JSON de `bench.sh --json` (omitido = stdin)")
    p.add_argument("--record", action="store_true", help="adopta esta corrida como baseline da máquina")
    p.add_argument("--tolerance", type=float, default=DEFAULT_TOLERANCE, help="fracção (0.25 = 25%%)")
    args = p.parse_args()
    if args.tolerance <= 0 or args.tolerance >= 1:
        sys.exit("--tolerance fora de (0, 1): 0.25 é 25%")
    run = load_run(args.run)
    return record(run) if args.record else check(run, args.tolerance)


if __name__ == "__main__":
    sys.exit(main())

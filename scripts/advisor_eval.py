#!/usr/bin/env python3
"""Mede se um modelo consegue ler o estado de recursos de um anfitrião.

Existe porque a escolha de um modelo para este trabalho estava a ser feita por
opinião — «o 8B chega», «o 14B é melhor» — e este é um dos raros casos em que a
resposta certa é CALCULÁVEL: o `bottleneck()` e os achados `DLX-RES-nnn` do
motor são deterministas, portanto há gabarito. Um modelo que não bate o gabarito
não serve, valham-lhe os benchmarks que valerem.

O que se mede, por esta ordem de importância:

  1. JSON válido. Um conselheiro que devolve prosa não se liga a nada. Um
     modelo abaixo dos 100% aqui está fora, sem discussão.
  2. Gargalo certo — a pergunta que qualquer um sabe fazer.
  3. Achados certos (Jaccard) — a que separa modelos.
  4. Latência e falsos positivos.

O 4 conta tanto como o 3: um modelo que inventa achados num anfitrião saudável
é pior do que um que não diz nada, porque ensina o operador a ignorá-lo.

    scripts/advisor_eval.py --backend ollama --model qwen3:8b
    scripts/advisor_eval.py --backend openai --model gpt-x --base-url https://… --api-key-env FOO
    scripts/advisor_eval.py --backend stub          # prova o arnês sem modelo nenhum

O `--backend openai` fala o dialecto `/v1/chat/completions`, que qualquer coisa
serve hoje. O utilizador final escolhe o seu modelo e o seu fornecedor; isto só
diz quanto custa a escolha.
"""

import argparse
import json
import os
import pathlib
import statistics
import sys
import time
import urllib.error
import urllib.request

FIXTURES = pathlib.Path(__file__).resolve().parents[1] / (
    "crates/delonix-runtime/tests/fixtures/advisor"
)

SYSTEM = """You read the resource state of one Linux host running the Delonix
container engine and answer with JSON only. No prose, no markdown fence.

Answer this exact shape:
{"bottleneck": <"cpu"|"memory"|"io"|null>, "findings": [<"DLX-RES-nnn:subject">, ...]}

bottleneck: the resource stalling the host the most over the LAST 10 SECONDS
(avg10), and only if it stalls at least 5% of the time. Below that, null.

findings: every rule that applies. Use the exact ids, each with the subject it
is about, and nothing else:
  DLX-RES-001:cgroup   a controller the engine needs is not delegated, so
                       --cpuset-cpus/--io-max are accepted and ignored. NOT for
                       a missing `io` controller on a rootless host (see 002).
  DLX-RES-002:io       rootless and `io` is not delegated. Always both.
  DLX-RES-003:slice    aggregate_slice is false: no aggregate ceiling.
  DLX-RES-004:<res>    that resource stalled >=10% of the last 5 MINUTES
                       (avg300) — chronic.
  DLX-RES-005:<res>    avg10 >= 25% but avg300 < 10% — a spike, not chronic.
  DLX-RES-006:memory   more than 1 GiB swapped out AND memory avg60 >= 10%.
                       Swap alone, on a calm host, is NOT a finding.
  DLX-RES-007:disk     less than 10 GiB free under the state root.
  DLX-RES-008:cpu      cpu temperature >= 85 C.
An empty list is a correct answer for a healthy host."""


def load_cases(only):
    cases = []
    for path in sorted(FIXTURES.glob("*.json")):
        doc = json.loads(path.read_text())
        if only and doc["name"] not in only:
            continue
        cases.append(doc)
    if not cases:
        sys.exit(f"sem casos em {FIXTURES} (corre o teste advisor_fixtures primeiro)")
    return cases


def post(url, payload, headers, timeout):
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json", **headers},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def ask(args, prompt):
    """Devolve (texto, segundos). Uma excepção aqui é falha do modelo, não do arnês."""
    t0 = time.monotonic()
    if args.backend == "stub":
        # A "model" that always answers the same thing. It exists to prove the
        # scorer tells right from wrong: if the stub scored 100%, the
        # scoreboard would be broken, not the models good.
        text = '{"bottleneck": null, "findings": []}'
    elif args.backend == "ollama":
        body = post(
            f"{args.base_url.rstrip('/')}/api/chat",
            {
                "model": args.model,
                "messages": [
                    {"role": "system", "content": SYSTEM},
                    {"role": "user", "content": prompt},
                ],
                "stream": False,
                "format": "json",
                # Reasoning models (the Qwen3 family, and more every month) emit
                # a thinking block before the answer and it dominates the
                # latency: the numbers here come from a fixed table, not from a
                # chain of reasoning, so the block is spent on nothing. Ollama
                # ignores the field on models that do not think, so it is safe
                # to always send. `--think` puts it back for a fair comparison
                # against a model whose answer genuinely needs it.
                "think": args.think,
                "options": {"temperature": 0, "num_ctx": args.ctx},
            },
            {},
            args.timeout,
        )
        text = body["message"]["content"]
    else:
        key = os.environ.get(args.api_key_env, "")
        body = post(
            f"{args.base_url.rstrip('/')}/chat/completions",
            {
                "model": args.model,
                "messages": [
                    {"role": "system", "content": SYSTEM},
                    {"role": "user", "content": prompt},
                ],
                "temperature": 0,
                "response_format": {"type": "json_object"},
            },
            {"Authorization": f"Bearer {key}"} if key else {},
            args.timeout,
        )
        text = body["choices"][0]["message"]["content"]
    return text, time.monotonic() - t0


def parse(text):
    """Aceita JSON puro e JSON dentro de uma cerca markdown, e mais nada.

    Ser mais tolerante do que isto mediria a minha capacidade de adivinhar, não
    a do modelo de responder — e o consumidor real (o MCP) também não adivinha.
    """
    t = text.strip()
    if t.startswith("```"):
        t = t.split("```")[1]
        t = t[4:] if t.startswith("json") else t
    try:
        d = json.loads(t)
    except (json.JSONDecodeError, IndexError):
        return None
    if not isinstance(d, dict) or "bottleneck" not in d or "findings" not in d:
        return None
    if not isinstance(d["findings"], list):
        return None
    return d


def jaccard(a, b):
    a, b = set(a), set(b)
    return 1.0 if not a and not b else len(a & b) / len(a | b)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--backend", choices=["ollama", "openai", "stub"], default="stub")
    p.add_argument("--model", default="stub")
    p.add_argument("--base-url", default="http://localhost:11434")
    p.add_argument("--api-key-env", default="OPENAI_API_KEY")
    p.add_argument("--ctx", type=int, default=8192)
    p.add_argument(
        "--think",
        action="store_true",
        help="deixar o modelo pensar antes de responder (ollama). Triplica a latência.",
    )
    p.add_argument("--timeout", type=int, default=120)
    p.add_argument("--repeat", type=int, default=1, help="corridas por caso (temperatura 0 não garante determinismo)")
    p.add_argument("--only", nargs="*", help="correr só estes casos, pelo nome")
    p.add_argument("--json", action="store_true", help="resultado como JSON, para um painel")
    args = p.parse_args()

    cases = load_cases(args.only)
    rows, lat = [], []
    for doc in cases:
        truth = doc["truth"]
        for _ in range(args.repeat):
            prompt = json.dumps(doc["input"], indent=2)
            try:
                text, dt = ask(args, prompt)
            except (urllib.error.URLError, OSError, KeyError, TimeoutError) as e:
                sys.exit(f"o modelo não respondeu ({type(e).__name__}: {e})")
            lat.append(dt)
            got = parse(text)
            rows.append(
                {
                    "case": doc["name"],
                    "valid_json": got is not None,
                    "bottleneck_ok": got is not None
                    and got["bottleneck"] == truth["bottleneck"],
                    "findings_score": jaccard(got["findings"], truth["findings"])
                    if got
                    else 0.0,
                    # Findings invented on a host with nothing to report.
                    "invented": bool(got)
                    and not truth["findings"]
                    and bool(got["findings"]),
                    "seconds": dt,
                }
            )

    # The trivial baseline: what answering "nothing" every time scores. Without
    # it alongside, 70% looks good and may be below silence.
    trivial_b = 100.0 * sum(1 for c in cases if c["truth"]["bottleneck"] is None) / len(cases)
    trivial_f = 100.0 * sum(1 for c in cases if not c["truth"]["findings"]) / len(cases)

    n = len(rows)
    valid = sum(r["valid_json"] for r in rows)
    summary = {
        "model": args.model,
        "backend": args.backend,
        "cases": len(cases),
        "runs": n,
        "valid_json_pct": 100.0 * valid / n,
        "bottleneck_pct": 100.0 * sum(r["bottleneck_ok"] for r in rows) / n,
        "findings_jaccard": sum(r["findings_score"] for r in rows) / n,
        "invented_findings": sum(r["invented"] for r in rows),
        "p50_seconds": round(statistics.median(lat), 2),
        "p95_seconds": round(sorted(lat)[max(0, int(len(lat) * 0.95) - 1)], 2),
        "trivial_bottleneck_pct": round(trivial_b, 1),
        "trivial_findings_pct": round(trivial_f, 1),
        "think": args.think,
    }

    if args.json:
        print(json.dumps({"summary": summary, "rows": rows}, indent=2))
        return 0

    print(f"modelo: {summary['model']}  ({summary['backend']})")
    print(f"{'caso':<32}{'json':<7}{'gargalo':<10}{'achados':<10}{'s':>6}")
    for r in rows:
        print(
            f"  {r['case']:<30}"
            f"{'ok' if r['valid_json'] else 'MAU':<7}"
            f"{'ok' if r['bottleneck_ok'] else 'errado':<10}"
            f"{r['findings_score']:<10.2f}"
            f"{r['seconds']:>6.1f}"
        )
    print()
    print(f"  json válido      {summary['valid_json_pct']:.0f}%")
    print(f"  gargalo certo    {summary['bottleneck_pct']:.0f}%")
    print(f"  achados (jaccard){summary['findings_jaccard']:.2f}")
    print(f"  inventados       {summary['invented_findings']}")
    print(f"  latência p50/p95 {summary['p50_seconds']}s / {summary['p95_seconds']}s")
    print(
        f"  (quem responde sempre «nada» tira {trivial_b:.0f}% no gargalo — "
        f"abaixo disso o modelo é pior do que o silêncio)"
    )
    if summary["bottleneck_pct"] <= trivial_b:
        print("\n  este modelo não bate a resposta trivial.")
    if summary["valid_json_pct"] < 100:
        print("\n  json válido abaixo de 100%: este modelo não serve para ligar a nada.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

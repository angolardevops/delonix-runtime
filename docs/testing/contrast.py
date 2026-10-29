#!/usr/bin/env python3
"""Verificador de contraste WCAG AA dos tokens de cor da documentação (§8).

Determinístico, sem browser. Sai != 0 se algum par de texto falhar AA (4.5:1
normal, 3.0:1 texto grande / componentes). Mantém-se em par com os tokens de
`CSS` em `docs/gen.py`; actualizar aqui quando a paleta mudar.

    python3 docs/testing/contrast.py
"""
import sys


def _lin(c):
    c /= 255
    return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4


def _lum(hexs):
    h = hexs.lstrip("#")
    r, g, b = (int(h[i:i + 2], 16) for i in (0, 2, 4))
    return 0.2126 * _lin(r) + 0.7152 * _lin(g) + 0.0722 * _lin(b)


def ratio(a, b):
    la, lb = _lum(a), _lum(b)
    hi, lo = max(la, lb), min(la, lb)
    return (hi + 0.05) / (lo + 0.05)


# (nome, fg, bg, limiar). 3.0 = texto grande/componente; 4.5 = texto normal.
PAIRS = [
    # Claro
    ("claro: ink/bg", "#1a1a2e", "#ffffff", 4.5),
    ("claro: muted/bg", "#5a6472", "#ffffff", 4.5),
    ("claro: muted/side", "#5a6472", "#f7f8fa", 4.5),
    ("claro: link/bg", "#b8420a", "#ffffff", 4.5),
    ("claro: link/soft (nav+passos)", "#b8420a", "#fff0e6", 4.5),
    ("claro: btn/pill branco/strong", "#ffffff", "#b8420a", 4.5),
    ("claro: out-ink/bg", "#4a5563", "#ffffff", 4.5),
    ("claro: tok-comment/code", "#9da7b3", "#0f172a", 4.5),
    ("claro: code-head-ink/head", "#cbd5e1", "#1e293b", 4.5),
    # Escuro
    ("escuro: ink/bg", "#e6e8ee", "#0d1117", 4.5),
    ("escuro: muted/bg", "#9aa4b2", "#0d1117", 4.5),
    ("escuro: muted/side", "#9aa4b2", "#10151c", 4.5),
    ("escuro: link/bg", "#f4733b", "#0d1117", 4.5),
    ("escuro: link/soft", "#f4733b", "#2a1810", 4.5),
    ("escuro: tok-comment/code", "#8b949e", "#161b22", 4.5),
    ("escuro: code-head-ink/head", "#9aa4b2", "#1b222c", 4.5),
]

fails = 0
for name, fg, bg, need in PAIRS:
    r = ratio(fg, bg)
    tag = "PASS" if r >= need else "FAIL"
    if r < need:
        fails += 1
    print(f"  [{tag}] {r:5.2f} (>= {need}) {name}  {fg} on {bg}")

print(f"\n{'OK' if not fails else 'FALHAS: ' + str(fails)} — contraste WCAG AA")
sys.exit(1 if fails else 0)

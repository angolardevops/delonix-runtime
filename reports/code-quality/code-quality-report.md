# Revisão de qualidade de código — Delonix Runtime

Skill: `delonix-code-quality-architecture`
Repositório: `delonix-runtime`
Commit medido: `cebf895` (= `origin/main`, 0 commits de atraso)
Versão: `1.0.0`
Data: 2026-08-29

## Inventário medido

| Métrica | Valor |
|---|---|
| Crates no workspace | 14 (+ `delonix-runtime-bin`) |
| Ficheiros `.rs` seguidos por git | 147 |
| Linhas de Rust | ~138 500 |
| Itens `pub` em produção analisados | 1 190 (161 struct, 68 enum, 2 trait, 848 fn, 111 mod) |
| Identificadores marcados como PT pelo gate | 1 039 |
| Comentários marcados como PT pelo gate | 3 453 |

## Portões automáticos — o que está VERDE, corrido agora

| Portão | Comando | Resultado |
|---|---|---|
| Formatação (§89) | `cargo fmt --all --check` | **VERDE**, rc=0 |
| Clippy (§87, §88) | `cargo clippy --workspace --all-targets` | **VERDE**, zero avisos |
| Gate de língua (ratchet) | `python3 scripts/lang_ratchet.py` | **alinhado**, rc=0 |

Aviso de leitura: o gate de língua é um **ratchet**, não um tecto. «Alinhado»
significa «a dívida não subiu», **não** significa «cumpre a Regra de Ouro §1».

## Placar

| Dimensão | Nota | Porquê |
|---|---|---|
| Conformidade com inglês (§1–§2) | **82/100** | Zero identificadores públicos em PT; residem 54 `let` locais + 985 nomes de teste + 3 453 comentários |
| Qualidade de nomes (§4–§11) | **90/100** | Convenções Rust respeitadas (clippy verde); desconto pelos nomes locais em PT e por `util.rs` |
| Documentação (§18–§23, §68) | **86/100** | 13/13 crates com `//!`; structs 87,6 %, fn 85,1 %; **enums 58,8 %** é o buraco |
| Arquitectura (§26–§39) | **71/100** | Só **3 traits** em 138k linhas; `VmBackend` é um porto a sério, mas o resto é módulos concretos |
| Tratamento de erro (§23–§24, §51) | **74/100** | 45 `.map_err(\|_\| …)` a perder causa; 12 `Result<_, String>`, 3 no caminho de segurança |
| Legibilidade por agentes (§84) | **88/100** | `ARCHITECTURE.md` + `AGENTS.md` existem e são **testados** (`tests/architecture.rs`) |

## Achados por severidade

| Severidade | Nº | Já fechados |
|---|---|---|
| CRITICAL | 0 | — |
| HIGH | 2 | **1** (ARCH-0002, PR #170) |
| MEDIUM | 6 | **1** (QUALITY-0001, PR #170) |
| LOW | 6 | — |
| INFO | 3 | — |

Dois achados foram corrigidos ainda durante esta auditoria, no PR
[#170](https://github.com/angolardevops/delonix-runtime/pull/170):
os 37 argumentos posicionais do `container_init` passaram a um tipo agrupado,
e três `#[allow(clippy::too_many_arguments)]` que silenciavam a função errada
foram removidos. O resto está no `refactoring-backlog.md`, priorizado.

Detalhe em `architecture-report.md`, `naming-report.md`, `documentation-report.md`
e `refactoring-backlog.md`.

## O que NÃO foi validado (a segunda metade do relatório)

- **Nenhum teste foi corrido.** Não se mediu cobertura, nem se validou nada ao vivo.
- A cobertura de documentação foi medida por análise sintáctica própria
  (`///` imediatamente acima do item), **não** por `cargo doc`. Itens documentados
  com `/** */` contam como não documentados.
- A classificação produção-vs-teste dos identificadores usa contagem de chavetas
  a partir de `#[cfg(test)]`; ficheiros com macros que abram blocos podem
  desalinhar (não foram encontrados casos, mas não foi provado exaustivamente).
- **Não foi feita revisão de segurança.** Os achados de `unsafe` são de
  *documentação* (§17), não de correcção.
- Os outros repos do workspace (`delonix-paas`, `delonix-portal`, `delonix-admin`,
  …) **não foram analisados**. Este relatório é só do motor.

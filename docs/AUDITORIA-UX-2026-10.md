# Auditoria UX/UI da documentação Delonix — 2026-10

Diagnóstico do site gerado por `docs/gen.py` (publicado em GitHub Pages:
`https://ngolacloud.github.io/delonix`), contra a proposta visual fornecida e
as referências Docker/Podman/Kubernetes. **Preserva-se** o framework (gerador
Python único, sem CDN), o conteúdo técnico e os URLs.

Capturas de evidência (antes/depois) em `scratchpad/before/` e `scratchpad/after/`.

## Problemas observados (com evidência e prioridade)

| # | Prioridade | Problema | Evidência | Estado |
|---|---|---|---|---|
| 1 | P1 | **Sem cabeçalho global.** Marca, pesquisa, idioma e tema viviam no topo da *sidebar*, não numa barra superior. | `sidebar()` (brand+toggles+search embebidos); captura `before/index-light-1440` | **Corrigido** (Fase 1) |
| 2 | P1 | **Sidebar permanentemente expandida, sem divulgação progressiva.** Toda a árvore + *todos* os comandos CLI e subcomandos renderizados de uma vez. | `before/kinds-light-1440` (lista CLI gigante) | **Corrigido** (grupos `<details>`; CLI recolhida) |
| 3 | P1 | **Homepage abre com prosa densa**, sem percurso de acção (sem CTA, sem 3 passos acima da dobra). | `INDEX_PT`; `before/index-light-1440` | **Corrigido** (hero + CTA + 3 passos) |
| 4 | P2 | **Margens/espaço desperdiçado.** Coluna de artigo (860 px) centrada numa coluna larga → vazio entre sidebar↔artigo e artigo↔TOC. | `.layout 270\|1fr\|240 max 1600`; `main margin:auto` | **Corrigido** (grelha 256\|1fr\|220 max 1400; artigo à esquerda) |
| 5 | P2 | **Blocos de código sem cromado; cópia só em *hover*.** Sem barra de terminal/ficheiro; YAML e `curl` sem cópia. | `.copy-btn{opacity:0}`; `before/kinds` (YAML sem cópia) | **Corrigido** (barra Terminal/YAML + cópia sempre visível) |
| 6 | P2 | **Pesquisa pouco proeminente** (dentro da sidebar). Indexação real já existia. | `search-index.json` (775 registos) | **Corrigido** (movida para o cabeçalho; indexação intacta) |
| 7 | P3 | **Scroll horizontal da página no mobile.** Tabela "Porque é diferente" e `code` inline longo transbordavam a 390 px. | medição Playwright (`index 438>390`, `kinds 405>390`) | **Corrigido** (tabelas com scroll próprio; `overflow-wrap`) |

## Hipóteses a validar (fases seguintes)

- **Contraste WCAG AA** do texto secundário (`--muted`), *syntax highlighting* e
  estados de navegação, nos dois temas (§8). *Por medir com ferramenta.*
- **TOC muito longa** em páginas grandes (p. ex. `kinds` lista ~25 âncoras) —
  avaliar *scrollspy*/agrupamento.
- **Zoom 200% e âncoras profundas** abertas directamente (§14).
- **Paridade i18n** dos novos textos do cabeçalho/top-nav (garantida via `bi()`).
- **Nome de ficheiro** nos blocos YAML dos kinds (hoje rótulo genérico "YAML";
  emitir `data-filename` a partir de `examples/<grupo>/<kind>.yaml`).

## Pontos fortes preservados (não tocar sem razão)

- Pesquisa Ctrl/Cmd+K real, indexada, com teclado e *focus-trap*.
- Temas claro/escuro com *tokens* semânticos + anti-FOUC (`THEME_INIT_JS`) +
  *fallback* `prefers-color-scheme`.
- i18n PT/EN via `bi()`/spans de idioma; `contract_gate()` impede deriva
  binário↔docs; *highlight* próprio sem CDN; cópia com *fallback* legado.
- Identidade laranja (`--accent #e8590c`) consistente.

## Plano por fases

- **Fase 1 — Layout global (FEITA e validada).** Cabeçalho, grelha/espaçamento,
  sidebar recolhível, cromado de código, homepage orientada à acção. Validada
  com Playwright (sem overflow em 5 páginas × 4 larguras; Ctrl+K; gaveta mobile;
  cópia) e capturas claro/escuro/mobile.
- **Fase 2 — Páginas por tipo.** Tabelas de parâmetros, *callouts* tipados,
  nome de ficheiro nos YAML, *breadcrumbs* + anterior/seguinte, afinação de
  cada tipo (kind, lab, CLI, conceito).
- **Fase 3 — Acessibilidade + testes.** Contraste AA, foco visível, zoom 200%,
  suite Playwright a 390/768/1440/1920 × claro/escuro (§14).
- **Fase 4 — Diagramas e metadados.** Diagramas de arquitectura com identidade
  (§11) e estado de validação dos exemplos (§10/§13).

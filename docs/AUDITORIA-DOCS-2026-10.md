# Auditoria da documentação do Delonix — 2026-10-28

Base verificável para a consolidação do site de documentação (`github.com/ngolacloud/delonix`).
Cada linha foi confirmada contra o código, o `--help` real ou os schemas — **não** assumida a
partir da própria documentação. Ramo: `docs/auditoria-consolidacao`.

## Factos canónicos confirmados

| Facto | Valor | Evidência |
|---|---|---|
| Remoto oficial | `github.com/ngolacloud/delonix` | `git remote -v` |
| Release Latest publicada | **`v0.1.0`** (2026-10-28T11:16:35Z) | `gh release list`; `releases/v0.1.0.md:19,49` |
| Tags históricas | `v4.1.1`…`v5.1.1` (preservar) | `git tag --list` |
| Base URL do Pages | `ngolacloud.github.io/delonix` (sem CNAME/domínio próprio) | `releases/v0.1.0.md:19,49`; `RELEASES.md:3806,3836`; ausência de `CNAME` |
| Imagem oficial | `ghcr.io/ngolacloud/delonix` | `releases/v0.1.0.md` |
| Nome anterior do produto | `delonix-runtime` → `delonix` (crate real `bins/delonix-runtime-bin` mantém o nome) | código em disco |

## Arquitectura da documentação (dois sites, dois geradores)

- **Site principal** — `docs/gen.py` (6421 linhas): **91 páginas** (15 de topo + 76 em `comandos/`).
  Fonte da verdade é o **binário** `target/release/delonix` (`gen.py:23`): embebe `--help` real via
  `help_of()` (`gen.py:128`); i18n PT/EN por classes CSS `lang-pt`/`lang-en` no mesmo ficheiro (`bi()`, `gen.py:166`);
  temas via `data-theme` + `prefers-color-scheme` (`gen.py:3228-3239`).
- **Manual** — `scripts/dev_docs_site.py`: **105 páginas** (`handbook/index.html` + 26 × 4 idiomas: `en`, `pt-AO`, `fr-FR`, `zh-CN`) a partir de `docs/dev/**/*.md`.
- **Publicação**: GitHub Pages serve a pasta `docs/` da branch (modo "branch → /docs") com `docs/.nojekyll`.
  **Não há workflow de deploy**; os `.html` são commitados directamente. O `ci.yml` **valida** (corre `gen.py` + `dev_docs_site.py --check` e falha se `git diff` em `docs/` acusar diferença).
- **Regra de edição**: `gitops.html`, `estabilidade.html`, `labs.html`, `kinds.html` etc. são **gerados**.
  Corrigir a **fonte** (`gen.py` ou os `.md`) e regenerar — nunca editar o HTML gerado.

---

## Convenção da matriz

Colunas: **Página/componente | Problema | Evidência | Alteração | Prioridade | Validação**.
Prioridade: 🔴 Alta · 🟠 Média · 🟢 Baixa.

---

## A. Gerador e componentes transversais

| Página/componente | Problema | Evidência | Alteração | Prio | Validação |
|---|---|---|---|---|---|
| `gen.py` / binário | Binário `target/release/delonix` **stale** (5.1.1, sem `bucket`); regenerar agora DESCARTA `bucket.html` e comandos novos | `gen.py:23`; binário rejeita `delonix bucket` apesar de `main.rs:295`; docs commitados já têm `bucket.html` (v0.1.0) | Rebuild obrigatório antes de `gen.py`; documentar no README dos docs; idealmente gate que cruze `COMMAND MAP` do binário vs `GROUPS` | 🔴 | `make build` + `python3 docs/gen.py` sem abortar; `git diff docs/` vazio |
| Pesquisa Ctrl+K | **Ausente** em todo o site (0 handlers de teclado, 0 índice, 0 modal) | `gen.py:3461-3597` (SITE_JS só tem highlighter/ToC/toggles); grep `ctrlKey`/`metaKey`=0 | Gerar índice JSON estático (títulos+h2/h3+taglines) + modal Ctrl/Cmd+K client-side vendored | 🔴 | Ctrl+K abre modal; Esc fecha; navegação por teclado; resultados com título+secção+excerto |
| Botão de cópia | **Ausente** em todos os `<pre><code>` | `gen.py:3669-3671,3693,3707`; grep `clipboard`=0 | Botão em `pre` de comando (não em `.out`/`.help`), `aria-label` bilingue, fallback `execCommand` | 🟠 | Copia em dark/light; indica sucesso/falha; não copia output nem `--help` |
| Páginas Markdown (EN) | `md_page()` não é bilingue: `gitops.html`, `estrutura.html`, `estabilidade.html`, `guia-vm.html` mostram **corpo só em PT** mesmo em modo EN | `gen.py:4598`, `6404-6410` | Traduzir corpo ou marcar explicitamente como PT-only; a longo prazo bilingue em `md_page` | 🔴 | Trocar p/ EN e confirmar corpo traduzido |
| Blocos `--help` (i18n) | `--help` do clap é EN-only; em modo PT os blocos `<div class='help'>` ficam em inglês | `gen.py:3693,3707` | Gerar com `--l18n=pt` ou rotular "saída real do binário (EN)" | 🟠 | Modo PT: bloco rotulado ou traduzido |
| Taglines EN em falta | `backup`, `config`, `namespace`, `oci-runtime` em `GROUPS` mas não em `GROUPS_EN` → tagline/intro só-PT | `gen.py:1740`, fallback `3680-3685` | Acrescentar entradas em `GROUPS_EN` | 🟠 | Grupos mostram intro EN |
| `<html lang>` fixo | Fixo em `"pt"`; o toggle só muda `data-lang` → leitores de ecrã anunciam EN como PT | `gen.py:3612` | Actualizar `lang` no toggle | 🟠 | Inspeccionar `lang` após troca |
| Tema **claro** — tokens de código | `--code-bg:#0f172a` (escuro) com `--tok-*` escuros (GitHub-light): azul/verde/castanho sobre quase-preto, contraste baixo | `gen.py:3229-3231` | No tema claro usar a paleta de tokens brilhante (fundo de código é sempre escuro) | 🔴 | Rácio ≥4,5:1 em blocos de código no tema claro |
| Output `muted` | Corpo de `.out pre` em `--muted` no limite de legibilidade | `gen.py:3304-3305` | Subir contraste do texto de output | 🟠 | Rácio ≥4,5:1 |
| `tag`/`callout` fora dos tokens | `.tag.ok/mid/no`, `.callout.warn` com hex fixos + overrides manuais de dark | `gen.py:3324-3332` | Migrar para tokens de tema | 🟢 | Trocar tema e inspeccionar |
| pt.po vs dicionários do site | i18n do site é hard-coded (`GROUPS_EN`/`CATEGORY_PT`), independente do `pt.po` do binário → risco de drift | `gen.py:56-59,1740` | Documentar a separação; opcional derivar do binário | 🟢 | — |
| Navegação mobile | Sem botão de menu; <840px a sidebar inteira (incl. Referência CLI) empilha acima do conteúdo | `gen.py:3340-3342` | Nav colapsável/hambúrguer em mobile | 🟢 | Testar <840px |

## B. Referências de repositório e release

| Página/componente | Problema | Evidência | Alteração | Prio | Validação |
|---|---|---|---|---|---|
| `$schema` do GitOps | URL de Pages com org+nome antigos: `angolardevops.github.io/delonix-runtime/schema/v1/delonix.json` (não resolve) | `gitops.md:225` → `gitops.html:376` | `ngolacloud.github.io/delonix/schema/v1/delonix.json` | 🔴 | `curl` ao URL devolve 200 |
| `$schema` estabilidade | Idem `delonix.json` e `providers.json` | `cli-stability.md:368,448` → `estabilidade.html:548,616` | `ngolacloud.github.io/delonix/schema/v1/...` | 🔴 | `curl` 200 |
| Instalador no roteiro | `curl … angolardevops.github.io/delonix-runtime/install.sh` (o site já usa `releases/latest/download/install.sh`) | `ROTEIRO-VIDEOS.md:48` | Alinhar com one-liner canónico (`gen.py:171` já correcto) | 🔴 | Comparar com `gen.py:171` |
| Imagens golden VM | `ghcr.io/angolardevops/delonix-vm-{base,k8s,appliances}` descritas como **oficiais** (conflita com `ghcr.io/ngolacloud/delonix`) | `sovereignty-engine.md:104,120`; `dev/microvm-setup.md:255-257`; `guia-vm.html:1006`; `guia-vm-lab.md:967`; `adr/0036:64` | Alinhar org → `ngolacloud` (ou confirmar que goldens ficam em `angolardevops`) | 🟠 | Decisão do dono + `docker pull` |
| Proveniência dos labs | 24 blocos "Validado em delonix **5.1.1** … 2026-10-24" | `gen.py:4937,4973,5016,5050,5087,5132,5261,5297,5339,5373,5410,5457`; `labs.html` 187…605; `labs/lab-*/README.md` | Re-correr labs e reetiquetar p/ **0.1.0**, ou marcar baseline histórica | 🟠 | Labs passam em 0.1.0 |
| Conformidade medida | CRI/OCI/rede "medido na v5.1.1" | `network-compatibility.md:124`; `dev/plano-conformidade-oci-cri.md` (+ handbook 4 idiomas); `testing/folhas-inventario.md:17,316,544` | Re-medir na 0.1.0 ou marcar histórico explícito | 🟠 | Nova medição datada |
| Versão anómala | `Delonix 0.53.0` (não bate 5.1.1 nem 0.1.0) | `comparacao-medida.md:34` | Corrigir p/ versão real medida (0.1.0) | 🟠 | — |
| Fronteira de contrato | "Alteração de contrato **depois da v5.1.1**" | `cli-stability.md:75,131,140,150` → `estabilidade.html:216,269,277,286` | Reenquadrar fronteira para a nova linha 0.1.0 | 🟠 | — |
| Resíduo de marca | 346× "Delonix Runtime" como nome de produto (handbook 4 idiomas) | `handbook/**/*.html` | Substituir por "Delonix" na próxima regeneração do handbook | 🟢 | grep "Delonix Runtime" no handbook = 0 |
| Extensão VS Code | `github.com/angolardevops/delonix-vscode` — org a confirmar | `estabilidade.html:558` | Confirmar onde vive a extensão; alinhar se necessário | 🟢 | Repo existe |
| Título duplicado | `<title>Delonix Engine · Delonix Engine</title>` | `index.html:6` | Corrigir template em `gen.py` | 🟢 | Ver `<title>` |
| Metadados ausentes | Sem `<base>`, `canonical`, `og:`/`twitter:` em todo o site | `index.html` só tem `viewport`+`title` | Acrescentar meta (base `ngolacloud.github.io/delonix`) | 🟢 | Ver `<head>` |
| Placeholders ghcr | `ghcr.io/angolardevops/{app,...}` como exemplo de utilizador | `gen.py:523,525`; `kinds.html:2125`; `comandos/{push,pull,image}.html` | Clarificar como placeholder | 🟢 | — |
| Histórico (não mexer) | `RELEASES.md`, `releases/v5*/v4*`, `releases/archive/v0.md`, `discovery/*` | — | Preservar como registo histórico | — | — |

## C. Integridade estrutural

| Página/componente | Problema | Evidência | Alteração | Prio | Validação |
|---|---|---|---|---|---|
| Páginas órfãs | **Nenhuma** — 196/196 alcançáveis de `index.html`; 0 entradas de nav p/ ficheiro inexistente | BFS em `scratchpad/audit.py` | — (sem acção) | — | Re-correr BFS |
| `guia-vm.html` — índice | 10 âncoras do índice mantêm acentos mas os `id` dos cabeçalhos são ASCII (slugify divergente) | `#1-antes-de-começar` vs `id="1-antes-de-comecar"` (e +9) | Slugify partilhado único em `gen.py` | 🟠 | Clicar nas 10 entradas salta |
| Manual — `environment-variables` | 12 âncoras cruzadas em EN mas `id` traduzidos (FR/PT-AO/ZH-CN) | `handbook/{fr-FR,pt-AO,zh-CN}/{index,start-here,environment-variables}.html` | Reescrever âncoras ao traduzir cabeçalhos | 🟠 | Clicar nas ligações cruzadas |
| `comandos/image.html` | ✅ **JÁ RESOLVIDO** — `vm.html` emite `id` por subcomando (`gen.py:3700`, `<h2 id='{sub}'>`); `vm.html#image` existe e resolve | `comandos/vm.html:530` (`id='image'`) | — (nenhuma; era deriva do binário stale) | 🟢 | `vm.html#image` salta p/ `vm image` ✅ |
| Manual zh-CN | ✅ **FEITO (Secção 3)** — zh-CN acrescentado ao redirect e à lista de links da landing `handbook/index.html` | `dev_docs_site.py:817,820` | zh-CN na landing (redirect `/zh/` + link 简体中文) | 🟢 | Landing lista zh-CN ✅ |
| Labs não ligados | ✅ **FEITO (Secção 3)** — `labs_page()` liga cada lab (lab-1..6) ao `README.md` + `lab.sh`; lab-7/8 ficam sem link (ainda sem arnês em disco, guardado por `os.path.isdir`) | `gen.py` `labs_page()`; `labs.html` | Bloco "Materiais" por lab com materiais reais | 🟢 | Links a partir de `labs.html` ✅ |
| Traduções stale | Quase todas PT-AO/FR-FR/ZH-CN marcadas `stale`; `engineering-process.md` **missing** nos 3 | `dev_docs_site.py --status` | Actualizar traduções (Secção 9) | 🟠 | `--status` sem `missing` |

## D. Correspondência documentação ↔ código

| Página/componente | Problema | Evidência | Alteração | Prio | Validação |
|---|---|---|---|---|---|
| `kinds.html` — Storage/ShareVolume | Doc promete que `kind: Storage`/`ShareVolume` **ainda carregam** (reescritos com aviso); código **rejeita com erro duro** | Doc `kinds.html:1836,1846,1854,149`; código `manifest.rs:861,882,886` (`removed_kind_hint`); não estão em `FACTS` | Corrigir texto: foram **REMOVIDOS** (como já diz a secção Egress `kinds.html:2794`) | 🔴 | `delonix … apply` de `kind: Storage` dá erro; doc coerente |
| `kinds.html` — tagline EN | Usa nome antigo `Tunnel` (hoje `Gateway`); ordem de apply omite 10 kinds reais | `kinds.html:149` (EN); ordem real `kinds.rs:304-685,774` | Actualizar nomes e ordem na tagline EN | 🟠 | Comparar com `stack_kinds()` |
| `kinds.html` — FirewallPolicy | Secção titulada pelo alias `FirewallPolicy`; nome canónico é `NetworkPolicy` | `kinds.html:2793`; `kinds.rs:82` (`FIREWALL_POLICY="NetworkPolicy"`) | Titular pelo canónico, alias como nota | 🟠 | `api-resources` mostra `NetworkPolicy` |
| `kinds.html` — NetworkZone | Aceite no load mas sem secção própria | `kinds.rs:373` (`Sunset`); sem `<h2>` | Acrescentar nota (é sunset) | 🟢 | — |
| `kinds.html` — Application/VolumeClaim | Citados no pedido mas **não existem** nem na doc nem no código (ausência limpa) | sem `<h2>`; sem linha em `FACTS` | Nenhuma (confirmar que não são prometidos) | — | — |
| CLI — cobertura | Nenhum comando documentado ausente do **código-fonte**; `ingress-proxy` oculto (correcto) | enum `Cmd` `main.rs`; `main.rs:648` (`hide`) | — | — | — |
| CLI — deriva | Risco materializado pelo binário stale (ver linha A) | `gen.py:24` | (coberto em A) | 🔴 | — |
| Variáveis de ambiente — referência | **Limpa e imposta por CI**: ~150 `DELONIX_*` do código estão todas documentadas; nenhuma indocumentada | `docs/dev/environment-variables.md`; `scripts/dev_docs.py::env_var_problems` | — (sem acção) | — | `dev_docs.py --check` verde |
| Env vars em ADR/discovery | `DELONIX_OPENSTACK_AUTH_URL/SECRET` e `DELONIX_NET_PROVIDER` descrevem funcionalidades **não implementadas** | `adr/0039:77-78`; `discovery/62_...:70` | Marcar como desenho/não implementado | 🟠 | grep no código = 0 |

---

## Síntese de prioridades

**🔴 Alta (promete o que não existe, ou quebra uma função nuclear):**
1. `kinds.html` diz que Storage/ShareVolume ainda carregam — código rejeita. *(D)*
2. Binário stale → regeneração descarta `bucket` e comandos novos. *(A)*
3. 3× `$schema` + instalador apontam para `angolardevops.github.io/delonix-runtime` (não resolve). *(B)*
4. Páginas Markdown mostram corpo só-PT em modo EN (4 guias). *(A)*
5. Contraste de tokens de código no tema claro. *(A)*
6. Pesquisa Ctrl+K ausente (92 páginas sem pesquisa). *(A)*

**🟠 Média:** imagens golden `angolardevops` vs `ngolacloud`; proveniência 5.1.1→0.1.0 (labs + conformidade); `Delonix 0.53.0`; botão de cópia; `--help` EN-only; taglines EN em falta; `<html lang>` fixo; 22 âncoras quebradas (slugify); traduções stale; FirewallPolicy/Tunnel na tagline; env vars de desenho.

**🟢 Baixa:** 346× "Delonix Runtime" no handbook; título duplicado; metadados ausentes; zh-CN fora da landing; labs não ligados; nav mobile; cores fora de tokens.

## Mapeamento para o backlog (frentes seguintes, uma sessão cada)

| Secção do pedido | Entradas da matriz que a alimentam |
|---|---|
| 2 — Referências repo/release | Todas as linhas 🔴/🟠 de **B** |
| 3 — Navegação/redireccionamentos | ✅ **FEITO** — **C** (zh-CN landing, labs não ligados); `image→vm#image` já resolvia |
| 4 — Introdução/onboarding | — (reescrita; usar `delonix init` real) |
| 5 — Layout/largura | nav mobile (**A**) |
| 6 — Temas/contraste | tokens tema claro, output muted, tags/callouts (**A**) |
| 7 — Pesquisa Ctrl+K | **A** (ausente) |
| 8 — Botão de cópia | **A** (ausente) |
| 9 — Traduções PT/EN | Markdown só-PT, `--help` EN-only, taglines EN, `<html lang>`, traduções stale, âncoras traduzidas |
| 10 — Kinds | Storage/ShareVolume, tagline, FirewallPolicy, NetworkZone (**D**) |
| 11 — Labs | labs não ligados; proveniência 5.1.1 |
| 12 — Variáveis | referência já limpa; marcar env vars de ADR/discovery |
| 13 — Glossário | — (novo) |
| 14 — Arquitectura | diagramas (`arquitectura.html`/`c4.html`) a rever |
| 15 — Exemplos testáveis | gate binário/`gen.py`; schema $ref |
| 16 — Validação visual/publicação | sem workflow de deploy; validar Pages base URL |

## Contagens

- **Páginas**: 196 HTML (91 `gen.py` + 105 manual). **Órfãs: 0.**
- **Links de ficheiro quebrados: 0.** **Âncoras quebradas: 23** (10 `guia-vm`, 12 manual traduzido, 1 `image→vm`).
- **Referências repo/release accionáveis**: ~5 URLs Pages/instalador (🔴), ~10 imagens ghcr (🟠), 24 blocos de proveniência labs (🟠), ~12 de conformidade (🟠), 1 versão anómala (🟠).
- **Kinds**: 26 documentados; 2 (`Storage`/`ShareVolume`) prometidos mas rejeitados; 1 (`NetworkZone`) aceite sem secção.
- **Variáveis**: ~150 `DELONIX_*` — referência completa e imposta por CI; 3 só em docs de desenho.

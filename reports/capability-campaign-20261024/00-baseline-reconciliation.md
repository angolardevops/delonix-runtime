# Sprint 0 — Reconciliação da baseline 5.1.1 e da matriz de capacidades

Data: 2026-10-24 · Engenheiro responsável: campanha de conformidade de capacidades
Documento de checkpoint. Docs-only. Não altera código, catálogo nem contadores.

## 1. Baseline 5.1.1 fixada

| item | valor |
|---|---|
| Tag publicada | `v5.1.1` |
| Commit exacto | `a0da90f50abccb8d80a14136263d549dce257837` |
| Data da tag | 2026-10-15 09:48:18 +0100 |
| Assunto | `version: bump to 5.1.1 and regenerate documentation` |
| Tag anterior | `v5.1.0` @ `1843b3217f3054f4f5807f8a03f40f313a239929` (2026-10-15 08:44) |
| Engine version | 5.1.1 (`Cargo.toml:41`, lido via `env!("CARGO_PKG_VERSION")`) |
| Catalog version | 1.4.0 (`crates/contexts/delonix-compute/src/capability.rs:39` — `CATALOG_VERSION`) |
| Binário em uso | `target/release/delonix` (38 MB, compilado 2026-10-24) e `~/.local/bin/delonix`; ambos reportam `delonix 5.1.1` |

**Relação develop ↔ baseline (orientação do dono: a branch-baseline de trabalho é a mais recente):**

- `git rev-list --left-right --count v5.1.1...develop` → **0 atrás, 258 à frente**.
- `git merge-base v5.1.1 develop` → `a0da90f5` (a própria tag).

Ou seja `develop` é um **descendente linear limpo** da v5.1.1: nenhum rebase/reset reescreveu o histórico da baseline. A tag v5.1.1 permanece como âncora histórica imutável; o trabalho da campanha assenta em `develop` (HEAD `558e5f20` no arranque) e cada frente nova nasce de `develop` por worktree, preservando a rastreabilidade à v5.1.1 (base comum = a tag). **Não se faz reset das branches principais nem se substitui a tag.**

## 2. Matriz regenerada a partir do binário correcto

Gerada com o binário 5.1.1-descendente, não editando strings de versão:

- Vista DECLARADA (host assumido completo, é o que o Desktop publica):
  `delonix provider matrix` → `matrix-declared-5.1.1-descendant.md`.
- Vista MEDIDA neste host (rootless, cgroup2 delegado):
  `delonix provider ls -o json` → importada para `matrix-structured.json`.

`matrix-structured.json` é a importação **lossless**: 515 linhas, chave `provider|capability`, preservando `state`, `justification` (o `detail`/`evidence` do catálogo), `test_ref` (prefixo `check:`/`e2e:`/`live:`/`test:`/`chaos:` quando presente), `domain`, `kind` e `supported`. O ADR citado vive na docstring da variante (ex. ADR-0050) e o "source line" é a identidade da variante Rust no `match` por provider (não é campo serializado — ver §4).

## 3. Reconciliação contra o relatório recebido

Totais do relatório (export do Desktop) vs. medido em develop HEAD:

| estado | relatório | medido @ develop | Δ |
|---|---|---|---|
| supported | 103 | **112** | +9 |
| partial | 93 | **86** | −7 |
| not-implemented | 74 | **72** | −2 |
| unsupported-by-provider | 230 | 230 | 0 |
| requires-external-component | 12 | 12 | 0 |
| unavailable-on-host | 3 | 3 | 0 |
| **total** | **515** | **515** | **0** |

**Diff explicável e conservativo.** O total (515) é idêntico e os três estados estruturais (unsupported-by-provider, requires-external-component, unavailable-on-host) batem exactamente — confirma que o catálogo 1.4.0 e o conjunto de providers são os mesmos do relatório. A única diferença é interna aos estados "accionáveis": **+9 supported, −7 partial, −2 not-implemented, soma = 0**. Nenhuma entrada apareceu nem desapareceu.

Causa: os 258 commits que `develop` ganhou desde a tag v5.1.1 **promoveram 9 células** (partial/not-implemented → supported) ao adicionar implementação + evidência de teste real. O relatório foi exportado de um build anterior à v5.1.1 ou da própria tag; `develop` está à frente. A identificação célula-a-célula das 9 promoções fica para o Sprint final (regressão/inventário), cruzando este JSON com um export do relatório original quando o ficheiro `Pasted text(6).txt` for recuperado (não estava no disco nesta sessão — a matriz foi regenerada do código/binário, como o ponto 1.5 do mandato exige).

Nota metodológica: o relatório traz `unavailable-on-host=3`, estado que **só existe na vista medida** (nunca na declarada). Logo o relatório é um export MEDIDO num host da mesma classe deste (rootless, sem NFS/CIFS/WebDAV montáveis — os 3 `unavailable-on-host` são storage.linux dessas três famílias).

## 4. Modelo do catálogo (como "supported" é imposto)

Mapa do subsistema (ficheiros-chave):

- `crates/contexts/delonix-compute/src/capability.rs` — `enum CapabilityState` (6 variantes, `#[serde(tag="state", rename_all="kebab-case")]`, linhas 988-1019), `enum Capability` + `Capability::ALL` (catálogo/denominador), `ProviderReport::build` (percorre `ALL`, `match` sem wildcard → esquecer uma entrada não compila), métrica de células `CellMetric`.
- `bins/delonix-runtime-bin/src/cmd/provider.rs` — CLI `provider ls|describe|matrix|config`; `declared_reports()` e `measured_reports()`; gerador Markdown; teste de não-drift `the_published_matrix_is_the_generated_one`.
- Classificação por provider (um `match` de estados cada): cloud-hypervisor `delonix-provider-cloud-hypervisor/src/lib.rs:265`, libvirt `delonix-provider-libvirt/src/lib.rs:301`, proxmox `delonix-proxmox/src/lib.rs:7359` (+ network `:7131`), linux `delonix-linux/src/provider_report.rs:47`, sdn `delonix-sdn/src/provider_report.rs:51`, volume `delonix-volume/src/provider_report.rs:38`, opnsense `delonix-opnsense/src/capabilities.rs:20`.
- Endpoint do Desktop: `GET /v1/providers` (gRPC `ListProviders`), `crates/interfaces/delonix-node-api/src/{service.rs,providers.rs}`; `catalog_version` no contrato.
- Gate: `scripts/capability_ratchet.py` + `scripts/capability_baseline.json` (`{"proved":112,"applicable":273}`) + `docs/providers/capability-matrix.md` (artefacto publicado).

**Guard-rail que satisfaz o critério #16 do mandato por construção:** uma célula só serializa `supported` se o braço do `match` devolver `Supported { evidence: "..." }` **e** a string de evidência apontar uma battery/section/test/live/chaos que um teste da crate do bin verifica existir. Portanto é **impossível** promover uma capability a `supported` só mudando a etiqueta — exige backend real + evidência rastreável. A campanha trabalha a favor deste invariante, não contra ele.

## 5. Limite do catálogo: sem provider OpenStack (ponto 13)

Confirmado: os providers do catálogo 1.4.0 são **5** — `cloud-hypervisor`, `libvirt`, `proxmox`, `linux`, `opnsense`. Não existe provider `compute/openstack`. Não é filtro do Desktop nem omissão do relatório: não há adapter OpenStack registado em `declared_reports()`/`measured_reports()` nem crate correspondente. Classificação para o inventário: **ausência de implementação na 5.1.1** (não é regressão nem feature-flag). Fica registado como decisão de âmbito; implementar um provider OpenStack é uma sprint própria de maior dimensão, fora do fecho de lacunas das entradas existentes.

## 6. Artefactos deste checkpoint

- `matrix-structured.json` — importação lossless das 515 entradas (fonte-de-verdade da campanha).
- `matrix-declared-5.1.1-descendant.md` — snapshot da matriz declarada regenerada.
- `00-baseline-reconciliation.md` — este documento.
- `01-sprint-plan.md` — plano de sprints por frente, com especialistas e critérios verificáveis.
- `README.md` — índice + checkpoint retomável.

# Relatório de nomes — Delonix Runtime (`cebf895`)

## O número que engana

O gate `scripts/lang_ratchet.py` conta **1 039 identificadores** ainda em
português e está alinhado com a linha de base. Isso lê-se como «resolvido» e
não é. Mas a distribuição, quando se separa produção de teste, muda a
prioridade por completo:

| Classe | Nº | Severidade |
|---|---|---|
| Nomes de teste (`#[cfg(test)]`, `tests/`) | **985** | LOW |
| Identificadores em código de produção | **54** | ver abaixo |

Método: para cada ocorrência, contaram-se as chavetas a partir de cada
`#[cfg(test)]` para delimitar o módulo de teste, e verificou-se se as 4 linhas
anteriores trazem `#[test]`/`#[tokio::test]`. Script em
`scratchpad/classify.py`.

## NAMING-0001 — Zero identificadores PÚBLICOS em português — **PASSA**

Dos 54 de produção, **um único** é `pub`: `pub fn cap_num` — e é falso
positivo (`num` é abreviatura inglesa de *number*, não português).

Consequência prática: **a Regra de Ouro §1 não está violada em nenhum contrato
público.** Nenhum consumidor externo — CLI, API, SDK, manifesto — vê um nome em
português. Isto é a diferença entre dívida cosmética e dívida de compatibilidade,
e aqui é cosmética.

## NAMING-0002 — Três itens NOMEADOS privados em português — LOW

| Ficheiro | Item | Proposta |
|---|---|---|
| `crates/delonix-net/src/infra.rs:4457` | `fn netdef_path_legado` | `fn legacy_netdef_path` |
| `crates/delonix-runtime/src/lib.rs:4086` | `static AVISO` | `static WARNED_ONCE` |
| `crates/delonix-runtime-bin/src/cmd/dockerapi.rs:1188` | `const CONSUMIDOS_TOPO` | `const CONSUMED_TOP_LEVEL` |

Todos privados. Mudança segura (§97): não há impacto em serialização, config,
CLI nem API. Falsos positivos do léxico a NÃO tocar: `fn todo<T>` (inglês
*to-do*), `fn topo_sort` (topológico), `enum StringOrNum`, `pub fn cap_num`.

## NAMING-0003 — ~44 ligações locais em português — LOW

`let alvos`, `let regra`, `let trinco`, `let legado`, `let erros`, `let espera`,
`let estado`, `let pedido`, `let nomes`, `let vivos`, `let orfaos`, …
Concentradas em `delonix-net/src/infra.rs` (12) e
`delonix-runtime-bin/src/cmd/kindmode.rs` (8). Auto-corrigíveis por §97;
lista completa em `scratchpad/ids_prod.txt`.

## NAMING-0004 — 985 nomes de teste em português — LOW, mas é a dívida real

Exemplos:

```
crates/delonix-cri/src/cap_ceiling.rs:288: valor_invalido_e_erro_nunca_tecto_vazio
crates/delonix-cri/src/runtime_svc/lifecycle.rs:1958: ceiling_reduces_so_avisa_quando_um_pedido_explicito_foi_cortado
```

Contra §59: os nomes **descrevem comportamento** e são bons nomes de teste —
falham só na língua. É por isso que valem LOW e não MEDIUM: o dano é de
consistência, não de compreensão. É também 95 % de toda a dívida de
identificadores, e é o que faz o número 1 039 parecer alarmante.

## NAMING-0005 — Convenções Rust (§6) — **PASSA**

`cargo clippy --workspace --all-targets` corrido agora: **zero avisos**.
As lints `non_snake_case`, `non_camel_case_types` e `non_upper_case_globals`
estão activas por omissão, portanto §6 está provado por execução, não por
inspecção.

## NAMING-0006 — `cmd/util.rs` — LOW (§46, §47)

Um único módulo de nome vago em 147 ficheiros. Não está a crescer descontrolado.
Recomendação: classificar o conteúdo e realojá-lo quando alguém lhe tocar; não
justifica uma mudança dedicada.

## NAMING-0007 — Nome de provider dentro da lógica — MEDIUM (§43)

```
crates/delonix-runtime-bin/src/cmd/conditions.rs:319: if backend == "libvirt" {
crates/delonix-vm/src/lib.rs:3981: net_mode: (vm.backend == "libvirt").then(|| vm.tap.clone())
```

O porto `VmBackend` existe (ver `architecture-report.md`), mas estes dois sítios
ramificam por **string do nome do provider** em vez de por capacidade do porto.
São exactamente a fuga que a `ngolacloud-arch` regista como ARCH-002, vista do
lado do motor.

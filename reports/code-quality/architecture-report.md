# Relatório de arquitectura — Delonix Runtime (`cebf895`)

## ARCH-0001 — Três traits em 138 500 linhas — HIGH (§26, §38, §39)

Medido: o workspace inteiro declara **3 traits**, dos quais 2 são públicos.

```
crates/delonix-vm/src/lib.rs:601        pub trait VmBackend        (16 métodos)
crates/delonix-runtime-bin/.../workload.rs:244  pub trait ComputeDriver (5 métodos)
crates/delonix-runtime-bin/src/cmd/mapped.rs:690    trait RtIno      (privado)
```

Leitura honesta, e é dupla:

**A parte boa.** `VmBackend` é um porto a sério, com adaptadores reais:

```
VmBackend  ←  CloudHypervisorBackend   (delonix-vm)
           ←  LibvirtBackend           (delonix-vm)
           ←  ProxmoxBackend           (delonix-proxmox)
           ←  FakeBackend/Counting/…   (testes)
```

E a direcção de dependência está **certa**: `delonix-proxmox` depende de
`delonix-vm` (adaptador → porto), nunca o contrário. Isto é Ports & Adapters
correcto e passa §38 no domínio de VM. `ComputeDriver` repete o padrão para
container-vs-VM, com `FakeDriver` nos testes.

**A parte por dizer.** Fora destes dois domínios não há abstracção nenhuma.
Rede (14 740 linhas), imagem (5 724), CRI (5 771), volume (1 890), scan e
segurança são módulos concretos de funções livres. Não há
`NetworkProvider`, `StorageProvider`, `ImageProvider`. A pergunta de fecho da
`ngolacloud-arch` — «se o Proxmox for substituído amanhã, mudam só os
providers?» — tem, no motor, a resposta: **muda o backend de VM e mais nada
precisa de mudar; mas trocar o plano de rede ou de armazenamento é reescrita,
não substituição de adaptador.**

Recomendação: **não** criar traits especulativos (§36 — YAGNI). O porto de rede
só se justifica quando existir um segundo caminho de pacote a sério; hoje há um.
O que se deve fazer já é registar isto por escrito como limite conhecido, não
descobri-lo outra vez daqui a seis meses.

## ARCH-0002 — `container_init` com 37 parâmetros posicionais — HIGH (§49)

`crates/delonix-runtime/src/lib.rs:2546`

37 parâmetros posicionais, com `#[allow(clippy::too_many_arguments)]`. Entre
eles `bool` adjacentes e intermutáveis à vista do compilador:
`read_only`, `seccomp_unconfined`, `seccomp_detect`, `no_new_privs`,
`has_own_netns`, `host_pid`, `inherit_userns`, `privileged`, `node_cgroup`.
Trocar dois numa chamada compila e **desliga uma barreira de segurança em
silêncio**. É por isto que sobe a HIGH e não fica em MEDIUM com os outros
19 casos de `>8` parâmetros.

O repo já sabe — há um FIXME etiquetado por cima (§16, bem escrito):

```rust
// FIXME(follow-up): 30 positional arguments — a real smell. Refactor to a typed
// `ContainerInitSpec` (groups rootfs/hostname/argv/limits/flags) in a
// dedicated, reviewed change; do not mix with the lint gate.
```

Duas notas: (a) a proposta do FIXME é a correcta e mantém-se; (b) **o número no
comentário está errado — são 37, não 30** (§14, comentário desactualizado), o
que sugere que a assinatura cresceu depois do FIXME ser escrito. É a prova de
que um FIXME sem portão não trava crescimento.

## ARCH-0003 — Campos de manifesto aceites e ignorados — MEDIUM

`crates/delonix-runtime-bin/src/cmd/httproute.rs`, `kind: Ingress`:

| Campo | Linha | Razão escrita? |
|---|---|---|
| `spec.ingressClassName` | 476 | **sim** — «the embedded proxy is the only ingress class» |
| `spec.rules[].http.paths[].pathType` | 504 | **sim** — «`Exact` is accepted but treated as prefix» |
| `spec.rules[].…backend.service.port.name` | 526 | **sim** — e emite erro claro a pedir `port.number` |
| `spec.tls[].hosts` | 591 | **NÃO** — `#[allow(dead_code)]` sem uma palavra |

Três dos quatro cumprem a regra da casa («um campo que o cliente escreve e o
sistema ignora é pior do que um campo que não existe») porque *dizem-no*. O
quarto não: `tls.hosts` é uma lista de nomes SNI que o utilizador escreve e o
motor deita fora sem aviso — serve sempre um certificado só (o primeiro
elemento de `tls`). Um utilizador com dois hosts na lista recebe TLS partido
para o segundo e não é avisado em lado nenhum.

Nota separada sobre `pathType: Exact`: está documentado no código, mas o
utilizador que escreve `Exact` continua a receber correspondência por **prefixo**
— encaminha tráfego que ele não pediu. Documentar não é avisar; devia sair um
aviso no `apply`.

Positivo a registar: o `kind: Ingress` **tem** lista de campos conhecidos
(`INGRESS_SPEC_FIELDS`, usada por `manifest.rs:422` e `schema.rs:218`), ao
contrário do compose, que segundo o `CLAUDE.md` engole chaves desconhecidas em
silêncio por lhe faltar `deny_unknown_fields`.

## ARCH-0004 — `Container`: 71 campos, 8 `bool` — MEDIUM (§31, §41)

`crates/delonix-runtime-core/src/lib.rs:528`. É o objecto-deus do modelo de
domínio: identidade, imagem, rede, limites, segurança e estado no mesmo tipo,
serializado para disco. Atenuantes reais: **está documentado campo a campo**, e
é o registo persistido — parti-lo é migração de estado (§80), não refactorização
cosmética. Não recomendo mexer sem ADR.

## ARCH-0005 — Direcção de dependências — **PASSA** (§90, §91)

Grafo declarado nos `Cargo.toml`, verificado:

```
delonix-runtime-core  ← toda a gente (fundação, não depende de ninguém)
delonix-vm            ← delonix-proxmox            (porto ← adaptador) ✔
delonix-net           ← delonix-runtime, -cri, -bin
delonix-runtime-bin   → todos os 13                (a CLI é o topo)     ✔
```

Nenhuma inversão: nenhum crate de domínio depende da CLI, nenhum porto depende
de um adaptador, e `delonix-proxmox` não é alcançado por ninguém a não ser o
binário e o crate do porto. A única menção a «Proxmox» dentro de `delonix-vm`
é em **documentação e mensagens de erro** — deliberada, e das melhores que li
neste repo:

```rust
/// The distinction is not pedantry. `delonix-proxmox` exists in this workspace
/// and implements the trait — answering `--backend proxmox` with «unknown
/// backend» would be a lie.
```

## ARCH-0006 — Testes de arquitectura executáveis — **PASSA** (§66, §67, §85)

`crates/delonix-runtime-bin/tests/architecture.rs` verifica que
`ARCHITECTURE.md` e `AGENTS.md` **nomeiam todos os crates que existem em disco**
e que a contagem declarada é a real. É a única defesa que vi neste workspace
contra documentação de arquitectura a apodrecer, e é exactamente o que §85 pede.
Isto é o que empurra a nota de legibilidade-por-agentes para 88.

## Padrões identificados (§27, §95)

| Padrão | Local | Qualidade | Nota |
|---|---|---|---|
| Ports & Adapters / Provider | `delonix-vm::VmBackend` + `delonix-proxmox` | **GOOD** | 3 adaptadores reais, fakes nos testes, direcção certa |
| Strategy | `workload.rs::ComputeDriver` | **GOOD** | container vs VM atrás de um contrato |
| Adapter | `httproute.rs::ingress_to_httproute` | **GOOD** | traduz forma k8s → modelo interno |
| Newtype (§42) | — | **MISSING** | ids passam como `String` crua |
| Máquina de estados (§40) | — | **REVIEW** | estado por combinação de `bool` no `Container` |
| God Object (§31) | `Container` (71 campos) | **REVIEW** | atenuado por documentação e por ser estado persistido |

## Anti-padrões procurados e NÃO encontrados

- Código comentado (§54): **0** — 40 candidatos brutos, todos falsos positivos
  (continuações de prosa inglesa a começar por `for`/`while`/`if`).
- `AbstractXFactory` / Java-em-Rust (§30): **0**.
- Blanket `#![allow]` injustificado (§88): **0**. Os dois que existem têm razão
  escrita — `result_large_err` (o `Status` do tonic é grande por natureza) e
  `clippy::all` limitado ao módulo de stubs gerados pelo protobuf.
- TODO/FIXME nus (§15): **0** reais — 3 no repo inteiro, 2 etiquetados, e o
  «terceiro» é a palavra portuguesa «TODOS» num comentário.

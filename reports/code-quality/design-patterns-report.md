# Catálogo de padrões — Delonix Runtime (`cebf895`)

Para `docs/architecture/design-patterns.md` (§71), se se quiser promover.

## Ports & Adapters — `VmBackend` — **GOOD**

**Porto:** `crates/delonix-vm/src/lib.rs:601`, 16 métodos.
**Adaptadores:** `CloudHypervisorBackend` e `LibvirtBackend` (`delonix-vm`),
`ProxmoxBackend` (`delonix-proxmox`).
**Fakes:** `FakeBackend`, `FailingRemote`, `Counting`, `OnlyStop`, `Resumable`.

**Razão:** normaliza o ciclo de vida de uma VM sobre três hipervisores muito
diferentes.
**Direcção:** adaptador → porto (`delonix-proxmox` depende de `delonix-vm`). ✔
**Prova de que o porto é real:** cinco fakes diferentes nos testes. Um porto que
ninguém consegue falsificar não é um porto; este consegue-se.
**Recomendação:** manter. Não engordar o trait — 16 métodos já está no limite
de §32.

## Strategy — `ComputeDriver` — **GOOD**

`crates/delonix-runtime-bin/src/cmd/workload.rs:244`, 5 métodos,
implementado por `ContainerDriver` e `VmDriver` (+ `FakeDriver`).
Contrato pequeno e coeso — é o contra-exemplo saudável do god trait de §32.

## Adapter — tradução de esquema k8s — **GOOD**

`httproute.rs::ingress_to_httproute` converte `networking.k8s.io/v1 Ingress`
no `HttpRouteSpec` interno. Fronteira bem posta: a forma externa não contamina
o modelo interno. Ressalva em ARCH-0003 (campos aceites e ignorados).

## Newtype (§42) — **MISSING**

Não há `struct ContainerId(String)`, `struct NetworkId(String)`,
`struct TenantId(_)`. Identidades circulam como `String` crua por 138k linhas.
**Não recomendo** uma campanha de newtypes agora: o ganho concreto aparece
quando dois ids do mesmo tipo primitivo se podem trocar na mesma chamada, e o
sítio onde isso dói já está identificado — é a assinatura de 37 parâmetros de
ARCH-0002. Fazer newtypes lá, primeiro; generalizar só se pagar.

## Máquina de estados (§40) — **REVIEW**

`Container` (`delonix-runtime-core/src/lib.rs:528`) carrega 8 `bool` entre 71
campos em vez de um `enum` de estado. Combinações contraditórias são
representáveis. Atenuante: é o registo **persistido** — mudar isto é migração
de estado (§80), não limpeza. Requer ADR.

## God Object (§31) — `Container` — **REVIEW**

71 campos num tipo. Documentado campo a campo, o que o torna legível apesar do
tamanho. Ver acima: não se toca sem ADR.

## Reconciliation / Desired State — fora de âmbito desta medição

Existe (`cmd/reconcile.rs`, `stack apply`), mas não foi auditado neste passe.

# 54 — P2: o `container run` como caso de uso do Compute

**Data:** 2026-09-16 · **Base:** `origin/main` `db9efa3b` (v3.1.0 +78) · **ADR:** 0040 (P2, D3)

## A pergunta

O ADR-0040 põe no `delonix-compute` "a especificação de execução única" e os casos de uso
que a executam, e dá como critério de saída da P2 que cada caso de uso migrado tenha
**zero caminhos por subprocesso** no CRI, no mgmt e no MCP. Hoje o CRI arranca um container
montando o argv de `delonix container run` (`start_argv`) e executando o binário.

O `cmd_run` (`bins/delonix-runtime-bin/src/cmd/container.rs`) tem **1074 linhas** e depende
do binário inteiro. Não se move de uma vez: um contexto não pode depender de adapters
(`delonix-image`, `delonix-net`, `delonix-runtime`, `delonix-volume`). Este documento fixa as
fases, as portas e a ordem.

## O que já está feito

| PR | O quê |
|---|---|
| #340 | `RunOpts` (a especificação) no `delonix-compute` |
| #342 | tipos da forma de Pod e os seus tradutores; avisos como `Notice` |
| #344 | `preflight::check_run_opts` — as combinações que só lêem a especificação, antes de qualquer efeito |

## As fases do `cmd_run`, medidas

| # | Fase | Natureza | Depende de |
|---|---|---|---|
| 1 | política do nó (`policy::enforce`) | pura sobre um ficheiro lido | estado do nó |
| 2 | validar a especificação, expandir portas, recusar portas ocupadas | pura + sondas do host | `delonix-net` (parse), store (dono da porta) |
| 3 | re-exec para a netns de uma rede custom/pod | efeito | `infra`, processo |
| 4 | resolver volumes e CDI | I/O | `VolumeStore`, `/etc/cdi` |
| 5 | resolver/puxar a imagem, id, preparar o rootfs | I/O | `ImageStore` |
| 6 | **construir o registo `Container`** | **pura dado o que 4 e 5 resolveram** | nome livre (store), `--env-file`, `--user` contra o rootfs, perfil seccomp, segredos |
| 7 | anexar à rede/pod, publicar portas | efeito | `infra`, slirp |
| 8 | montar o `RunSpec` | puro, mas o tipo é do adapter | `delonix-runtime` |
| 9 | criar ou supervisionar | efeito | `runtime::create_with`, fork |
| 10 | pós-arranque (`--rm`, id, `--wait`) | efeito + terminal | store, output |

A fase 6 é a maior porção de regra de negócio (nome por omissão e a sua validação, limites
da casa e a hierarquia do kubelet, env da imagem + `--env-file` + `-e`, labels, userns,
`--security-opt`, logs, DNS, knows) e hoje está entrelaçada com I/O pontual.

## As portas

Os contextos-alvo (`networking`, `artifact`, `storage`) ainda não existem. As portas nascem
no `delonix-compute`, com o nome que o ADR lhes dá, e mudam de casa quando o contexto
existir — um `pub use` no Compute mantém os chamadores.

| Porta | Casa final (ADR) | Implementação hoje | Fases |
|---|---|---|---|
| `ImageStore` (resolver config + preparar rootfs) | `delonix-artifact` | `delonix-image::ImageStore` | 5 |
| `StorageProvider` (resolver `-v`) | `delonix-storage` | `delonix-volume::VolumeStore` | 4 |
| `NetworkProvider` (attach, publish, shaping, firewall) | `delonix-networking` | `delonix-net::infra` | 3, 7 |
| `WorkloadRuntime` (criar, supervisionar, remover) | `delonix-compute` | `delonix-runtime` | 9 |
| `ContainerRecords` (listar, gravar) | `delonix-compute` | `delonix-runtime-core::Store` | 2, 6, 9 |

O `RunSpec` (fase 8) é o contrato de `WorkloadRuntime` e fica do lado do adapter: o caso de
uso entrega um `Container` completo mais o que só existe no momento de arrancar (rootfs,
netns a juntar, hooks), e o adapter monta o `RunSpec`.

## Ordem de migração

Cada passo é um PR, sem mudança de comportamento salvo defeito medido, e com a bateria E2E
de `container run` a passar.

1. **Construtor do registo (fase 6) puro** — `compute::run::build_record(opts, ResolvedRun)`
   onde `ResolvedRun` traz o que as fases 4–5 e as leituras pontuais já resolveram (config
   da imagem, mounts, conteúdo dos `--env-file`, uid/gid do `--user`, nomes existentes no
   namespace, o id, o perfil seccomp lido, os defaults de memória/CPU, `rootless`). O
   `cmd_run` passa a: resolver → `build_record` → efeitos. Testável sem host.
2. **Portas de leitura** (`ImageStore`, `StorageProvider`, `ContainerRecords`) e a resolução
   de `ResolvedRun` como função do Compute sobre elas.
3. **`NetworkProvider` e `WorkloadRuntime`**, e o caso de uso `run` inteiro no Compute; o
   `cmd_run` fica com o terminal (id, avisos, `--wait`) e com a composição dos adapters.
4. **CRI em processo**: `start_container` constrói `RunOpts` e chama o caso de uso em vez de
   `start_argv` + subprocesso. Critério da P6, que este passo antecipa só para `run`.

## Riscos

- **O re-exec em rede custom/pod** (fase 3) corre o `cmd_run` duas vezes em processos
  diferentes; o caso de uso tem de ser idempotente entre as passagens (já é: o id viaja em
  `DELONIX_REEXEC_ID`). Nenhum passo acima muda onde o re-exec acontece.
- **O supervisor faz `fork` cru** e assume um chamador single-threaded; o CRI é
  multi-thread. O passo 4 herda a restrição que a Docker API já resolveu por re-exec
  (`__apirun`) — não se remove sem medir.
- **A mesma classe de defeito que o #344 corrigiu** ("aplicado só depois do retorno do
  supervisor") pode existir noutros campos; o passo 3 junta as fases de efeito num só
  sítio, e é aí que se revê.

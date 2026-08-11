---
name: delonix-runtime-container
description: Domínio do motor de CONTAINERS do delonix — clone/namespaces/cgroups/capabilities rootless, o ciclo de vida (run/exec/update/start), e as armadilhas que este repo já pagou nesse caminho. Usa quando mexeres em `crates/delonix-runtime` (`spawn`, `container_init`, `exec`, cgroups, seccomp, capabilities), no grupo `delonix container` da CLI, ou quando avaliares paridade com Docker/Podman a nível de container.
---

# Motor de containers — o que este código já sabe, e o que já custou

Este ficheiro existe para uma pessoa nova não voltar a pagar o que já foi pago.
**Nada aqui é teoria**: cada regra veio de um bug que este repositório teve.

## Antes de tocar em código: onde as coisas estão

- `crates/delonix-runtime/src/lib.rs` — o motor. **118 blocos `unsafe`** e a
  `spawn()` tem ~405 linhas cobrindo hostname/argv, pty/socketpair, flags de
  clone, o `clone()`, o handshake de userns, o fork do shim de logs, o hook de
  rede, o cgroup e o `Store::save`. Está marcada como risco de manutenção no
  `CLAUDE.md` e continua assim.
- `crates/delonix-runtime-bin/src/cmd/container.rs` — a CLI.
- O comentário **`// CRITICAL ORDER`** dentro do `spawn` descreve um deadlock
  que já existiu. Reordenar os blocos à volta dele reintroduz um bug que ninguém
  vai reproduzir num teste.

## As armadilhas, e são todas reais

**`container.userns` NÃO é «está num userns diferente do meu».** Diz se o
container CRIOU o seu; os do ingress rootless HERDAM o do holder e ficam
`false` estando noutro. Guardar um `setns` por esse campo fez o `mount_live`
falhar inteiro com EPERM (código 124) — o mesmo bug que o `exec` já tivera. Abre
sempre o ns `user` e deixa o skip-por-inode do `open_container_ns` decidir.

**Cgroup: `container.cgroup()` é a fórmula ESTÁTICA (`delonix.slice/...`), só
válida como root.** Em rootless delegado o cgroup real está algures sob
`.../dlx-containers/dlx-<id>`, descoberto em runtime via `/proc/<pid>/cgroup` —
é a razão de existir do `live_cgroup()`. O `update_limits` usava a estática: o
comando dizia «actualizado», o registo mudava, e o cgroup do processo a correr
ficava intocado. Para QUALQUER operação a quente, `live_cgroup`.

**Limites de recurso exigem cgroup delegado.** Numa sessão SSH normal, `-m` e
`--cpus` são inertes — o scope da sessão é IRMÃO de `user@<uid>.service` e a
migração exige escrever no cgroup da root. `systemd-run --user --scope -p
Delegate=yes` é o remédio, e `cgroup.controllers` conter `memory` **não** prova
delegação (o cgroup raiz contém-no sempre): o que discrimina é a POSSE do
`cgroup.subtree_control`.

**Estado necessário para RECONSTRUIR tem de ser persistido, não só usado na
criação.** Custou quatro vezes: `-v` nunca era gravado (um `start` perdia os
volumes em silêncio e escrevia no rootfs), `-p` em rede custom, as redes extra,
e `Container.pod`. Ao mexeres num caminho de `start`/`restart`, compara campo a
campo o que a criação USA com o que o registo GUARDA.

**`c.ip` não é «o endereço do container» — é «o endereço na rede primária».**
Com `--net-connect` há um segundo, e ele foi invisível para a firewall e para o
isolamento de namespace até a v0.42. Sempre que uma função de rede receber UM
ip vindo do registo, pergunta o que acontece com `extra_networks` não vazio.

**Capabilities**: `resolve_cap_keep`/`KEPT_CAPS` são a fonte única (agora
públicas, para o tecto do CRI não manter uma segunda tabela). `privileged`
IGNORA o `cap_drop` por inteiro — um teste que modele `privileged` como
`drop ALL` codifica um bug, não o comportamento.

## O que fazer sempre

1. **Fail-closed, nunca aceite-e-ignorado.** Este repo já corrigiu isto quatro
   vezes (`--security-opt seccomp=<perfil>`, `-v …:z`, `--network-alias`,
   `--namespace` em libvirt). Se uma opção não é implementável, RECUSA a nomear
   a alternativa.
2. **Exit codes com verdade.** Um container `-d` sem `--restart` não tem o seu
   código capturável — o motor não é o pai real. A resposta é `Exited (unknown)`
   e um `wait` que explica, não um 137 fabricado.
3. **Valida ao vivo.** `cargo test` não alcança namespaces, cgroups nem
   capabilities. A prova é o kernel: `CapEff` em `/proc/<pid>/status`,
   `memory.max` no cgroup real, o ficheiro do host visível dentro do container
   ANTES e DEPOIS de um `stop`+`start`.
4. Corre a skill `delonix-testing` para a disciplina, e a `delonix-runtime-sec`
   se mexeres em fronteira de privilégio (userns, caps, mounts, seccomp).

## Paridade Docker/Podman — o que decide a adopção

Ler `docs/COMPARACAO-DOCKER-PODMAN.md` antes de propor uma feature «que o Docker
tem»: metade já cá está com outro nome. O que este motor tem e eles não —
**reconfigurar portas, volumes, redes, memória e CPU a QUENTE sem mudar o PID**
(`container update`) — é o argumento mais forte que existe, porque no Docker
mudar uma porta obriga a recriar. Uma feature que quebre essa propriedade custa
mais do que traz.

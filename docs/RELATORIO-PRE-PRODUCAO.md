# Relatório pré-produção — CLI `delonix`

**Data**: 2026-07-17 · **Binário**: `delonix 0.3.0` (build local, `target/debug`)
**Bateria**: [`scripts/e2e.sh`](../scripts/e2e.sh) — 140 verificações sobre a CLI real, com
containers/redes/volumes a sério. **Resultado: 139 PASS · 1 FAIL · 0 SKIP.**

O `e2e.sh` cobre: `--help` de todos os grupos **e de cada subcomando** (descoberto por parsing do
próprio help, portanto não fica desactualizado sozinho), todos os comandos de leitura, um bloco de
**erros esperados** (a CLI tem de RECUSAR, não aceitar em silêncio), ciclo de vida de
volume/network/container, hot reconfig nos dois caminhos de rede, e `stack apply`/`describe`.

> **Regra que custou horas noutras sessões e continua válida**: nunca testar com o `delonix` do
> `PATH`. Processos de longa duração correm o binário de quando NASCERAM, e um binário velho em
> `~/.local/bin` sabota testes de código novo. O `e2e.sh` usa o build local por omissão.

---

## 1. Bugs encontrados e CORRIGIDOS nesta sessão

| # | Severidade | Bug | Estado |
|---|---|---|---|
| 1 | **CRÍTICA** | **Toda a montagem a quente falhava.** `mount_live`/`unmount_live` gatavam o `setns(user)` em `container.userns`, mas esse campo diz se o container **criou** o seu userns — os do ingress rootless **herdam** o do holder e ficam com `userns=false` apesar de estarem num userns diferente. Sem o setns, o `unshare(NEWNS)` seguinte dava EPERM (código 124). Nunca tinha aparecido porque estas funções **não tinham um único chamador**. | Corrigido + validado com container real |
| 2 | **ALTA** | **`stop && start` falhava sempre.** O `unpublish_ports` fazia `return` no caminho slirp-por-container, afirmando que "o slirp morre com o netns do container". É falso: o slirp só sai quando NOTA que o netns desapareceu, e até lá segura a porta. Medido: **0/3 arranques** antes, **5/5** depois. | Corrigido (`reap_slirp_for`, cirúrgico e síncrono) |
| 3 | **ALTA** | **Os testes de regressão dos 4 CRÍTICOS de segurança estavam MORTOS.** `cargo test -p delonix-runtime-bin` não compilava há vários commits (26 erros em `cmd/cluster.rs`: as specs ganharam campos e os testes não acompanharam). O CLAUDE.md afirmava que esses testes existiam e corriam. | Corrigido — 61→72 testes a correr |
| 4 | **MÉDIA** | **`ssh @host` em vez de `ssh delonix@host`.** `SshSpec` derivava `Default`, logo um `cloud.yaml` **sem bloco `ssh:`** dava `user: ""` (o `#[serde(default = "…")]` só actua quando o bloco existe). | Corrigido + 2 testes de regressão |
| 5 | BAIXA | `image ls` mostrava **"1 weeks ago"** para 7–13 dias (código novo desta sessão). | Corrigido — porta o `HumanDuration` do docker à letra |

---

## 2. ⚠️ ACHADO PRINCIPAL — as portas publicadas morrem sozinhas (**e a culpa não é do runtime**)

**Este é o bloqueio nº1 para produção, e estava mal diagnosticado há várias sessões.**

### Sintoma
Uma porta publicada funciona (HTTP 200) e **~10–16s depois deixa de servir**, com o container ainda
`Running` e sem ninguém ter feito `stop`/`rm`.

### O que estava documentado (e está ERRADO)
O `CLAUDE.md` dizia *"as duas metades do `publish_port` (slirp_add_hostfwd + control_send) falham em
SILÊNCIO"* e mandava procurar quem chamava `unpublish_port`. **Ambas as premissas são falsas** — foi
por isso que a investigação nunca fechou.

### O que ficou PROVADO, por medição
1. **A metade do DNAT está sempre intacta** — `nft list table ip dlxing` mostra a regra na chain
   `pre` muito depois de o `curl` já dar `000`. Só o `hostfwd` do slirp desaparece.
2. **O `publish` FUNCIONA** — `list_hostfwd` mostra a entrada e o `curl` dá 200. Não falha em silêncio.
3. **Nenhum código do `delonix-runtime` o remove.** Instrumentei `unpublish_port`,
   `slirp_remove_hostfwd`, **todos** os comandos não-`list` do `slirp_api` (o que apanha o
   `remove_hostfwd` que o `reap_orphan_hostfwds` envia directamente, sem passar pelas funções
   nomeadas) e o `control_send`. **Zero ocorrências**, em todas as reproduções.
4. **O slirp não reinicia** e o **holder não reinicia** (mesmo pid). O `control_loop` do holder é
   puramente reactivo — não tem nada periódico.
5. **Um hostfwd adicionado À MÃO pelo api-socket, sem delonix nenhum envolvido, também desaparece**
   (16,51s). Logo não é o nosso caminho de publicação.
6. **Não é bug do slirp4netns** — um `slirp4netns` de sala limpa, com **exactamente as mesmas flags**,
   contra um alvo `unshare -r -n`, manteve o hostfwd os 33s todos.

### Causa-raiz (provada com SIGSTOP)

**É o `delonix-engine` — o PaaS privado — a reapar as portas do runtime.**

```
engines A CORRER   →  hostfwd criado a t=0,00s  ·  DESAPARECE a t=12,01s
engines CONGELADOS →  hostfwd criado a t=0,00s  ·  PERSISTE os 30s todos
(SIGSTOP/SIGCONT, sem matar nada; engines retomaram normalmente)
```

Dois factores que se somam, ambos em `delonix-paas`:

1. **`crates/delonix-api/src/ui.rs:12937`** — `delonix_net::infra::reap_orphan_hostfwds(&live)`. O
   `live` só contém os containers **do próprio PaaS**. Toda a porta publicada pela CLI do runtime é,
   para o engine, um "órfão" a limpar. Ele está a reapar estado que não é dele.
2. **`crates/delonix-api/Cargo.toml:15`** — `delonix-net = { git = "…delonix-runtime", tag = "v0.1.0" }`.
   O PaaS está preso à **tag v0.1.0**, ou seja à versão ANTIGA do reaper (a do bug fail-open:
   lista vazia ⇒ conclui que nada está em uso ⇒ apaga tudo). É por isso que remover o chamador no
   `delonix-runtime` (commit `9bbbd11`) não resolveu nada — a cópia que corre é outra.

Isto explica tudo o que não fazia sentido: porque é que o DNAT sobrevive (o reaper só toca em
hostfwds), porque é que o trace nunca disparou (o removedor é outro processo, de outro produto), e
porque é que a "tábua rasa" de uma sessão anterior não ajudou (o engine nunca entrou na limpeza).

### Recomendação
**Não é trabalho do `delonix-runtime`** — a correcção vive no `delonix-paas` e **não a fiz**
(regra de isolamento: um worktree, um produto). Sugestões, por ordem:

1. **`delonix-paas`**: o engine não pode reapar hostfwds que não criou. Ou o `live` passa a incluir
   tudo o que está publicado no ingress partilhado, ou o reaper deixa de correr de todo (foi essa a
   decisão tomada no runtime, em `9bbbd11`: reap **reactivo e cirúrgico**, só a porta que falha, só
   quando falha).
2. **`delonix-paas`**: subir o pin de `delonix-net` de `v0.1.0` para a versão actual.
3. **`delonix-runtime`** (defesa em profundidade): `reap_orphan_hostfwds` é hoje **código morto** —
   zero chamadores. Uma função pública que apaga estado partilhado e falha ABERTO com uma lista
   vazia é uma armadilha para qualquer consumidor. **Apagá-la**, ou dá-la a fail-closed.

---

## 3. Gaps e divergências em aberto (não corrigidos)

| # | Sev. | Achado | Nota |
|---|---|---|---|
| 1 | MÉDIA | **`ssh: { port: 2222 }` é aceite e ignorado** — o `SshSpec.port` é parseado e nunca lido (`SshTarget` nem tem o campo). O SSH vai sempre à 22. | Decidir: ligar ao `conn_args()` ou tirar do schema. É o **único FAIL** que deixei vermelho no e2e, de propósito. |
| 2 | MÉDIA | **`vm rm <inexistente>` devolve 0** e imprime o nome, como se tivesse removido. O `container rm` erra; o docker erra. | `delonix_vm::remove` é idempotente de propósito (limpa restos sem registo, serve o `cluster delete`). Mudar a CLI sem `--force` partiria esse caminho. **Decisão sua.** |
| 3 | MÉDIA | **Não há forma de recarregar o binário dos supervisores.** Um processo de longa duração corre o código de quando nasceu — em produção não há como aplicar um fix sem recriar containers. | Problema real de operação, não de teste. |
| 4 | BAIXA | `Container` não guarda `finished_at` nem exit code. O STATUS não pode dizer `Exited (0) 2 minutes ago` como o docker — só `Exited (0)`. | Preferi mostrar menos a fabricar um tempo do `created_unix`. |
| 5 | BAIXA | `vm describe <inexistente>` erra com **"no such container"** — substantivo errado, vem do store da VM. | |
| 6 | BAIXA | `image ls` SIZE = soma dos blobs no CAS = tamanho **comprimido em disco**. Não bate com o `docker images` (que mostra o rootfs descomprimido). | Documentado no código; os números são reais. |
| 7 | BAIXA | `ImageConfig` não faz parse do `ExposedPorts` do OCI → o `image describe` não os mostra. | Precisa de mudança no `delonix-image`. |
| 8 | BAIXA | `image --vm pull` perde os metadados (`Ubuntu`/`K8s` vêm `<unknown>`). | Já conhecido. |
| 9 | BAIXA | Rede órfã chamada **`dlx-`** (sufixo vazio) no store. | Parece nome truncado de um bug antigo. |
| 10 | BAIXA | `cmd_rm` duplica a lógica do `remove_container`. | Divergem no dia em que um mudar. |
| 11 | INFO | **`--net-connect`/`--net-rate` exigem `--net <rede>`** — o veth e o shaping vivem no netns do holder, que o caminho `--net host/none` não tem. | Por desenho. Erro explícito. |
| 12 | INFO | **`--publish-add` num container criado sem `-p` E sem `--net <rede>`** é impossível: o api-socket do slirp só é aberto quando o `run` leva portas. | Por desenho. Erro que ensina. |

Continuam válidos os gaps já registados no `CLAUDE.md` e não re-testados aqui: leak do refcount do
ingress, rootfs órfãos a encher o disco, `image --vm pull` sem metadados.

---

## 4. O que ficou validado a sério

**Hot reconfig — com container real, sem restart, PID idêntico do princípio ao fim:**

- `--publish-add` → HTTP **200** na porta nova, a quente.
- `--publish-rm` → porta fecha; as outras do mesmo container ficam intactas.
- `--volume-add` → montado a quente; ficheiro escrito **dentro** do container aparece no volume do
  host; `--volume-add …:ro` **recusa** escrita e permite leitura.
- `--volume-rm` → deixa de ser mountpoint dentro do container.
- `--net-connect`/`--net-disconnect`/`--net-rate`/`--net-rate-clear` → todos PASS pelo ingress.
- Todos os erros esperados **recusam** (porta duplicada, destino duplicado, rede já ligada, taxa
  inválida, container parado).

**Veredicto**: as funcionalidades desta sessão estão prontas. O que **bloqueia produção não está
neste repo** — está no reaper do `delonix-paas` (secção 2), e é uma correcção de uma linha de
política num produto que não é este.

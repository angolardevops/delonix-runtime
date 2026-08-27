# Fase CLI-0 — inventário, matriz de destinos e lacunas

> Medido a **2026-08-26** contra `origin/main` (`a3e7fa1`, v0.63.1), com o binário
> construído desse commit — **não** com o `delonix` instalado no host, que estava
> quatro dias atrasado e não tinha `namespace`, `net l4guard`, `stack history`,
> `stack rollback`, `system doctor` nem `system features`. Medir o binário à mão
> teria dado um inventário com seis comandos a menos e uma matriz que os deixava
> sem destino sem ninguém dar por isso.

## 1. A superfície actual, em números

| medida | valor |
|---|---|
| comandos na árvore pública | **263** |
| folhas invocáveis | **233** |
| grupos de topo | **28** |

Reproduzir:

```bash
scripts/cli-tree.sh --count      # os totais desta secção
scripts/cli-tree.sh --leaves     # as 233 folhas
scripts/cli-tree.sh --classify   # folha + classe de impacto (secção 4)
```

O script usa o binário da árvore e **diz qual** na saída — a primeira medição
desta fase foi feita contra o `delonix` do `PATH` e ficou seis comandos
atrasada.

### Duplicação de verbo, contada e não afirmada

O ponto §2 da especificação pede confirmação de que há sobreposição. Há, e o
número é este — a contagem do segundo token de cada folha:

| verbo | ocorrências | onde |
|---|---|---|
| `ls` | **12** | cluster, container(`ps`), image, image vm, namespace, network, pod, secret, sharevolume, stack, storage, vm, volumes, workload |
| `apply` | **10** | cluster, container, image, network, secret, sharevolume, stack, storage, vm, volumes |
| `describe` | **10** | container, image vm, namespace, network, pod, sharevolume, stack, vm, volumes, workload |
| `rm` | **10** | container, image, image vm, pod, secret, sharevolume, storage, vm, volumes, workload |
| `prune` | **7** | cluster, container, image, stack, system, vm, volumes |
| `create` | **7** | network, pod, secret, storage, volumes, vm snapshot, volumes snapshot |
| `inspect` | **5** | container, image(`describe`), network, secret, storage, volumes |
| `init` | **5** | cluster, container, image, image vm, stack, vm (+ o `init` de topo = 7) |
| `dash` | **5** | container, image, network, storage, vm (+ o `dash` de topo = 6) |

Nove verbos declarativos repetidos dez vezes cada é a medida exacta do problema
que a especificação existe para fechar: **o CRUD de cada Kind está escrito uma
vez por grupo**, e nada obriga as dez cópias a concordar. É a mesma família do
defeito que o `cmd/kinds.rs` já corrigiu do lado do reconciliador — seis listas
que tinham de concordar e derivaram — só que aqui está na superfície pública.

### Os onze pontos do §2, verificados um a um

| # | alegação | verificado | evidência |
|---|---|---|---|
| 1 | sobreposição `container`/`pod`/`vm`/`workload` | **sim** | `workload ls` lista containers E vms; `pod ls`/`vm ls`/`container ps` listam os mesmos objectos por outra porta |
| 2 | sobreposição `volumes`/`storage`/`sharevolume` | **sim** | três grupos, 26 folhas, um só `kind: Volume` por baixo desde a 5.ª fusão (v0.53.x) |
| 3 | sobreposição `network`/`net` | **sim** | 13 + 45 folhas; `network route` e `net ingress` actuam no mesmo dataplane |
| 4 | `build` separado de `image` | **sim** | `delonix build` e `delonix image build` coexistem |
| 5 | `image --vm` muda de store | **sim** | flag global que troca `ImageStore`↔`VmImageStore`; e há **três** portas para o mesmo (`vm pull`, `image vm pull`, `image --vm pull`) |
| 6 | CRUD duplicado grupo↔`stack` | **sim** | 10 `apply` por-grupo contra `stack apply` |
| 7 | `schema`/`explain` fora da gestão de manifestos | **sim** | `schema print` e `explain` são grupos de topo |
| 8 | `backup`/`restore` em grupos distintos | **sim, e pior** | são **quatro** portas: `backup`, `restore`, `system backup`, `system restore` |
| 9 | `dash` global e repetido | **sim** | 6 ocorrências |
| 10 | aliases de argv no topo | **sim** | `ps`/`run`/`exec`/`logs`/`rm`/`images`, por reescrita de `argv` antes do `clap` |
| 11 | `ls`/`ps`/`get`/`status`/`inspect`/`describe` divergentes | **sim** | `vm status` e `vm describe` respondem à mesma pergunta com formatos diferentes |

O ponto 8 é o único onde a especificação **subestimou** o problema: listava dois
grupos e são quatro.

## 2. Conflitos com contratos JÁ PUBLICADOS

A especificação diz que prevalece sobre a CLI antiga. Prevalece — mas três das
mudanças não são só reorganização: **quebram promessas escritas** em
`docs/cli-stability.md`, que existe precisamente para dizer o que não parte. Ou
o documento é revisto no mesmo commit, ou a promessa passa a mentir.

### 2.1 Os atalhos de topo estão declarados ESTÁVEIS

`docs/cli-stability.md`, secção «Estável — não quebra sem um major»:

> **Os atalhos de topo** (`ps`, `run`, `exec`, `logs`, `rm`, `images`), que são
> literalmente o mesmo comando por reescrita de argv.

O §3.4 da especificação manda removê-los. É uma decisão legítima do dono do
produto, mas é **um major**, não uma limpeza — e o custo cai inteiro sobre o
público de adopção (quem vem do Docker e escreve `delonix ps`).

### 2.2 Os códigos de saída colidem com a tabela da v0.49.0

Publicada, com nota de migração e exemplo em bash:

| publicado (v0.49.0) | proposto (§19) | colisão |
|---|---|---|
| `3` = existe mas não está a correr | — | o `3` desaparece sem substituto nomeado |
| `4` = **não existe** | `66` = não encontrado | **um script escrito contra a tabela publicada passa a classificar mal em silêncio** |
| `5` = conflito | `73` = conflito | idem |

O caso do `4` é o grave: o valor não passa a ser inválido, passa a ser
*outra coisa* — um `case $? in 4)` que hoje cria o recurso em falta passará a
não casar com nada, e o ramo `*) exit 1` engole-o. A tabela publicada traz
literalmente esse `case` como exemplo recomendado.

**Recomendação**: manter `3`/`4`/`5` e acrescentar os códigos novos apenas para
classes que hoje não existem (`69` capacidade indisponível, `75` retryable, `77`
permissão, `124` timeout). Isto cumpre o objectivo do §19 — dar classe ao que
não a tem — sem invalidar o que já foi prometido. A alternativa (adoptar
`64`–`77` por inteiro) exige nota de migração própria e um major.

### 2.3 `delonix build` está declarado estável

A mesma secção lista `image pull ls rm build (delonix build)`. O §10 move-o para
`image build --type container`. Mesmo tratamento que o 2.1.

### 2.5 O schema dos manifestos — a quebra mais séria, e a menos visível

`docs/cli-stability.md` tem uma secção intitulada **«O schema dos manifestos —
estável, e é o que mais importa»**. Para `Container`, `Pod`, `Volume` e
`Network` promete, dentro do `0.x`:

* um campo **nunca** é removido, nem muda de tipo, nem de significado;
* um nome renomeado **mantém-se aceite como alias**;
* **`apiVersion: delonix.io/v1` só muda com um `v2`, e um `v2` não sai sem o
  `v1` continuar a ser aceite.**

A reestruturação dos 12 Kinds quebra as três:

| promessa | o que a reestruturação faz |
|---|---|
| campo nunca removido | `kind: Container` desaparece — **todos** os seus campos com ele, e é um dos quatro Kinds cobertos pela promessa |
| alias para nome renomeado | `Vm`→`VirtualMachine`, `Ingress`/`FirewallPolicy`→`NetworkPolicy` sem degrau declarado |
| `v1` só muda com `v2` | vai para `compute.delonix.io/v1alpha1` — que não é `v2` e é um degrau **abaixo** na maturidade anunciada |

Há ainda um gate de CI que isto acciona: `scripts/schema-diff.sh` compara campo
a campo entre duas tags e **assinala um campo removido como quebra de
contrato**. A reestruturação fá-lo disparar por desenho.

**E o repo já tem a resposta certa escrita**, no fim dessa mesma secção, sobre
três Kinds que foram fundidos:

> os nomes antigos continuam a carregar, com aviso de depreciação — a regra do
> «corte limpo» aplica-se a comandos, e um manifesto em git merece um degrau em
> vez de um erro.

É a distinção que o §3.4 da especificação não faz. **Corte limpo em comandos:
correcto, e é o precedente da v0.30.0.** Corte limpo em manifestos: parte
ficheiros que estão em git, revistos em PR e referenciados por
`$schema` em editores — e parte-os sem que ninguém corra um comando.

**Recomendação**: `delonix.io/v1` continua a CARREGAR com aviso de depreciação e
a baixar para os Kinds novos no `load` — exactamente o mecanismo que
`Egress`→`FirewallPolicy` e `Storage`→`Volume` já usam, e que o `cmd/kinds.rs`
modela como `Form::Deprecated`. Custa um braço no `load` por Kind antigo e
mantém a promessa. `kind: Container` baixa para `kind: Pod` de um container,
que é literalmente o que o §3.3 diz que ele é.

### 2.4 `--l18n` → `--language`

Não está na lista de estáveis, portanto é a mudança mais barata das quatro. Mas
`DELONIX_L18N` aparece em documentação publicada e em `scripts/e2e.sh`. Aceitar
as duas grafias durante um ciclo custa três linhas e evita partir CI alheio;
recusar a antiga em silêncio é a armadilha que este repo já catalogou.

## 3. Matriz de destinos — as 233 folhas

Legenda da coluna **impacto**:
`=` sem quebra (renomeação transparente ou já equivalente) ·
`~` quebra de grafia, mesma capacidade ·
`!` quebra de contrato publicado ·
`+` capacidade nova exigida pela especificação ·
`?` **sem destino na árvore proposta** — decisão em aberto

### 3.1 Declarativo — o CRUD que colapsa nos verbos genéricos

Estas 47 folhas são a razão de ser da reestruturação: dez cópias de um CRUD que
passa a ser escrito uma vez.

| antigo | novo | razão semântica | impacto |
|---|---|---|---|
| `container apply` | `apply -f` | Kind `Container` deixa de existir (§3.3) | `!` |
| `image apply` | `apply -f` | CRUD declarativo unificado | `~` |
| `network apply` | `apply -f` | idem | `~` |
| `secret apply` | `apply -f` | idem | `~` |
| `sharevolume apply` | `apply -f` | grupo eliminado (§12) | `~` |
| `storage apply` | `apply -f` | grupo eliminado (§12) | `~` |
| `volumes apply` | `apply -f` | idem | `~` |
| `vm apply` | `apply -f` | idem | `~` |
| `cluster apply` | `apply -f` | `KubernetesCluster` (§13) | `~` |
| `stack apply` | `apply` | verbo canónico | `~` |
| `stack plan` | `plan` | verbo canónico | `~` |
| `stack destroy` | `delete -f --owner` | verbo canónico (§5.7) | `~` |
| `stack wait` | `wait -f` | verbo canónico (§5.8) | `~` |
| `stack validate` | `manifest validate` | §6.1 | `~` |
| `stack ls` | `get -f` / `get <kind>` | listagem genérica | `~` |
| `stack describe` | `describe -f` | §5.6 | `~` |
| `stack prune` | `apply --prune` | a poda é modo do apply, não comando | `~` |
| `stack init` | `init` | o `init` de topo já delega | `=` |
| `stack history` | `manifest history` **ou** `get stackrevisions` | ADR-0019; **a especificação não o prevê** | `?` |
| `stack rollback` | `apply --revision <n>` | ADR-0019; **não previsto** | `?` |
| `schema print` | `manifest schema` | §6.4 | `~` |
| `explain` | `explain` | mantém-se no topo (§4) | `=` |
| `pod create` | `apply -f` | §8 | `~` |
| `pod ls` | `get pods` | §8 | `~` |
| `pod rm` | `delete pod` | §8 | `~` |
| `pod describe` | `describe pod` | §8 | `~` |
| `pod logs` | `pod logs` | day-2, mantém-se | `=` |
| `vm create` | `apply -f` | §9 | `~` |
| `vm ls` | `get virtualmachines` | §9 | `~` |
| `vm rm` | `delete virtualmachine` | §9 | `~` |
| `vm status` | `get vm <nome>` | §9 — funde-se com `describe` | `~` |
| `vm describe` | `describe vm` | §9 | `~` |
| `network create` | `apply -f` | §11 | `~` |
| `network ls` | `get networks` | §11 | `~` |
| `network rm` | `delete network` | §11 | `~` |
| `network describe` | `describe network` | §11 | `~` |
| `network inspect` | `get network -o json` | `inspect`≡`get -o json` | `~` |
| `network route` | `apply -f` (`NetworkRoute`) | §11 | `~` |
| `volumes create` | `apply -f` | §12 | `~` |
| `volumes ls` | `get volumes` | §12 | `~` |
| `volumes rm` | `delete volume` | §12 | `~` |
| `volumes describe` | `describe volume` | §12 | `~` |
| `volumes inspect` | `get volume -o json` | §12 | `~` |
| `storage create/ls/rm/inspect` | `apply`/`get`/`delete` (`Volume`) | grupo eliminado (§12) | `~` |
| `sharevolume ls/rm/describe` | `apply`/`get`/`delete` (`Volume`) | grupo eliminado (§12) | `~` |
| `cluster create` | `apply -f` (`KubernetesCluster`) | §13 | `~` |
| `cluster kubeadm` | `apply -f` (provider `kubeadm`) | §13 | `~` |
| `cluster ls` | `get kubernetesclusters` | §13 | `~` |
| `cluster delete` | `delete kubernetescluster` | §13 | `~` |
| `secret ls/rm/inspect` | `get`/`delete`/`get -o json` | §5 | `~` |
| `workload ls/rm/stop/describe` | `get`/`delete`/… sobre Pod+VM | grupo eliminado (§5) | `~` |

### 3.2 Compatibilidade imperativa — `container` mantém-se

As 27 folhas do grupo `container` ficam onde estão (§7), com quatro excepções
listadas em 3.5. É a ponte de adopção e a especificação preserva-a
explicitamente.

`run · ps · start · stop · restart · kill · wait · rm · exec · logs · attach ·
inspect · update · port · rename · pause · unpause · cp · diff · commit · top ·
stats · healthcheck · ssh` — **todas `=`**.

### 3.3 Day-2 — sobrevivem porque não são CRUD

| antigo | novo | impacto |
|---|---|---|
| `pod logs` / `pod exec`¹ | `pod logs` / `pod exec` | `=` |
| `vm start/stop/restart/console/ssh` | idem | `=` |
| `vm snapshot create/ls/rm/restore` | `vm snapshot …` | `=` |
| `volumes snapshot create/ls/rm/restore` | `volume snapshot …` | `~` (plural→singular) |
| `image pull/push/build/scan/verify/convert/import/export` | `image …` | `=` |
| `cluster load` | `cluster load` | `=` |
| `secret create` | `secret create` | `=` |
| `secret rotate-key` | `secret rotate` | `~` |
| `net flow` | `network flow` | `~` |
| `net boot enable/disable/status` | `system boot …` | `~` |
| `namespace ls/describe` | `system namespace …` | `~` |
| `system info/events/df/prune/doctor/features` | idem | `=` |
| `dash` | `dashboard` | `~` |

¹ `pod exec`/`attach`/`cp`/`port-forward` **não existem hoje** — são capacidade
nova (§8), marcada `+` no total.

### 3.4 Política — o que colapsa em Kinds novos

| antigo | novo | impacto |
|---|---|---|
| `net ingress allow/deny/policy/clear/rm/ls` | `apply -f` (`NetworkPolicy`) | `~` |
| `net egress allow/deny/host/net/policy/clear/rm/ls/show` | `apply -f` (`NetworkPolicy`) | `~` |
| `net ingress publish/unpublish` | `apply -f` (`Service`) | `~` |
| `net httproute apply/ls/rm` | `apply -f` (`HTTPRoute`) | `~` |
| `net tunnel expose/rm/ls/describe/apply` | `apply -f` (`Gateway`) | `~` |

### 3.5 **Lacunas — folhas sem destino na árvore proposta**

Este é o achado que a Fase CLI-0 existe para produzir. **31 folhas** não têm
destino na árvore do §4, e cada uma exige uma decisão antes de a Fase CLI-5
(remoção) poder correr — senão são eliminadas por omissão, que é a forma mais
cara de partir uma CLI.

| folha | o que faz | proposta | porquê |
|---|---|---|---|
| `net l4guard set/clear/status` | limitador L4 por origem | `apply -f` (`NetworkPolicy.spec.rateLimit`) | é política; entrou depois da especificação ser escrita |
| `net netns` (9 folhas) | plumbing do holder | **subcomando oculto** | o §11 manda ocultar, mas não diz para onde — fica `__netns`, fora da árvore pública |
| `system setup` | prepara delegação de cgroup no host | `system setup` | administração local, cabe no §15 |
| `system virt` | reporta suporte de virtualização | `system doctor` | é uma pergunta de capacidade |
| `system thermal` | temperaturas do ferro | `system metrics` | §15 já prevê `metrics` |
| `system monitor` | monitor contínuo | `dashboard` | duplicava o dash |
| `system backup` / `system restore` | backup do estado do motor | `backup create --scope engine` | 4.ª e 3.ª porta do mesmo (ver §2 ponto 8) |
| `cluster kube generate` | gera YAML k8s de um manifesto | `manifest render --target kubernetes` | §6.2 é o sítio natural |
| `cluster init` / `container init` / `image init` / `image vm init` / `vm init` | scaffold por tipo | `init` (topo, já delega) | §4 mantém um `init` só |
| `container prune` / `image prune` / `vm prune` / `volumes prune` / `cluster prune` | GC por domínio | `system prune --scope <x>` | um GC, um comando (§15) |
| `container dash` / `image dash` / `network dash` / `storage dash` / `vm dash` | dash por recurso | `dashboard --scope <x>` | §22 manda eliminar os duplicados |
| `image tag` | reetiquetar imagem local | `image tag` | day-2 legítimo, falta na árvore |
| `image login` / `image logout` | credenciais de registo | `image login`/`logout` | day-2 legítimo, falta na árvore |
| `image load` / `image save` | archive Docker | `image import`/`export` | §10 já tem os dois verbos |
| `image history` | camadas da imagem | `image inspect` | mesma pergunta |
| `image ls-remote` / `vm ls-remote` | tags de um repositório | `image list --remote` | falta na árvore |
| `vm vnc` | consola gráfica | `vm console --vnc` | modo de consola, não comando |
| `vm reach` | diagnóstico VM→container | `network diagnose` | §11 já tem o sítio |
| `vm bridge` / `vm unbridge` | ponte privilegiada VM↔SDN | `network bridge`¹ | privilegiado e EXPERIMENTAL |
| `vm default-backend` | preferência de hypervisor | `config set-context --vm-backend` | §16 é o sítio de preferências |
| `vm build/convert/pull/push` | a 3.ª porta do `image --vm` | `image … --type virtual-machine` | §10 fecha as três portas numa |
| `network vlan` | VLAN de uma rede | `apply -f` (`Network.spec.vlan`) | é campo, não comando |
| `network node key` / `node init` | chaves do overlay WireGuard | `network node …` | falta na árvore |
| `sharevolume migrate` | absorve `ShareRecord` legado | `manifest migrate` | §6.3 é o sítio |
| `serve api` | API de gestão local | `serve management-api` | §4 renomeia-o |
| `syntax` | realce de VMfile | `completion editor` | §21 |
| `container ssh` | SSH para dentro do container | `container exec` com `--ssh`? | **em aberto** — duplica `exec` |
| `stack history` / `stack rollback` | ADR-0019 | ver 3.1 | **em aberto** |

¹ `vm bridge` continua a exigir root e a ser a única excepção declarada ao
modelo rootless. Movê-lo para `network` não muda isso, e a árvore proposta não
tem sítio para um comando privilegiado — decisão em aberto.

## 4. Contagem final da matriz

Derivada da classificação folha a folha (`scripts/cli-tree.sh --classify`), não
estimada — uma primeira versão desta secção trazia números que somavam certo e
tinham sido inventados, que é o relato desonesto que este repo persegue.

| classe | folhas |
|---|---|
| `~` quebra de grafia, mesma capacidade | **144** |
| `=` sem quebra | **70** |
| `?` sem destino / decisão em aberto | **17** |
| `!` quebra de contrato publicado (folhas) | **2** |
| **total** | **233** |

Às 2 folhas `!` juntam-se **quatro quebras que não são folhas** e por isso não
aparecem nesta contagem: os seis atalhos de topo (são reescrita de `argv`, não
subcomandos `clap`), a tabela de códigos de saída, o `--l18n`, e — a mais séria —
o schema dos manifestos (§2.5).

**Critério de saída da Fase CLI-0 cumprido**: as 233 folhas têm destino
documentado; 17 delas com o destino marcado como decisão em aberto, que é
precisamente o que esta fase existe para expor antes de alguém as apagar.

## 5. Dependência bloqueante

A Fase CLI-2 (verbos declarativos) **não pode começar** antes da reestruturação
dos 12 Kinds. Medido: `origin/main` serve `apiVersion: delonix.io/v1` com 15
Kinds, e não existe `Pod`, `VirtualMachine`, `Service`, `Gateway`,
`NetworkPolicy` nem `KubernetesCluster`. Construir `get pods` contra um Kind que
não existe é escrever um comando que não pode ter chamador — o padrão de código
morto que este repo já apagou quatro vezes (`publish_port_allow`, `Net`,
`mount_live`, `set_net_rate`).

Ordem que respeita a dependência:

```
Kinds (12)  →  CLI-1 (contratos)  →  CLI-2 (verbos)  →  CLI-3 (day-2)
                     ↑
              pode correr JÁ, em paralelo com os Kinds
```

A Fase CLI-1 (ResourceRef, contexts, output, erros JSON, exit codes,
cancelamento) é independente dos Kinds e é o trabalho que pode arrancar hoje.

## 6. Duas armadilhas de método que esta fase pagou

Ficam registadas porque nenhuma das duas é sobre a CLI — são sobre como se mede,
e as duas já estavam catalogadas no `AGENTS.md` antes de eu voltar a cair nelas.

**O `$?` depois de um pipe é do último comando.** Corri
`cargo test --workspace 2>&1 | tee log | grep …` e o harness anunciou **exit code
0**. O `grep` saiu 0; o `cargo` tinha falhado. Se tivesse aceitado o número, teria
reportado a bateria verde sobre uma suite com um teste vermelho. O que deu por
isso foi contar as suites (**4**, quando uma só corrida do binário já tinha 672
testes) — um número bom demais é o sinal para desligar o filtro e voltar a contar.

**Meia-isolação é pior que nenhuma.** O teste que falhou
(`delonix-mgmt::redes_lista_get_e_estado`) passa **29/29 duas vezes** quando a
suite corre sozinha, e falha quando o `--workspace` corre vários binários de
teste em paralelo contra o mesmo estado real da máquina. Não é fragilidade do
teste nem efeito da alteração — é o `DELONIX_ROOT` e o `DELONIX_NET_RUNTIME_DIR`
partilhados, exactamente o incidente de 2026-08-12 que reiniciou um container de
produção. A bateria desta fase corre com **os dois** redireccionados:

```bash
DELONIX_ROOT=<iso>/root DELONIX_NET_RUNTIME_DIR=<iso>/netrt cargo test --workspace
```

Isto é dívida a fechar antes da Fase CLI-2: o `cargo test --workspace` a seco não
é reprodutível neste repo e ninguém o diz em lado nenhum.

## 7. Fase CLI-1 — o que já aterrou

| contrato | estado | consumidor real hoje |
|---|---|---|
| exit codes (`69`, `124`) | **feito** | `stack wait`, `wg`, `virt-customize`, `ngrok`, `cloudflared`, `systemd-run` |
| identidade textual `DX_*` | **feito** | corpo de erro do `serve api` |
| shortnames vindos do registo | **feito** | `explain`, `stack apply --replace` |
| `ResourceRef` (o TIPO) | **adiado, de propósito** | — |
| contexts (`config set-context`) | **em conflito com o ADR-0010** | — |
| formatos de output, cancelamento, request IDs | por fazer | — |

### O `ResourceRef` foi escrito e apagado

O tipo (`kind`/`kind/name`, com testes) chegou a existir e saiu antes do commit:
nada consome uma REFERÊNCIA de recurso até os verbos declarativos existirem, e
este repo já apagou quatro APIs públicas que ficaram sem chamador e criaram bugs
latentes que ninguém podia notar — `publish_port_allow`, `Net`, e os parâmetros
ignorados no `mount_live` e no `reexec_start`. Uma quinta, escrita pela mesma
mão que escreve esta frase, não seria melhor.

O que ficou registado no doc do módulo, para não ser re-derivado: `kind/name` é
um argumento e `kind name` são dois; nomear o recurso duas vezes recusa-se em
vez de se resolver; e `pod/` recusa-se em vez de se ler como a colecção —
senão um erro de escrita num `delete` passa a ser todos os pods.

Ficou o que TEM chamador: `resolve_kind`, mais as colunas `plural` e `short` na
tabela de Kinds. Duas arestas reais fecharam-se de caminho:

- **`explain` só aceitava a grafia canónica exacta.** `explain pods` e
  `explain po` falhavam — e `pods` é precisamente o que se tem nos dedos depois
  de escrever `get pods`. As quatro grafias funcionam agora, e as duas recusas
  (Kind sem schema tipado, token que não é Kind) mantêm as dicas dirigidas.
- **`stack apply --replace` comparava strings.** `--replace container/web`
  (minúsculas) não casava: a autorização era ignorada em silêncio e o apply
  recusado a mandar passar a flag que a pessoa julgava ter passado. Num portão
  destrutivo, é a pior maneira de falhar. O KIND passa pelo registo; o **NOME
  nunca** — resolver nomes é como um `--replace` começa a autorizar um recurso
  que ninguém mencionou.

### Contexts: o §16 colide com o ADR-0010

O §16 pede `config set-context --endpoint`, com identidade e TLS, «para preparar
a CLI para gestão local e remota». O **ADR-0010 deste repo recusou a API de
gestão remota**, e a razão aplica-se tal e qual ao cliente: *remoteness* sem
identidade, autorização e auditoria não é remoteness que valha a pena, e essa
metade vive no `delonix-paas`.

Construir o lado-cliente de uma capacidade que o motor decidiu não ter é a mesma
classe de código-sem-consumidor de que o `ResourceRef` acabou de sair. O que
**não** colide, e é a parte útil, é um contexto puramente LOCAL: namespace por
omissão, formato de output preferido, `DELONIX_ROOT`. Fica como decisão a tomar
antes de a CLI-1 fechar.

## 8. O contrato de output do §18 — medido antes de mexer

A especificação lista onze regras para o que a automação lê. Medi-as contra o
binário **antes** de escrever código, e o resultado inverteu o plano: das que são
testáveis hoje, **todas já se cumpriam**.

| regra do §18 | estado medido |
|---|---|
| arrays vazios continuam arrays | **cumpre** — `[]` nos seis `ls`/`ps` |
| JSON sem texto traduzido | **cumpre** — `-o json` byte a byte igual em EN e PT, em listagem e em plano |
| sem ANSI quando o stdout não é TTY | **cumpre** — zero escapes num pipe |
| dados no stdout, o resto no stderr | **cumpre** — `-o json` não escreve nada no stderr |
| segredos redigidos | **cumpre** — `ls` traz nomes de chave, `inspect` redige e diz como revelar |
| `table`/`wide`/`name`/`yaml` | **`wide`, `name` e `yaml` não existem** |

Portanto o trabalho útil aqui **não era acrescentar formatos**. Primeiro porque
`yaml`/`name`/`wide` nos `ls` actuais é trabalho que a CLI-2 deita fora quando o
`get` os substituir; e depois porque o que estava mesmo em falta era outra coisa:
**nenhuma das cinco propriedades que já se cumprem estava travada por um gate**.
O que ninguém verifica é o que volta a partir-se, e cada uma delas parte em
silêncio — um `[]` que vire `""` rebenta todo o `jq '.[]'` lá fora sem uma
mensagem, e um escape ANSI que passe a sair num pipe faz um `grep` deixar de
casar por uma razão invisível a olho nu.

Ficam 13 checks novos no `scripts/e2e.sh`, e são propriedades do OUTPUT e não de
um comando: quando o `get` substituir estes `ls`, o bloco muda de alvo e não de
sentido. As três asserções cuja lógica podia estar subtilmente errada foram
verificadas a discriminar contra entrada deliberadamente má — sobretudo a do
ANSI, cujo `$'\033'` dentro de citação aninhada era o ponto frágil.

**Por decidir**: `wide`, `name` e `yaml` entram com o `get` (CLI-2) e não antes.

## 9. Cancelamento — e o único bug real que a CLI-1 encontrou

O §20 pede «TTY restaurado após falha». Medido a **2026-08-26**, não estava:

```
antes:   raw=False
durante: raw=True     (sessão interactiva, correcto)
SIGTERM →
depois:  raw=True     ← a shell de quem chamou fica sem eco e sem edição de linha
```

Um `SIGTERM` a um `container exec -it` deixava o terminal em modo raw. Quem o
apanhava ficava a escrever às cegas até se lembrar de `reset`.

**A causa não é descuido.** O `restore_mode` corre em todas as saídas normais —
incluindo a de erro, e é por isso que o `?` do `exec` está DEPOIS dele, o que
alguém pensou com cuidado. O que não corre em código Rust nenhum é um sinal: sem
destrutores, sem unwinding. Acontece com qualquer morte por sinal — um `kill`,
um timeout de CI, um teardown de sessão, o OOM killer.

**A correcção fica na fronteira** (`set_raw_mode`, no motor) e não no comando,
para todos os chamadores da via interactiva a herdarem — a mesma disciplina do
`missing_wg`. Um `AtomicPtr` e não um `static mut`, porque o handler lê isto de
contexto assíncrono e uma leitura atómica é das poucas coisas legais aí;
`tcsetattr`, `signal` e `raise` estão na lista de async-signal-safe do POSIX e
mais nada no handler aloca, tranca ou formata.

**O re-raise com disposição default é o que mantém o estado de saída honesto**:
um processo morto por `SIGTERM` tem de continuar a parecer morto por `SIGTERM`
(`128+15`) a quem espera por ele. Engolir o sinal para sair limpo trocava um
terminal partido por uma mentira sobre como o processo acabou. Medido nos
quatro: `rc=-15`, `-2`, `-1`, `-3`.

**O gate mede o TERMINAL e não o comando** — um `check` por exit code ficava
verde sobre o bug, porque o processo morria na mesma e com o mesmo estado.
Verificado a falhar com a correcção revertida: `15: o terminal FICOU em modo
raw`. `SIGKILL` fica de fora de propósito: não é capturável, e prometer repor
nesse caso seria mentira.

### O que NÃO se fez, e porquê

`delonix` continua **sem handler de SIGINT** para as operações não interactivas.
É deliberado por agora: a pergunta «um Ctrl-C a meio de um `stack apply` deixa a
stack meio convergida?» tem resposta conhecida e escrita — o apply é fail-fast
sem rollback, e o que já foi aplicado FICA aplicado. Dar-lhe um handler que
tentasse desfazer seria inventar transacionalidade que o motor não tem, e é
matéria de ADR, não de uma fatia de CLI.

## 10. As decisões em aberto, fechadas (2026-08-26)

A Fase CLI-0 expôs 17 folhas sem destino e dois conflitos de especificação. Ficam
todos decididos aqui, para a CLI-5 não os apagar por omissão e a CLI-2 poder
arrancar sem nada pendurado.

### 10.1 `net netns` (9 folhas) → subcomando OCULTO

`net netns {up,down,status,exec,attach,detach,firewall,publish,unpublish}` passa
a `__netns`, fora da árvore pública, como o `ingress-proxy` já é.

Não é uma remoção de capacidade: o `docs/cli-stability.md` já declara «**tudo o
que começa por `net netns`** — plumbing interno exposto por conveniência de
depuração» como NÃO estável. O que muda é deixar de estar à mesma distância do
utilizador que o `container run`. O `net netns down` continua a ser o comando de
recuperação de um upgrade in-place, e a mensagem do `stale_holder_message` tem de
passar a nomear a forma nova — senão a diagnose manda escrever um comando que já
não existe.

### 10.2 `net l4guard` (3 folhas) → `network l4guard`

É um guarda L4 **do nó** (taxa de ligação por origem e tecto de concorrentes),
ingress-wide, não uma política por workload. Por isso **não** vira campo de
`NetworkPolicy`: essa Kind descreve o que é permitido a UM alvo, e um limiar
global espremido lá dentro seria a mesma fusão de duas perguntas que este motor
recusa entre `NetworkRoute` e `FirewallPolicy`.

Fica no grupo de rede day-2, ao lado do `diagnose`/`flow`/`capture`. Entrou
depois de a especificação ser escrita, o que é a razão de não estar na árvore
dela.

### 10.3 `stack history` / `stack rollback` → `get revisions` e `apply --revision`

O `rollback` **é um apply** — o próprio `--help` di-lo: repete o manifesto da
revisão N pelo caminho normal e ganha revisão própria. Um verbo próprio para
«apply com outra entrada» seria um segundo caminho a manter de acordo com o
primeiro.

* `stack rollback --to N` → **`apply --revision N`**
* `stack history` → **`get revisions`**, com `--show N` a manter-se

Isto acrescenta uma linha ao registo de Kinds (`Revision`, `revisions`, sem
abreviatura). É um REGISTO e nunca fonte de verdade — o ADR-0019 é explícito — e
o `presence` dele é `Registry`, porque há mesmo um store por baixo.

### 10.4 `container ssh` → REMOVIDO, com o comportamento absorvido pelo `exec`

O nome promete SSH e entrega outra coisa: o `--help` diz «shortcut for `exec -t`»
e o que faz é tentar `bash` e cair para `sh`. Nem o Docker nem o Podman têm este
verbo, e o `container` existe precisamente para espelhar o que eles ensinaram.

A capacidade útil não é o comando, é o **fallback**: `container exec -it <id>`
sem comando passa a tentar `bash` e a cair para `sh`, que é o que a pessoa
queria. Feito isso, `ssh` não acrescenta nada e sai no corte limpo da CLI-5.

### 10.5 `vm bridge` / `vm unbridge` → `network bridge` / `network unbridge`

**Recebem uma REDE como argumento** (`vm bridge <NETWORK>`), o que decide a
questão: é uma operação sobre a rede, não sobre uma VM. Passam para o grupo de
rede day-2 com tudo o que as protege intacto — o dry-run por omissão, a
exigência de root, e o rótulo EXPERIMENTAL.

Continua a ser a **única excepção declarada ao modelo rootless**, e mudar de
grupo não muda isso. A árvore proposta não tinha sítio para um comando
privilegiado; passa a ter, e é o mesmo sítio onde já vive o `capture`, que
também exige autorização.

### 10.6 Contexts (§16) → **só o contexto LOCAL**

O §16 pede `endpoint`, `identity` e `tls configuration`. O **ADR-0010 recusou a
API de gestão remota**, e a razão aplica-se tal e qual ao lado cliente:
*remoteness* sem identidade, autorização e auditoria não é remoteness que valha a
pena, e essa metade vive no `delonix-paas`. Construir o cliente de uma capacidade
que o motor decidiu não ter é a mesma classe de código-sem-consumidor de que o
`ResourceRef` já saiu.

Fica o que é útil e não conflitua — um contexto **puramente local**:

```
namespace   o `-n` por omissão
output      o `-o` por omissão
root        o `DELONIX_ROOT` a usar
```

`endpoint`, `identity` e `tls` **não entram**. Reabrem com o ADR sucessor do
0010, que terá de nomear o consumidor concreto — e nessa altura o campo nasce
com um servidor do outro lado, em vez de esperar por um.

### 10.7 A matriz fica sem folhas em aberto

| classe | antes | agora |
|---|---|---|
| `~` muda de grafia | 144 | 144 |
| `=` sem quebra | 70 | 70 |
| `?` em aberto | **17** | **0** |
| `!` quebra contrato | 2 | 2 |
| `→` movida por decisão desta secção | — | **17** |

**A Fase CLI-0 fecha aqui.** O que resta antes da CLI-2 não é decisão nenhuma —
é a reestruturação dos 12 Kinds, que é o outro prompt.

## 11. O primeiro comando da CLI-2: `api-resources`

A CLI-2 está bloqueada nos 12 Kinds — **excepto num comando**. O `api-resources`
lista o que houver no registo, portanto as LINHAS mudam com a reestruturação sem
o mecanismo mudar. É por isso o primeiro a aterrar, e é o que dá superfície
visível a tudo o que a CLI-0 e a CLI-1 construíram.

```
NAME               SHORTNAMES   APIVERSION      KIND             NAMESPACED   DOMAIN         FORM
secrets            sec          delonix.io/v1   Secret           false        artifact       primary
pods               po           delonix.io/v1   Pod              true         compute        primary
egresses                        delonix.io/v1   Egress           false        net-policy     deprecated → FirewallPolicy
```

**Não há segunda tabela por baixo.** A listagem deriva do mesmo `cmd::kinds` que
o parser, o schema, a completação e o reconciliador leem — uma listagem escrita à
mão ao lado é como as duas começam a discordar sobre que Kinds existem, que é o
defeito que aquele módulo foi escrito para remover.

**O `FORM` não está na tabela do `kubectl` e é a coluna que não se adivinha:**
diz se um documento daquele Kind sobrevive ao load com o próprio nome. É a
resposta a «porque é que o meu `kind: Egress` nunca aparece no plano com esse
nome».

**O `apiVersion` passou a COLUNA e não constante**, apesar de hoje as 19 linhas
direm o mesmo. A reestruturação parte-o por domínio, e um Kind cuja versão viva
num `const` partilhado não se consegue mover um de cada vez — que é a única forma
de essa migração ser revista.

### Dois invariantes que ficaram travados

* **O `NAMESPACED` não pode discordar do carregador.** O `honors_namespace` é
  quem decide se um `metadata.namespace` é honrado ou avisado; a tabela dizer
  outra coisa era pior que não dizer nada — a pessoa escreve a namespace porque a
  tabela disse que o Kind a leva, e o load ignora-a em silêncio. O
  `PerDocument` é o que torna isto não-trivial.
* **Todo o plural listado resolve no `explain`** (E2E). Um nome documentado que
  não funcionasse era a tabela a publicar uma grafia inválida.

### Duas notas de honestidade

O `api-resources` **não ganhou página no site**, e é uma escolha: das 234
folhas, o gerador documenta 32 GRUPOS, e o `explain` — o irmão mais próximo — também
não tem página. Uma entrada nova ali custa título, tagline, intro e exemplos em
PT e EN, e faz mais sentido escrita de uma vez para o conjunto de verbos da
CLI-2 do que para um comando isolado.

**Mas «o site não exige cobertura» não é «nada exige»**, e escrevi isso antes de
medir. O **manual** exige: o `todo_o_comando_tem_entrada_no_manual` chumbou com
«1 comando(s) sem entrada em manual_entries.rs: api-resources», e o
`a_descricao_curta_cabe_numa_linha` chumbou a par, porque o `about` que escrevi
tinha 140 caracteres para um tecto de 110. As duas são exigências reais que eu
tinha dado por inexistentes depois de olhar só para o `docs/gen.py`. Ambas
fechadas — a descrição curta partida em duas (o resto passou a `long_about`,
nada se perdeu) e a entrada escrita, com os comentários dos exemplos traduzidos.

A lição é a de sempre neste repo: **procurar num sítio e concluir sobre todos**.
A cobertura da CLI é vigiada em dois sítios diferentes, e eu tinha visto um.

E o gate da superfície **disparou numa alteração real**, não fabricada: a folha
nova apareceu como não classificada e obrigou a decidir a classe (`=`, porque
nasce no destino e nunca teve outra grafia). É a primeira prova de que ele
funciona fora de um teste negativo.

## 12. O nome de um Kind passou a ter um sítio

A primeira renomeação (`Tunnel`→`Gateway`) custou **15 sítios de string em cinco
ficheiros**, e mediu o problema: o nome de um Kind era um literal repetido, com
nada a apanhar um sítio esquecido. E um sítio esquecido **não falha alto** — faz
um caminho de código deixar de reconhecer um Kind que o resto do motor continua a
servir.

É o mesmo defeito que o `cmd/kinds.rs` já tinha removido para os FACTOS sobre um
Kind — seis listas que tinham de concordar e derivaram — deixado de pé para o
NOME.

### O que a medição mudou no plano

A estimativa inicial era 106 literais. **São 460.** O número anterior era só dos
quatro Kinds a renomear. Mas a contagem crua não é a resposta, porque a maioria
não é identidade de Kind:

| categoria | sítios | decisão |
|---|---|---|
| despacho (`match`, `of_kind`, `kind:`) | **113** | migrados para constantes |
| fixtures de teste | ~138 | **ficam literais** |
| rótulos, documentação, `manual_entries` | o resto | não tocados |

**Os testes ficam com literais, e é deliberado.** Um teste que escreve
`"Gateway"` à mão testa o formato REAL que vai para o disco; trocá-lo pela
constante torna-o tautológico — passaria a SEGUIR uma renomeação em vez de a
APANHAR. A rede de segurança tem de estar do lado de fora do que vigia.

### O idioma é seguro por causa de uma configuração deste repo

Uma constante `&'static str` é um padrão de `match` legal, e um nome mal escrito
degrada para um BINDING que apanha tudo — footgun conhecido. Verifiquei-o com um
programa de dois minutos antes de adoptar o idioma, em vez de assumir: o binding
torna os ramos seguintes inalcançáveis, e este repo corre com `-D warnings`,
portanto é erro de compilação. **Quem desligar o `-D warnings` perde esta
garantia**, e é por isso que está escrita no doc-comment das constantes.

### O retorno, medido

```
pub(crate) const CLUSTER: &str = "KubernetesCluster";   ← uma linha
→ 0 erros de compilação
```

Uma renomeação passou de 15 sítios em cinco ficheiros para **uma linha**, mais o
alias no `canonical_kind` e a sua linha no teste que mantém os nomes antigos a
carregar. As três que faltam — incluindo o `Vm`, que era o mais caro com 54
sítios — passam a ter o mesmo custo.

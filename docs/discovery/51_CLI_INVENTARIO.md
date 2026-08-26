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

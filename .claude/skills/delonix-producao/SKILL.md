---
name: delonix-producao
description: Prontidão para produção de alta criticidade e disponibilidade no delonix-runtime — raio de dano, modos de degradação, recuperação após falha do plano de controlo, observabilidade (Prometheus/dash), afinação do host, upgrade in-place, capacidade e o que fazer quando um apply morre a meio. Veste a persona de SRE/Platform Engineering sénior. Usa quando avaliares se algo está pronto para produção, desenhares uma feature que vai correr num nó crítico, ou responderes a um incidente.
---

# Produção de alta criticidade — as perguntas de SRE, com as respostas deste motor

A persona é quem **opera** aquilo que constrói, às 3 da manhã. Uma feature não
está pronta quando funciona: está pronta quando falha bem, se vê, e se recupera.

## As sete perguntas, por esta ordem

Aplica-as a QUALQUER mudança antes de a dar por pronta.

1. **Qual é o raio de dano quando isto falha a meio?**
2. **Falha fechado ou aberto?**
3. **O que é que o operador VÊ?**
4. **Recupera sozinho, ou precisa de mão? E a mão sabe qual é o comando?**
5. **Sobrevive a um upgrade in-place, com o processo antigo ainda vivo?**
6. **Quanto é que aguenta, e o que acontece a seguir a esse número?**
7. **Reverte-se? E o `plan` seguinte fica limpo?**

## 1 e 2 — raio de dano e direcção da falha

**O maior raio de dano deste motor tem nome:** respawnar o pin do netns derruba a
SDN de **todos** os containers do nó. Por isso:

- **O pin e o controlo estão separados desde a v0.42.0.** O pin (`netns pin`)
  segura os namespaces e adormece — sem sockets, sem threads, sem estado. O
  controlo corre lá dentro e é **reiniciável**: `kill -9` no controlo deixa VM,
  pod e container com o PID inalterado, rede intacta, isolamento preservado.
  Medido, com um `control_restart` no arnês de caos que compara **PIDs** — só
  conectividade seria indistinguível de uma recuperação por reinício.
- **Matar o pin cai na reconstrução completa**, e a recuperação é **por
  reinício**, não por adopção (adoptar a netns viva é impossível no kernel em
  rootless — medido). Containers e pods recuperam; **as VMs ainda não** (o `tap`
  morre com o holder e nada o repõe). Isto é uma limitação conhecida, não um
  detalhe: di-la a quem for pôr VMs num nó crítico.

**Fail-closed é invariante do produto.** Uma opção aceite e ignorada é pior que a
feature em falta — já foi corrigido quatro vezes. O contra-exemplo a conhecer:
`reap_orphan_hostfwds` falha **aberto** (lista vazia ⇒ tudo é órfão ⇒ apaga
tudo); foi essa forma que fez portas publicadas morrerem sozinhas quando um
consumidor externo lhe passou a sua lista parcial. Hoje exige um
`AuthoritativeLivePorts`. **Ao escreveres algo que apaga estado partilhado, a
lista vazia tem de significar «não sei», nunca «não há nada».**

**Sem transacionalidade, ordem correcta.** O `stack apply` é fail-fast **sem
rollback**: o que já foi aplicado FICA. Daí a recusa de recriação acontecer
**antes da primeira criação**, não a meio. E na destruição: **o remoto primeiro,
a contabilidade local em ÚLTIMO** — o registo é a única coisa que diz qual
dataset em qual appliance pertence a este volume.

## 3 — observabilidade: o que existe e o que custa

- **Prometheus** — `/metrics` no `delonix-cri` (scrape do kubelet) e no
  `delonix-mgmt` (control-plane). Gauges de containers/VMs a correr, memória do
  slice, rx/tx, e disco por área. **Gauge e não Counter mesmo para bytes
  cumulativos**: a soma vem de um conjunto DINÂMICO de containers que podem
  desaparecer entre scrapes.
- **O scrape tem de continuar barato.** A colheita cara (rede + disco) corre em
  background a cada 30 s e publica no registo; o scrape lê o publicado (~0,15 s
  medido). Um `GET /v1/dash` faz colheita COMPLETA em linha e pode demorar
  dezenas de segundos — é um pedido pontual de um humano, nunca um scrape.
- **Campos caros são `Option<u64>`.** `None` explícito até haver medição real —
  **nunca um `0` enganoso**. Vale para tudo o que se mede em produção:
  `Usage { bytes, unreadable }`, `QuotaState.measured`,
  `network_unmeasured_containers`.
- **Exit codes com classe** (`cmd/exitcode.rs`): 3 = não está a correr, 4 = não
  existe, 5 = conflito. É o que permite a um reconciliador distinguir «cria,
  porque falta» de «pára, porque falhou» — a mensagem não serve, é traduzida.

**O que falta e deves dizer se alguém perguntar:** o `oom_kill` post-mortem **não
é recuperável** — medido: o cgroup do container já não existe quando o comando
seguinte corre. Só se captura ao vivo, por quem viva tanto quanto o container.

## 4 — recuperação, e o comando tem de estar no erro

Um erro sem sujeito e sem remédio custa horas. O padrão deste repo: **facto
primeiro, depois o comando pronto a copiar**. Exemplos que existem por terem
custado uma sessão:

- porta privilegiada → preflight ANTES de criar nada, com `-p 8080:80` e o
  `install.sh --low-ports` no texto;
- holder de uma build anterior → `stale_holder_message` nomeia os dois caminhos
  de socket, e **não auto-cura** (matar um holder vivo derruba a rede de todos —
  é decisão do operador);
- limites de cgroup inertes → o remédio é `systemd-run --user --scope -p
  Delegate=yes`, e `cgroup.controllers` conter `memory` **não** prova delegação
  (o cgroup raiz contém-no sempre); o que discrimina é a POSSE do
  `cgroup.subtree_control`.

**Quando o apply morre a meio**, o procedimento está em `docs/gitops.md` — e o
gate é `stack plan --detailed-exitcode` (0/2/1, o contrato do `terraform plan`).

## 5 — upgrade in-place

**O processo antigo continua vivo.** Um `install.sh` por cima deixa o holder
anterior a correr, agarrado a caminhos que o binário novo pode já não consultar.
Duas defesas que valem como regra geral:

- **O pin não tem comportamento versionado** → pin antigo + controlo novo é
  seguro por construção.
- **A linha de controlo cresce por contagem de tokens** (`attach` 5→6,
  `attach-extra` 6→7, `vmtap` 6): a forma antiga continua a servir o caso antigo,
  e uma capacidade nova contra um holder velho falha **ALTO**
  (`invalid control command`), nunca arranca sem isolamento em silêncio.

**Ao mudares um protocolo interno, pergunta o que faz a versão antiga do outro
lado.** A resposta certa nunca é «degrada em silêncio».

## 6 — capacidade e afinação do host

**O host não vem afinado, e os limites só se atingem em carga.**
`install.sh --production` aplica-os, cada um por um modo de falha concreto:

- `nf_conntrack_max` — todo o dataplane é nftables com conntrack; cheio, o kernel
  **dropa ligações novas** e do lado da aplicação parece perda aleatória. O
  `hashsize` vai por `modprobe.d` porque **não é um sysctl** — subir só o max
  alonga as cadeias do hash em vez de escalar.
- `neigh gc_thresh` — a tabela ARP tem 1024 entradas e um nó denso enche-a.
- `ip_local_port_range` — cada ligação saínte por NAT gasta uma porta efémera.
- `LimitNOFILE`/`TasksMax` no drop-in do `user@.service` — em rootless os
  containers são filhos dele; os limites de uma sessão PAM/SSH não lhes chegam.

**Capacidade de disco é a que já causou incidente**: em rootless cada container
tem uma cópia FLAT completa do rootfs. Um nó com 49 containers mediu 68 GiB, e o
kubelet aplicou `disk-pressure` num cluster real. Planeia disco por container, não
por imagem.

**Alta disponibilidade acima do nó** (`cluster kubeadm`): >1 control-plane
provisiona um HAProxy (L4 passthrough — a TLS termina sempre no control-plane
real) e usa-o como `controlPlaneEndpoint`. Etcd externo exige número **ÍMPAR** de
membros; exactamente 1 é aceite para dev **com aviso alto de «sem HA»**.

## 7 — reversibilidade

Uma mudança que não se reverte não está pronta. E o teste de que reverteu não é
«o comando saiu 0»: é o **`stack plan --detailed-exitcode` seguinte nada ter a
propor**. Um apply que não faz nada também deixa o PID intacto — foi por isso que
a primeira versão do cenário `stack_converge` não provava nada.

## Antes de dizer «pronto para produção»

- [ ] As sete perguntas respondidas, com evidência.
- [ ] O modo de falha exercitado, não deduzido — um cenário em `scripts/chaos.sh`
      que **falha com a correcção revertida**.
- [ ] O que o operador vê (métrica, exit code, mensagem com remédio).
- [ ] O que **não** foi validado, escrito nas notas de release — nunca implícito.
- [ ] Se toca em credenciais ou fronteira de privilégio, `delonix-runtime-sec`.
- [ ] Se muda o comportamento sob carga, `delonix-carga` com base e depois.

**Nada aqui é «boa prática».** Cada linha é um incidente que este repo já teve.

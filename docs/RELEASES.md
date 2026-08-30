# Delonix Runtime — features por release

> Gerado por `scripts/gen-releases.sh` a partir de `docs/releases/<tag>.md`
> (regenerado automaticamente pelo pipeline de release a cada tag publicada).
> Não editar à mão — edita a nota da release respectiva.

## v1.1.0 — a admissão passa a ser um ponto só, e o motor ganha uma superfície para agentes

Duas superfícies novas e dois bugs que deixavam trabalho a correr fora de onde
devia. É MINOR e não PATCH porque entram comandos novos; e não é MAJOR porque os
três subcomandos que saem (`storage dash|inspect|rm`) estão no grupo que o
[`cli-stability.md`](../cli-stability.md) declara **não estável** desde que existe.

### `delonix-security-runtime` — a admissão deixa de ter um buraco do tamanho de uma VM

Até aqui, `policy::enforce` tinha **exactamente um chamador**: o caminho de
container. O `cmd/vm.rs` não tinha nenhum. Um nó que escrevesse

```json
{ "denyPrivileged": true }
```

recusava `container run --privileged` e **aceitava** `vm create --device
0000:01:00.0` — passthrough VFIO, que dá ao convidado DMA ao hardware do host, e
é um buraco mais largo do que aquele que estava a recusar. Aceitava também
`vm create --url-img https://qualquer-sitio/x.qcow2`, cuja própria ajuda admite
que sem um `.sha256` publicado ao lado o descarregamento é confiado só no TLS.

O `delonix-security-runtime` é a decisão, e só a decisão — política, admissão,
evento, score e redacção de segredos. Três dependências, sem sensores, sem
daemon, sem privilégios. O caminho de VM chama-o **antes** de resolver,
descarregar ou escrever seja o que for. Ver [ADR-0026](../adr/0026-security-runtime-decision-crate.md).

**Nada começa a ser recusado ao actualizar.** As regras de VM são campos NOVOS,
todos desligados por omissão:

| campo | recusa |
|---|---|
| `denyDevicePassthrough` | `vm create --device` (VFIO PCI) |
| `denyLatestVmImage` | imagem de disco de VM sem etiqueta ou `:latest` |
| `allowedImageUrlHosts` | `--url-img` de um host fora da lista |

Alargar o `denyPrivileged` às VMs em silêncio teria fechado o buraco partindo
nós que já corriam. Em vez disso, o nó **aponta a metade que ficaste a deixar
aberta** — pelo nome, com identificador estável, no caminho onde podes agir, uma
vez por comando, e silenciável com `DELONIX_POLICY_LINT=0`:

```
aviso: política de runtime [POLICY-VM-PASSTHROUGH-OPEN] este nó recusa containers
`--privileged` mas permite `vm create --device` (passthrough VFIO PCI)…
```

Cada recusa deixa uma linha em `events.jsonl` com `kind: security` e um
identificador estável (`ADM-DEVICE-PASSTHROUGH`, `ADM-IMAGE-URL-HOST`, …).
Alerta sobre o identificador, não sobre o texto.

Guia do operador: [`docs/guia-politica-de-seguranca.md`](../guia-politica-de-seguranca.md).

### `delonix mcp` — uma superfície local para agentes de IA

`delonix mcp serve|doctor|capabilities`: um servidor MCP (Model Context Protocol)
para um agente descobrir, inspeccionar e — com `confirm` explícito em tudo acima
de `SAFE_WRITE` — operar este motor, **sem se tornar um desvio à volta dele**.

Deliberadamente mais estreito do que o pedido original: **sem OAuth/OIDC, sem
`tenant`/`project`/`environment`, sem HTTP remoto**, por
[ADR-0010](../adr/0010-remote-management-api.md) (a API de gestão remota foi
recusada) e [ADR-0003](../adr/0003-capability-model.md). Só stdio nesta passagem.

### Dois bugs que punham trabalho fora do sítio

**Um `exec` logo a seguir a um `run -d` podia correr no filesystem do HOST.**
Foi reportado como «o exec escreve para o rootfs em vez de para o volume»; a
medição disse pior — não era o destino errado *dentro* do container, era **fora
dele**. 3 de 12 corridas aterraram no host e duas criaram ficheiros reais em
`/tmp` do host, com exit 0. No caminho detach o `spawn` devolvia antes de o init
fazer `setup_rootfs`, e o `exec` fazia `setns` para um mnt namespace que ainda
não era o do container.

**O `exec` do CRI matava por número de pid.** O servidor lança um filho por
sessão e mata-o quando o cliente desaparece. Guardava o NÚMERO: enquanto o filho
é zombie o número está preso, mas quem espera por ele ceifa-o — e ceifar é
exactamente o que liberta o número para outro processo. Passou a `pidfd`.

### O que deixa de ser aceite em silêncio

O `kind: Ingress` lia três campos e deitava-os fora sem dizer nada. Nos três
casos o motor servia ou encaminhava coisa **diferente** do que o documento dizia
— o que é pior do que recusar o campo, porque o manifesto parece aplicado.
`tls[].hosts` é o exemplo: o proxy serve UM certificado e não faz selecção por
SNI, portanto quem escrevia dois hosts recebia o mesmo certificado nos dois.

### Mensagens

- **Nove erros diziam «não existe» a coisas que existem.** O padrão era
  `fs::read(p).map_err(|_| NotFound(...))`: certo para `ENOENT`, mentira para uma
  permissão negada, um erro de I/O ou uma montagem partida — e o operador ia
  fazer `ls` a um ficheiro que estava lá.
- **Oito mensagens ao operador levavam a indentação do ficheiro-fonte dentro.**

### CLI

`storage dash|inspect|rm` colapsam em `volume` — eram duplicados genuínos,
medidos campo a campo. `storage create|ls` **ficam** (`create` não tem
equivalente genérico; `ls` filtra a drivers de rede e mostra a coluna `DEVICE`).
O `volume inspect` ganhou `device`/`options` para não se perder a única
informação que só o `storage inspect` dava, e o `volume rm` passou a limpar as
credenciais do cofre incondicionalmente, fechando um gap que existia.

`sharevolume` foi medido e **não se cortou nada**: os cinco subcomandos têm gaps
de capacidade genuínos contra os verbos genéricos. É um resultado medido, não uma
omissão.

### Limitações conhecidas

- **O `delonix-security-runtime` não vigia nada em execução.** Decide na
  admissão, e mais nada: sem sensores eBPF, sem monitorização de integridade de
  ficheiros, sem malware, sem detecção comportamental de ransomware, sem motor de
  resposta. Todos precisam de processo residente, e este motor é daemonless por
  desenho.
- **O `events.jsonl` é sinal operacional, não prova.** É *best-effort* por
  desenho, não detecta as suas próprias falhas, e quem tiver escrita na raiz de
  estado consegue editá-lo.
- **As três guardas de VM nascem desligadas.** Quem actualizar e não ler nada
  fica com o aviso, não com a protecção — é o preço deliberado de não partir
  frotas a correr.
- **O `delonix mcp` é só stdio, local, sem noção de inquilino.** Um plano de
  controlo de frota para agentes é matéria do PaaS, não deste repo.

---

## v1.0.0 — os atalhos de topo saem, o resto do contrato fica como estava

Esta é a release que fecha o bloco "major" da reestruturação da CLI
(`docs/discovery/52_CLI_PLANO_MIGRACAO.md`, B8+B9). É a primeira quebra de
contrato publicado desde que `docs/cli-stability.md` existe — por isso é
`1.0.0`, e não outro `0.x`. É também a nota de migração própria que esse
plano exigia para este bloco.

### O que sai

Os seis atalhos de topo — `ps`, `run`, `exec`, `logs`, `rm`, `images` — eram
reescrita de argv para `container <verbo>`/`image list`, e estavam
**declarados estáveis**. Saíram. Corte limpo, sem alias: a grafia antiga
falha com `unrecognized subcommand`, nunca em silêncio — a mesma regra que a
reorganização da v0.30.0 já seguia.

| antiga | nova |
|---|---|
| `delonix ps` | `delonix container ps` |
| `delonix run` | `delonix container run` |
| `delonix exec` | `delonix container exec` |
| `delonix logs` | `delonix container logs` |
| `delonix rm` | `delonix container rm` |
| `delonix images` | `delonix image list` |

Todos os outros comandos, flags e a semântica dentro de cada grupo continuam
exactamente iguais — só a grafia de topo desaparece.

### O que NÃO sai, apesar de o plano original pedir

O bloco B8 do plano também pedia para colapsar `delonix workload` "por
coerência" com os atalhos. Não saiu. `workload` (ADR-0002) é uma superfície
deliberada — desambiguação real entre um container e uma VM que partilham
nome (`delonix container` ou `delonix vm` directamente, se soubermos qual
é). Não é uma duplicata: `get`/`describe`/`delete` respondem por Kind,
`workload` é o único caminho que atravessa Container+VM pelo nome. Cortá-lo
precisa de um ADR sucessor que nomeie a perda — uma linha de plano não
chega, e a mesma disciplina já valia para B4–B7 ("só cortar o que é mesmo
duplicado").

### Os códigos de saída não mudam

O bloco B9 do plano pedia `2→64`, `4→66`, `5→73`. Medido contra o código
actual: `cmd/exitcode.rs` já tem um desenho fechado, exaustivamente testado
e alinhado a LSB/`systemctl` (`3`=não corre, `4`=não existe, `5`=conflito) e
a `sysexits.h` (`69`/`74`/`77`/`124`) — construído DEPOIS do texto do B9, e
o próprio módulo já avalia e rejeita o pedido literal: `Error::Invalid` é
usado em 643 sítios para duas classes que a proposta separa (flag inválida
vs manifesto inválido), e remapear precisaria de dividir essa variante
primeiro — engenharia nova, não uma renumeração. Um teste já existente
(`nenhum_codigo_colide_com_uma_convencao_instalada`) já guarda exactamente
as colisões que o B9 temia. **Nada muda nos códigos de saída nesta
versão** — quem escreveu automação contra a tabela publicada não tem nada
para actualizar.

### O que a v1.0.0 significa a partir de agora

`docs/cli-stability.md` deixa de ser "a lista do que se compromete dentro do
0.x" — passa a ser o contrato de semver do projecto. Uma quebra como esta
deixa de caber num `1.x`.

Achado ao medir: o próprio documento prometia `image rm` como estável —
morto desde o B2 (`image remove`, sem alias, sem ninguém ter reparado).
Corrigido na mesma janela.

### Também nesta janela

"Delonix Runtime" nas superfícies que o utilizador vê a correr
(`--help`/`--about`, `system info`, a página de manual, os campos
`Platform`/`OperatingSystem` do `serve docker-api`) passa a dizer "Delonix
Engine" — é a marca pública; o nome do repositório e os doc-comments
internos continuam "Delonix Runtime" de propósito.

### A conta de folhas, do princípio ao fim de B4–B9

| | valor |
|---|---|
| folhas na v0.67.0 (antes de B4) | 247 |
| folhas nesta tag | **235** |
| folhas da especificação (alvo final) | 103 |

Doze folhas saíram por serem duplicatas genuínas (B4–B7); os atalhos desta
versão não contam nessa conta — nunca estiveram na árvore do `clap`, por
serem reescrita de argv antes do parse, e por isso `scripts/cli_baseline.tsv`
continua em 235. É um corte real e visível para quem escreveu um script
contra `delonix ps`, mesmo sem mexer no número que a `cli-tree.sh` mede.

### O que fica por fazer, e não tem data

O resto do day-2 de `vm`/`pod`/`cluster` além do CRUD já medido (B7) é
capacidade nova — `pod port-forward`, `vm migrate`, `cluster upgrade` — não
mais varrimentos mecânicos, e cada uma precisa da sua validação ao vivo
contra infra real. Não há estimativa de tempo para isso, nem promessa de
quando chega.

---

## v0.69.0 — B6 e B7 medidos: mais 7 duplicatas genuínas, dois blocos deixam claro o que falta é desenho

Esta janela fecha o primeiro corte de B6 (`vm build`) e o primeiro de B7
(o CRUD de `vm`/`pod`/`cluster`) — sete folhas a menos, cada uma provada
como duplicata byte-a-byte antes de sair, e dois relatórios do que NÃO sai
sem uma decisão de desenho.

| | valor |
|---|---|
| folhas nesta tag | **235** |
| folhas na v0.68.0 | 242 |
| folhas da especificação (alvo final) | 103 |
| folhas genuinamente duplicadas encontradas em B6+B7 | **7** |

### B6 — `vm build` era o mesmo `image --vm build`

`vm build` só sabia construir a partir de um `VMfile` (sempre exigido —
nunca cai no recipe dourado) e chamava `super::vmfile::build(...)`, a
MESMA função que `image --vm build` chama quando encontra um `VMfile`. A
única diferença aparente — a flag `-v/--verbose` — já não era diferença
nenhuma: `Progress::new()` já lê `$DELONIX_VERBOSE`, honrado por ambos os
caminhos. `vm build` sai; `image --vm build` fica como o único caminho
para o recipe dourado E para o `VMfile`.

O plano queria um `image build --type container|virtual-machine` novo a
substituir `build`/`image --vm build`/`vm build` inteiros — isso é desenho
novo (uma flag nova, migração de três entradas), fora do âmbito de "só
cortar o que é mesmo duplicado". `build` (raiz, container) e o resto de
`image --vm` não têm duplicata em lado nenhum, por isso ficam.

### B7 — seis duplicatas de CRUD em `vm`/`pod`/`cluster`

`get`/`describe`/`delete` já roteavam estes três grupos desde o B1, mas
cada arma construía o enum da grafia antiga e chamava `run()` inteiro — a
porta duplicada nunca tinha saído. Medido campo a campo:

| antigo | novo |
|---|---|
| `vm rm` (alias `vm delete`) | `delete vms` |
| `vm describe` | `describe vms` |
| `pod rm` | `delete pods` |
| `pod describe` | `describe pods` |
| `cluster ls` (alias `cluster list`) | `get clusters` |
| `cluster delete` | `delete clusters` |

Cada corpo foi extraído para uma função `pub(crate)` que o verbo genérico
chama directamente — a mesma disciplina do B4.

**O que NÃO sai, e porquê:** `vm ls`/`pod ls` ficam — `get vms`/`get pods`
fixam deliberadamente `ports: false`/`namespace: None` (um `get` não faz
I/O de rede não pedido nem filtra por namespace), e cortar `vm ls --ports`
ou `pod ls -n <ns>` seria perder capacidade real. `vm status` fica —
reconcilia estado AO VIVO e aceita omitir o nome, o que nenhum verbo
genérico cobre. `vm create`/`pod create`/`cluster create` ficam — não há
verbo "create" genérico, só `apply` declarativo, sem a ergonomia
imperativa de nenhum dos três.

### O que isto muda no plano

B4 até B7 já mostram o mesmo padrão: os alvos de folhas do plano original
presumem que toda a superfície duplicada pode sair sem mais nada — medido
grupo a grupo, uma fracção real tem capacidade que o verbo genérico ainda
não cobre (filtros, I/O ao vivo, ergonomia imperativa) e fica de fora até
essa capacidade ser construída. B4+B5+B6+B7 juntos prometiam bem mais de
100 folhas de corte; a soma do que era mesmo duplicado nos quatro foi
**12** (247 na v0.67.0 → 235 aqui). O caminho até às 103 da especificação
passa por construir essa capacidade nova, não por mais varrimentos
mecânicos.

### Correcções empacotadas nesta janela

- O scaffold do `VMfile` (`vmfile::scaffold`) e a mensagem "Next:" de
  `image vm init` apontavam para `delonix vm build` — corrigidos para
  `delonix image --vm build`.
- `docs/schema/v1/delonix.json` ficou desactualizado por dois doc-comments
  editados em `VmBuildSpec` (o `schemars::JsonSchema` deriva a
  `description` do doc-comment) — apanhado pela 1.ª corrida de
  `cargo test --workspace`, corrigido, confirmado numa 2.ª corrida.
- O aviso de `vm rm --force`, o scaffold de `stack init` e o hint de
  `cluster kubeadm` sobre um nó existente apontavam para
  `cluster delete --name`/`vm rm --force` — corrigidos para
  `delete clusters <nome>`/`delete vms <nome> --force`.
- O lab-7 do site já tinha `cluster ls -o json | jq` morto ANTES desta
  janela (`ClusterCmd::Ls` sempre foi um unit variant sem `-o json`) —
  trocado pelo caminho directo do kubeconfig.

### O que NÃO está feito, e não tem data

O resto de B7 (o day-2 puro de `vm`/`pod`/`cluster` além do CRUD já
medido) e os blocos B8/B9 (o major — atalhos de raiz `ps`/`run`/`exec`/
`logs`/`rm`/`images` e os exit codes que ainda colidem) continuam por
fazer. B8/B9 precisa de uma decisão do utilizador sobre versionamento
(0.x com nota de migração vs 1.0.0) antes de qualquer código.

---

## v0.68.0 — B4 e B5 medidos: o plano de corte encontra o seu limite real

Esta janela fecha os primeiros cortes dos blocos B4 (`net`) e B5
(`storage`/`sharevolume`) — e, medindo cada um linha a linha contra a regra
"só cortar o que é mesmo duplicado", descobre que os alvos de folhas do plano
original (`docs/discovery/52_CLI_PLANO_MIGRACAO.md`) são mais ambiciosos do
que a superfície real suporta sem perder capacidade.

| | valor |
|---|---|
| folhas nesta tag | **242** |
| folhas na v0.67.0 | 243 |
| folhas da especificação (alvo final) | 103 |
| folhas genuinamente duplicadas encontradas em B4+B5 | **5** |

### B4 — `net tunnel`/`net httproute` perdem o que já era duplicado

O plano descreve um "colapso do `net`" de −41 folhas. Medido: `net ingress
allow`/`deny`/`publish`/`l4guard` e `net egress net`/`host` são operações
**incrementais** (acrescentam uma regra, mantendo as outras) sem equivalente
hoje num `kind: NetworkPolicy` declarativo, que substitui a direcção inteira.
Cortá-los tiraria capacidade, não a arrumaria — por isso ficam.

O que sobrou depois da medição foi só o que já delegava, byte-a-byte, nas
mesmas funções que os verbos genéricos chamam desde o B1:

| antigo | novo |
|---|---|
| `net tunnel ls` | `get gateways` |
| `net tunnel describe` | `describe gateways <nome>` |
| `net tunnel rm` | `delete gateways <nome>` |
| `net httproute ls` | `get httproutes` |

A grafia antiga falha com `unrecognized subcommand` — nunca em silêncio.

Apanhadas na mesma janela: o `--version` ainda anunciava `delonix dash`
(morto desde a v0.67.0), o `after_help` das SHORTCUTS ainda dizia `delonix
image ls` (idem), e a cheatsheet dizia `completion <shell>` em vez de
`completion shell <shell>`.

Também medido e revertido: agrupar o `--help` de topo por secções com
`#[command(next_help_heading = ...)]` não funciona em subcomandos — a própria
doc do clap 4.6 diz que o atributo só troca o título único de "Commands:",
nunca cria várias secções. Confirmado ao vivo (zero efeito) antes de reverter,
não assumido a partir da doc.

### B5 — `storage apply` era o único duplicado real

O plano previa `storage`/`sharevolume` inteiros a dobrarem-se em `kind:
Volume` (−22 folhas). Medido comando a comando, só um qualificava:
`storage apply` já delegava inteiramente em `super::volume::apply(&docs)`,
sem filtro nenhum, desde que `kind: Storage` deixou de existir depois de
`manifest::load` (reescrito para `kind: Volume` com bloco
`nfs`/`cifs`/`webdav`). Removido — era literalmente o mesmo `volume apply`
para o mesmo manifesto.

O resto não é duplicata:

- `storage create` tem onboarding de credenciais NFS/CIFS/SMB/WebDAV que
  `volume create` não tem.
- `storage ls`/`inspect`/`rm` filtram para drivers de rede, com forma de
  linha própria e, no `rm`, limpeza de credenciais + aviso específico sobre
  o `ShareVolume` por baixo.
- `storage dash` é o ÚNICO caminho para o ecrã de armazenamento — o
  `dashboard` do topo está fixo no âmbito global, sem selector.
- `sharevolume` inteiro (`ls`/`describe`/`rm`/`apply`/`migrate`) tem
  semântica de namespace, quota e storage-mãe que `volume` não expõe, mais o
  `migrate` de registos de antes do scoping por namespace.

### O que isto muda no plano

Os blocos B4 e B5, como especificados, não são alcançáveis sob a regra "só
cortar o que é mesmo duplicado" sem primeiro construir capacidade
declarativa que hoje não existe (um `NetworkPolicy` incremental, ou um
motivo novo para justificar remover o `storage`/`sharevolume` amigável). Os
−63 folhas que os dois blocos prometiam juntos tornaram-se **6** — as 5
medidas aqui mais o `schema print` da v0.67.0. O caminho até às 103 folhas da
especificação passa por B6/B7 (ainda por medir) e pelas decisões de desenho
que B4/B5 deixaram em aberto, não por mais cortes mecânicos nestes dois
grupos.

### O que NÃO está feito, e não tem data

B6 (colapso `image`/`build`), B7 (day-2 de `pod`/`vm`/`cluster`) e o major
B8/B9 (atalhos de raiz `ps`/`run`/`exec`/`logs`/`rm`/`images` e exit codes)
continuam por fazer. B8/B9 precisa de uma decisão do utilizador sobre
versionamento (0.x com nota de migração vs 1.0.0) antes de qualquer código.

---

## v0.67.0 — a reestruturação da CLI fecha o bloco B2, sem quebrar o que estava estável

A especificação da CLI (`docs/discovery/52_CLI_PLANO_MIGRACAO.md`) pede que as
243 folhas medidas a 2026-08-27 desçam para as 103 da especificação. Esta
versão não fecha essa distância — fecha o primeiro troço com prova em cada
passo: os verbos genéricos ganham os Kinds que faltavam, as nove renomeações
mecânicas do bloco B2 estão todas feitas, e duas peças de capacidade nova
(`backup` consolidado, `network diagnose`) desbloqueiam o bloco seguinte.

| | valor |
|---|---|
| folhas medidas nesta tag | **247** |
| folhas da especificação (alvo final) | **103** |
| blocos de renomeação mecânica fechados | **B2 completo — 9 de 9** |
| folhas genuinamente removidas (não só renomeadas) | **1** (`schema print`) |

### O bloco B2, corte limpo e sem alias em todos

Nove renomeações, cada uma com PR, testes e prova ao vivo próprios — e cada
grupo tocado já estava declarado **NÃO estável** em `docs/cli-stability.md`,
que é a condição que separa um corte limpo de uma quebra de contrato:

| de | para |
|---|---|
| `dash` | `dashboard` |
| `syntax <editor>` | `completion editor <editor>` |
| `completion <shell>` | `completion shell <shell>` |
| `namespace` | `system namespace` |
| `net boot` | `system boot` |
| `volumes` | `volume` |
| `image ls` / `image rm` | `image list` / `image remove` |
| `schema print` | `manifest schema` (já existia; a porta duplicada saiu) |
| `restore` (raiz) | `backup restore` |

A grafia antiga falha sempre com `unrecognized subcommand` — nunca em
silêncio, nunca com um alias a disfarçar a migração.

### Verbos genéricos: mais três Kinds

`get`/`describe` já cobriam a maioria dos Kinds com estado próprio; ganham
agora `NetworkRoute`, `NetworkPolicy` (roteados para os `ls` já existentes dos
seus grupos — zero lógica nova) e `Secret` no `describe` (que não tinha um
`describe` próprio; `secret inspect` já era a vista de detalhe, valores
redigidos salvo `--reveal`).

### `backup` consolidado, `network diagnose` novo

O `backup`/`restore` de raiz e o `system backup`/`system restore` eram
**quatro portas** para duas operações. `backup` ganha seis verbos
(`create`/`list`/`inspect`/`restore`/`schedule`/`remove`) e passa a ser o único
caminho para arquivar UM recurso (container/pod/vm/stack); o `system backup`
continua a ser o caminho do NÓ inteiro — são âmbitos diferentes, não a mesma
coisa por dois nomes.

`network diagnose` responde a uma pergunta que o `system doctor` não faz:
aquele pergunta se o HOST consegue fazer o trabalho (capacidade estática, uma
vez); este pergunta se a rede que ESTÁ aqui agora é coerente (estado vivo — o
plano de controlo responde, cada rede declarada está realizada, o registo de
endereços bate com as cargas). É também o que desbloqueia o próximo bloco de
corte: remover capacidade do grupo `net` sem um comando de diagnóstico seria
tirar visibilidade, não arrumá-la.

### Três ADRs

- **ADR-0019** — um `stack apply` cujo plano é inteiramente `NoOp` deixa de
  gastar uma revisão. Medido: quatro applies sem mudança nenhuma inflacionavam
  o histórico de 1 para 5 entradas; a retenção (20) empurrava para fora a
  revisão que tinha mudado alguma coisa a sério.
- **ADR-0020** — dois códigos de saída novos, `74` (I/O) e `77` (permissão
  negada), medidos um a um contra o que já estava publicado: são os únicos
  dos sete pedidos pela especificação sem caminho que quebre um script escrito
  contra a tabela da v0.49.0.
- **ADR-0021** — `kind: GitOpsSource`, opt-in e sem daemon: fecha as duas
  metades do GitOps que faltavam (pull automático + reconciliação contínua),
  por um timer systemd a invocar o `stack apply` que já existe.

### Correcções empacotadas nesta janela

- **A imagem VM base voltava a não arrancar** depois de um `virt-resize`
  (GRUB precisa de reinstalação depois de redimensionar o disco) — a golden
  publicada tinha este defeito.
- **O caminho do log deixava de ser legível** quando `--log-file` era usado —
  passa a ser registado, não perdido.
- **Anti-spoofing de MAC/ARP no tap de uma VM libvirt** — os veths já tinham a
  regra desde sempre, o tap nunca teve, que é onde mais importa (o kernel do
  convidado não é nosso).
- **Uma regra de firewall que nomeia um workload** escolhia o inquilino em
  silêncio pelo nome — passa a ser explícito.
- **O aviso de namespace num manifesto** só nomeava três de sete Kinds
  namespaced.
- **As quatro listagens de rede** (`net ingress`/`egress`/`httproute`/`tunnel`)
  ganham `-o json` de verdade — eram só tabela.
- **Isolamento do teste de IPAM**: corria por processo contra um caminho
  fixo em `/tmp`, e duas sessões em paralelo (o normal neste workspace) apagavam
  o directório uma à outra a meio da corrida.

### As duas metades: uma regressão encontrada e fechada na mesma janela

O primeiro PR desta série (`dash`→`dashboard`) deixou `main` com o job `docs
geradas e exemplos válidos` **vermelho** — o `docs/gen.py` nunca foi
actualizado, e as páginas publicadas de `dash`/`syntax` mostravam
`Usage: delonix [OPTIONS] <COMMAND>` genérico em vez do `--help` real, porque a
sonda ao vivo do gerador falha em silêncio quando o comando já não existe.
Ficou vermelho durante o PR seguinte também — só foi encontrado ao preparar
este, e corrigido no mesmo commit que continuava a mexer no mesmo mecanismo.
`main` está verde nos 7 jobs da CI real desde então, confirmado a cada PR
posterior.

### O que NÃO está feito, e não tem data

Os blocos B4–B7 (colapso de `net`/armazenamento/`image`+`build`, e o day-2 de
`pod`/`vm`/`cluster`) e o major B8/B9 (os atalhos de raiz `ps`/`run`/`exec`/
`logs`/`rm`/`images` e os exit codes que ainda colidem) continuam por fazer.
O B4 em particular não é mecânico como o B2: `net ingress allow`/`deny` são
operações **incrementais** (acrescentam uma regra, mantendo as outras) sem
equivalente hoje num `kind: NetworkPolicy` declarativo, que substitui a
direcção inteira. Cortar aí sem resolver essa lacuna tira capacidade em vez de
a arrumar — por isso não tem prazo aqui, e não terá até essa decisão de desenho
estar tomada.

---

# v0.66.1 — o instalador não conseguia instalar a extensão que a v0.66.0 publicou

Dois defeitos no caminho novo do `install.sh`, os dois só encontráveis a
**correr** o instalador contra uma release a sério — a v0.66.0 foi a primeira a
publicar o asset, e até haver asset o caminho tomava sempre o desvio.

## O nome do asset levava um sufixo que ele não tem

O `fetch_asset` existe para BINÁRIOS e compõe sempre `-x86_64[-v3]-linux`.
Reutilizei-o para o `.vsix`, que é independente de arquitectura e se chama
`delonix-vscode` e mais nada. Resultado, medido contra a release:

```
delonix-vscode-x86_64-v3-linux   HTTP 404
delonix-vscode-x86_64-linux      HTTP 404
delonix-vscode                   HTTP 200
```

E o desvio anunciava `this release ships no editor extension` — **uma frase
falsa sobre uma release que a traz**. Pior: eu tinha silenciado o `stderr` do
download, que teria mostrado o 404 e a causa. Duas decisões erradas a
reforçarem-se, que é como um bug fica invisível.

`fetch_named_asset` passa a existir para assets cujo nome é o nome.

## O CLI do editor exige que o ficheiro acabe em `.vsix`

Sem isso lê o argumento como um ID de extensão e responde «make sure you use the
full extension ID, including the publisher» — uma frase que não tem nada que ver
com a causa, e que manda procurar no sítio errado.

O asset **mantém** o nome `delonix-vscode`, porque é esse que o `SHA256SUMS`
assinado cobre: verifica-se com o nome dele e instala-se uma cópia com a extensão
que o editor pede.

## Fazia downgrade da extensão

A release do motor traz a versão da extensão que existia quando ela foi
construída, e essa pode ser **mais velha** do que a que o editor já tem. Medido
neste host: um `--install-extension --force` cego trocou a 0.2.0 pela 0.1.0 e
levou a árvore de recursos com ela, sem uma palavra.

Passa a nunca sobrepor uma extensão instalada. Quando ela estiver nas galerias é
o editor que a mantém em dia; o trabalho deste script é a PRIMEIRA instalação.

## E a guarda que evitava o downgrade matava o script

`HAVE=$(... | grep ...)` sob `set -e` com `pipefail`: um `grep` sem
correspondência sai 1, a atribuição falha, e o instalador morria em silêncio no
`[editor] plugin: downloading...` — **no caso mais comum que existe**, que é
ainda não ter a extensão. É o mesmo defeito que a etiqueta de GPU já custou a
este ficheiro.

Contra um editor que já tinha a extensão, o `grep` casava e nada se via. Foi um
editor de teste no PATH, a reportar zero extensões, que o mostrou — e como o
falso e o real coexistem, uma só corrida prova os dois ramos.

## Um flake de teste que estava latente

O CI apanhou `a_unique_bare_name_still_resolves_exactly_as_before` a falhar em
`matches!(find(&store, "nope"), Err(Error::NotFound(_)))`, para um nome nunca
inserido — e não reproduz localmente.

O `tmp_store()` do `util.rs` é partilhado por seis testes e nomeia a pasta com
`pid + nanos`. O cargo corre-os em paralelo, no mesmo processo, logo o pid é
igual e só o relógio os separa; duas threads no mesmo tick recebem o mesmo
caminho, e cada teste acaba com `remove_dir_all`. O `vmimage.rs` tem um gémeo.

O sintoma foi **provado**: com a pasta apagada por baixo, o `find` devolve
`Io(ENOENT)` e não `NotFound`. A correcção é um contador atómico, que não pode
empatar. Reproduzir a corrida em si não é possível aqui — perdê-la precisa da
temporização de um runner carregado.

## Verificado ao vivo

Contra a release v0.66.0 real, num host com carga a sério:

```
[binary] signature: minisign verified (SHA256SUMS)
[binary] delonix -> /home/walter/.local/bin/delonix: OK
[editor] antigravity: OK
```

Binário em `0.66.0`, `angolardevops.delonix` instalado, e os containers de
produção com o mesmo id e o mesmo uptime — substituir o binário não toca em
carga a correr, porque o motor é daemonless.

## Continua por fechar

Um `--user` num host cujas dependências já estão todas satisfeitas ainda pede
`sudo` e sai **1**, mesmo tendo feito tudo o que havia a fazer. O portão de
autenticação corre antes de se saber se alguma coisa a seguir precisa mesmo de
root. A v0.66.0 já o tinha adiado para depois do binário — o que resta é
autenticar só quando há mesmo trabalho de root, e isso precisa de uma
pré-verificação própria.

---

# v0.66.0 — o validador concordava com o motor em nada

A v0.64.0 deu a cada Kind o grupo do seu domínio e a v0.65.0 removeu três Kinds.
O schema publicado não soube de nenhuma das duas coisas, e é o ficheiro que um
editor consome.

## O achado

Medido contra o schema que o site servia, não contra uma teoria sobre ele:

| documento | schema | motor |
|---|---|---|
| `kind: Egress` | valida limpo | recusa — «was removed» |
| `kind: Contaner` (typo) | valida limpo | recusa |
| `kind: Banana` | valida limpo | recusa |
| `apiVersion: compute.delonix.io/v1alpha1` | **recusa** | aceita |

Errado nos dois sentidos ao mesmo tempo: visto verde a um typo, sublinhado
vermelho na grafia que o `api-resources` publica como canónica. É a pior
combinação que um validador pode ter — quem confia no verde aplica um manifesto
que não aplica, e quem vê o vermelho deixa de confiar no linter.

## Três causas, e nenhuma é a mesma

**Uma segunda lista de Kinds, à mão.** O `TYPED_KINDS` do gerador de schema vive
ao lado do registo e é escrito à parte. A v0.65.0 tirou o `Egress` do registo e a
entrada aqui sobreviveu-lhe. O gate que existia pedia que todo Kind **vivo**
tivesse schema; ninguém pedia o inverso — e é o inverso que uma remoção parte.
Mesma família das três listas convergentes que já derivaram uma vez.

**Um `allOf` de `if`/`then` é advisório sem o `kind` fechado no topo.** Um
documento que não casa com ramo nenhum satisfaz todos os `then` por vacuidade.
O `kind` era `{"type": "string"}` — daí o `Banana` passar. E é por isto que
corrigir só a primeira causa não mudava nada: tirado o ramo do `Egress`, o
documento continuava a validar.

**O `apiVersion` era `const "delonix.io/v1"`**, de antes dos grupos por domínio.
O motor decide **por Kind** — um `Pod` sob `networking.` é recusado a nomear o
grupo certo — e o schema não sabia disso.

## O que muda

`kind` e `apiVersion` de topo passam a enums **derivados**, e cada ramo fixa
também o grupo do seu Kind.

As grafias saem do `KIND_ALIASES`, que passa a ser tabela em vez de um `match`:
o conjunto de nomes aceites existia só como fluxo de controlo, ilegível de fora,
e o schema precisa exactamente dele. Derivá-lo à mão seria a primeira causa outra
vez. Uma tabela, dois consumidores.

**Nada a migrar.** Os 65 documentos dos `examples/` continuam a validar — era o
risco real de fechar um enum: recusar o que sempre foi bom. O que passa a ser
recusado é o que o motor já recusava.

## A extensão de VS Code

Repo próprio: [`angolardevops/delonix-vscode`](https://github.com/angolardevops/delonix-vscode).

Validação e autocomplete contra o schema que este motor publica — buscado **ao
vivo**, não embutido, por isso o editor e o `stack apply` não podem discordar e
um Kind acrescentado a montante é conhecido sem a extensão ser relançada. Mais um
template por Kind, gerado do registo, e realce para `VMfile` e `Delonixfile`.

Serve o VS Code e os forks que partilham a sua CLI de extensões — VSCodium,
Cursor, Windsurf, Antigravity. O `install.sh` instala-a em todos os que encontrar,
com `--no-editor-plugin` para não o fazer, e o artefacto entra no **mesmo
`SHA256SUMS` assinado** que o resto da release: um caminho de confiança, uma
assinatura.

O que ela **não** faz, e está escrito com o comando que o prova: não há integração
com Dev Containers. Essa extensão conduz um daemon Docker, e o `serve docker-api`
serve 14 rotas e recusa 12 — entre elas `exec` e `attach`, que precisam de HTTP
hijacking que este motor decidiu não implementar. É uma fronteira de desenho, não
uma tarde de trabalho em falta.

## Quem vem de uma 0.64.x

O instalador avisa **antes** de trocar o binário. A v0.64.0 diz em três sítios
que «nada é obrigatório; os manifestos existentes carregam sem alteração», e a
v0.65.0 tornou isso falso no mesmo dia ao remover `Storage`, `ShareVolume` e
`Egress`. Quem leu as notas da 0.64 e adiou a migração tinha razão nesse dia.

## Também

O README deixou de escrever a lista de Kinds: passa a ser `delonix api-resources`,
a única cópia que não pode envelhecer. Estava a mentir em seis pontos, todos da
mesma causa. E o exemplo dele estava errado — `ports: ["5432:5432"]` num
`kind: Pod`, a grafia do docker numa forma que é k8s; o schema e o motor
recusaram-no os dois, que é esta série a funcionar contra o texto que a anuncia.
Os três documentos passaram a ser validados contra o schema no ar e aplicados com
`--dry-run`.

---

## v0.65.0 — QUEBRA: três Kinds foram removidos

**Esta versão parte manifestos.** É a primeira desde que a promessa de
estabilidade existe que o faz de propósito, e a nota está no topo em vez de no
fim porque é o que interessa saber antes de actualizar.

### O que sai

| removido | escrever em vez dele |
|---|---|
| `kind: Storage` | `kind: Volume` com um bloco `nfs:`/`cifs:`/`webdav:` |
| `kind: ShareVolume` | `kind: Volume` com um bloco `share:` |
| `kind: Egress` | `kind: NetworkPolicy` com `direction: egress` |

Os três **já eram reescritos** para exactamente estas formas no carregamento. O
que o motor FAZ não mudou; mudou quem tem de escrever a forma final.

### A recusa nomeia o substituto

```
$ delonix stack validate -f antigo.yaml
error invalid argument: `kind: Storage` was removed — write `kind: Volume` with
an `nfs:`/`cifs:`/`webdav:` block instead (the same VolumeStore, described once
instead of twice)
```

Não é «unknown kind». Um manifesto correcto até ontem a receber um erro genérico
faria quem o apanha duvidar se escreveu mal, em vez de saber que algo mudou.

### Isto contradiz a v0.64.0, e é deliberado

A v0.64.0 saiu há horas a dizer «nada é obrigatório; os manifestos existentes
carregam sem alteração». **Deixou de ser verdade nesta.** Não houve um degrau
intermédio — a decisão foi cortar, e o número da versão e este texto existem
para que ninguém descubra isso pelo erro.

Quem seguiu o conselho da v0.64.0 e adiou a migração tem de a fazer agora. Os
`sed` são estes:

```bash
# kind: Egress → NetworkPolicy com direction
sed -i 's/^kind: Egress$/kind: NetworkPolicy/' *.yaml   # e acrescentar `direction: egress` ao spec
```

Para o `Storage` e o `ShareVolume` não há `sed` honesto: o corpo do documento
muda de forma (`type: nfs, server: h` passa a `nfs: { server: h }`). Os
exemplos publicados mostram as duas formas finais.

### Efeito lateral: a forma `deprecated` desapareceu

O registo de Kinds tinha seis formas; ficou com cinco. A `Deprecated` — um Kind
REESCRITO no carregamento, com aviso — ficou sem um único utilizador e saiu, que
é o que este repo faz a código sem chamador. A distinção que ela guardava fica
escrita no `Sunset`, que é quem a contrastava: um `Sunset` **não** é reescrito,
porque reescrevê-lo mudaria o que o motor faz. Volta quando um Kind precisar de
ser reescrito em vez de anunciado.

### O que NÃO sai

Os grupos de comandos `delonix storage` e `delonix sharevolume` **ficam**. A
medição que o decidiu: o `storage create` tem flags que o `volumes create` não
tem — `--type`, `--server`, `--share`, `--username`, `--password-secret`,
`--options`. Remover o grupo perdia a criação imperativa de armazenamento de
rede; remover o Kind não perde nada.

Também ficam, e não são depreciações: `kind: Container` (`sunset`, tem apply
próprio), `kind: Ingress` (`compat`, é schema do k8s aceite de propósito), e o
açúcar `kind: Workload` e `kind: Dependency`.

### Medido

36 suites · **1249 testes** · 0 falhas, com os dois roots isolados.
`fmt` · `clippy -D warnings` 0 · `lang_ratchet` · gate da superfície da CLI —
todos verdes. A dívida de língua **desceu** com os Kinds (1050→1042
identificadores, 3479→3460 comentários), e a linha de base foi baixada no mesmo
commit, como o ratchet exige.

---

## v0.64.0 — os Kinds ganham grupos e nomes definitivos, e o `delonix.io/v1` continua a carregar

A reorganização dos recursos do motor. **Nada do que está escrito hoje deixa de
funcionar** — é o ponto de partida desta versão, não uma nota de rodapé.

### O que se mediu antes de mexer

| | valor |
|---|---|
| comandos na árvore pública | **263** (233 folhas) |
| literais do NOME de um Kind espalhados pelo código | **284** |
| … estimados na primeira contagem | 106 |
| verbos declarativos repetidos (`ls`, `apply`, `rm`, `describe`) | **10 cópias cada** |

A estimativa errou por um factor de quase três, e só o compilador a corrigiu: as
regex contavam padrões, não sítios. Está registado porque é o número que decidiu
introduzir constantes em vez de continuar a renomear à mão.

### Os grupos

Cada Kind passa a viver num grupo, e o grupo faz parte da identidade — um
`apiVersion: storage.delonix.io/v1alpha1` num `kind: Pod` é recusado, com as
**duas** formas aceites nomeadas no erro.

```
core.delonix.io/v1alpha1             Secret · Stack
compute.delonix.io/v1alpha1          Pod · VirtualMachine · Container · Workload
networking.delonix.io/v1alpha1       Network · NetworkRoute · NetworkPolicy · Dependency · Egress
gateway.delonix.io/v1alpha1          Gateway · HTTPRoute · Ingress
storage.delonix.io/v1alpha1          Volume · Storage · ShareVolume
artifact.delonix.io/v1alpha1         Image
infrastructure.delonix.io/v1alpha1   KubernetesCluster
```

**O `apiVersion: delonix.io/v1` continua a carregar.** A promessa de
estabilidade diz que só muda com um `v2` que o continue a aceitar, e mantém-se:
os 27 exemplos publicados foram migrados para os grupos novos e **os antigos
validam na mesma**, verificado e não afirmado.

### Quatro renomeações, com o nome antigo aceite

| antes | agora |
|---|---|
| `Vm` | `VirtualMachine` |
| `FirewallPolicy` | `NetworkPolicy` |
| `Tunnel` | `Gateway` |
| `Cluster` | `KubernetesCluster` |

Alias **silencioso**, não depreciação: uma renomeação não muda o que o documento
significa, portanto não há nada para migrar — e avisar sobre isso treinava as
pessoas a ignorar avisos. Uma **fusão** (`Egress`→`NetworkPolicy`) continua a
avisar, porque aí a semântica mudou.

O alias vale em todo o lado, não só no carregador. Isso era um bug: o
`explain Cluster` recusava um nome que o manifesto aceitava.

### `kind: Container` é anunciado, não reescrito

O `kind: Pod` passa a ser o caminho para declarar containers, e o
`kind: Container` ganha um aviso por carregamento a dizê-lo. **Não é reescrito**,
e a razão é a que quase nos escapou: um Pod constrói sempre uma netns partilhada
e os membros entram nela por re-exec, portanto baixar o `Container` daria a cada
container declarado um holder de netns extra e um caminho de rede diferente.
Isso não é um degrau de grafia — é uma mudança de forma de execução aplicada em
silêncio a manifestos que já correm.

Daí uma forma nova no registo, `sunset`, distinta de `deprecated`: aquele é
reescrito, este não. Ver [Estrutura de recursos](../estrutura.html).

### `delonix api-resources`

O primeiro comando da árvore nova. Lista o que o motor serve — plural,
abreviaturas, `apiVersion` e o que cada documento se torna — a partir do mesmo
registo que o parser, o schema, a completação e o reconciliador leem.

```
NAME               SHORTNAMES  APIVERSION                      KIND            FORM
pods               po          compute.delonix.io/v1alpha1     Pod             primary
containers                     compute.delonix.io/v1alpha1     Container       sunset → Pod
egresses                       networking.delonix.io/v1alpha1  Egress          deprecated → NetworkPolicy
```

`get`, `describe` e `explain` aceitam agora as quatro grafias: `Pod`, `pod`,
`pods`, `po`.

### Códigos de saída: duas classes novas

| código | quando |
|---|---|
| `69` | capacidade que este host não tem (`wg`, `virt-customize`, `ngrok`… por instalar) |
| `124` | o prazo esgotou-se (`stack wait --timeout`) |

As duas respondiam `1` — o mesmo número de um apply rebentado. Um `stack wait`
que esgota o tempo lido como «rebentou» faz um reconciliador recriar um recurso
que estava a subir. `69` é o `EX_UNAVAILABLE` do `sysexits.h`; `124` é o do
`timeout(1)`. **`3`, `4` e `5` não mudam.**

Os erros ganham também identidade textual (`DX_NOT_FOUND`, `DX_TIMEOUT`, …),
visível no corpo de erro do `serve api`, para quem classifica por texto em vez
de `$?`.

### Três bugs corrigidos

* **Um sinal deixava o terminal em modo raw.** Um `SIGTERM` a um
  `container exec -it` deixava a shell sem eco e sem edição de linha até se
  escrever `reset` às cegas. Acontecia com qualquer morte por sinal — um `kill`,
  um timeout de CI, o OOM killer.
* **`stack apply --replace` ignorava autorizações em silêncio.** `--replace
  container/web` (minúsculas) não casava, e o apply era recusado a mandar passar
  a flag que a pessoa julgava ter passado. Num portão destrutivo é a pior
  maneira de falhar.
* **`kind: Pod` estava recomendado e inutilizável ao mesmo tempo.** As
  referências de `Dependency`, `NetworkPolicy` e `HTTPRoute` só reconheciam
  `kind: Container`.

Os três foram encontrados a **usar** o caminho novo, não a lê-lo.

### O que NÃO entra

* **`kind: Service`** — previsto, não implementado. A publicação de portas
  continua a fazer-se pelo `-p` do `container run` e pelo
  `delonix net ingress publish`.
* Os verbos declarativos (`get`, `describe`, `delete`, `wait`) — o
  `api-resources` é o primeiro; os restantes vêm a seguir.

### Migrar

* **Nada é obrigatório.** Os manifestos existentes carregam sem alteração.
* Quem quiser o vocabulário novo: trocar o `apiVersion` pelo grupo do Kind e o
  nome pelo canónico. `delonix api-resources` diz qual é qual.
* Quem tem `kind: Container` verá um aviso por carregamento. Migrar para
  `kind: Pod` **muda a forma de execução** (netns partilhada) — não é uma troca
  de nome, e a página de estrutura explica porquê.

---

## v0.63.1 — o instalador que não instalava a sua própria actualização

Bug report real: `~/.local/bin/delonix` de um host de desenvolvimento estava preso
na v0.59.0 — quatro releases atrás da v0.63.0 já publicada — apesar de o
`install.sh` ter sido corrido depois. O sintoma foi confirmado ao vivo antes de
tocar em código: a causa era `sudo -v`, autenticado incondicionalmente logo no
arranque do script, para qualquer utilizador não-root, **antes** de saber se
alguma coisa ia mesmo precisar de root.

### O caminho que ficava sempre por correr

`--user` (binário em `~/.local/bin`, sem tocar em nenhum pacote do sistema) é
precisamente o modo para quem não tem — ou não quer usar — root. Mas o
`sudo -v` corria de qualquer forma, e sem TTY ou credencial em cache
(automação, um utilizador restrito, uma sessão sem terminal), o script morria
em `sudo authentication failed` **antes de sequer descarregar o binário**. Voltar
a correr o instalador só para apanhar uma release nova — o caso mais comum de
todos — falhava sistematicamente nesse host.

Isolar a lógica de download/verificação/instalação do binário e corrê-la à parte,
sem sudo nenhum, confirmou que ela própria nunca teve problema nenhum: actualizou
um binário de teste para a v0.63.0, checksum e assinatura minisign incluídos.

### A correcção

A autenticação eager passa a correr **depois** da secção do binário, não antes.
Isto não relaxa a garantia original ("um `pkg_install` falhado adiante significa
mesmo 'pacote indisponível', nunca 'sudo falhou em silêncio'") para as secções que
genuinemente precisam de root (deps core, subuid, AppArmor, VM, tuning) — só adia
o momento em que a autenticação acontece até depois do único caminho que não
precisa de root nenhum.

O caminho por omissão (root, `/usr/local/bin`) ganhou de caminho uma guarda `||
die` explícita à volta do `install` do binário, que passou a ser a 1ª chamada a
sudo nesse caminho — sem ela, um sudo sem TTY/cache abortava ali com o stderr cru
do `set -e`, em vez de um erro claro e accionável.

### Validado ao vivo, nos dois modos

- **`--user --no-vm --no-tune`, sem sudo disponível**: o binário deste host foi
  actualizado de **0.59.0 para 0.63.0** com sucesso; só a secção de dependências
  (que precisa mesmo de root) falha a seguir — com o binário já instalado.
- **por omissão (root), sem sudo disponível**: falha com uma mensagem clara
  (`could not install the delonix binary to /usr/local/bin — sudo failed or the
  destination isn't writable...`) em vez de um abort cru.

### As duas metades

**Provado**: os dois modos, ao vivo, contra um host real com um binário
genuinamente desactualizado. **Não validado**: não há gate de CI dedicado a
`install.sh` (sem `shellcheck` no `ci.yml`) — a prova desta versão é `bash -n` +
execução real, não um teste automatizado que trave uma regressão futura.

---

## v0.63.0 — o código passa a escrever-se em inglês, e um portão no CI garante que fica assim

Português no código nunca foi decisão de arquitectura — foi o que sobrou de
antes de haver catálogo de tradução. O `help` da CLI já era autorado em inglês
e traduzido por catálogo (`cmd::po::has_pt_translation`) desde a v0.32.2; o que
ficou em português ficou por ter **saltado** esse catálogo, não por escolha.

### O que se mediu antes de mexer

| | valor |
|---|---|
| nomes de função em PT (runtime + paas) | **1165** |
| … que são nomes de **teste** | **1145 (98,3%)** |
| … privados (não-teste) | 18 |
| … públicos | **2** |
| itens públicos com PT em `serde(rename)` / campos `pub` / flags de CLI | **0 de 1690** |

A conclusão que os números sustentam: quase toda a dívida é cosmética — nomes
de teste, não superfície pública — mas «quase toda» não é «toda», e sem um
número não havia como saber a diferença nem como impedir que crescesse.

### O gate

`scripts/lang_ratchet.py` (job `lang` no CI) conta três dívidas —
identificadores, comentários e mensagens ao utilizador — contra
`scripts/lang_baseline.json`. É um **ratchet nos dois sentidos**, tal como o
`ARG_HELP_PENDING` do `help_i18n_tests`: falha se o número **subir** (entrou
português novo) e falha se **descer** sem a linha de base ter sido baixada no
mesmo commit. Um `<=` deixaria a dívida a ler-se como verde para sempre, mesmo
que ninguém a estivesse a pagar.

```bash
python3 scripts/lang_ratchet.py --list --only identifiers   # o que falta
python3 scripts/lang_ratchet.py --update                    # baixar a base
```

Provado a correr, não afirmado: injectar uma função nova em português falha o
gate (`identifiers: 1085 > 1084`); traduzir sem `--update` falha do outro lado
(`comments: 3482 < 3543`). O gate reage às duas direcções, não só à que
interessa hoje.

### Uma armadilha do próprio detector

O léxico de palavras em português não levava em conta homógrafos PT/EN —
`remove`, `media`, `data`, `base`, `no`, `so`, `ate`, `ver`, `pos`, `seg`. `nas`
foi o pior caso: colide com **NAS**, o armazenamento, e na primeira passagem
deu seis falsos positivos que contavam comentários já em inglês como dívida.
Um contador com falsos positivos não é um contador — é ruído com um número à
frente, e teria inflacionado a linha de base logo na primeira medição.

### Primeira tranche traduzida

`crates/delonix-runtime-bin/src/cmd/prune.rs` por inteiro — 34 identificadores,
65 comentários, 4 mensagens de asserção (por exemplo, `vm_esta_viva` passa a
`vm_is_alive`). `cargo test -p delonix-runtime-bin`: 625 passed, 0 failed.
`cargo fmt --all --check` limpo.

### As duas metades

**Provado:** o gate apanha nas duas direcções; `prune.rs` traduzido compila,
formata e passa os 625 testes do crate; a convenção fica escrita no
`AGENTS.md`.

**Não validado:** a suite completa do workspace não correu sobre este ramo —
só o crate tocado. Ficam por traduzir **1049 identificadores, 3482 comentários
e 129 mensagens**, agora contados em vez de invisíveis. O léxico de homógrafos
foi corrigido para os casos encontrados nesta passagem; não há garantia de que
seja exaustivo.

---

## v0.62.0 — uma plataforma que não conseguia puxar do seu próprio registo

A correcção que dá nome a esta versão não saiu de leitura de código nem de um
teste: saiu de uma plataforma em produção que esteve **onze horas e vinte
minutos** sem API, e do gate nocturno que, na primeira corrida real, apontou
para o sítio certo.

### O motor impunha HTTPS e não havia como dizer o contrário

O `scheme_for` falava HTTP à família do loopback e impunha HTTPS a tudo o resto,
sem botão nenhum. O Docker tem `--insecure-registry`, o containerd tem
`certs.d`; este motor não tinha nada — e a consequência não era teórica.

Medido no cluster de produção da NgolaCloud a 2026-08-19: o `POST
/api/images/pull` respondia **HTTP 200 com `{"ok":false}`** e, por baixo,
`registry error: error sending request for url (https://192.168.1.11:5000/...)`.
O registo do próprio cluster serve HTTP simples. O `/v2/images` do control-plane
tinha **zero imagens** — nenhuma carga de inquilino alguma vez tinha passado por
ali, e todos os `apply` de uma `Application` eram recusados antes de reconciliar
o que quer que fosse.

Um botão que ninguém alcança não é uma fronteira de segurança; é uma parede.

`DELONIX_INSECURE_REGISTRIES` (lista por vírgulas, `host` ou `host:port`) declara
os registos que falam HTTP simples. Adesão explícita por host — nunca uma gama,
nunca uma regra do género «detectar a LAN»: baixar uma ligação em silêncio
porque um endereço parece privado é como uma credencial acaba na rede num sítio
que ninguém inspeccionou. Uma entrada **com** porta prende a promessa àquele
extremo; **sem** porta prende-a à máquina — a mesma assimetria do Docker.

**O default não se move:** um host não declarado continua em HTTPS, e há um
teste que falha se isso alguma vez mudar.

### Dois defeitos apanhados a testar, um deles mais velho que esta mudança

* **`[::1]:5000` nunca era reconhecido como loopback.** O host era extraído com
  `split(':').next()`, que numa autoridade IPv6 entre parênteses rectos devolve
  `"["` — por isso o literal `"[::1]"` na lista de loopback era inalcançável.
  Uma verificação que se lia como coberta e não estava. Agora o `bare_host`
  interpreta os parênteses, e o teste que o apanhou fica.
* **A comparação baixava a caixa de um só lado**, portanto uma lista escrita com
  maiúsculas era um botão que parecia posto e não estava.

O erro de transporte ganhou também a dica que o transforma em acção — nomeia o
host e a variável — e só onde ela pode ser verdadeira: um host já declarado, ou
loopback, recebe a mensagem inalterada. Assim nunca convida ninguém a abrir um
registo que não teve nada a ver com a falha.

### O resto da versão

* **`vm ls --namespace`, e `prune` para `vm`/`cluster`/`stack`** — o `prune`
  cobria containers e imagens e deixava de fora precisamente o que ocupa mais
  disco.
* **`system prune --dry-run`**, com as duas metades separadas por nome: o que
  seria apagado e o que ficaria.
* **O tecto AGREGADO em rootless tinha três das quatro dimensões** — faltava o
  disco. Um tecto que não cobre uma dimensão não é um tecto, é uma estatística.
* **`close_range` em musl passa a ser chamado pelo número da chamada**, não pelo
  wrapper do libc, que não existe em todas as versões.
* **A pré-semeagem de imagens saltava em silêncio** e a golden saía vazia — o
  modo de falha mais caro que há, porque só aparece quando a imagem é usada.
* O help do `-m`/`-c` ainda anunciava o default antigo, corrigido na v0.61.0.

---

## v0.61.0 — a distância entre o que o motor diz e o que o motor faz

Duas correcções, e o fio comum é desconfortável: em ambas o mecanismo estava
construído, testado e a funcionar — e em ambas havia um caminho onde o motor
**sabia** o que era preciso fazer e não o fazia. Uma delas contradizia
explicitamente a sua própria documentação, no mesmo ficheiro, a poucas linhas de
distância. Nenhuma das duas saiu de leitura de código: a primeira apareceu num
registo de container de produção, a segunda numa máquina onde duas portas
serviam HTTP sem que registo nenhum soubesse delas.

### Uma carga sem limites declarados já não fica sem tecto nenhum

O `-m` tinha por omissão `max` — a palavra do cgroup-v2 para **sem tecto**,
copiada do Docker. Uma só carga com fuga podia consumir tudo o que o motor tem,
e o motor limitava-se a **imprimir um aviso** a dizê-lo, no
`warn_if_unprotected_memory`: «a leak in the workload can take the whole host
down».

O que transforma isto de default infeliz em incoerência é o que estava escrito
ao lado. No `write_limit`: «limits are MANDATORY — a container should never run
without a resource ceiling». No `spawn`: «Unlike Docker (which by default limits
nothing), Delonix refuses to run a container without resource ceilings». Duas
afirmações a dizer o contrário do que o código fazia, e um aviso em runtime a
admitir o problema em vez de o resolver.

Encontrado num registo real: `memory_max: max` num container de produção com
2.9 GiB residentes.

Agora uma carga sem `-m`/`--cpus` recebe **um quarto do orçamento do motor** — o
`delonix.slice`, que é `DELONIX_RESERVE_PCT`% do host (85 por omissão). Fracção
do **slice** e não do host, e a diferença não é cosmética: assim quatro cargas
sem limites enchem o orçamento exactamente, e nenhuma sozinha o esgota. Se
fossem 25% do host, quatro dariam 100% da máquina e só o tecto agregado as
apanharia — o número anunciado e o real deixariam de bater certo.

O declarado sobrepõe sempre. Isto é o valor por omissão, não um tecto duro:
`-m 12G` continua a dar 12G. `DELONIX_DEFAULT_PCT` afina a fracção, limitada a
`1..=100` para que uma gralha caia no default em vez de desligar em silêncio o
tecto que a função existe para garantir.

**O CPU só aperta, nunca alarga**, e vale a pena dizer porquê. Toma o *menor*
entre esse quarto e o histórico de 1 core. Num host de 32 threads um quarto
seriam 6.8 cores — alargar o default de 1.0 para 6.8 teria sido uma regressão
vestida de funcionalidade de segurança. Num nó de 2 vCPU o quarto é 0.42 e é o
`1.0` que sempre foi metade da máquina. O default absoluto nunca esteve errado
numa direcção: estava errado em **ambas**, conforme o ferro, e é exactamente
isso que uma fracção corrige.

Medido contra o kernel, não contra a suite:

```
sem -m             → memory.max 6968836096 (6646M)   cpu.max 100000 100000
-m 256M --cpus 0.5 → memory.max  268435456 (256M)    cpu.max  50000 100000
```

**O que NÃO foi feito:** o tecto de I/O. O systemd delega `cpu memory pids` e
**não** `io` ao `user@.service` — medido — portanto nenhum motor sem privilégio,
Podman incluído, escreve `io.max` numa folha. Não é lacuna deste motor; é uma
fronteira de delegação que se fecha no substrato. Dos três recursos, esta
release cobre dois, e diz qual falta.

### Um porto publicado pela API deixa de desaparecer no reinício da rede

O `POST /v1/net/publish` chamava `infra::publish_port` e parava aí. Escrevia o
estado **actual** e saltava o **desejado** — a inversão exacta do modelo que o
resto do motor segue.

Uma publicação vive em dois sítios, ambos voláteis: o `hostfwd` dentro do
processo `slirp4netns`, e as regras nft dentro da infra netns. A **única** cópia
durável é o campo `ports` do registo do container, e é exclusivamente dela que o
`cmd_start` e o `reconcile_after_respawn` a repõem. Um porto publicado por esta
via funcionava até ao reinício do ingress e depois desaparecia sem deixar de
onde o reconstruir. O motor não o podia restaurar — não sabia que ele era
pedido.

Medido num host vivo: **18 registos de container todos com `ports: []`** enquanto
`127.0.0.1:8077` e `:8079` respondiam 303 e 200. Publicações que nenhum registo
conhecia, a funcionar até ao próximo arranque do slirp.

Ambas as rotas passam agora a escrever a mesma fonte de verdade que o `-p` da
CLI já escrevia. O publish **grava primeiro e publica depois** — a ordem
inversa perde o registo se o processo morrer entre as duas, e a assimetria
importa: um porto gravado mas não publicado é a falha recuperável (o próximo
start republica-o), enquanto um porto publicado mas não gravado é precisamente
este bug. Se a publicação falhar, o registo é revertido; senão o arranque
seguinte repunha um porto que o operador nunca chegou a ter. O `DELETE`
apaga-o do registo, ou o arranque seguinte republicaria fielmente um porto
acabado de retirar.

Um IP que não pertence a nenhum registo nosso — uma VM, uma netns alheia — não é
erro. O ingress endereça cargas por IP e esta função regista o que consegue; não
é um segundo portão de admissão, e a publicação segue à mesma.

**O que NÃO foi feito:** as publicações órfãs que já existem num host não são
reparadas. Não estão em registo nenhum, e adivinhar a que container pertence um
`hostfwd` vivo seria inventar estado — precisamente o que este motor não faz.
Para essas o caminho é republicá-las pela rota já corrigida, ou declará-las e
fazer `container restart`.

### Nota de migração

A primeira mudança **altera comportamento por omissão**. Um container que hoje
arranca sem `-m` e cresce acima de um quarto do orçamento do motor passará a
bater num tecto onde antes não havia nenhum — que é o objectivo, mas convém
sabê-lo antes e não durante. Duas saídas, ambas explícitas: declarar o limite
real (`-m 12G`), ou subir a fracção com `DELONIX_DEFAULT_PCT`.

O CPU não regride em máquina nenhuma: a regra toma o menor entre o novo cálculo
e o `1.0` de sempre, portanto nenhum container fica com menos CPU do que já
tinha, excepto em nós pequenos onde o `1.0` era mais de um quarto da máquina —
onde apertar era o ponto.

---

## v0.60.0 — o que o kubelet pediu, o que a migração não pode destruir, e o container que se fecha a si próprio

Cinco correcções, e o fio comum é o de sempre neste repo: nenhuma saiu de leitura
de código. Duas saíram de um cluster real a falhar, uma custou um container em
produção, uma só apareceu quando alguém pôs uma carga a sério em cima, e a
última estava escondida atrás de um gate vermelho que ninguém tinha lido até ao
fim.

### O kubelet mandava um filtro e nós listávamos o nó inteiro

A 6.ª parede do control-plane do DKS. Os static pods levavam um teardown
gracioso 30 segundos depois de arrancarem, com `NRestarts=0` no `delonix-cri`
(logo já não era o systemd — isso fora fechado na v0.58.0) e um SIGTERM
**explícito** no apiserver. Ao lado, `"Container not found in pod's containers"`
**617 vezes em 3 minutos** e os registos de sandbox a crescer de 67 para 135 com
quatro pods no nó.

Uma causa só, e explica os quatro sintomas ao mesmo tempo: o `ListContainers` e
o `ListPodSandbox` recebiam o pedido como `_r` e **descartavam o filtro**. O
kubelet constrói o estado de um pod listando os containers *daquela* sandbox;
sem filtro recebia os do nó inteiro, e o que não encontra no spec do pod, mata.
O teardown gracioso não era uma falha nossa a jusante — era o kubelet a fazer
exactamente o que lhe dissemos.

O `list_pod_sandbox_stats`, no MESMO ficheiro, já honrava o seu filtro. Não é
mecanismo em falta; é um consumidor esquecido, a mesma família do
`safe_to_signal` e do `write_private_temp`.

Os predicados são puros e correm sobre o objecto JÁ construído e não sobre o
registo — o `state` é derivado, logo casar no valor construído é o que impede
derivá-lo duas vezes e o que garante que o filtro nunca discorda do que a mesma
chamada reporta. O `label_selector` é **subconjunto** e não igualdade de mapas:
um container real carrega todas as labels que o kubelet lhe pôs, e comparar
mapas inteiros não casaria com nada. Filtro ausente ou vazio continua a listar
tudo — é o contrato do CRI, e é o que o `crictl ps` usa.

### Um container que se fecha a si próprio deixou de não conseguir arrancar

O allowlist de seccomp do motor não continha o **próprio `seccomp(2)`**. Um
payload que instala o seu próprio filtro — o que qualquer runtime moderno faz
para se endurecer — levava EPERM na primeira coisa que tentava fazer, e o
sintoma chegava ao operador como «o container não arranca», sem nomear o
syscall.

Não abre superfície nenhuma: `SECCOMP_SET_MODE_FILTER` exige `CAP_SYS_ADMIN`
**ou** `no_new_privs`, e filtros empilhados são combinados por AND pelo kernel.
Um container só se pode restringir MAIS a si próprio — nunca menos.

### A migração para layers partilhadas, e a guarda que custou um container

A v0.59.0 pôs os containers rootless a partilhar a imagem em vez de a copiarem,
mas os criados antes disso ficaram cada um com a sua cópia privada, e não há
forma de converter um **em vida**: o processo fez `pivot_root` para essa árvore e
tem ficheiros abertos lá dentro. O que há é a paragem que já aconteceu — um
`start` é o único momento em que a árvore não é de ninguém.

A ORDEM é correcção e não gosto: `rename(rootfs→upper)` (atómico), depois os
**whiteouts**, e só então o `overlay-lowers`. Um rootfs flat é a imagem mais as
escritas já fundidas, logo um ficheiro que o container APAGOU está simplesmente
ausente — sem whiteout o overlay serve-o de volta a partir da lower, e uma
config purgada ou um segredo rotado a reaparecer é bem pior que o disco que se
poupa. O `overlay-lowers.pending` é o ponto de COMMIT: renomeá-lo é o único
passo que torna o container overlay, e tudo antes dele reverte.

Validado ao vivo com o binário **0.58.0 real** (para o container flat ser
genuíno e não fabricado): 9 MB → 1 MB, escrita própria preservada, e o ficheiro
apagado a NÃO ressuscitar.

**E depois a produção encontrou o que a validação não tinha.** O
`whiteout_missing` lê «está na lower, falta na upper» como «o container apagou
isto». Para um ficheiro é a leitura certa. Para um directório de SISTEMA de topo
não é: nenhum container apaga `/usr`, `/etc` ou `/bin`. Com a árvore flat vazia
ou truncada, cada ausência dessas virava um whiteout e o resultado era um
container permanentemente vazio — o rootfs **destruído pelo passo que existia
para o encolher**. Medido: um `postgres:15-alpine` migrou para uma upper com
doze whiteouts e mais nada, e o arranque seguinte morreu em `could not write
'etc/hostname': Not a directory` — o `etc` era, a essa altura, um character
device. (Os dados estavam num volume e sobreviveram; foi essa a única razão de
isto ter sido um susto e não uma perda.)

A guarda fica no NÍVEL DE TOPO, onde a ausência é implausível; os whiteouts mais
fundos continuam a funcionar tal e qual. Recusar custa exactamente o disco que a
migração ia poupar, e o container continua a correr flat. Destruir custa o
container.

É a mesma classe que este repo já cataloga em «X não é Y»: um directório vazio
não é «o container apagou tudo» — é muito mais provavelmente uma árvore que
nunca esteve completa.

### O `qemu-guest-agent` viaja na golden

Medido primeiro, com `virt-ls`: nem a golden 1.34 nem a
`delonix-vm-base:ubuntu-24.04` traziam o agente. Num nó Proxmox isso faz o
`ProxmoxBackend::ip` responder `None` — e ali `None` é resposta de primeira
classe, por isso a ausência do agente era **invisível** até alguém precisar de um
endereço. É também o que dá ao PBS um backup quiesced.

O obstáculo era o `--offline`, que constrói com `--no-network`: um `apt-get
install` dentro do convidado não existe. O fecho foi medido contra o índice
real — `qemu-guest-agent` em `universe`, `liburing2` em `main`, e as outras
quatro dependências já na cloud image: dois ficheiros a carregar.

A cadeia de confiança é a do apt, feita no host, mas com um âncora **melhor** que
o do caminho do k8s: a chave do arquivo Ubuntu vem com a distro, em vez de se ir
buscar ao próprio repo em que se está a confiar. Keyring ausente **falha** em vez
de cair para o âncora fraco. E o codinome sai do `/etc/os-release` da própria
imagem, não de uma tabela `24.04→noble` que envelhece na próxima release.

### Os gates da main voltaram a passar — e um teste morto voltou a correr

Medido num worktree isolado de `origin/main`, não na árvore partilhada: o `fmt`
acusava 21 hunks em sete ficheiros e o `clippy -D warnings` chumbava o
`delonix-cri`. **Não era regressão recente** — a tag v0.59.0 tem exactamente os
mesmos 21 hunks, ou seja a release anterior saiu de uma main que falha os seus
próprios gates.

O `clippy` parar no primeiro crate a falhar é o que tornou isto mais caro do que
parece: enquanto o `delonix-cri` não compilava sob `-D warnings`, os crates a
seguir **nunca eram analisados**. Corrigido o primeiro, apareceram quatro
achados escondidos atrás dele — e dois são defeitos a sério:

- `no_k8s_rejeita_k8s_version_offline_e_cri_bin` tinha perdido o `#[test]`. Não
  era um teste a falhar: era um teste que não existia para o harness, com o
  aspecto de estar lá. O sintoma visível era `function is never used`, que se lê
  como código morto e é, na verdade, cobertura em falta.
- `let eng = PathBuf::from("/tmp/delonix");` escrito duas vezes seguidas; a
  segunda sombreia a primeira e não faz nada.

Um gate vermelho de que ninguém lê o fim deixa de ser um gate. Depois disto:
`fmt` 0 hunks, `clippy -D warnings` limpo, e as suites dos crates tocados em
594 + 25 + 1 testes sem uma falha.

---

## v0.59.0 — containers rootless partilham a imagem em vez de a copiarem

Um bug report abriu isto, e a proposta que trazia estava certa na intenção e
apontada ao sítio errado: «os containers estão a encher o disco, vamos comprimir
os volumes». A medição não concordou com a premissa, e é daí que sai tudo o
resto.

> **Nota de proveniência.** Nada aqui saiu de leitura de código. Saiu de `du`,
> `stat` e `zstd` sobre o disco de um host a 93%, e de correr a coisa contra o
> kernel antes de a desenhar. Duas das correcções só apareceram a validar, e
> ambas estão descritas abaixo com o sintoma real.

### O disco não estava cheio de dados compressíveis — estava cheio de duplicados

`containers/` tinha 47 GiB. Medido, com `nlink` e não com suposições:

| | |
|---|---|
| containers da MESMA `kaeso-odoo:16` | 21 |
| cópia física por container | 2,1 GiB |
| `nlink` de `libwkhtmltox.so` em cada um | **1** |
| duplicação byte-a-byte | **~39 dos 47 GiB** |
| custo de cada `run` | **13 s e 2,2 GiB** |

Em rootless o `prepare_rootfs` fazia `export_rootfs` — uma cópia FLAT completa da
imagem por container. O caminho de overlay existia (`mount_rootfs`) e nunca corria
em rootless, com uma razão boa: `mount(2)` de um uid sem privilégio é EPERM.

**Mas o mount não tem de acontecer na CLI.** O `setup_rootfs` já corre dentro do
clone, como criador do user namespace e com caps completas sobre ele — a mesma
janela que já lhe permite fazer bind-mount e `pivot_root`. Medido neste kernel
antes de escrever uma linha: um `mount -t overlay` sem privilégio dentro de
`unshare --user --map-root-user --mount` monta, lê da lower e escreve na upper.

Fica então: `ImageStore::prepare_overlay` faz fora o que tem de ser feito fora
(extrair e cachear as layers, criar a camada de escrita) e deixa um
`overlay-lowers` ao lado do `merged/`; `mount_overlay_if_marked` lê-o e monta
dentro do clone. É contrato em disco e não um campo no `RunSpec` de propósito —
os caminhos rootless re-executam o binário (`--net <custom>`, `--pod`) e o que o
mount precisa tem de sobreviver a essa fronteira.

**A partilha é segura por construção, e foi verificada e não deduzida**: contra um
container a correr como uid 0 no seu userns, escrever e apagar um ficheiro da
lower deixou-a com o mesmo inode e os mesmos bytes. O overlayfs faz copy-up antes
da primeira escrita e regista remoções como whiteouts na upper.

Medido depois, com seis containers da mesma imagem:

| | antes | depois |
|---|---|---|
| por container | uma cópia completa | **1 MiB** |
| árvore partilhada | — | 17 MiB de layers |
| custo por `run` | 13 s / 2,2 GiB | mount |

E no host real, com o `prune` dos órfãos a acompanhar: `containers/` de **47 para
7,2 GiB**, disco de **93% para 87%**.

### O `chown_tree` que nunca fez nada

O `chown_tree(…, USERNS_UID_BASE)` que seguia a cópia desaparece, e a razão é que
nunca funcionou: `lchown` para o uid 100000 a partir de um uid sem privilégio é
EPERM, e o `lchown_tree` engole o erro. Medido — as layers extraídas e todos os
rootfs flat deste host são uniformemente `1000:1000`.

Funcionam porque o mapa rootless é `0 <euid> 1`: o uid 0 DENTRO do namespace É o
uid que invocou, e os ficheiros já se lêem como `root` para o container. É também
isso que permite a uma layer extraída servir todos os containers sem tocar em
nada.

### Dois defeitos que só a validação revelou

**O `commit` arrastava o `/proc` do host.** Ao ler por `/proc/<pid>/root`, a
árvore passa a incluir os mounts do container — o empacotador descia pelo procfs
real. Falhava com `Permission denied` muito antes de acabar; se acabasse, teria
escrito um retrato do estado do kernel do host dentro de uma imagem publicada.
`pack_rootfs_tar` mantém `proc`/`sys`/`dev` como directórios VAZIOS, que é o que
uma imagem precisa de ter para o runtime ter onde montar.

**`Command` + `pre_exec` faz deadlock com um handshake de userns.** O `spawn`
espera que o filho chegue ao `exec` (lê um pipe CLOEXEC que só fecha lá) para
poder reportar falhas de exec; o handshake bloqueia o filho ANTES do exec, à
espera dos mapas que o pai só escreve depois do `spawn` retornar. Os dois ficam à
espera um do outro. Sintoma medido: `container cp` de um container parado
pendurado até ser morto. O `reexec_mapped` ao lado usa `fork` cru exactamente por
esta razão, e o novo `reexec_mapped_hold` passou a fazer o mesmo.

### `cp` e `commit` num container overlay PARADO

A mudança deixou uma regressão: um container parado não tem árvore legível de
fora — o `merged/` é um directório vazio até alguém montar o overlay.

As duas saídas óbvias são ambas más: empacotar a árvore para um directório
temporário custa exactamente a cópia que este motor acabou de deixar de fazer, e
refazer a fusão do overlayfs em userspace é uma segunda implementação da
semântica do kernel, que deriva dela.

Usa-se a MESMA porta que um container a correr já usa: `__ovlhold` segura o mount
na sua própria namespace e o resto do código lê por `/proc/<pid>/root`. Nada a
jusante muda — `cp` e `commit` continuam a receber um caminho normal. O
`HeldChild` mata E REAPA no `Drop`; sem o reap, um servidor de vida longa como o
`serve docker-api` acumula zombies, defeito que esse caminho já pagou uma vez.

Validado ao vivo: `cp` para fora devolve o conteúdo certo, `cp` para dentro
persiste na upper e o container vê o ficheiro depois do `start`, e o `commit`
produz uma imagem cuja execução lê a escrita feita antes do stop.

### ADR-0016 — ficar em ext4, e o que reabre a decisão

A proposta original era comprimir. Medido por área com `zstd:1` (o default do
btrfs), sobre os dados reais:

| área | zstd-1 |
|---|---|
| layers (imagem extraída) | **7,4×** |
| blobs OCI | 3,0× |
| overlays qcow2 de VM | 2,3–2,7× |
| filestore Odoo (38 GiB) | **1,12×** |
| `vm-images/` | **1,00×** — a golden já sai com `qemu-img -c zstd` |

Extrapolado, a compressão transparente valeria **~15% do store** — real, mas uma
ordem de grandeza abaixo do que a correcção da duplicação já devolveu. E o
argumento mais forte para um filesystem com CoW gastou-se nesta mesma série: o
reflink existia para tornar barata a cópia por container, e essa cópia deixou de
acontecer. Partilhar uma árvore é melhor que copiá-la barato.

Decisão: **ficar em ext4**, com gatilhos escritos para reabrir e um spike de
loopback btrfs como pré-condição.

### O que NÃO foi validado, e limitações conhecidas

- **Containers criados por um binário anterior continuam FLAT.** Não há migração
  automática, e não é omissão: separar as escritas do container da imagem base
  num rootfs achatado exige um diff contra a imagem, e um diff mal feito perde
  dados. Saem por recriação, ou pelo `system prune` se forem descartáveis. O
  `existing_rootfs_path` resolve os três layouts, por isso os antigos continuam a
  arrancar, parar e ser removidos normalmente.
- **O `build` fica deliberadamente no caminho flat** (`prepare_rootfs_flat`): o
  `COPY` escreve na árvore a partir do host, o `FROM <estágio>` clona-a com
  `cp -a` e o `commit_flat_rootfs` empacota-a, tudo de fora de qualquer namespace.
  É também o caso em que a duplicação não acumula — o container de trabalho morre
  no fim do estágio.
- **O spike de btrfs do ADR-0016 não foi corrido**: montar um loopback exige
  `CAP_SYS_ADMIN` e a máquina de desenvolvimento não tem sudo. Rácios de
  compressão e a ausência de reflink em ext4 foram medidos sem privilégio;
  latência sob carga e a migração do store não.
- **A bateria `scripts/e2e.sh` não foi corrida nesta série** — corre contra o
  estado real da máquina e este host tinha workloads de produção vivos durante a
  sessão. O que foi corrido: `cargo build`/`test`/`clippy` limpos (35 suites, 0
  falhas) e validação ao vivo com dois roots isolados, incluindo o ciclo
  run→stop→start→cp→commit e o caso de `merged/`/`work/` apagados à mão.

---

## v0.58.0 — o disco de um nó passa a ser um número da quota, e reiniciar o CRI deixa de matar containers

Duas mudanças, e ambas saíram da mesma corrida: tentar levantar um cluster
Kubernetes real sobre a golden e ir a cada parede que apareceu.

> **Nota de proveniência.** Nada aqui saiu de uma ideia. Saiu de `kubeadm init`
> repetido numa VM da golden 1.36, a ler o log do etcd e o estado dos containers
> em cada tentativa. Duas das paredes eram desta casa; as duas foram corrigidas.

### `--disk-size`: o nó nasce no piso e cresce até ao que o inquilino paga

Todo o nó herdava o tamanho da imagem golden. Não havia como dimensionar um nó
pelo armazenamento que um inquilino paga — o overlay saía sempre do tamanho da
base, e ponto.

`VmConfig.disk_size_gib` novo, com a flag `--disk-size` no `vm create`, passado
ao `qemu-img create` do overlay. Medido ponta a ponta com a golden 1.36 no piso
de 10 GiB:

| | |
|---|---|
| golden | 10 GiB virtuais |
| nó `--disk-size 40` | **40 GiB virtuais, 26,3 MiB reais em disco** |
| dentro da VM | raiz de 38G, 1,9G usados, **36G livres** |

Nasce no mínimo, promete o que o inquilino paga, e consome só o que usa: o
overlay é fino e o `growpart` do cloud-init estende a raiz no arranque. O número
**provisionado** é o que uma quota conta; o consumido é o que custa hoje.

Uma guarda que nomeia os dois números: pedir menos que a imagem base é
**recusado**, porque um overlay qcow2 não encolhe o seu backing file e algumas
versões do `qemu-img` aceitam-no — o resultado seria uma VM que arranca e
corrompe o filesystem.

```
--disk-size 5G é menor que a imagem base (10 GiB): um overlay qcow2 não
encolhe o seu backing file
```

O tamanho lê-se dos **bytes entre parênteses** do `qemu-img info`, não do número
humano: `2.2 GiB` é arredondado, e uma quota comparada com um arredondamento é
uma quota que deixa passar o que queria recusar. Função pura, com teste.

### Reiniciar o `delonix-cri` deixa de matar os containers que ele lançou

A unit era `Type=simple` com `Restart=always` e **sem `Delegate=`**, logo o
`KillMode` era o `control-group` por omissão. Os containers que o CRI lança são
**netos** desse serviço e vivem no seu cgroup: qualquer reinício levava um
SIGTERM a todos ao mesmo tempo. Num nó Kubernetes isso é o control-plane inteiro
a cair de uma vez — e o sintoma não aponta para o systemd.

Medido: os quatro static pods saíam com `Exited (0)` — saída **limpa** e não
crash, que é o que um SIGTERM a um etcd produz — os quatro no mesmo instante, sem
uma única falha de sonda no kubelet. O mesmo container lançado à mão pelo CLI
ficava `Up`, porque aí não há cgroup de serviço a matá-lo. Era essa assimetria
que não fazia sentido.

`Delegate=yes` dá ao CRI a sua própria subárvore de cgroup, e `KillMode=process`
faz o systemd parar só o processo principal. É exactamente o que a unit do
containerd faz, e pela mesma razão.

Com a unit corrigida os containers passam a manter-se `Up` e a porta 6443 abre
aos ~100s — contra nunca abrir antes de o disco ser corrigido, e cair de
imediato depois.

### O que NÃO foi validado, e é preciso dizer

**Nenhum cluster Kubernetes subiu por completo.** O `kubeadm init` continua a
falhar em `wait-control-plane`. A diferença é que o sintoma passou de silencioso
a legível: o apiserver recebe agora um SIGTERM **explícito**
(`Got signal: terminated. Sending down to process`) com o `delonix-cri` a
reportar `NRestarts=0` — ou seja, já não é o systemd, é outra coisa a pedir a
paragem. Fica identificado como o defeito seguinte, com pista concreta.

**Nenhum worker se juntou a um cluster.** O caminho de join do DKS depende do
control-plane estabilizar primeiro.

**O `--disk-size` só existe na CLI.** O caminho declarativo (`kind: VM`) continua
a herdar o tamanho da imagem porque o `VmSpec` ainda não tem o campo — é o que
liga a quota de um inquilino ao manifesto, e não está feito.

**A golden publicada (`delonix-vm-k8s:1.36`) tem 10 GiB e não levanta um
cluster.** Serve para arrancar um nó e chegar até onde esta release chega.

---

## v0.57.0 — um limite que vale para um GRUPO de cargas, não só para cada uma

Todos os limites deste motor eram **por container**. `memory.max`, `cpu.max` e
`pids.max` aterram na folha do próprio container, e isso responde bem a «esta carga
não pode passar de X». Não responde — nem pode — a uma pergunta diferente: **quanto
podem estas N cargas ter juntas?**

Dez containers de 1 GiB cada são dez containers válidos e 10 GiB de pressão. Nada por
folha repara, porque nada está errado por folha.

> **Nota de proveniência.** Isto não saiu de uma ideia: saiu de uma medição num host a
> correr este motor — onze containers, todos com `memory.max = max`, 4.89 GiB entre
> eles. Cada um dentro do seu limite (ausente). O agregado sem dono nenhum.

### Um nível de cgroup intermédio, com tecto próprio

O `Container` ganha `cgroup_parent` opcional: um cgroup **entre** a base delegada e a
folha, com o seu próprio tecto.

```
<base>/<grupo>/dlx-<id>          em vez de          <base>/dlx-<id>
```

No manifesto:

```yaml
kind: Container
spec:
  image: app:1
  memory: 512M
  cgroupParent: { name: grupo-a, memoryMax: "1073741824", cpus: "2", pidsMax: "512" }
```

O tecto do grupo é escrito **antes** de o processo do container entrar, pela mesma
razão que os limites da folha já o eram: um limite aplicado depois da primeira
alocação é um limite que não estava lá quando fez falta.

**O motor não aprende o que o grupo significa.** O nome é opaco. Quem agrupa cargas —
um PaaS a facturar um cliente, um runner a cercar um job, um operador a repartir uma
máquina — mapeia o seu conceito para ele. Isto mantém intacto o guarda-rio que este
repo tem desde o início: nada de tenancy, licenças ou facturação no motor. É a mesma
forma do ADR-0003. A decisão inteira está no **ADR-0015**.

### Duas coisas que a medição obrigou a corrigir

- **`memory.swap.max = 0` viaja com o tecto de memória.** A primeira sonda falhou de
  forma instrutiva: um grupo capado a 64 MiB deixou um processo alocar 200 MiB **e
  terminar** — as páginas foram para o swap. Só com o swap fechado é que o kernel fez
  o que o operador pediu (`memory.events`: tecto atingido 1466 vezes, depois
  `oom_kill 1`). Uma quota de memória que o swap contorna não é quota. É a mesma
  correcção que a folha do container já tinha.
- **Um nome de grupo inseguro é descartado, não sanitizado.** O nome vem de fora e é
  interpolado num caminho; um `..` sai da base delegada para um cgroup que este motor
  nunca recebeu. Reescrevê-lo em silêncio seria pior do que recusá-lo — quem chamou
  acreditaria ter limitado um grupo que nunca limitou.

### O que foi medido, e o que não

Provado com o binário real e containers reais: duas cargas com folhas de 32 MiB cada
debaixo de um grupo de 32 MiB; cada uma escreveu 20 MiB; o `memory.events` do **grupo**
registou 70 acertos no tecto enquanto **ambas** as folhas registaram zero. É
exactamente o que um limite por container não consegue fazer.

**Não validado, e dito:**

- `cgroupParent` **não é campo quente**. Mudá-lo é mover um container em execução entre
  cgroups, coisa que este motor não faz em vida. O reconciliador reporta-o em
  `FieldsNotCompared` — o operador é avisado e recria. O silêncio é que teria sido o bug.
- O cgroup do grupo **não é removido** quando sai a sua última carga. Um cgroup vazio
  custa um directório, e reciclá-lo corre contra a carga seguinte do mesmo grupo.
  Deliberado.
- A aplicação do tecto é **best-effort**: um tecto que não se consegue escrever não pode
  impedir o container de arrancar. O corolário é que quem chama não pode *confiar* no
  tecto sem o ler de volta do grupo (`memory.max`).
- Onde a base delegada não desce os controladores até ao grupo, o aninhamento acontece
  e o tecto não. O teste de integração diz `NOT PROVEN` nesse caso, em vez de passar
  calado.

### Compatibilidade

Sem `cgroupParent`, nada muda: a folha pendura directamente da base, exactamente como
antes. O campo é `#[serde(default)]` e omitido na serialização quando ausente, por isso
todo o registo já em disco continua a desserializar e os registos sem grupo ficam
byte-a-byte iguais. O JSON Schema publicado regenera-se do código (ADR-0007).

---

## v0.56.0 — cloud-init deixa de ser um ficheiro deste disco

Um tema só, e é uma correcção de vocabulário: o `VmConfig` sabia dizer
`seed: <caminho>` — um ISO NoCloud NESTE sistema de ficheiros — e mais nada. Isso é
um mecanismo no lugar de uma intenção, e a consequência era estrutural: **qualquer
backend cujo convidado corre noutra máquina ficava fora do cloud-init por
construção**, por muito que o produto do outro lado o suportasse.

O Proxmox suporta. Tem cloud-init próprio e fala `ciuser`/`sshkeys`/`ipconfig0` —
só nunca recebeu nada, porque o único vocabulário disponível era um caminho que ele
não pode abrir.

> **Nota de proveniência.** Tudo nesta série foi medido contra um nó **Proxmox VE
> 9.2 real** (clone de um template → boot → SSH lá dentro), e as três correcções da
> última secção só apareceram por causa disso: as três compilavam, passavam nos
> testes, e tinham comentários confiantes a explicar porque estavam certas.

### Cloud-init como INTENÇÃO

O `VmConfig` passa a levar o que o operador QUIS — `hostname`, `ci_user`,
`ssh_keys`, `cloud_init` — e cada backend realiza-o à sua maneira: um ISO NoCloud
nos backends locais, os parâmetros do nó no Proxmox. O `seed` fica como escapatória
para quem traz o seu.

- **Os construtores mudaram-se para o motor** (`delonix_vm::cloudinit`). Viviam no
  crate BINÁRIO, que ninguém pode importar, por isso cada consumidor tinha a sua
  cópia — a CLI, o `cluster kubeadm`, e o PaaS privado noutra. Três cópias de um
  formato com que o convidado tem de concordar.
- **A intenção é PERSISTIDA** (`VmBootSpec`), pela regra que o doc-comment desse
  struct já enuncia: o `vm start` delega no auto-heal do `create`, e uma VM cujo
  convidado desapareceu é reconstruída — sem estes campos voltava com o hostname
  por omissão e SEM chave nenhuma, ou seja inalcançável.
- **`cloud_init` é `Option<bool>` e não `bool`** porque o struct deriva `Default`:
  um bool cairia em `false` e desligaria o seed a todos os chamadores que usam
  `..Default::default()` — e uma VM sem datasource salta a fase de rede do
  cloud-init e sobe sem endereço.
- O `--user-data` continua recusado num backend remoto, e a razão não mudou: um
  documento arbitrário não tem equivalente na API do nó (precisaria de um snippet
  na storage dele, que o endpoint de upload não aceita), logo honrá-lo obrigaria a
  um segundo canal privilegiado.

### `cluster kubeadm` provisiona para um backend remoto

- **A imagem era sempre resolvida contra o store LOCAL** — `qcow2_path()`
  incondicional transformava uma referência válida do nó (`template:9000`) num
  caminho deste host. Num backend com storage própria a referência passa VERBATIM,
  e sem imagem explícita RECUSA em vez de descarregar uma golden que ninguém vai
  usar.
- **`backend_manages_own_storage(None)` respondia pela AUTO-DETECÇÃO.** A
  precedência do backend escolhido uma vez em vez de por comando
  (`DELONIX_VM_BACKEND`, depois o default persistido) vivia inteiramente dentro do
  `create_with`. Numa máquina configurada para Proxmox, qualquer chamador que
  perguntasse «isto é remoto?» recebia **não** — e seguia a preparar um overlay
  local para um convidado que vai correr noutra máquina. Extraído para
  `delonix_vm::standing_backend_choice`, usado pelos dois.

### Três defeitos que só um nó real revelou

1. **`configure_clone` mandava `authed: false`.** O nó respondeu **401 logo a
   seguir a um clone bem-sucedido** — a VM ficava com a CPU e a memória do template
   e sem chave nenhuma.
2. **Uma falha depois do clone deixava a VM no nó.** Medido: a VMID 100 ficou lá,
   invisível ao `vm ls` (nenhum registo escrito) e removível só à mão pela UI. O
   `create_with` não pode limpar isto — a limpeza dele é de um overlay local, e o
   vmid só se conhece dentro do `boot`, que é onde o undo passou a viver.
3. **O `sshkeys` tem de ir percent-encoded por nós**, por cima do encoding do
   formulário. O comentário original afirmava o contrário, com uma explicação
   convincente; o nó respondeu `400 … "invalid format - invalid urlencoded
   string"`. É o único parâmetro desta API com essa forma.

**E o clone nunca aplicava configuração nenhuma.** A API de clone leva
`newid`/`name`/`full` e mais nada, por isso uma VM pedida com 4 vCPU e 8 GiB subia
com o que o template tivesse, na bridge do template, sem endereço e sem chave. É o
caminho de um nó DKS a partir de uma golden registada como template.

### Validado ao vivo

Contra PVE 9.2, com a VM removida no fim e zero restos no nó:

```
cores   = 2        ← do chamador, não do template
memory  = 1024     ← idem
net0    = virtio=...,bridge=vmbr0
ciuser  = delonix
sshkeys = ssh-ed25519 AAAA... (descodificado, exacto)

$ ssh -i <chave> delonix@192.168.122.56
LOGIN-OK / ci-pve-01 / delonix / nproc=2 / Mem 955 MiB
```

E o caminho local, que não podia regredir: VM libvirt real, seed gerado pelo MOTOR,
chave sob `users: - name: delonix` (e não na conta da distro), SSH lá dentro.

### O que NÃO foi validado

- **Um cluster Kubernetes completo sobre Proxmox.** O provisionamento, a
  configuração e o SSH foram exercitados de ponta a ponta pelo caminho declarativo;
  o `kubeadm init` parou por duas razões que não são deste código: o template do
  lab não traz `kubeadm` (é uma base rootless, não a golden k8s), e o
  `cluster apply` corre `apt-get` sem esperar que o cloud-init do primeiro arranque
  assente — uma corrida real, registada aqui como achado por fechar.
- **A forma password do `sshkeys` com chaves não-ASCII**: o `urlencode` codifica
  bytes, e há teste para isso, mas nenhuma chave real deste tipo foi enviada a um nó.

### Nota de método

Duas diagnoses erradas nesta série tiveram a mesma causa: **um `CARGO_TARGET_DIR`
partilhado entre worktrees em commits diferentes**. Um build a partir de outro
worktree sobrescreve o binário, e as corridas seguintes medem código que não é o
que se está a editar; e um output de build script de uma sessão já fechada deixou
caminhos absolutos para um worktree removido, com o `cargo test` a falhar por
ficheiros que nunca existiram nesta árvore. O `AGENTS.md` manda partilhar o target
dir para reaproveitar cache — o que faltava dizer é que isso só é seguro entre
worktrees no MESMO commit.

---

## v0.55.0 — o isolamento que desaparecia em silêncio, e as varreduras que não perguntavam de quem era

Vinte commits desde a v0.54.0, e o tema é a **segmentação**: três lugares onde duas
coisas que o operador criou separadas partilhavam alguma coisa sem o dizer — o mesmo
registo, o mesmo `/16`, os mesmos sockets — e um onde uma limpeza destruía o que não
era dela. Numa arquitectura cujo ponto é segmentar, isto é o isolamento a
desaparecer sem uma mensagem.

Ao lado, a fronteira do `delonix-net` continuou a arrumar-se: as regras puras saíram
para um crate sem dependências, e a API de gestão local passou a saber falar de rede.

> **Nota de proveniência, e importa para saber o que confiar.** Esta série foi
> escrita por várias sessões em paralelo. A secção **Varreduras destrutivas** foi
> medida e validada nesta máquina, com o binário real, e diz o que ficou por
> validar; as restantes vêm do trabalho das outras sessões e estão descritas a
> partir do que os commits documentam.

### Segmentação: duas redes que não estavam separadas

**`network rm` de uma rede destruía outra de nome parecido.** O registo de uma rede
de ingress era nomeado com `sanitize(name)`, que corta a 12 caracteres — um limite
que existe para nomes de DISPOSITIVO (`IFNAMSIZ` do kernel) e que num nome de
ficheiro é perda pura:

```
producao-alpha  ─┐
                 ├─→ producao-alp.json
producao-alpine ─┘
```

O que se lê num registo é a **bridge**. Duas redes criadas separadas — e que o
`network ls` mostrava separadas — ficavam com o mesmo registo, portanto os workloads
da segunda iam parar à bridge da primeira. O `rm` era a metade destrutiva: remover a
`producao-alpine` resolvia para o registo da `producao-alpha`.

**Duas redes podiam ficar no mesmo `/16`.** Escolher um prefixo livre é ler o registo
inteiro, decidir e escrever — e não havia fechadura. Medido no host, com o binário
real, com 8 nomes que colidem no mesmo candidato:

| | Resultado |
|---|---|
| em série | 8 subnets distintos (220..227) |
| em paralelo | 3 a 5 distintos, com **até cinco redes em `10.220.0.0/16`** (reprodutível em 3/3 corridas) |

As bridges diferem (o nome deriva do da rede), por isso as redes **parecem**
separadas no `network ls`. O que não está separado é o espaço de endereços: os
workloads tiram IPs do mesmo `/16`, e qualquer regra indexada num IP fica ambígua
entre duas redes que o operador julga isoladas. Eram dois alocadores com o mesmo
defeito; ambos levaram fechadura.

### Varreduras destrutivas passam a provar posse do que destroem  [validado ao vivo]

Uma classe: **operação destrutiva sobre um recurso PARTILHADO, autorizada pelo
bookkeeping do próprio actor em vez de pelo recurso.** Quatro instâncias, mais duas
apanhadas ao validar.

**O reaper de slirp ceifava processos de outras ferramentas.** O `list_slirps`
identificava por `argv[0]` e mais nada — e o `slirp4netns` não é nosso: o Podman
rootless usa-o com a mesma forma de argv. Corre a partir do `publish_with_retry`,
logo um conflito de porta NOSSO mandava SIGTERM à rede de um motor sem relação, no
mesmo host. Reproduzido ao vivo com um slirp arrancado à mão com a forma de argv do
Podman: marcado para ceifa, enquanto os quatro slirps reais do delonix eram
poupados. O token de posse passa a ser o `--api-socket`, o único elemento do argv
cujo caminho nós escolhemos.

**`kill_pidfile` matava por presença de PID, não por identidade.** Um pidfile
obsoleto com o número reciclado levava SIGTERM a um processo alheio. O
`ingress_proxy::running_pid` já tinha a guarda certa; este caminho ficou sem ela.
Aceita `netns holder` além de `netns pin` de propósito — o `teardown` é o comando de
recuperação de um upgrade in-place, onde o processo vivo é de um binário pré-split.

**`runtime_dir` passa a ser por state root** (ADR-0014). Os sockets eram por-UID e os
pidfiles por-ROOT, por isso dois roots num login concluíam ambos «não há infra» e o
segundo apagava os sockets do primeiro. Medidos **quatro `slirp4netns` ligados ao
mesmo socket, de quatro roots diferentes**. O root **por omissão mantém o nome nu,
byte a byte** — nenhum holder já a correr muda de caminho, logo não há a armadilha de
upgrade in-place que a v0.34.2 teve de existir para recuperar.

**`ensure_up`/`teardown` passam a tomar o `FileLock`** que já existia no ficheiro. O
lock estava no âmbito errado (por-ROOT, contra sockets por-UID); com a mudança acima
passa a ser o âmbito certo.

E duas que só o `scripts/e2e.sh` revelou:

**O pin segurava o `stderr` do chamador para sempre.** Dorme toda a vida da infra,
logo quem capture a saída por pipe bloqueia num `read` que nunca vê EOF —
`out=$(delonix …)`, um passo de CI, a própria bateria. A correcção já existia duas
funções acima, no `start_control`, com a razão escrita; o pin ficou para trás. Passa
a `ingress/pin.log`.

**Dois self-deadlocks, introduzidos pela fechadura acima.** `flock` é por *open file
description*, logo uma função já dentro da secção crítica que chame a variante
pública bloqueia contra si própria. O primeiro (`ensure_up` → `teardown`) foi visto a
ler; o **segundo (`acquire` → `ensure_up`) só a bateria o apanhou**, com um
`container run` 31 minutos em `locks_lock_inode_wait` e dois fds de lock abertos.
**Os 1086 testes unitários passavam com ele lá dentro.** Daí `teardown_locked` e
`ensure_up_locked`, e a lição registada no código: ao acrescentar um lock a uma
função que já tem chamadores, os dois sentidos têm de ser percorridos.

**O que NÃO foi validado nesta secção:** o `SIGTERM` do reaper nunca foi executado
contra um decoy — a identificação está reproduzida, o disparo é leitura de código. E
a reciclagem de PID não é demonstrável neste host: o `pid_max` é 4 194 304 com o
último PID em ~417 000, logo o wraparound está a ~3,8 milhões de distância. A guarda
continua certa (um host de vida longa dá a volta, e `pid_max` a 32768 é comum
noutras máquinas), mas quem priorizar isso acima da fechadura troca a ordem.

### A fronteira do `delonix-net`

**As regras PURAS saem para um crate sem dependências** — `delonix-net-model` passou
a chamar-se **`delonix-net-rules`**, que é o que faz: nome de bridge, aritmética de
`/16`, `fnv32`, `Cidr`. São regras que o control-plane precisa de calcular
exactamente iguais (a bridge que ele espera tem de ser a que este motor cria), por
isso vivem num crate que se pode importar sem arrastar o motor.

Voltaram por esse caminho, agora rootless: o **resumo de firewall**, os **três
métodos que faltavam** do antigo `Net`, o **`service_vip`**, e o **`lbset`** — o
instalador que faltava ao `clear_service_lb`.

### A API de gestão local passa a saber de rede

`delonix-mgmt` (HTTP+JSON num socket unix, só o próprio uid) ganhou quatro fatias:
**ler redes** e dizer o que está de pé; **publicar e despublicar portos**; **governar
firewall e política de saída**; e **endereços e ligação a redes** — DHCP, IP do
container, `attach`/`detach`.

Continua a valer o que o `cli-stability.md` diz: é uma API **local**, e não se deve
construir automação sobre ela (ver ADR-0010).

### Outros

- **Guia de rede do runtime** (`docs/guia-rede.md`, 529 linhas), do básico ao avançado.
- **`clippy -D warnings` do CI estava VERMELHO na `origin/main`**, e por isso em todas
  as PRs abertas — o que faz o check deixar de significar alguma coisa. A causa era
  `drop(&p)`, que larga a REFERÊNCIA e é um no-op; o comportamento pretendido vinha
  todo do `Drop` do próprio valor ao sair do escopo. O código estava correcto por
  acidente e o comentário descrevia uma intenção que a linha não executava.

### Gates desta release

| | |
|---|---|
| testes | **1086, 0 falhas** (33 suites) |
| `clippy --all-targets` | limpo |
| `fmt` | limpo |
| `scripts/e2e.sh`, com os dois roots isolados | **PASS=321 FAIL=0 SKIP=1** |

O único SKIP é declarado: o caminho de falha do `wg` não é exercitável num host que
tem `wg` instalado.

**Um teste do `delonix-cri` falha em hosts com infra de rede avariada**, e isso não é
regressão desta série: o `network_ready_reflecte_infra_rootless_real_nao_fabricada` lê
o state root REAL da máquina (o comentário do próprio teste di-lo), por isso um host
com a infra em baixo e marcadores de ref órfãos vê `NetworkReady: false` — que é o
comportamento correcto do código. Reproduz-se e desaparece desviando o
`XDG_DATA_HOME` para um directório limpo.

---

## v0.54.0 — o que existia e ninguém chamava, e o que se dizia sem ser verdade

Cinquenta e nove commits desde a v0.53.0, e um tema atravessa quase todos: **a
capacidade estava escrita e o caminho não lá chegava**, ou o comando reportava uma
coisa e o kernel tinha outra. Não há aqui um subsistema novo — há sete lugares onde
o motor deixou de mentir e três onde uma função pública ganhou finalmente um
chamador.

> **Nota de proveniência, e ela importa para saber o que confiar.** Esta série foi
> escrita por várias sessões em paralelo. As secções marcadas **[validado ao vivo]**
> foram exercitadas contra o binário real nesta máquina ou numa VM criada pelo
> próprio motor, com a medição descrita; as restantes vêm do trabalho das outras
> sessões, com os seus próprios gates, e estão aqui resumidas a partir do histórico
> — não reexercitadas. `git log v0.53.0..v0.54.0` tem o detalhe de cada uma.

---

### Limites de recurso não se aplicavam em rede custom **[validado ao vivo]**

O re-exec de `--net <rede>`/`--pod` passa por `ip netns exec`, que monta um sysfs
NOVO sobre `/sys`. Medido: `/sys/fs/cgroup` fica com **zero entradas**. Sem
`cgroup.controllers` visível, o `setup_cgroup` devolvia `Ok(())` com um aviso cujo
texto é falso («rootless WITHOUT cgroup delegation» — a sessão TEM delegação; o que
falta é visibilidade) e o container ficava no cgroup **do processo que o lançou**.

Consequências, nenhuma delas a dar erro: `-m`/`--cpus`/`--pids-limit` não aplicavam
nada em tudo o que precisa de DNS interno, isolamento de namespace, `--expose` ou
HTTPRoute; o `stats` reportava a sessão; e — porque o `live_cgroup` também é usado
para ESCREVER — um `container pause` congelava a sessão do operador e um `container
update --memory` punha-lhe um tecto à medida do container.

A correcção já estava escrita (`reveal_cgroup2_if_masked`, com este sintoma no
próprio doc-comment) e **nunca tinha tido um chamador**. Sétima ocorrência do
padrão.

> **Não é retroactivo.** O cgroup escolhe-se no arranque: containers já a correr em
> rede custom só saem do cgroup da sessão quando forem reiniciados.

### `start`/`restart` devolvia o container sem confinamento **[validado ao vivo]**

AppArmor, label SELinux, `--host-pid`, `--host-ipc` e `--log-cri` desapareciam num
`stop`+`start` — e na recuperação automática a seguir a um respawn do holder, que
corre sem ninguém pedir. O `apparmor` já *estava* persistido e mesmo assim não era
lido: não faltava o estado, faltava a leitura. O `run` recusa arrancar unconfined
quando o perfil falha; o `start` fazia o que o `run` proíbe.

Medido contra o binário da tag anterior: um container criado com `--log-cri`
escrevia `2026-08-13T…Z stdout F linha` e, depois do restart, `linha` — com o `logs
--timestamps` a responder «*require the container to have been run with
--log-cri*» a um container criado exactamente com `--log-cri`.

**Gate**: um teste lê o próprio código-fonte e exige que o `RunSpec` do `cmd_start`
reproduza o do `cmd_run`, campo a campo, com allowlist onde a excepção traz a razão.

### Três silêncios fechados **[validado ao vivo]**

- **`container update`** dizia «resource limits updated» também quando não escrevia
  nada. Passa a distinguir três casos (aplicado / adiado / não imposto), e o «não
  imposto» é **erro** — senão o `converge` do `stack apply` carimbava o
  `last-applied` e o gate de deriva do CI ficava verde por cima de um limite ausente.
- **`--tmpfs`** engolia o erro do `mount` com `let _ =` e o container corria com o
  caminho a ser um directório normal em disco — o oposto exacto do pedido. Agora
  aborta, como o `seccomp` e o `--device` já faziam ao lado.
- **`emptyDir.medium`** era `#[allow(dead_code)]`. Em Kubernetes, `medium` ausente
  significa disco do nó; aqui é sempre tmpfs, sem `size=`. Passa a avisar — mudar o
  comportamento exigia um ciclo de vida próprio para o directório, e isso merece
  desenho, não arrasto.

### `stack plan` de um `kind: Container` comparava 11 de 43 campos **[validado ao vivo]**

Mudar `env`, `user`, `capAdd`, `readOnly` ou `command` num container a correr dava
`Summary: no changes`. O plano passa a **nomear** o que não compara — nomear não é
convergir, e a diferença é deliberada: convergir estes obriga a recriar o container.
O `--detailed-exitcode` continua 0, porque o apply não vai mudar nada; quem quiser
que isso chumbe um pipeline decide-o no pipeline.

### Docker API: o `Warnings[]` vinha sempre vazio **[validado ao vivo]**

`User`, `WorkingDir`, `ReadonlyRootfs`, `Tmpfs` e `Ulimits` passam a ser traduzidos;
tudo o resto que o tradutor não consome sai no `Warnings[]` da resposta do `create`.
Por ser uma lista de **exclusão**, apanha também os campos que ainda não existem.

Um `docker create -u 1000` subia o container como **root da imagem**, em silêncio.
Agora `id -u` responde `1000` e `pwd` responde `/tmp` de dentro do container.

O `compose` deixa de recusar `127.0.0.1:9000:80` e ranges de portas — capacidade que
o motor já tinha desde que o `-p` ganhou `parse_publish_addr`. Confirmado com `ss`
(bind em `127.0.0.1`), `curl` loopback **200** e `curl` pela LAN **000**.

De caminho, um bug de apresentação que era absurdo e não só enganador: o `container
port` de um serviço em `127.0.0.1:19555:80` dizia **`19555:80/tcp ->
0.0.0.0:127.0.0.1`** — um endereço que não existe, na saída que se lê para decidir o
que está exposto.

---

### Rede: CIDR arbitrário, rotas reversíveis, gateway declarado

O `NetworkStore` deixa de assumir um `/16`: o prefixo estava assado em três sítios e
o IPAM passa a alocar dentro do prefixo real. As rotas entre redes (`NetworkRoute`)
ganharam contadores, teardown do lado do registo **e** do `@netpair`, e deixam de
sobreviver à rede que nomeiam. O `overlay` remove um peer do registo, do FDB **e** do
túnel WireGuard — um nó retirado da malha deixava o canal cripto de pé.

### VM: registo de backends e o backend Proxmox

O `backend_for` era um `match` privado que caía num default para um nome
desconhecido. Passa a registo com factories, e o `delonix-proxmox` — que estava no
workspace **sem um único chamador** — ganha o seu. Corrigidos pelo caminho: o `vm
stop` a apagar o disco de uma VM remota, o `vm start` a criar uma VM a mais, e o
`ip()` a devolver um `169.254` como se fosse endereço.

### Appliances, boot e reconciliador

Os scripts de appliance passam a obter **e verificar** o media sozinhos, fail-closed.
O `net boot` ganhou units para VMs e uma âncora para membros de pod (arrancavam todos
ao mesmo tempo a disputar a netns). O `stack` perdeu seis listas de Kinds que tinham
de concordar entre si — passam a sair de uma tabela — e o `stack wait` deixou de dar
por ausente um Kind declarativo, que era o que fazia a CI esperar pelo timeout inteiro.

---

### Segurança

Passagem `delonix-runtime-sec` sobre as mudanças de cgroup/tmpfs/campos persistidos:
**sem CRÍTICOS e sem ALTOS**. Verificado que o `reveal_cgroup2_if_masked` não tem
vector para desmontar o `/sys` do host, e que o `apply_tmpfs` fatal é fail-closed
correcto (os specs vêm só da CLI/compose/manifesto; o CRI e a Docker API não
alimentam tmpfs).

**Registado por fechar, e dito em voz alta em vez de escondido**: `--tmpfs`/
`emptyDir` montam sem `size=` (metade da RAM — o Docker põe 64 MiB); `--selinux` é
aceite, persistido, mostrado no `describe` e o `apply_selinux` é fail-open, sem
irmão do `ensure_apparmor`; e o `delonix restore` desserializa e arranca um
`Container` completo vindo de um arquivo não-confiado, com o `id` de dentro do JSON
a entrar cru num join de caminho — pré-existente, e o mais grave dos três.

### Comparação medida com Docker e Podman

`docs/comparacao-medida.md` foi refeito por inteiro contra o 0.53.0, com os **mesmos
binários** de docker 29.1.3 e podman 4.9.3 da corrida anterior. Os números antigos
foram retirados: os três motores deram agora 208/268/91 ms contra 1406/1351/640, ou
seja ~6× mais rápidos — três ferramentas a acelerarem ao mesmo tempo não é melhoria
de nenhuma, é a bancada, e aquilo media contenção da VM.

Entra a linha que faltava: com **rede isolada por container**, a comparação justa, o
Delonix faz 216 ms contra 208 do docker (empate), e 344 ms quando o plano de rede
tem de subir do zero — custo que docker e podman não pagam porque o daemon e a
`docker0` são permanentes.

---

### Gates

`fmt`, `clippy` sem erros, **27 suites de teste verdes**, arnês de caos **23 PASS ·
0 FAIL · 1 SKIP** (TrueNAS, por falta de credenciais). Três gates novos: o cenário
`cgroup-netns` (com container de controlo, para não acusar o ambiente), a paridade
`RunSpec` run↔start, e a condição de campos não-comparados do `kind: Container`.

> **Um teste desta série depende do estado da MÁQUINA, e vale a pena saber.** O
> `network_ready_reflecte_infra_rootless_real_nao_fabricada` (delonix-cri) lê o
> ingress real: com a infra em baixo e um `refcount` acima de zero devolve
> `NetworkReady: false` — correctamente, porque é o caso que ele existe para
> proteger. Um marcador de referência órfão deixado por um container que morreu sem
> teardown faz o teste falhar sem que haja regressão nenhuma no código, e num runner
> limpo passa sempre. Se falhar numa máquina de desenvolvimento, olhar primeiro para
> `delonix net netns status`.

### Incidente registado, porque é a demonstração do primeiro item

Durante esta série, quatro containers de longa duração desta máquina de
desenvolvimento passaram de `Up` a `Exited`. A causa foi medida e é exactamente o
bug do cgroup mascarado: viviam no cgroup do processo que os lançou
(`app-<app-de-desktop>-3786.scope`), esse scope terminou, e levou-os. Com a
correcção desta versão um container em rede custom passa a ter leaf própria sob
`user@<uid>.service` e deixa de estar preso ao ciclo de vida de quem o criou —
mas **só a partir do próximo arranque**: o cgroup escolhe-se no `start`, e nada
migra containers já a correr.

---

## v0.53.0 — o isolamento entre redes deixa de ser uma malha, e as redes passam a poder falar

Série de rede completa: o isolamento entre redes passou de uma malha O(n²) sem
contadores para duas regras fixas, abriu-se a possibilidade de declarar caminhos
DIRIGIDOS entre redes, e a primeira peça privilegiada (VLAN 802.1Q) entrou pelo
caminho contido que o `vm bridge` já tinha aberto — sem alterar o princípio
rootless de tudo o resto.

Tudo validado ao vivo num holder isolado. Ver [ADR-0013](../adr/0013-network-topology.md).

### O isolamento entre redes era O(n²) e não tinha contadores

Medido num host vivo antes de mexer: **8 bridges, 73 regras**. Criar a n-ésima
rede acrescentava `2(n-1)` regras `iifname "<a>" oifname "<b>" drop`, uma por par
ordenado, e cada pacote encaminhado percorria-as todas.

Passa a **duas regras fixas**, independentemente de quantas redes existam — a
mesma correcção que o `@fwmap` já tinha feito ao dispatch por-container:

```
iifname . oifname vmap @netpair              # isenções
iifname @dlxbr oifname @dlxbr counter drop   # tudo o resto entre bridges
```

Uma rede entra com DOIS elementos e **nada por par**. O `counter` é novo: a malha
não tinha nenhum, por isso «este par alguma vez tentou falar?» — a única pergunta
que a regra existe para responder — não tinha resposta.

### Rotas dirigidas entre redes (`network route`, `kind: NetworkRoute`)

As redes continuam isoladas por omissão; agora pode declarar-se um caminho:

```bash
delonix network route web db          # a web alcança a db; a db NÃO alcança a web
delonix network route web db --rm
```

e a forma declarativa, `kind: NetworkRoute` com `from`/`to`. Ambas as pontas são
validadas — nomear uma rede inexistente é recusado nomeando qual falta.

**Uma rota diz por onde um pacote PODE ir; nunca diz que é permitido.** As chains
por-workload continuam a decidir, e atravessar uma fronteira de namespace
continua a exigir política própria.

Documento próprio e não um campo dentro do `kind: Network`, porque uma rota é uma
relação e não pertence a nenhuma das pontas — exprimível dos dois lados é como
dois documentos passam a discordar sobre a mesma rota.

### `network vlan` — a primeira peça privilegiada, e contida

VLAN 802.1Q sobre uma NIC física. **É o único comando de `delonix network` que
precisa de root**, e di-lo em todas as corridas. A razão foi medida, não assumida:

```
ip link add … type vlan                              → Operation not permitted
systemd-run --user --scope -p Delegate=yes -- (idem) → Operation not permitted
CapEff desse scope                                   → 0000000000000000
```

`Delegate=yes` delega controladores de **cgroup** — é o que faz o `-m`/`--cpus`
valer — e não tem nada que ver com a netns do host.

O princípio mantém-se por **contenção**, seguindo o precedente do `vm bridge`:
comando separado (nunca uma flag que escala outro em silêncio), **dry-run por
omissão**, recusa clara sem root em vez de degradar, e nada fica a correr.

### A fundação do CIDR (camada A, 1.ª fatia)

Um tipo `Cidr` com a aritmética de prefixo e os testes que a provam — sem
dependência nova. A forma legada de um octeto continua a querer dizer
`10.<n>.0.0/16`, exactamente o que sempre quis.

**Não muda comportamento nenhum**: o IPAM ainda recebe `"10.X"` e assume `/16`.
É a fundação, não a ligação.

### Dois bugs apanhados só por testar ao vivo

1. **O tráfego dentro da própria rede ficou partido** a meio desta série, e foi
   corrigido antes de sair: um `accept` não é terminal entre base chains — a
   isenção no `fwdeny` (-10) evita o drop dali, mas o pacote segue para a
   `forward` (0), que tem `policy drop`. O mesmo mapa passa a ser consultado nas
   duas chains, com teste a exigir as duas consultas.
2. **O `vm init` gerava um projecto que falhava o seu próprio `stack validate`** —
   um `kind: Vm` com `network: <nome>-net` e nenhum `kind: Network` que a criasse.
   Passa a gerar os três Kinds.

### Também nesta versão

* **`kind: Vm` ganha `build:`** — a face declarativa do `delonix vm build`, com a
  regra «exactamente um de `disk`/`build`». A tag deixa de ser copiada à mão
  entre dois comandos.
* **`vm ls` diz o que a VM É** — `IMAGE`, `BACKEND`, `AGE`, `NAMESPACE`. E uma
  coluna sem nada a dizer deixa de ser impressa: uma listagem fica mais rica por
  responder a mais perguntas, não por carregar mais colunas.
* **O schema cobre os 18 Kinds** (eram 5). Nos outros 13, o `additionalProperties:
  false` — a razão de existir dele — não se aplicava: um typo validava limpo e
  não fazia nada no apply.
* **O progresso do `pod create`** diz qual membro está a arrancar, e o aviso de
  cgroup deixou de sair uma vez por membro.

### O que NÃO está feito, e é preciso dizê-lo

* **CIDR e gateway à escolha ainda não funcionam.** O `--subnet` continua a só
  escolher qual `10.<200-254>.0.0/16` e o `--gateway` só aceita o derivado. Falta
  ligar o IPAM ao `Cidr` — é a peça mais perigosa que resta, porque um erro de
  máscara ali não dá erro, dá containers com endereços sobrepostos.
* **O `macvlan` continua por realizar** (uma rede que tira IP do DHCP da LAN do
  host). Já é registado e AVISA que não foi realizado, em vez de fingir. A
  decisão registada no ADR é que ele pertence a um **workload de fronteira** —
  com uma perna na LAN e outra na rede interna — e não aos inquilinos, que ficam
  atrás da firewall. Depende de mover o masquerade para essa fronteira.
* **O `--apply` do `network vlan` não foi corrido com root de verdade** — o `sudo`
  do host de desenvolvimento pede password. Os dois comandos que executa estão
  fixados por teste; a corrida real fica por fazer.
* **IPv6 continua desligado**, por decisão de segurança (v0.37.1) e não por
  esquecimento.

---

## v0.52.0 — um instantâneo de VM deixa de desaparecer com o `stop`, e o grupo `vm snapshot` fica completo

O fio deste ciclo é o mesmo do anterior, uma camada mais fundo: **estado
necessário para reconstruir um recurso a ser deitado fora sem uma palavra**. Na
v0.51.0 eram vinte e um campos de uma VM que morriam com o `vm create`; aqui é a
contabilidade dos instantâneos, que o `vm stop` apagava enquanto o disco os
mantinha intactos. Nenhum dos dois falha — é isso que os torna caros.

## BREAKING: `vm snapshot` passou a grupo, sem aliases

| antes | agora |
|---|---|
| `delonix vm snapshot <vm> <n>` | `delonix vm snapshot create <vm> <n>` |
| `delonix vm snapshots <vm>` | `delonix vm snapshot ls <vm>` |
| `delonix vm restore <vm> <n>` | `delonix vm snapshot restore <vm> <n>` |
| *(não existia)* | `delonix vm snapshot rm <vm> <n>` |

A forma antiga falha **alto** (`unrecognized subcommand`, rc=2), nunca em
silêncio — é a regra de quebra deste projecto, e o grupo `vm` está declarado NÃO
estável em [cli-stability.md](../cli-stability.md).

O motivo directo foi o quarto verbo: **não havia forma nenhuma de apagar um
instantâneo pela CLI**. A única saída era o `virsh snapshot-delete` — e era a
própria mensagem de erro do motor que o mandava fazer, o que é o sinal de que
faltava um comando. Os quatro verbos são agora os mesmos, pela mesma ordem, do
`volumes snapshot` que já existia: um checkpoint é um checkpoint, e quem aprendeu
um não devia ter de aprender o outro.

## O `stop` deitava fora os instantâneos, e o disco tinha-os o tempo todo

Bug report reproduzido antes de qualquer código: `snapshot` → `stop` → `start`
deixava a lista **VAZIA com rc=0** e o restore a responder «Domain snapshot not
found». A causa é o `virsh undefine --managed-save --snapshots-metadata --nvram`
que o `stop` reutiliza do `libvirt_cleanup` — e esse undefine está certo, é o que
evita domínios órfãos.

**A medição que mudou a leitura do problema**: `qemu-img snapshot -l` sobre o
overlay mostrava o instantâneo **intacto** depois do `stop`. O `undefine` não
apaga o instantâneo, apaga só o que aponta para ele. Não era um limite do
mecanismo — era contabilidade a ser descartada.

- `VmBackend::preserve_snapshots` (default: nada, logo o Cloud Hypervisor e o
  Proxmox ficam byte a byte iguais) guarda o `snapshot-dumpxml` de cada um em
  `vms/<vm>/snapshots/`, **dentro do directório que o `remove` já apaga inteiro**
  — com teste a exigi-lo, porque noutro sítio um `vm rm` deixaria metadados a
  apontar para um disco que já não existe. Corre no `stop` PÚBLICO, antes do
  `backend.stop`: uma falha aborta o stop sem nada perdido.
- O `boot` devolve-os ao libvirt logo a seguir ao `define`. O que faltava para
  isso não era óbvio: o `snapshot-create --redefine` **RECUSA** um XML cujo uuid
  de domínio não seja o actual, e o uuid é atribuído em cada `define`.
  `snapshot_xml_with_uuid` reescreve-o em TODAS as ocorrências — são duas no
  ficheiro real, e substituir só a primeira dá o mesmo erro pela que ficou.

## Os quatro verbos passam a servir uma VM PARADA

Era a limitação que restava. Sem domínio, o virsh respondia `failed to get
domain` — uma frase que manda procurar uma VM que está ali no `vm ls`.

`with_stopped_domain` **define o domínio só durante o comando**, a partir do
`vms/<vm>.xml` que o último `boot` escreveu (o mesmo ficheiro que o libvirt
tinha, seclabel DAC incluído, em vez de derivar uma descrição que depois teria de
bater certo com o `boot` à mão), devolve os metadados preservados, corre o verbo,
volta a guardá-los e desfaz o domínio. Por isso o `stop` **deixou de apagar o
`<vm>.xml`**.

Um instantâneo tirado assim é **só do disco** (`state=shutoff`), que é o honesto
para uma VM sem memória para capturar, e a VM **continua parada**. A única
excepção em que o domínio fica definido é um restore de um checkpoint tirado a
correr: o revert repõe a memória, logo o convidado fica a correr — desfazer o
domínio por baixo de uma VM viva não é limpeza, é matá-la. Nesse caso avisa
(`note: … is RUNNING again`) e o registo é reconciliado pelo `status` que já
existe, em vez de um segundo reconciliador que possa discordar dele.

## Cloud Hypervisor deixou de recusar tudo — offline, e diz porquê

Os quatro verbos servem também uma VM CH: `qemu-img snapshot -c/-a/-d` no overlay
da própria VM, e `ls` por `qemu-img info -U` (só leitura, a única forma que o
force-share permite — o `snapshot -l` abre em leitura-escrita e falha com o vmm a
correr). Com a VM **a correr**, os três verbos que escrevem são **recusados com
erro dirigido**; o `ls` responde sempre.

**Porque não se expôs a `vm.snapshot` do próprio CH**, que existe e funciona
(medido ao vivo: `pause` → `PUT /api/v1/vm.snapshot` → `resume` escreve
`config.json`, `state.json` e um `memory-ranges` do tamanho da RAM inteira do
convidado): ela guarda **memória e dispositivos, não o disco**, e o CH não tem
API de instantâneo de disco ao vivo nenhuma — enquanto o vmm corre segura o
`qcow2` em exclusivo, por isso mais ninguém o pode capturar (`Failed to lock byte
100`, com e sem `-U`). Restaurá-la mais tarde, contra um disco que continuou a
ser escrito, não é voltar atrás: é um convidado cuja memória acredita num
filesystem que já mudou. Expô-la como `snapshot` faria o MESMO comando significar
«volta atrás no tempo» no libvirt e «retoma este instante, se ninguém tocou no
disco» aqui. Um par `vm suspend`/`vm resume` é onde essa capacidade pertence.

## A classe da falha deixa de depender de a VM estar acesa

`NotFound` (**4**) para um instantâneo que não existe, `Conflict` (**5**) para um
nome já usado — iguais com a VM parada ou a correr. As duas saíam antes como
**1** genérico com a resposta crua do virsh, onde «domain moment off1 already
exists» usa uma palavra (`moment`) que esta CLI nunca diz.

## VM: quatro achados que vinham do mesmo sítio

- **O manifesto resolvia a imagem de outra maneira que a CLI** — `resolve_vm_disk`
  devolvia `spec.disk` cru e nunca consultava o `VmImageStore`, por isso a MESMA
  string funcionava como `--disk` e respondia `image not found` como `spec.disk`.
  E sem `image_meta` o `apply` também não sabia que a imagem é um APPLIANCE, e
  gerava-lhe um seed de cloud-init que a CLI recusa em voz alta.
- **A recusa num appliance era contornável pelo caminho do qcow2** — o mesmo
  appliance recusava por nome e era aceite por caminho absoluto, que é
  precisamente o contorno que alguém tenta depois de levar `image not found`. A
  busca passa a ser inversa quando o `store.get` falha.
- **`--wait` do CH anunciava `is up` em 0,062 s sobre uma VM que não arrancou** —
  em CH o endereço é DERIVADO do MAC, e tomar esse número por resposta é
  confundir «tem endereço» com «está viva». O `--boot-timeout` passa a ter em que
  se gastar, e um `--wait` esgotado di-lo.
- **As imagens saíam do build sem etiquetas SELinux** (e não era um problema da
  imagem Fedora, como estava registado): mediu-se «não ganha IP» e concluiu-se
  «não arranca», quando um screenshot da consola mostrava a VM no prompt de
  login. Arrancava sempre. Qualquer `dnf install` dentro do `virt-customize`
  re-corre o `ldconfig`, e o `/etc/ld.so.cache` reescrito volta sem xattr
  nenhum — com o PID 1 negado (`avc: denied { map } … tcontext=unlabeled_t`)
  não há `dbus-broker`, sem D-Bus não há NetworkManager, e sem NetworkManager a
  interface fica DOWN. **195 negações num só arranque**, e de fora via-se apenas
  uma VM sem lease. O relabel passa a correr no BUILD, como último passo, no
  único ponto por onde os dois caminhos de build passam. **Não era do Fedora**:
  o `delonix-vm-base:rocky-9` estava no mesmo estado.
- **O `network-config` do seed casava a NIC por NOME, com um glob** — funciona
  onde o renderer é o netplan (Ubuntu, Debian) e está partido onde é o
  NetworkManager (Fedora, Rocky): sem MAC, o renderer nomeia a interface a
  partir da CHAVE do netplan, e o convidado ficava com `interface-name=eth-all`,
  um dispositivo que nunca existe. Passa a casar por **MAC**
  (`delonix_vm::mac_for`, o mesmo valor que os dois backends carimbam), que é a
  única coisa da NIC conhecida antes de o convidado existir — e é o que faz o
  renderer omitir o `interface-name`, o que permite ao mesmo ficheiro servir
  todas as distros.

## O firmware do CH não arrancava imagem nenhuma, e o `--wait` tapava-o

Achado a fechar a validação do ponto anterior, e é o que lhe dá sentido: com o
**`rust-hypervisor-fw`** que o instalador punha, **NENHUMA imagem deste projecto
arranca em Cloud Hypervisor**. As `delonix-vm-base:*` não passam do firmware e a
golden k8s morre no shim de Secure Boot (`import_mok_state() failed:
Unsupported`, lido na consola série) sem chegar ao kernel. Enquanto o `--wait`
anunciava `is up` em 0,062 s, isto era invisível.

Com o EDK2 `CLOUDHV.fd` (fork `cloud-hypervisor/edk2`):

| imagem | `hypervisor-fw` | EDK2 `CLOUDHV.fd` |
|---|---|---|
| `delonix-vm-base:ubuntu-24.04` | não arranca | **is up**, 7,8 s |
| `delonix-vm-base:ubuntu-26.04` | não arranca | **is up**, 5 s |
| `delonix-vm-base:debian-bookworm` | não arranca | **is up**, 5 s |
| `delonix-vm-base:rocky-9` | não arranca | **is up**, 32 s |
| `delonix-vm-k8s:1.34` | shim de Secure Boot | **is up**, 7 s |
| `delonix-vm-base:fedora-42` | não arranca | não arranca (ver abaixo) |

O instalador passa a buscar os dois e o motor prefere o EDK2
(`DEFAULT_CH_FIRMWARES`, **com teste a exigir a ORDEM** — um host que tenha
ambos e escolha o `hypervisor-fw` volta ao silêncio de origem, que é o pior que
há: processo a correr, registo a dizer `Running`, convidado sem ter executado
uma instrução). O `rust-hypervisor-fw` fica como recurso: ~150 KB, mais rápido
onde funcione, e tirá-lo mudaria o comportamento de uma VM que hoje dependa dele.

É por aqui que o caminho POSITIVO do `--wait` fica provado, que era o que
faltava: `is up — ip 10.233.254.177` em 7,8 s, com o endereço confirmado à parte
por ARP (`REACHABLE` com o MAC real do convidado), pelo kernel na consola série,
e por 3/3 ICMP de um container na mesma rede.

**Num host já instalado o firmware novo não aparece sozinho** — ou se corre o
`install.sh` outra vez, ou:

```
sudo curl -fsSL -o /usr/local/share/delonix/CLOUDHV.fd \
  https://github.com/cloud-hypervisor/edk2/releases/latest/download/CLOUDHV.fd
```

**O Fedora continua a não arrancar em CH, e isso não é nosso**: o GRUB anuncia
`Booting 'Fedora Linux (6.14.0-63.fc42.x86_64)'` e o EDK2 leva um `#PF` de
escrita a carregar o kernel — igual com `CLOUDHV_EFI.fd`, igual com 2 GiB, e
**igual com a imagem original do fabricante** (mesmo RIP). Há caminho com flags
que já existem — **arranque directo de kernel**, que salta o GRUB por inteiro, e
foi validado (`is up` em 24 s, ARP do convidado confirmado no holder):

```
delonix vm create fed --disk delonix-vm-base:fedora-42 \
  --backend cloud-hypervisor --network <rede> \
  --kernel <vmlinuz> --initrd <initramfs> \
  --cmdline "console=ttyS0,115200n8 root=UUID=<uuid> rootflags=subvol=root rw"
```

O `vmlinuz`/`initramfs` tiram-se da imagem com `virt-copy-out /boot/...` e o
`root=`/`rootflags=` estão na entrada BLS em `/boot/loader/entries/*.conf`.
Automatizá-lo tem perguntas próprias — onde fica o kernel extraído, e o que
acontece quando o convidado actualiza o dele — e não se fez às pressas.

## Rede: a coluna BRIDGE nomeava um dispositivo que não existe

`network ls`/`inspect`/`describe` imprimiam `dlxne9623e` onde o dispositivo real
é `dlxn0536623e`. Duas fórmulas independentes para a mesma coisa, uma por store —
e só a do plano físico nomeia o que é criado. Mesma família do `ingress ls` a
dizer `allow` sobre uma porta bloqueada: a superfície de relato a discordar do
dataplane.

## Imagens: um blob cortado a meio deixa de recomeçar do zero

`vm pull` de 276 MiB morria aos 8m19s numa ligação a 416 KB/s, e a tentativa
seguinte recomeçava no byte zero — abaixo de ~600 KB/s a imagem **nunca**
acabava de descarregar, por mais vezes que se tentasse. É o primeiro comando que
um administrador corre. A atribuição ao timeout estava desactualizada (o tecto é
de 4 horas desde a v0.47.1): 8m19s é a ligação a cair, e contra uma queda um
tecto maior não faz nada — só a retomada resolve.

## Documentação

- **Guia do administrador de VMs** ([guia-vm.html](../guia-vm.html)), com um
  laboratório de seis VMs (BIND9, ISC DHCP, Samba, cliente, TrueNAS, OPNsense)
  montado por manifestos e provado ponta a ponta. Ganhou **página própria no
  site**: era Markdown solto no repositório, invisível nas Pages (o `.nojekyll`
  desliga a renderização de Markdown), e passa a ser gerado pelo mesmo `md_page`
  do `gitops.md` — a mesma fonte, nunca uma segunda cópia do texto.
- **ADR-0013** (topologias roteadas) com o spike da camada B corrido rootless num
  host vivo: o isolamento entre redes **não é ausência de rota**, é um drop
  par-a-par explícito por par ordenado de bridges.

## Gates

- Secção nova no `scripts/e2e.sh` a correr o **ciclo** (create → stop → start →
  restore, mais os quatro verbos com a VM parada e as classes 4/5) contra uma VM
  libvirt real: **20/20**. A versão que cobria só o ciclo original foi verificada
  pela regra do repo em **9/9 com a correcção e 4/9 com ela revertida** — tinha
  de ser o ciclo, porque antes da correcção cada comando devolvia 0 por si.
- Secção irmã para o Cloud Hypervisor: **13/13**, incluindo a recusa com a VM a
  correr e o `rm` confirmado com `qemu-img snapshot -l` (sair da LISTA não é sair
  do disco).

## O que NÃO foi validado

- **O CI não exercitou estes gates.** O job `Chaos` fica verde mas os passos «Corre
  o arnês de caos» e «Bateria E2E da CLI» são SALTADOS pelo preflight daquele
  runner (sem userns). A prova das duas secções é a corrida local, num host com
  libvirt e cloud-hypervisor reais.
- **A secção CH da bateria recusa-se a correr meio isolada** — `DELONIX_ROOT`
  isolado sem `DELONIX_NET_RUNTIME_DIR` põe dois roots a disputar
  `/tmp/delonix-net-<uid>/`, e isso custou um incidente real durante este ciclo: o
  root isolado subiu um pin/slirp por cima dos mesmos caminhos e a reconciliação
  seguinte, corrida do root real, reconstruiu a infra e reiniciou um container.
  Fica também um **achado por investigar**: o guarda do motor devia olhar para
  quem está VIVO no socket partilhado, não só para a presença de um pidfile no
  root actual.
- **Backend Proxmox inalterado** — continua a recusar os verbos de instantâneo
  pelo default do trait, fail-closed, como antes.

---

## v0.51.0 — o registo passa a guardar a máquina inteira, e o que era descartado em silêncio passa a dizer-se

Um fio atravessa quase tudo neste ciclo: **coisas que eram aceites e deitadas
fora sem uma palavra**. Vinte e um campos de uma VM que morriam com o `vm
create`; sete campos de um manifesto que o plano reconhecia, parseava e
descartava; doze Kinds cujo schema validava limpo sem validar nada; um `create`
de overlay que saía **0** sobre uma rede que não subiu. Nenhum deles falha — é
isso que os torna caros.

## VM: o registo guardava dez de trinta campos

A `VmConfig` tem ~30 campos e o registo `Vm` persistia **dez**. Os outros vinte e
um — kernel/initrd/firmware/cmdline, seed, hugepages, afinidade de CPU, bridge,
volumes, VNC, IP estático, machine, modelo e topologia de CPU, TPM, vídeo, ordem
de arranque, discos e placas extra, XML do libvirt — existiam durante o `vm
create` e morriam com ele.

Duas consequências, ambas medidas:

- **`vm start`/`restart` reiniciava uma máquina materialmente diferente** da que
  o operador criara. O próprio `--help` documentava a perda como se fosse um
  limite natural.
- **O reconciliador não pode comparar o que o registo não guarda.** Um `kind: Vm`
  aceita 36 campos e convergia cinco.

**Quinta vez que esta base paga a mesma regra** (antes: `-v`, `-p` em rede
custom, redes extra, `Container.pod`): *estado necessário para RECONSTRUIR o
recurso tem de ser persistido, não só usado na criação*.

`VmBootSpec` fica no core (o registo vive lá e a dependência não pode correr ao
contrário); o `delonix-vm` re-exporta os tipos, logo nenhum chamador parte. Bloco
inteiro `skip_serializing_if`: **uma VM sem opções avançadas não cresce um byte
em disco**, e um registo antigo não tem a chave — ausente é *desconhecido*, não
«esta VM não tinha nenhum».

O `boot_spec_of` desestrutura a `VmConfig` **exaustivamente** e o `config_from`
constrói-a **sem `..Default::default()`**: um campo novo parte a build nos dois
sítios e obriga a decidir se tem de sobreviver a um `start`. Foi um
`..Default::default()` que deixou vinte e um campos virarem defaults em silêncio
a cada reinício.

## `stack plan`: a VM deixa de descartar em silêncio o que o manifesto declara

Numa criação o descarte é inofensivo — o `create` aplica o spec inteiro. Numa VM
que **já existe**, um plano com `cpuTopology`, `tpm`, `vnc`, `machine`,
`bootOrder`, `extraDisks` e `extraNics` imprimia `Summary: 1 to adopt` e mais
nada, e o apply reportava sucesso.

**O controlo é o que torna isto conclusivo**: um campo genuinamente desconhecido
AVISA no plano. Logo aqueles sete eram reconhecidos, parseados e deitados fora —
a pior das três formas, porque quem escreveu o manifesto tem todas as razões para
julgar que pegaram.

**Nomear não é convergir**, e a diferença é deliberada: convergir estes campos
obriga a reiniciar a VM, que é capacidade nova sobre o contrato do `-o json`
(ADR-0005). A honestidade sai primeiro; a convergência tem ADR própria
([ADR-0012](../adr/0012-vm-reboot-convergence.md), **Proposta**, com quatro
perguntas em aberto). A lista do aviso é **derivada** do `RECONCILED_VM_FIELDS` —
um campo promovido a comparado desaparece do aviso sozinho.

**As conditions passam a sair do plano**, e não de uma segunda contagem: o
`print_missing_conditions` recalculava-as com um segundo `Env::probe()` na mesma
execução, e passou a estar errado no momento em que uma condition dependeu de
algo que só o plano sabe. Sintoma: o aviso aparecia no `stack plan` e
**desaparecia no `stack apply`** — o comando que a maioria das pessoas corre.
Uma fonte, três comandos (plan, apply, describe).

## Schema: doze Kinds validavam limpo sem validar nada

O schema está publicado e **declarado estável**, e cobria 5 dos 17 Kinds. Nos
outros doze o `additionalProperties: false` — a razão de existir dele — não se
aplicava: **um typo num nome de campo validava limpo no editor e depois não fazia
nada no apply**. Entram `Secret`, `Image`, `Tunnel`, `ShareVolume`, `Dependency`,
`HTTPRoute`, `Ingress`, `FirewallPolicy`, `Egress`, `Workload`, `Cluster` e
`Stack`.

O nome da definição era **adivinhado** (`format!("{kind}Spec")`) e um palpite
falhado fazia `continue` **em silêncio** — o Kind entrava sem estritez e nada o
dizia. Passa a vir explícito, e um erro é fatal. O teste de estritez é agora
derivado do `allOf`: foi uma lista à mão de três nomes que deixou doze Kinds
entrarem sem ninguém dar por isso.

Dois casos em que o schema descrevia o que o motor **não** faz foram corrigidos —
`Dependency.to` na forma escalar, e o facto de **o `schemars` não ver
`#[serde(alias)]`**, que fazia o `examples/cluster-ssh.yaml` *publicado e a
funcionar* ser recusado cinco vezes.

**Limites declarados**: `Storage` fica de fora (não tem spec própria — tipá-lo
seria a segunda cópia à mão que o ADR-0007 existe para abolir; a recusa diz o que
escrever em vez dele), e os filhos de um `kind: Stack` continuam por validar.

## `kind: Vm` ganha `build:`, e o `vm ls` diz o que a VM É

Construir uma imagem de VM a partir de um `VMfile` exigia dois passos e uma tag
copiada à mão entre eles (`delonix vm build -t x`, depois um manifesto com
`disk: x`) — a tag ficava escrita em dois sítios e nada os mantinha em passo.
**`spec.build` é a face declarativa do `delonix vm build`**, campo a campo com as
flags dele, e o `apply` chama o MESMO `vmfile::build`, não uma segunda
implementação. O `context` resolve-se contra a pasta do **manifesto** e não
contra o cwd (a regra que o `Secret.fromEnvFile` já segue), para um manifesto
querer dizer o mesmo seja de onde for aplicado.

**Exactamente um de `disk`/`build`**, fail-closed dos dois lados e com mensagens
diferentes, porque os enganos são diferentes: nenhum é um manifesto sem nada para
arrancar; os dois é um manifesto com duas respostas que não se conciliam. Deixar
o `disk` ganhar em silêncio faria o bloco `build:` **parecer** honrado enquanto
era ignorado — e essa falha quase entrou: com a resolução escrita e a compilar
limpa, o `VmConfig` continuava a levar o campo CRU, o `build:` produzia a imagem
e a VM arrancava de uma string vazia. Apareceu por ir confirmar que o valor
resolvido era mesmo usado, não por queixa do compilador.

No **`vm ls`**, um bug report com captura mostrava `UPTIME`, `ROLE` e `GPU` a
traço. Medido antes de lhes mexer: **estavam a dizer a verdade** — as VMs estão
paradas, nenhuma é nó de cluster, nenhuma tem passthrough. Preenchê-las era
inventar. O que faltava era o que uma VM **parada** ainda consegue responder:
entram `IMAGE`, `BACKEND`, `AGE` e `NAMESPACE` — e o `BACKEND` decide o que
sequer funciona nela (`--namespace` e a SDN são só cloud-hypervisor, os snapshots
só libvirt) sem aparecer em listagem nenhuma até aqui.

Doze colunas rebentariam qualquer terminal, e daí a segunda metade: **uma coluna
sem nada a dizer deixa de ser impressa** (`Table::drop_uninformative`). Uma
listagem fica mais rica por RESPONDER a mais perguntas, não por carregar mais
colunas.

## Rede: o overlay era anunciado como o que não é, e o `rm` deixava o uplink vivo

- **`stack plan` reportava o VXLAN como driver sem plano físico**, no mesmo braço
  do `match` que o macvlan/ipvlan. Falso nas duas metades: o `realize_overlay`
  sobe bridge + uplink VXLAN + WireGuard **inteiramente dentro do netns do
  holder**, e aloca um octeto como qualquer bridge. O teste antigo percorria os
  três drivers a exigir `DriverNotImplemented` — **fixava o defeito**.
- **`network rm` deixava o `dlxvx<vni>` para trás**, vivo, com o FDB dos pares e
  sem master. Sobrevive até o holder morrer, e cada ciclo create/rm deixava mais
  um. Encontrado a provar o VXLAN ao vivo, que é o único modo de o encontrar.
- **`network node init|key` devolvia um errno cru** (`spawn failed: No such file
  or directory`) quando falta o `wg`. O ENOENT de um spawn não é um ficheiro em
  falta — é a **ferramenta**. Classe já catalogada, corrigida na v0.45.0 no
  `vmimage::tool_package` e reaparecida noutro sítio; corrigida agora **na
  origem** (`delonix-net::wg`), para qualquer chamador herdar.
- **`network create --driver overlay --wg-ip` saía ZERO** com a rede por
  realizar, o erro rebaixado a *warning* que prometia reconciliar «no próximo
  `network create`». Medido: o segundo create dá **conflito (5)**, porque
  `create_overlay` não é idempotente — a promessa era falsa e a rede ficava
  `Realized=False` sem comando que a salvasse. Passa ao padrão do `bridge`
  (rollback + propagar o erro), que o comentário do próprio ficheiro já
  justificava.

Fica declarado o pré-requisito que é mesmo real: **um overlay cifrado precisa de
`wg` no host**, e a sonda usa a MESMA `wg::available()` do realizador — uma
condição que discorde de quem realiza é pior que condição nenhuma.

## Progresso: o silêncio passa a ter cobertura, e cada passo fecha-se

- **`container run` ganha spinner só onde há silêncio real.** Um `run` com a
  imagem pronta demora **0,31 s** — não há nada para cobrir. O silêncio está na
  PRIMEIRA corrida de uma imagem, onde `ensure_layers` extrai tudo sem imprimir
  nada. Daí um limiar (800 ms, não 400: o caminho em cache mede 0,43 s e um
  limiar tão perto acendia o spinner na corrida vulgar).
- **Um passo que se anuncia tem de se fechar.** Fora de um TTY o `step()`
  imprimia o `•` ignorando o limiar, enquanto o `close_line()` o respeitava —
  medido: **um `•`, zero `✓`** em CI ou num pipe. *Abaixo do limiar* não é *não
  se anunciou*; só o é num TTY.
- **`stack apply` anuncia cada camada e diz quanto demorou**, e só as camadas que
  o manifesto declara: um `Secret` que ninguém pediu não ganha um ✓ verde por
  trabalho que não fez.
- **`pod create` diz qual é o membro.** Três linhas `unpacking the image`
  idênticas e anónimas não respondem à única pergunta que se faz — *qual*.
- **O aviso de cgroup saía uma vez por membro** (medido: o mesmo bloco de oito
  linhas três vezes num pod de três). A versão óbvia da correcção era **pior que
  o ruído**: calar com `cgroup_limits_apply()` levava o pod de 3 avisos a ZERO
  quando o certo é UM — calava um aviso **verdadeiro**. Mais uma da família «X
  não é Y»: *o cgroup do processo que lança não é o cgroup de quem vai correr*.

## `vm init` gerava um projecto que se recusava a si mesmo

Medido num `vm init` acabado de correr: o manifesto trazia `network: <nome>-net`
no `kind: Vm` e **nenhum `kind: Network` que a criasse** — o projecto falhava o
seu próprio `stack validate` («is not declared nor does it exist»,
`1 unresolved reference(s)`). **Um scaffold cujo primeiro acto é produzir algo
que não aplica ensina a coisa errada sobre a ferramenta.** Passa a gerar os três
Kinds, e valida: 3 documentos, todas as referências resolvidas.

O `Volume` fica declarado e **não** ligado à VM, com o comentário a dizer porquê
em vez de deixar o leitor descobrir — é uma restrição medida, não cautela:
`spec.volumes` precisa de virtio-9p, que só o libvirt faz, enquanto `network:` é
a SDN rootless, onde só uma VM Cloud Hypervisor entra. Ligar os dois geraria um
manifesto com bom aspecto que descarta um deles em silêncio.

## ADR-0013: topologias roteadas — o que é rootless, e o que não pode ser

**Proposta, nada implementado.** Existe para a primeira linha de código ser
escrita contra uma fronteira decidida em vez de descoberta a meio. O pedido
(VLANs, gateway e DNS externos, encaminhamento entre namespaces) divide-se em
três camadas **pelo privilégio que cada uma precisa**:

- **A — o espaço de endereçamento passa a valer** (CIDR arbitrário, gateway e
  resolvers declarados). Zero privilégio novo: **GO, desbloqueada**.
- **B — encaminhamento entre redes**, por um Kind novo que descreve o CAMINHO e
  não por um campo dentro do `kind: Network` (uma rota é uma relação e não
  pertence a nenhuma das pontas). **GO depois de um spike.**
- **C — 802.1Q sobre uma NIC física.** Precisa de `CAP_NET_ADMIN` na init-netns
  do host e **não tem caminho rootless** — a mesma parede do `macvlan`/`ipvlan`.
  Segue o precedente do `vm bridge`: comando explicitamente privilegiado,
  dry-run por omissão, e passagem `delonix-runtime-sec` antes do merge.

## Auditoria: a bateria mede o `--help` de tudo e executa um quarto

Primeira passagem do roteiro de auditoria (`skills/delonix-auditoria`,
novo). A CLI tem **245 comandos, 218 folhas invocáveis**; o `scripts/e2e.sh`
verifica o `--help` de **100%** e **executa 55 — 25%**. Os 163 restantes têm o
contrato verificado e nunca são corridos, concentrados em `net` (45), `image`
(31) e `vm` (24).

**Foi em comandos nunca executados que os dois achados de rede acima
apareceram**, ao primeiro contacto. Um verde total lê-se como «a CLI foi
testada», e o que foi testado é sobretudo o texto de ajuda — o cabeçalho do
script passa a dizê-lo, e a documentar que, ao contrário do `chaos.sh`, ele
**não isola o estado** por si.

`delonix backup`/`restore` ganharam a secção que faltava no guia do projecto: o
que o arquivo leva e porquê (registo e dados dos volumes; nem imagem nem rootfs,
que o `restore` deriva por pull; a VM é a excepção, porque o overlay dela É o seu
estado).

## Por fazer, declarado

- **163 comandos por executar** na bateria — a fatia com melhor retorno a seguir.
- **`scripts/e2e.sh` não isola o estado** por omissão; isolar de fora funciona e
  está documentado, mas forçá-lo partiria os checks que dependem de estado real.
- **[ADR-0012](../adr/0012-vm-reboot-convergence.md) fica Proposta** — a classe
  «reinício» (PID muda, disco sobrevive) entre convergir e recriar, com quatro
  perguntas em aberto e uma razão de calendário.
- **O agendamento do `backup`** («on demand or on a schedule», no `--help`) não
  foi exercitado — não está confirmado se é interno, um timer `systemd --user`,
  ou cron do utilizador.

---

## v0.50.0 — o plano de nomes passa a isolar como o dataplane já isolava, e o manifesto deixa de engolir o que não entende

Dois fios neste ciclo, e são o mesmo fio visto de dois lados: **uma resposta
afirmativa sobre trabalho que não foi feito**. Um resolvedor que devolve o
endereço de um inquilino a quem não lhe pode tocar; um `validate` que diz `OK`
sobre um campo que vai deitar fora. Nenhum deles falha — é isso que os torna
caros.

## O DNS responde o que o dataplane deixaria passar (ADR-0011)

A revisão de rede mediu um buraco sem equivalente do lado do dataplane: o
firewall isola namespaces nas duas direcções, e o **plano de nomes não isolava
nada**.

```
client(teamA) → ping   webb(teamB)                  → 100% packet loss   (correcto)
client(teamA) → lookup webb                         → 10.250.198.79      (fuga)
client(teamA) → lookup webb.teamB.delonix.internal  → 10.250.198.79      (fuga)
```

Um inquilino enumerava a existência e o endereço exacto de cada workload de
todos os outros. O `dns_server_main` **tinha** o endereço de quem pergunta — vem
do `recv_from` — e deitava-o ao chão antes de decidir.

Agora o resolvedor decide com esse endereço, e a política não é nova: é a que já
está persistida nas regras de firewall.

| alvo | resolve? | porquê |
|---|---|---|
| mesma namespace | sim | o dataplane aceita |
| namespace `default` | sim | alcançável de qualquer uma, por desenho |
| outra namespace | **não → NXDOMAIN** | o dataplane descarta |
| outra namespace, com um allow de entrada que cobre o cliente | sim | um `kind: Dependency` abriu-a de propósito |

A última linha é o que mantém o `kind: Dependency` utilizável: uma dependência
atravessa a fronteira numa direcção, e um resolvedor que recusasse o nome
deixaria a funcionalidade a funcionar só por IP — a mesma forma «aceite e depois
ignorado» que este repositório continua a ter de remover.

Derivar a resposta das regras que já existem é o ponto. Uma allowlist separada
para o DNS seria uma segunda fonte de verdade sobre alcançabilidade, e as duas
divergiam na primeira vez que alguém mexesse numa.

**Os nomes passam a ser únicos por (namespace, nome).** Uma namespace que não é
um espaço de nomes contradiz o próprio nome — e sem isto o âmbito acima nunca
poderia ser exercido, porque dois inquilinos não podiam ambos ter `db`.

## DNS de produção — três respostas erradas com cara de resposta certa

- **Um serviço morto continuava a atender pelo nome.** O registo ficava no
  índice depois de o workload morrer, por isso o nome resolvia para um endereço
  que já não atendia ninguém.
- **Os aliases nunca atenderam.** Estavam aceites na spec e não chegavam ao
  resolvedor.
- **O AAAA ia à internet perguntar por um container nosso** — e voltava
  `SERVFAIL`. Um nome interno não tem AAAA; perguntá-lo lá fora era, além de
  errado, uma fuga do nome para um resolvedor externo.
- **Cinco segundos até à primeira retentativa** no `resolv.conf` gerado: um
  arranque em que a primeira query se perde ficava cinco segundos parado, o que
  em produção lê-se como aplicação pendurada.

## Um pod em rede custom gravava o IP de OUTRA rede

E o DNS servia-o. O membro do pod ligado a uma rede própria registava o endereço
da rede errada, por isso o nome resolvia para algo inalcançável — o pior sintoma
possível, um nome que resolve e uma ligação que fica pendurada.

Na mesma família: com nomes por namespace, a regra «o mais recente ganha» do
store passou a poder escolher o recurso de **outro inquilino**, e uma referência
de VM morta era imortal — o `prune` não a limpava e o remédio que imprimia não
existia.

## O manifesto: o campo mal escrito deixa de passar em silêncio

O guarda de campos desconhecidos existia por Kind e era chamado do `apply` de
cada grupo; o `stack.rs` nunca o chamou. Medido com um campo inventado no spec
de cada um dos 16 Kinds:

| comando | antes | agora |
|---|---|---|
| `stack validate` | 3/16 | **16/16** |
| `stack plan` | 3/16 | **16/16** |
| `stack apply --dry-run` | 6/16 | **16/16** |

Enquanto isso, `delonix secret apply -f` no MESMO ficheiro respondia
`unknown field 'x' in spec — ignored`. O mesmo manifesto era verificado ou não
consoante o comando que lhe tocasse. Passa a haver uma lista
(`manifest::spec_fields_for`) e um sítio que a consulta (`manifest::load`), por
onde todos os caminhos passam — incluindo o `Cluster`, que era o último de fora.

**E uma sub-chave mal escrita dentro de um grupo desaparecia.** O hoist copia as
que conhece e a seguir apaga o grupo, por isso a prova era destruída antes de
alguém a poder ver. Medido num container real:

```yaml
resources:
  memoria: 128M      # o campo é `memory`
```

→ `created`, exit 0, nenhum aviso, e `memory_max: "max"`: sem limite nenhum.
Agora é reportado com o caminho exacto (`resources.memoria`), nos três comandos.

O `env: {KEY: value}` e o `network:` plano ficam de fora de propósito: ali cada
chave é dado do utilizador, não um nome de campo.

## `stack validate` deixa de dizer OK sobre o que ignorou

Imprimia o aviso e, na linha seguinte, `OK`. As duas frases eram verdadeiras e
juntas mentiam. Agora o veredicto condiz, e `--strict` transforma-o em exit code
para a CI que quer que o erro de escrita pare o pipeline. Um manifesto limpo
imprime exactamente o que imprimia antes.

## `kind: Vm` deixa de ser um `type: object` para o editor

A linha que a doc manda pôr no topo do manifesto —
`# yaml-language-server: $schema=…` — cobria 4 dos 17 Kinds. O `Vm` era o pior
buraco: a maior spec do manifesto (34 campos) e aquela sobre a qual o editor
nada dizia. São agora cinco, com `additionalProperties: false`. Os 59 documentos
de `examples/` validam contra o schema publicado.

## Documentação

- **Appliances**: a tabela dizia «web UI, console» para todas. Passa a dar o
  endereço e a porta de cada produto — as quatro do Proxmox deixam de estar numa
  linha só, e as portas são a tabela `CASES` do `verify-boot.sh`, ou seja aquela
  em que cada imagem foi provada a responder antes de ser publicada. Com as duas
  ressalvas que custavam uma sessão de depuração: o OPNsense não responde na WAN
  por desenho, e o TrueNAS foi provado na :80 mas o provisionamento fala na :443.
- **Três campos** que o motor aceita e nenhum exemplo mostrava — `addHost`,
  `hostAliases`, `pullSecret`. A página `kinds` é gerada a partir de
  `examples/`: o que não está lá não existe para quem lê.

## Breaking, estreito

Um manifesto ou script que resolva um nome **através de uma fronteira de
namespace** sem um `kind: Dependency` que a abra passa a receber `NXDOMAIN` em
vez do endereço. Era a fuga; deixa de o ser. Um nome único no nó continua a
funcionar como antes.

Nada mais muda de comportamento: os avisos novos do manifesto são avisos, e o
exit code do `validate` só muda com `--strict`.

---

## v0.49.0 — backup e restauro por recurso, a quente, e um `--pivot` que dava a VM à imagem dourada

Ciclo grande, e o fio que o atravessa é o de sempre neste projecto: a classe de
defeito que mais custa não é «falta um comando», é **relato desonesto** — um
comando que devolve 0 sobre trabalho que não fez, um erro que aponta para uma
saída que não existe, uma opção aceite e deitada fora. Sete dos achados desta
série são disso, e cada um foi reproduzido antes de corrigido.

## `delonix backup` / `delonix restore` — um recurso, não o nó inteiro

O `system backup` leva o nó completo, e por isso ninguém o corre duas vezes ao
dia. O par novo é por recurso:

```bash
delonix backup container db                              # aqui mesmo
delonix backup container db --to volume:nas-backups      # num NAS, via volume
delonix backup container db --max-for-day 2 --to /srv/bk # duas vezes por dia
delonix backup stack loja --cron "30 3 * * 1" --to /srv/bk
delonix restore container container-db-20260811-205312.tar.gz
```

**O que entra no arquivo saiu de uma medição.** Num container real o registo são
1,5 KB e o rootfs 435 MB — e o rootfs é derivável (a imagem é endereçada por
conteúdo e volta a descarregar-se). Viajam o registo e os **dados dos volumes**.
A VM é a excepção, e não por simetria: o overlay dela É o estado dela — medidos
6,88 MiB de overlay sobre uma base de 276 MiB, que é exactamente a razão de a
base não ir junto.

**Nada tem de parar.** Um container — e todos os membros de um pod ou de uma
stack, em conjunto, porque uma stack é uma aplicação — fica no *freezer* do
cgroup v2 durante o snapshot: nenhum processo escreve enquanto o `tar` lê, e o
PID não muda. Medido com um container a escrever em ciclo: 138 de 139 amostras
congeladas, janela de 7,27 s, PID igual antes e depois. Uma VM passa pelo
snapshot externo do libvirt e volta ao disco dela; medido, o convidado nunca
sai de `running`.

**A garantia é consistência-de-falha** — o que uma falta de energia deixa, e o
que um snapshot de LVM ou de storage array dá. Um filesystem com jornal e
qualquer base de dados com write-ahead log recuperam dela. O que NÃO é: a
memória não é guardada (isso é CRIU), e uma aplicação que só tenha estado em RAM
perde-o. `--stop` e `--quiesce` pedem mais, e cada um diz o que custa.

Ordem que não é arbitrária: o desempacotamento vai para uma pasta de trabalho
**antes** de tocar em nada (a lição que o `volsnap_restore` já carregava — um
arquivo truncado destruía os dados vivos e não tinha nada para repor); os
volumes vêm antes dos recursos (ao contrário, um container arranca contra um
volume vazio); e a retenção corre **depois** de uma escrita bem sucedida, senão
um backup falhado deixava o operador com um arquivo a menos e nenhum novo.

Os dados dos volumes passam pelo `__volsnap`, que lê de **dentro** do userns
mapeado. Lê-los de fora dá EACCES em cada pasta de subuid e empacota um volume
vazio indistinguível de um cheio — o único sítio onde cair nessa armadilha
produz um backup que repõe nada.

**Agendar num motor daemonless** é um temporizador de utilizador do systemd: não
há daemon onde pôr um timer, e acrescentar um trocava a propriedade central do
produto por uma conveniência. `--max-for-day N` espaça N corridas pelo dia;
`--cron` traduz a expressão (o systemd não fala cron — o passo é `0/N` e não
`*/N`, e o dia da semana é nome e não número, com o 0 e o 7 ambos domingo) e
**recusa o que não conseguir exprimir**, porque um horário que dispara a uma
hora diferente da escrita é pior do que um que se recusa a instalar.

### Dois defeitos que só a validação ao vivo mostrou

**O backup agendado escrevia noutro sítio, e dizia que sim.** `--to .` passava
para a unit tal e qual, o systemd corre a partir de `$HOME`, e o arquivo
agendado aterrava lá enquanto o operador via o de-agora aparecer na pasta onde
estava. Os dois devolviam 0. O destino é agora canonicalizado, e o `timer_argv`
— puro, separado só para isto — **recusa** um caminho relativo.

**E o pivot dava a VM à imagem dourada.** Um `blockcommit --active --pivot` sem
`--top`/`--base` funde a cadeia INTEIRA e faz o convidado assentar no fundo dela
— que em todas as VMs deste motor é a imagem dourada partilhada, backing file de
todas as outras. Imprimiu `Successfully pivoted`, o PID não mudou, e o domínio
ficou a escrever dentro de `vm-images/delonix-vm-base_*.qcow2`. A asserção que o
teria apanhado, e que passou a existir, é o **sha256 da base antes e depois**.
Há teste sobre o argv, e o código pergunta ao libvirt depois do pivot «onde é que
ele escreve agora?» — um `Successfully` não é resposta a isso. Segundo achado do
mesmo caminho: o `snapshot-create-as` a criar o overlay leva `Permission denied`
do AppArmor por-domínio, que só permite caminhos já no XML; o overlay passa a ser
criado por nós e entregue com `--reuse-external`.

## `system backup` / `system restore` — o nó inteiro

Salvaguarda o que não se reconstrói: registos, IPAM, segredos, PKI. `--volumes`
leva também os dados (a parte que pode ser centenas de GiB) e
`--include-master-key` torna um nó reconstruível do zero — sem a chave, os
segredos nunca decifram do outro lado. O `restore` recusa-se com workloads a
correr, e o formato é um portão: um arquivo de uma versão futura é **recusado**
em vez de interpretado à sorte.

## `prune` por recurso

Quem quer o disco de volta das imagens não tem de perder os containers:
`delonix prune images`, `containers`, `volumes`, `networks`, `builds` — e o
`prune` global continua a existir para quem o quer todo.

## Códigos de saída com classe

Medido antes de escrever código: `inspect` de um recurso inexistente e um erro
genérico davam **ambos 1**. As duas respostas que um reconciliador mais precisa
de separar — «cria, porque falta» e «pára, porque falhou» — eram o mesmo número,
e o único sinal restante era a mensagem. Que é **traduzida**: um script com
`grep 'no such'` deixava de classificar num nó com `--l18n=pt`.

Agora **3** = não está a correr, **4** = não existe, **5** = conflito (os dois
primeiros são os do LSB que o `systemctl` fala; o `2` do clap fica intocado, e o
`run`/`exec` continuam a devolver o código do workload). Um `match` exaustivo
força quem acrescentar uma variante de erro a decidir. Dois erros mal etiquetados
foram corrigidos de caminho, sem os quais isto era decorativo: o `util::find`
dizia `Invalid` para «não existe» — e é o resolvedor de todos os verbos de
container — e o `Error::Conflict` tinha **zero produtores**.

## Correcções de fidelidade

**`container exec` corria FORA do cgroup do container**, logo sem nenhum dos seus
limites: um `exec` num container com `-m 64M` podia consumir a memória toda do
host. Passa a entrar no cgroup e a aplicar os ulimits antes do `setns`.

**`--dns` perdia-se no primeiro restart, em silêncio.** O `cmd_start` reconstrói
o `RunSpec` a partir do registo e não replicava a configuração de DNS — a quinta
ocorrência da armadilha já documentada: *estado necessário para reconstruir o
recurso tem de ser persistido e replicado, não só usado na criação*.

**O erro sabia qual bind mount faltava, e não o dizia** — nomeia-os agora todos,
antes de montar seja o que for.

**`--net-burst` tinha DUAS bases** e os limites saturavam em silêncio; o
`ssh.port` do `kind: Cluster` estava no schema e ligava sempre a 22; o `secret`
acumulava `$` no dry-run e recusava um manifesto escrito à mão.

## Varredura das 70 flags do `container run`

Verificando o **efeito no kernel**, não o rc. 22 confirmadas aplicadas. Quatro
achados:

1. **`run -d` perdia a causa da falha** — respondia «the container did not start
   (see the error above)» e não havia nada acima: o erro era calculado no
   supervisor e deitado fora, com um comentário ao lado a afirmar que ele «ainda
   tem de chegar ao utilizador». A razão passa agora pelo pipe do handshake.
2. **`--apparmor <perfil>` aceitava qualquer nome** e o container saía
   **unconfined** — pior que não ter a flag, porque o operador julga-o confinado.
   Agora o kernel decide, no momento da transição, e um container que não
   consegue ser confinado não arranca. A verificação não pode ser um preflight na
   CLI: a lista de perfis é ilegível sem privilégio (medido), logo recusaria
   todos os perfis em rootless, **incluindo os válidos**.
3. **`--device-read-bps` e irmãs, `--cpuset` e `--io-weight`** eram no-ops
   silenciosos onde o controlador não está delegado — um tecto de largura de
   banda que alguém pôs para proteger um nó, e que não existe. Avisam agora, e a
   verificação é a existência do ficheiro **depois** de escrever.
4. **`--no-userns` em rootless** é estruturalmente impossível e descobria-se por
   um `EPERM` cru.

Mais o perfil AppArmor embutido, que ia por um caminho previsível em `/tmp` para
ser lido pelo `apparmor_parser` — o mesmo ficheiro temporário sequestrável já
corrigido no `bpf.rs`, e este carrega política de kernel.

## `pull` em paralelo, e o que a medição diz sobre ele

As layers em falta passam a descarregar em paralelo (4 de cada vez), com todos
os erros recolhidos em vez do primeiro. Ganho real medido em `python:3.12`: 7% —
e a razão de não ser mais está medida também: a maior layer é 58% do total, e
**mais ligações não rendem nada nesta ligação** (1 stream dá 9,4 MiB/s, 4 ranges
do mesmo blob dão 9,2, com o sha256 a bater). Contra o mesmo registo e a mesma
imagem: **delonix 48,0 s · podman 49,5 s · docker 51,7 s**.

O arranque conta outra história e fica registada para quem continuar: 111 ms para
`alpine` (3,3 MiB) e 538 ms para `debian:bookworm-slim` (26,9 MiB) — escala com o
tamanho, logo domina a cópia flat do rootfs em rootless. Bate o podman com folga
(254–4722 ms) e perde para o daemon já quente do docker (11 ms). Fechar isso é
overlay em rootless, mudança arquitectural e não afinação.

## Manifesto

**`kind: Secret` ganhou `fromEnv`** — a forma que um job de CI tem, sem escrever
o valor em lado nenhum. **`kind: Image` ganhou `pullSecret`**, para o manifesto
poder nomear a credencial em vez de depender de um login prévio da máquina.

## Observabilidade e API

`-o json` fechado nos cinco `inspect` (volumes, network, secret, storage,
sharevolume) — o `cli-stability.md` já o prometia. `system events` passa a poder
sair em JSONL, que é o que já era em disco. `vm --backend proxmox` deixou de
dizer «unknown» (o crate está no workspace desde o ADR-0008).

## `vm build` — a base vem do que o projecto publica, e um erro diz o que fazer

`FROM ubuntu:24.04` queria dizer uma coisa só: ir buscar a cloud image à
Canonical. Passa a resolver para a base que o projecto publica para essa distro
(`delonix-vm-base:ubuntu-24.04`), cópia local primeiro e ghcr a seguir, e só cai
para o publicador quando não há nenhuma. Medido no mesmo VMfile: cinco segundos
e zero rede, contra descarregar uma imagem inteira. E `FROM fedora:42` deixou de
morrer com «no such local VM image» — o downloader já sabia o formato, faltava a
entrada no `classify_base`.

Quando o build morre dentro do libguestfs, o erro deixa de ser apenas
`virt-customize failed (exit Some(1))`. O stderr **e** o stdout das ferramentas
passam a ser capturados, e as falhas conhecidas trazem a correcção com o comando
da FAMÍLIA do host — lida do `ID` e do `ID_LIKE` do `/etc/os-release`, porque um
derivado como o Zorin declara `ubuntu debian` ali e é precisamente aí que ler só
o `ID` imprime o gestor de pacotes errado:

- o kernel do host a 0600, que o supermin não consegue copiar para a appliance
  (`chmod 0644`, e a forma permanente por `dpkg-statoverride` só onde ela existe);
- o passt do `--network` barrado pelo perfil AppArmor, que só deixa escrever em
  `/tmp` e `$HOME` enquanto o libguestfs põe o socket em `/run/user/UID`;
- `/dev/kvm` sem grupo, que degrada tudo para emulação por software.

Uma falha que não reconhece não leva conselho nenhum: inventar afastaria o leitor
do output que a explica.

## O build passou a ler-se como um pipeline

Cada etapa é uma linha viva — spinner enquanto corre, ✓ verde e o tempo que
levou quando acaba — e o output das ferramentas fica dobrado atrás dela. **Um
passo que falha abre-se sozinho**, o único caso em que esconder custa mais do que
poupa. `-v` (ou `DELONIX_VERBOSE=1`) devolve o fluxo inteiro. O `vm create` leva
o mesmo tratamento, etapa a etapa.

Defeito apanhado a construí-lo, e que vale a pena registar: o `virt-customize`
narra o trabalho no **stdout**, não no stderr — a primeira versão dobrava metade
e deixava a outra metade a disputar a linha do spinner.

## `vm console` e `vm ssh` — entrar mesmo na VM

Três relatos que pareciam três avarias e não eram nenhuma.

**A consola «não entrava».** Reproduzido num guest Proxmox VE: o `vm console`
mostrava os dois banners do virsh e mais nada, enquanto uma screenshot VNC da
mesma VM mostrava um `pve login:` perfeitamente vivo. Uma consola serial é um
pty, e um pty não tem histórico — tudo o que o guest imprimiu antes de alguém se
ligar desapareceu, e o getty não tinha razão nenhuma para voltar a falar. Agora
leva um `\r` ao ligar e o prompt repinta-se.

**O Ctrl+] não era digitável.** Em teclado português `]` é `AltGr+9`, e
`Ctrl+AltGr+9` não produz `0x1d`: a consola abria, funcionava, e não havia como
sair dela senão matando o terminal. A tecla passa a ser escolhível por
`-e/--escape` ou `$DELONIX_CONSOLE_ESCAPE` (`^]` continua o default), e vale nos
dois backends.

**O `vm ssh` pedia a password de uma conta que não existe.** O default `delonix`
está certo para imagens cloud-init e errado para appliances, que nunca correram
cloud-init — daí três `Permission denied` seguidos contra o Proxmox. O
utilizador passa a sair da imagem (`cloud_init: false` → `root`), e como a
password é definida na construção da imagem e este host não a conhece, o comando
diz de onde ela vem em vez de deixar um prompt mudo.

## `delonix syntax` — o VMfile deixa de ser texto cinzento

Um `VMfile` não tem extensão, como um Dockerfile, por isso nenhum editor o
reconhece sozinho — e sem realce, a instrução mal escrita que o parser recusa
(ele falha fechado) parece igual a todo o resto até ao build. O comando novo
emite a gramática para vim/neovim e para o VS Code, e o `scripts/install.sh`
instala-a onde encontrar esses editores:

```bash
delonix syntax vim --dir ~/.vim
delonix syntax vscode --dir ~/.vscode/extensions/delonix.vmfile-0.1.0
```

Sai do binário (`include_str!`) e não do repositório, pela mesma razão que as
completions são geradas: a instalação documentada é `curl … | bash`, que não tem
repositório de onde copiar, e uma gramática guardada noutro sítio afasta-se do
parser que devia descrever.

## Breaking

**`delonix_net::Net` foi APAGADO** — 22 métodos públicos, 986 linhas, zero
chamadores no workspace. Não era descuido: era a arquitectura anterior ao holder,
que corria `ip link add` e `nft` directamente no processo chamador. Em rootless
não podia funcionar; com privilégio mexia na rede do host, fora do isolamento.
**Isto é breaking para quem consome a biblioteca**: o `delonix-paas` usa
`apply_container_firewall` e `firewall_summary` por tag de git — nada parte hoje,
mas subir esse pin passa a exigir uma travessia, e o `firewall_summary` não tem
substituto vivo.

## Skills e gates

Cinco skills de domínio do motor (`container`, `vm`, `network`, `stack`,
`image`) mais uma de prova de conceito E2E, com a disciplina que este repositório
pagou para aprender: nada entra num relatório sem ser reproduzido, o `rc` de um
comando não é o resultado, e um teste que salta em silêncio conta como **não
coberto** e nunca como verde.

A bateria `scripts/e2e.sh` cresceu para **260 verificações** e passou a varrer
redes de corridas anteriores — a subnet é fixa, uma corrida interrompida
deixava-a tomada, e a seguinte falhava por uma razão que nada tinha que ver com
o código. E o workflow de release deixou de publicar por cima de um gate que já
tinha disparado.

---

## v0.48.0 — provisionar numa NAS, um backend remoto deixa de ser impossível, e um defeito que fazia VMs não arrancar

Inclui o `delonix vm ssh` que estava preparado como v0.47.2 e nunca chegou a ser
publicado (ver [v0.47.2.md](v0.47.2.md) — o texto continua a valer, só a tag é
que passa a ser esta).

## `kind: Volume` cria o que antes só sabia montar

Um volume com um bloco `nfs:` sabia **montar** uma partilha e exigia que alguém
a tivesse feito à mão: o dataset, a quota, o dono, o export. O bloco
`provision.truenas` — inteiramente opcional, e sem ele este Kind comporta-se
exactamente como antes — cria-os.

```yaml
spec:
  provision:
    truenas:
      url: https://nas
      username: truenas_admin
      passwordSecret: nas-cred      # kind: Secret, nunca um literal
      insecureTLS: true
      dataset: tank/projectos
      quota: 2G
      owner: { uid: 1000, gid: 1000, mode: "0770" }
      share:
        networks: ["192.168.122.0/24"]
```

**A montagem não muda**: o `server` e o `share` são derivados do que a appliance
reporta, e seguem o caminho de sempre. Não há um segundo mecanismo.

Exercitado contra uma TrueNAS SCALE 25.10.5 real, e são os achados do alvo real
que moldam o desenho: **há operações que são jobs assíncronos** (o `setperm`
responde um id, não um resultado); **o endpoint de permissões mudou de sítio**
entre majors, que é a razão de o cliente pinar um; **a quota tem um mínimo de 1
GiB**, recusado do nosso lado em vez de arredondado; e **as propriedades
numéricas são objectos cujo número pode ser `null`** — ler a string ao lado
transformaria «sem limite» em «limite de zero bytes».

**Destruir é opt-in e por posse.** `volumes rm` nunca toca no remoto; o
`--destroy-remote` só age sobre o que este motor provisionou, reconhecido por um
carimbo nas anotações do volume que leva **só referências** — url, dataset, o
NOME do segredo. O remoto morre primeiro e o registo local em último: o registo
é a única coisa que diz qual dataset em qual appliance pertence a este volume.

Passagem `delonix-runtime-sec`, três achados, dois com exploit reproduzido: um
**pânico remoto** no caminho de erro (resumir a resposta cortava a String por
índice de byte); a **URL levava a credencial para onde quer que fosse** — passam
a ser recusados `http://` com credencial e userinfo na URL; e a **adopção de um
dataset existente não se via**, e agora avisa.

## `delonix image vm rm`

Havia quatro formas de trazer uma imagem VM para o store — `pull`, `build`,
`import`, a golden — e **nenhuma de a tirar**. Numa máquina de produção isso não
é inconveniência: é disco que não se liberta.

**Recusa enquanto uma VM assenta na imagem.** Uma VM corre sobre um overlay fino
cujo ficheiro base *é* a imagem; apagá-la não liberta a VM, torna-a ilegível.
A verificação lê o **disco** e não o registo — uma VM feita fora deste motor
segura a imagem na mesma. Disco primeiro, contabilidade em último.

## Um domínio passa a ter sempre ecrã

**Bug do motor, e o mais caro de atribuir.** O `<video>` do XML só era emitido
com `--vnc`, o que confunde duas coisas: **VNC é acesso remoto a um ecrã; VGA é
a máquina TER um.** Um domínio sem adaptador nenhum é atípico, e há convidados
que não arrancam sem ele.

Medido: **todas** as imagens Proxmox — incluindo as originais do fabricante,
nunca tocadas por este repo — entram num ciclo `SeaBIOS → GRUB → reset` sob
`qemu -vga none`, sem imprimirem uma linha de kernel. Com adaptador, a mesma
imagem arranca e ganha lease DHCP.

A consequência é a pior espécie: `delonix vm create <appliance>` funcionava
**com** `--vnc` e produzia uma máquina que reiniciava em silêncio sem ele — a
flag que uma pessoa usa para *olhar* para o convidado era o que o fazia
funcionar.

## Um backend de VM remoto deixa de ser impossível de escrever

O `create_with` resolvia `cfg.disk` neste filesystem e construía um overlay
local **antes** de consultar backend nenhum, e entregava esse caminho ao `boot`.
Para um hypervisor noutra máquina os três passos são sem sentido — e o primeiro
falhava aqui antes de o backend ser perguntado.

Dois métodos novos no trait, ambos com default e sem tocar em nenhuma assinatura
existente: **`manages_own_storage()`** (o `boot` recebe o `cfg.disk` verbatim) e
**`auto_selectable()`** (a auto-detecção nunca faz um pedido de rede para saber
se um backend remoto está disponível).

Com isso, o crate **`delonix-proxmox`**: cliente da API, validação de entrada e
espera de tarefas. O `boot` e o `stop` **recusam-se com erro claro** — este motor
não publica um backend de computação que nunca viu arrancar uma VM. O que a
espera de tarefas resolve é a armadilha central do Proxmox: quase tudo responde
`UPID:…`, e `{"status": "stopped", "exitstatus": "OK"}` quer dizer que a tarefa
**acabou**, não que falhou.

## `image vm pull` deixa de adivinhar

Um tag nu era assumido como produto do repositório de appliances, e isso mandava
`delonix vm pull rocky-9` para o sítio errado: `no such image
…/delonix-vm-appliances:rocky-9`, para uma imagem publicada, pública e noutro
repositório. Agora procura-se — três GETs de kilobytes antes de um download de
centenas de MB. Zero repositórios com o tag dá o comando para ver o que existe;
mais do que um é ambiguidade nomeada, nunca uma escolha.

## As quatro imagens Proxmox

Levavam um **IP estático do ambiente de build**: o `source = "from-dhcp"` do
answer file significa «obtém por DHCP durante a instalação e grava como
estática». Corrigidas para DHCP, com `net.ifnames=0` (o DHCP sozinho não chega —
a bridge nomeia uma porta física, e `ens18` num hypervisor é `enp0s3` no
seguinte), consola série, e host keys de SSH regeneradas no primeiro boot.

Republicadas, e validadas com o comando que uma pessoa escreve — **sem
`--vnc`**: as quatro obtêm IP por DHCP e respondem HTTP 200 na sua web UI.

## Também aqui

`delonix vm ssh <nome|endereço>`; o `--help` passou a manual com exemplos e
`man`; o `backend_for` deixou de cair num default silencioso para um nome
desconhecido; e o `warn_unknown_fields_in` aceita um caminho aninhado, para um
typo dentro de um bloco de um bloco não ser engolido.

---

## v0.47.2 — `delonix vm ssh`

Uma release de uma coisa só.

## O IP e o utilizador são o que custa

```bash
delonix vm ssh dev                                  # nome → o IP vem do registo
delonix vm ssh dev -- systemctl is-system-running   # corre e volta
delonix vm ssh 192.168.122.50 -l root -i ~/.ssh/k   # endereço directo
```

Não é açúcar por cima do `ssh`. Resolve as duas coisas que quem acabou de criar
uma VM **não tem**:

**O IP.** Vive no registo, e só lá chega bem depois do `create` — uma VM em nat
recebe o lease DHCP muito mais tarde. Sem isto é um `vm ls`, copiar, colar.

**O utilizador.** Por omissão `delonix`, e não o da distro. É a parte que mais
tempo faz perder: numa cloud image de Ubuntu o palpite natural é `ubuntu@`, essa
conta **existe**, **não** tem a chave, e responde `Permission denied
(publickey)` — que se lê como chave partida quando é só o nome errado.

Uma VM sem IP diz o que se passa, em vez de deixar o `ssh` falhar por outra
razão:

```
error invalid argument: VM 'demovm' has no IP yet — it is 'Running'. A VM only
gets one once it has booted AND its network came up; watch it with
`delonix vm console demovm`
```

## O desempate nome-vs-endereço

O store decide **primeiro**; a forma do argumento só desempata quando ele não
tem nada. `valid_vm_name` permite pontos, portanto uma VM pode chamar-se `a.b` —
tratar isso como endereço só porque *parece* um seria escolher pelo utilizador.

## Detalhes que a revisão deste repo pede

* `--` antes do destino: um nome começado por `-` seria lido como opção (a mesma
  defesa que o `ssh`/`scp` do `cluster apply` ganhou na primeira auditoria).
* `exec` e não `spawn`+`wait`: entrega o terminal inteiro ao `ssh` (pty, escape,
  shell interactiva) e não há nada a fazer depois. Só retorna em falha.
* `StrictHostKeyChecking=no` **dito em voz alta e não escondido**: uma VM é
  recriada no mesmo endereço a toda a hora, e a chave de host mudar é a NORMA
  aqui, não um ataque. É uma conveniência de laboratório, e o `vm ssh` não é a
  ferramenta para uma máquina que não é tua.

Validado ao vivo: resolveu um nome para `192.168.122.83` e ligou-se de facto; a
VM sem IP e o nome inexistente dão as duas mensagens certas.

---

## v0.47.1 — o que a v0.47.0 levou consigo, e três verificações que passaram a existir

Uma release pequena, de correcções e de gates. Nada aqui muda um schema nem uma
flag existente.

## `--replace lixo` era aceite em silêncio

A flag que AUTORIZA uma recriação destrutiva aceitava qualquer coisa. Duas
verificações agora, na fronteira do `apply`:

* a **forma** (`<Kind>/<nome>`, um nome nu, ou `all`) — `a/b/c`, `/x` e `x/` são
  recusados;
* e que o valor **nomeia algo deste manifesto**.

Sem a segunda, um `--replace Container/wev` com o erro de escrita lia-se como
autorizado; a recriação era depois recusada a jusante, e o erro que o utilizador
lia falava do recurso — nunca do engano que o causou.

```
$ delonix stack apply -f m.yaml --replace lixo
error invalid argument: --replace 'lixo': no resource with that name in this
manifest (`stack plan` lists them)
```

## A bateria E2E não corria desde a v0.3.0

Quarenta e quatro versões. Quando correu, tinha nove falhas — e **oito porque o
teste codificava um bug entretanto corrigido**: usava `--subnet …/24`, que o
motor aceitava e deitava fora com o driver bridge até lhe dar significado.
Fechado o bug, o teste que o fixava passou a falhar, em cascata.

É a mesma armadilha que o `default_project_name` do compose já tinha dado, e a
regra fica: **quando uma correcção faz um teste antigo falhar, a primeira
hipótese é que o teste estava a fixar o comportamento errado.**

A nona era uma guarda a comparar o REPOSITÓRIO quando o comando a seguir usava a
TAG (`alpine:latest` no store, `alpine:3.19` ausente → o `grep alpine` passava e
o `describe` falhava).

E um check que passava **pela razão errada**: `vm create --image /nao/existe`
esperava falha e obtinha falha, mas por «unexpected argument» — a flag `--image`
não existe, é `--disk`. Um teste que passa pela razão errada é pior que um teste
em falta: dá cobertura por adquirida.

A bateria ganhou 27 verificações do que a v0.47.0 acrescentou e que não tinha
uma única: o ciclo `plan`/`apply --dry-run`/`destroy`, o contrato de exit code
que um gate de CI usa, o `schema`/`explain`, o `init`, e
`workload`/`pod`/`secret`. **225 PASS · 0 FAIL.**

## Três gates novos

1. **`ci.yml` → `docs`** — regenera o site e falha se o commitado deixar de ser o
   gerado, mais o `--dry-run`/`validate` de todos os `examples/`. Pagou-se no
   mesmo dia: apanhou sete páginas fora de dia com o `--help` real. O que **não**
   verifica está escrito no job — um campo desconhecido escapa, porque o
   `warn_unknown_fields` só corre no apply REAL.
2. **`chaos.yml` → bateria E2E**, ao lado do caos porque precisa do mesmo
   ambiente rootless que aquele job já monta.
3. **Cenário de caos `stack_converge`** — dois containers de propósito, o segundo
   como controlo, a provar que a convergência tocou só no que o plano nomeou.

## A versão que se descarrega não é a tag que se publica

No workflow das appliances estavam acopladas num input só, e por isso a convenção
de tags deste projecto era inalcançável: o fabricante publica o ficheiro pela
versão exacta (`OPNsense-26.1.2-nano-amd64.img`), mas a tag canónica é a **série**
— é o que a golden já faz (`delonix-vm-k8s:1.34`, não `:1.34.9`). Pedir
`version=26.1` fazia o *download* falhar.

Input `tag` novo, com default = série da versão. O `--release` continua a guardar
a versão exacta: a tag diz a série, os metadados dizem o que lá está mesmo.

---

## v0.47.0 — o `apply` deixou de mentir, e o nó ganhou opinião sobre capabilities

Duas metades grandes e independentes. Uma fecha um defeito estrutural do caminho
declarativo — um `apply` que reportava sucesso sem convergir nada. A outra dá ao
nó a primeira palavra própria sobre privilégio, em vez de aceitar sem opinião
tudo o que a cadeia de admission lhe mandar. No fim, um terceiro bloco mais
pequeno: o primeiro comando que alguém corre, e uma extracção de rootfs que
acontecia a dobrar.

# O tecto de capabilities do CRI

Até aqui, tudo o que chegava a este runtime pelo CRI vinha já autorizado. O
`securityContext` de um pod era traduzido em flags sem opinião nenhuma —
`privileged: true` virava `--cap-add ALL`, `capabilities.add` virava um
`--cap-add` por nome — e isso está certo: o runtime não é o admission
controller. O problema é o que essa correcção implica. A única coisa entre um
`privileged: true` e todas as capabilities do kernel era a cadeia de admission
do API server: outro processo, noutra máquina, cuja configuração este nó não
consegue ver nem verificar.

**`DELONIX_CRI_CAP_CEILING` é a resposta local do nó.** Um limite máximo para as
capabilities de qualquer container criado através do CRI, que vale mesmo com o
Pod Security mal configurado, com um `crictl` a falar directamente com o socket,
ou com um static pod que nunca passou pelo API server. Defesa em profundidade —
não um substituto do admission.

```ini
# /etc/systemd/system/delonix-cri.service.d/ceiling.conf
[Service]
Environment=DELONIX_CRI_CAP_CEILING=default,NET_ADMIN
Environment=DELONIX_CRI_CAP_CEILING_MODE=reject
```

Ou, para quem serve o CRI pela CLI:

```bash
delonix serve cri --cap-ceiling default,NET_ADMIN --cap-ceiling-mode reject
```

Sem nenhuma das duas coisas **não há tecto** e o comportamento é byte-a-byte o
de sempre. Isto é opt-in por uma razão concreta: um tecto estreito num nó com
DaemonSets privilegiados (CNI, CSI, kube-proxy) é exactamente o que o operador
quer ou exactamente o que lhe parte o nó, e essa decisão não é nossa.

## O que se pode escrever

| Valor | Significado |
|---|---|
| ausente, vazio, `all` | sem tecto |
| `none` | capability nenhuma, para ninguém |
| `default` | o conjunto por omissão do motor |
| `default,NET_ADMIN,…` | esse conjunto mais as nomeadas |
| `CHOWN,NET_BIND_SERVICE,…` | exactamente as nomeadas (`CAP_` opcional, maiúsculas indiferentes) |

Um nome desconhecido, um modo desconhecido, ou um valor só com separadores
**impedem o servidor de arrancar**, antes de qualquer `bind`. Um tecto que
caísse em silêncio para «ilimitado» por causa de um typo seria pior que não ter
tecto nenhum — quem o configurou passaria a contar com uma protecção que não
existe.

## Recusar ou cortar, e porque a assimetria é deliberada

Um pedido **explícito** acima do tecto — `capabilities.add`, ou `privileged:
true` — falha no `CreateContainer` com `PermissionDenied` e a lista das
capabilities negadas pelo nome, que o kubelet mostra no pod de imediato. O modo
`clamp` corta-o e regista um aviso, para endurecer um nó cujos PodSpecs não se
podem mudar hoje; é a troca de honestidade por disponibilidade, e é por isso que
não é a omissão.

O **baseline implícito** — o conjunto que um container recebe sem pedir nada — é
reduzido ao tecto **sem erro, nos dois modos**. Baixar um default que o workload
nunca pediu é o que a palavra «tecto» significa; recusar todos os pods do nó
porque o default do próprio motor é mais largo que o limite tornaria a
funcionalidade inútil.

## O clamp não tem aritmética própria

O conjunto final é `resolve_cap_keep` **do motor** intersectado com o tecto, e
sai como `--cap-drop ALL` + um `--cap-add` por capability. Para isso, o módulo
`delonix_runtime::capabilities` passou a ser público (a tabela nome↔número, o
`KEPT_CAPS`, a resolução) em vez de viver escondido no meio do `lib.rs`. Uma
segunda tabela do lado do CRI divergiria no dia em que uma capability fosse
acrescentada aqui, e o sintoma seria o pior possível: uma capability que o
operador autorizou e que desaparece sem um erro.

Há teste de ida e volta nos dois sentidos — `cap_name`↔`cap_num` para todas as
capabilities conhecidas, e máscara → nomes → máscara.

## Limita capabilities, e diz que é só isso

Um pod privilegiado continua a ficar com `seccomp=unconfined`, `/sys`
escrivível e o seu próprio cgroup namespace. São eixos separados do
`--privileged`, e cortar capabilities não torna um pod privilegiado seguro. O
tecto é deliberadamente estreito e di-lo, em vez de sugerir um endurecimento que
não entrega.

## A armadilha que o teste apanhou

A primeira versão modelava `privileged` como `resolve_cap_keep(cap_drop,
["ALL"])`. Parece equivalente e não é: no motor, `privileged` **ignora** o
`cap_drop` por completo (`if privileged { all_caps_mask() }`). Um clamp tem de
prever o que o motor CONCEDE, não o que discutivelmente devia conceder — e o
primeiro teste a cobrir isto afirmava o comportamento intuitivo e errado, com o
código escrito para lhe corresponder. Ficou um teste com o nome do facto:
`privileged_ignora_o_cap_drop_como_no_motor`.

## Como se vê que está em vigor

O tecto é anunciado no arranque (stdout do servidor e `tracing`) e publicado em
`status(verbose)` → `info["capabilityCeiling"]`, onde um `crictl info` o lê. Sem
isto, um tecto activo mas invisível seria diagnosticado como «o runtime largou-me
as capabilities sem razão». O aviso do modo `clamp` só sai quando um pedido
explícito foi efectivamente cortado — avisar quando apenas o baseline baixou
daria uma linha por cada container que arranca no nó.

## Validação

Ao vivo, neste host: os dois pontos de entrada recusam um valor e um modo
inválidos, em EN e em PT, sem chegar a criar socket; um `delonix-cri` real
anuncia o tecto expandido; e o pressuposto central do clamp foi confirmado no
kernel — o argv que ele emite dá `CapEff 0x1001` (CHOWN e NET_ADMIN, exactos),
contra `0xa0042dfb` do baseline e `0x1ffffffffff` de um `--privileged`.

**Não foi validado com um kubelet ou `crictl` real**: nenhum dos dois existe
nesta máquina, e o `build.rs` não gera stubs de cliente gRPC (`build_client(false)`,
deliberado). O caminho de pedido está coberto por um teste sobre o
`create_container` verdadeiro — as duas formas de pedir privilégio, o caso que
passa, o caso sem tecto e o modo `clamp` — mais os testes das funções que
constroem os flags. O que fica por exercitar é a camada tonic, que são três
linhas de `blocking(...)`.

---

# O `apply` deixou de mentir

A mesma versão fecha um defeito estrutural do lado declarativo, e é o maior dos
dois. O `stack apply` só criava:

```
container/web: already exists, nothing to do
```

e devolvia **0**. Mudar a imagem no manifesto não fazia nada e reportava
sucesso. É o gémeo declarativo do relato desonesto que a v0.37.0 tirou do CLI
imperativo, e é pior, porque o utilizador mudou o ficheiro de propósito. Agrava-o
o facto de a capacidade já cá estar: o `container update` reconfigura portas,
volumes, redes, memória e CPU **a quente, sem mudar o PID** — e o caminho
declarativo nunca lhe chamou. Quinta ocorrência do padrão
`mount_live`/`set_net_rate`/`update_limits`/`JsonStore::update`: capacidade
testada, à espera do primeiro chamador.

## O ciclo completo

```bash
delonix stack plan    -f m.yaml   # o que mudaria, e porquê
delonix stack apply   -f m.yaml   # converge; recusa o que não converge
delonix stack destroy -f m.yaml   # leva o que a stack possui, e só isso
```

```
Plano do stack "web"  (manifesto: ./delonix-manifest.yaml)

  ~ container/web              actualizar a quente
      memory:  256M → 512M
  -/+ container/api            RECRIAR — `image` não converge a quente
      image:   nginx:1.24 → nginx:1.27
  !  cluster/prod              não convergente — procedimento remoto, não um recurso local
```

**`--detailed-exitcode`** devolve 0/2/1 como o `terraform plan`, o que faz de um
`plan` num cron um gate de deriva em CI sem escrever um parser.

## Sem ficheiro de estado — porque já era essa a promessa

O último spec aplicado vive **no próprio recurso**, na anotação
`delonix.io/last-applied`. É o mecanismo do `kubectl`, e é o terceiro lado que
distingue «tiraste o campo do ficheiro» (reverte) de «alguém pôs isto à mão»
(não mexe). A posse é a label `delonix.io/stack`, o mesmo idioma que o `pod` e o
`compose` já usavam: um recurso criado à mão é **adoptado** sem precisar de um
comando `import`, um de outra stack é **conflito** e nunca é tocado, e nem
`--prune` nem `destroy` vêem o que não tem a label.

Nada disto acrescenta um registo novo, o que mantém a linha que o projecto já
tinha publicado — `cluster ls` e `stack describe` derivam tudo do que existe.

## Recriar é fail-closed

Um `-/+` nomeia **todos** os campos frios e o `apply` recusa sem
`--replace <Kind>/<nome>`, antes da primeira criação. O apply é fail-fast sem
rollback: recusar a meio deixaria a stack meio convergida **e** com erro.

## Três Kinds deixaram de existir

18 → 15, cada fusão porque os dois lados já faziam o mesmo:

| Antes | Agora |
|---|---|
| `Egress` | `FirewallPolicy` com `direction: egress` — partilhavam a struct inteira |
| `Dependency` | açúcar reduzido para `FirewallPolicy` no load, fundindo por alvo |
| `Storage` | bloco `nfs:`/`cifs:`/`webdav:` de `kind: Volume` |

Os nomes antigos carregam com aviso de depreciação. O corte limpo aplica-se a
comandos; um manifesto em git merece um degrau, não um erro.

**A quarta fusão não se fez, e a razão vale mais que a fusão**: um `kind:
Container` com `spec.containers` **não** é um `kind: Pod` de um elemento. O
primeiro cria um container chamado `<name>`; o segundo cria a netns `pod-<name>`
e chama-lhe `<name>-c0`. Reescrever renomearia o container e partiria o DNS, os
backends de HTTPRoute e as referências cruzadas.

## O schema passou a ser gerado, e a ser estável

`delonix schema print` emite-o a partir do próprio código, `delonix explain
Container.ports` responde como o `kubectl explain`, e o ficheiro está publicado
em `docs/schema/v1/delonix.json` com um teste a falhar se o publicado deixar de
ser o gerado. Uma linha no topo do YAML dá completação e validação no editor:

```yaml
# yaml-language-server: $schema=https://angolardevops.github.io/delonix-runtime/schema/v1/delonix.json
```

O schema dos manifestos passou de **não estável** a **estável** em
`cli-stability.md` — estava o compromisso ao contrário, com a CLI mais protegida
que o formato que as pessoas põem em git.

`schemars` é a **segunda excepção deliberada** à regra de sem-dependências-novas,
depois do `ratatui`, com a mesma disciplina: confinada ao binário, os oito crates
de motor verificados dep-limpos.

## Dois bugs que só apareceram por causa disto

**Um exemplo publicado arrancava sem password.** A validação dos `examples/`
contra o schema gerado apanhou `env: { POSTGRES_PASSWORD: dev }` — a forma que
qualquer pessoa vinda do compose escreve — a ser aceite e **silenciosamente
descartada**. O Postgres do `examples/dependency.yaml` subia sem password.

**Três listas de Kinds convergentes tinham derivado.** A constante que decide se
o `apply` converge estava desactualizada face aos braços do `match` que fazem a
convergência, por isso `Vm`, `FirewallPolicy` e `ShareVolume` eram **saltados** —
e o sintoma escondeu-se porque o apply antigo de cada um é idempotente e
convergia pelo caminho errado. Há agora um teste a exigir que as três listas
concordem nos dois sentidos.

## Validação

Ao vivo, o ciclo inteiro: um campo quente convergido com o **PID inalterado**
(618350) e o `memory.max` do cgroup real a passar a 134217728; a recriação
recusada sem `--replace`, sem tocar em nada; e uma deriva provocada por um
`container update` fora do manifesto apanhada pelo plano seguinte.

Cenário de caos novo, `stack_converge`, com **dois** containers de propósito — o
segundo é o controlo, e prova que a convergência tocou só no que o plano nomeou.
A primeira versão verificava apenas que o PID não mudava, e isso não prova nada:
um apply que não faz nada também deixa o PID intacto. Falha com cada uma das duas
correcções revertida. Arnês: 20/20.

## Guia novo

[`docs/gitops.md`](../gitops.md) — `plan` num PR, `apply` no merge, gate de
deriva num cron, e o que fazer quando um apply morre a meio.
`scripts/schema-diff.sh` compara campo a campo entre duas tags e sai 1 com
diferenças, o que serve directamente como gate de CI.

---

# O primeiro comando, e uma extracção a dobrar

## `delonix init` — o passo que faltava antes do `stack init`

O `stack init` já gerava um projecto completo e preenchido, e o `vm init` fazia o mesmo para
uma VM. O que faltava era o passo ANTES desses: saber qual deles chamar, e com qual dos onze
templates.

```
$ delonix init
detected go.mod → stack init --template go
  created: ./Delonixfile
  created: ./delonix-manifest.yaml
  already exists, skipped: ./go.mod  (use --force to overwrite)
```

A detecção é uma função pura sobre os nomes de ficheiro presentes, ordenada do mais específico
para o mais genérico — um projecto Django também tem `.py` e um Next.js também tem
`package.json`, por isso a regra mais larga não pode ganhar só por ter sido verificada
primeiro. E **explica-se sempre**: um palpite errado que se vê corrige-se com `-t`; um palpite
errado em silêncio produz um projecto que não bate certo com o código ao lado.

Há um caso em que a resposta certa é **não gerar nada**:

```
$ delonix init
warning found docker-compose.yml — this project already runs natively with `delonix compose up`;
generating a second manifest would give it two sources of truth
```

**`delonix version`** passou a existir como subcomando além da flag, porque `<ferramenta>
version` é o que as pessoas escrevem primeiro — o git, o docker, o kubectl e o podman
respondem todos a isso. Imprime o texto da flag VERBATIM, para os dois nunca poderem divergir.

## O `image scan` mandava-te à Docker Hub buscar a tua própria imagem

Medido: `image scan delonix-vm-base:ubuntu-24.04` — uma imagem que **está** neste nó —
anunciava «not local», ia à Docker Hub buscar `library/delonix-vm-base` e morria num
**HTTP 401 Unauthorized**. Pediste para analisar uma coisa que tens e recebeste um erro de
autenticação de um registo público.

Agora é **recusado, não implementado**: analisar um qcow2 significa percorrer o sistema de
ficheiros do CONVIDADO (libguestfs), que é um caminho de SBOM inteiramente diferente. Um scan
que em silêncio não faz nada de útil é precisamente a falha que este comando existe para
evitar. O erro nomeia a alternativa (`virt-filesystems`/`guestfish`).

## O rootfs era extraído DUAS VEZES numa rede custom

Um `container run --net <rede>` faz um re-exec para dentro da netns nomeada. A 1.ª passagem
extraía a imagem para o disco e re-executava; a 2.ª passagem **extraía outra vez**, para o
mesmo caminho, por inteiro.

Medido neste host com `pgvector/pgvector:pg16` (10 296 entradas, 431 MB):

| | tempo |
|---|---|
| `--net none` (uma passagem) | 1 526 ms |
| `--net <rede custom>` (duas) | 3 143 ms |

O delta de 1 617 ms é exactamente UMA extracção — 1 666 ms medidos à parte com `image export` —
e o `strace` concorda: 2 060 canonicalizações do destino, exactamente 2× as 1 030 de uma
passagem só. Re-extrair por cima de uma árvore já preenchida custa preço inteiro; não há
poupança acidental.

**A correcção é rootless-only, de propósito.** Aí o rootfs é um directório plano em disco,
visível de qualquer mount namespace, por isso a 2.ª passagem usa-o tal como está. Como root o
`prepare_rootfs` **monta** um overlay, e um mount feito pela 1.ª passagem não é necessariamente
visível na namespace onde o re-exec aterra — saltá-lo ali trocaria um container lento por um
container partido.

## E a premissa que o perfil corrigiu

O perfil de tempo da criação de um cluster está em
[`docs/discovery/46_C2_PERFIL_CLUSTER.md`](../discovery/46_C2_PERFIL_CLUSTER.md), e a primeira
coisa que ele faz é dizer que a premissa do pedido estava errada: um cluster Kubernetes «no ar
em alguns milissegundos» é fisicamente impossível, e não por falta de optimização. O `kubeadm
init` ESPERA que o etcd, o apiserver, o controller-manager e o scheduler fiquem saudáveis, e um
nó kind arranca `systemd` e `containerd` por dentro antes de qualquer disso começar. O que se
pode fazer — e é o que o documento entrega — é medir onde está o tempo, encurtar o que é nosso,
e declarar o piso.

---

# Imagens de VM: appliances, bases de SO, e a subnet que era ignorada

Um terceiro bloco desta versão, feito em paralelo. Transforma media de instalação de
fabricantes em imagens VM do Delonix, acrescenta as imagens base de SO que faltavam, e fecha
pelo caminho um bug que tornava impossível escolher o espaço de endereçamento de uma rede.

## Seis appliances, instaladas pelo caminho do próprio fabricante

**OPNsense 26.1.2**, **Proxmox VE 9.1 / Backup Server 4.1 / Mail Gateway 9.0 / Datacenter
Manager 1.0** e **TrueNAS SCALE 25.10.5**. Nenhuma montada à mão: cada produto instala-se como
se instalaria em metal, e os scripts (`scripts/appliances/`) só conduzem o caminho não assistido
que ele já tem. É isso que faz de uma versão nova a montante um argumento, e não uma reescrita.

| Appliance | Via | Tamanho |
|---|---|---|
| OPNsense | imagem `nano` oficial (já instalada) | 646 MiB |
| Proxmox VE / PBS / PMG / PDM | auto-install nativo (`answer.toml` no ISO) | 1,45 / 1,11 / 1,22 / 1,06 GiB |
| TrueNAS SCALE | JSON-RPC do próprio instalador | 2,41 GiB |

A afirmação validada é «serve», não «arranca»: PBS responde na :8007 em ~20s, PMG na :8006 em
~30s, PDM na :8443 em ~20s, TrueNAS na :80 em ~50s, PVE na :8006. O OPNsense é a excepção
deliberada — a interface dele só escuta na LAN, e uma sonda pela WAN receberia recusa, que é o
comportamento CORRECTO de uma firewall e não uma falha.

## O `vm create` deixa de semear o que não lê

Nenhuma destas corre cloud-init. O `vm create` gerava **sempre** um seed NoCloud — o comentário
no código dizia «ALWAYS» — o que está certo para uma cloud image (sem datasource o cloud-init
salta a fase de rede e a VM fica sem IP) e errado para um sistema que se configura sozinho: um
ISO que ninguém lê, num CD-ROM que muda a lista de dispositivos do convidado.

`VmImage.cloud_init` marca-o. `None` — todos os metadados escritos até hoje — conta como `true`,
por isso nada muda para as imagens que este motor sempre construiu. Com `Some(false)` o seed é
saltado e `--hostname`/`--ssh-key`/`--user-data` são **recusados a nomear as flags**, em vez de
aceites e deitados fora.

`image vm import` regista um disco que este motor não construiu, nos três pontos de entrada de
sempre, comprimindo com zstd por omissão (um `convert` sem `-c` inflava 646 MiB para 2,15 GiB à
entrada, medido).

E o `push` de um artefacto OCI passa a carimbar os metadados em **annotations do manifesto**,
lidas pelo `pull` DEPOIS da verificação do digest. Sem isso um appliance publicado perdia a
marca e voltava a receber seed do outro lado; fecha de caminho o gap já documentado de
`ubuntu_release`/`k8s_version` desaparecerem num `vm pull`.

## Cinco imagens base de SO, e o Fedora

Ubuntu 24.04/26.04, Debian bookworm, Rocky 9 e **Fedora** — distro nova, da família dnf/RPM do
Rocky, com teste a comparar os passos gerados para as duas campo a campo para não divergirem.

`--fedora-release` exige `<release>-<build>` (`42-1.1`) e recusa um `42` nu. Medido: o nome do
artefacto carrega um build que a versão não determina, e o redirector do Fedora não serve
listagem de directório. Sem forma fiável de o descobrir, um `42` sozinho parece certo e dá 404
já com centenas de MB transferidos — pergunta-se em vez de adivinhar.

## A subnet de uma rede passou a valer

`network create --subnet 10.233.0.0/16` e o `spec.subnet` do `kind: Network` **nunca chegavam a
lado nenhum** com o driver `bridge`, que é o único que o rootless realiza: o octeto vinha do
hash do NOME da rede. Quem pedia um CIDR recebia outro e não era avisado.

O `create_with_base` existia para isto, dizia-o no doc-comment, e tinha **zero chamadores** — a
quinta ocorrência do padrão já catalogado (`mount_live`, `set_net_rate`, `update_limits`,
`publish_port_allow`).

Corrige de caminho uma **deriva eterna no reconciler**, que não andava a ser procurada:
`RECONCILED_NETWORK_FIELDS` já comparava `subnet`, logo um manifesto com `subnet:` produzia um
plano `-/+` a cada `stack plan` — e o apply nunca o resolvia, porque criava com o octeto do
hash. Validado no dataplane: um container na rede pedida ganhou `10.233.77.112/16`.

O espaço possível é `10.<200-254>.0.0/16` e a razão é estrutural: o registo de uma rede guarda UM
OCTETO, e bridge, gateway e range do IPAM são derivados dele. Tudo o resto é **recusado a nomear
a forma que funciona**, incluindo o vocabulário do módulo VPC do Terraform (`vpcCidr`,
`publicSubnets`, `privateSubnets`, `singleNatGateway`) — quem escreve `singleNatGateway` não
errou a escrita, tem um modelo mental que não mapeia num nó só, e merece uma frase que o diga.

## Dois bugs de download, ambos medidos

**O `stream_download` não tinha retry nenhum.** A cloud image do Rocky (646 MiB) morreu aos
3,8 MiB e a corrida seguinte recomeçava do zero. Passa a retomar por `Range:`; o que torna isso
seguro é o checksum que todos os chamadores já verificam — bytes costurados de dois ranges ou
dão o hash publicado ou o download é descartado.

**A ligação ficava presa num nó lento do CDN.** 242 KiB/s na transferência em curso contra
1732 KiB/s num pedido NOVO ao mesmo URL, no mesmo segundo; quatro mirrors testados, o canónico
era o mais rápido dos que respondiam. Reconectar levou o download real a 1441 KiB/s e o tempo
restante de 19 minutos para 2. Só passou a valer a pena porque a retoma existe: antes, largar a
ligação era largar o ficheiro. O limiar é RELATIVO ao melhor débito da própria transferência —
uma ligação uniformemente lenta não pode reconectar para sempre a perseguir uma velocidade que
nunca vai atingir.

## `image vm ls`/`ls-remote` dizem o que a imagem é

O `ls` ganhou `TYPE` (`cloud-init`/`appliance`) e `DEFAULTS` (`4cpu/8G`) — o que decide se o
`vm create` semeia a imagem (e portanto se o `--ssh-key` é aceite ou recusado) e com que recursos
arranca. `KERNEL` passou a ser preenchido também num `import` (por `virt-ls /boot`), e falha por
razão estrutural no OPNsense (FreeBSD) e no TrueNAS (raiz em ZFS), onde o libguestfs não vê
`/boot`. O `ls-remote` deixou de imprimir só a coluna TAG: lê o manifesto de cada tag (um GET,
sem transferir blob) e mostra distro, tipo e tamanho.

## `install.sh`: construir imagens, e um nó de produção

`--with-image-build` instala o que `image --vm build` exige e torna `/boot/vmlinuz-*` legível.
As duas faltas custaram um build cada e nenhuma se adivinha pelo erro: sem `isc-dhcp-client` no
HOST o appliance do supermin nasce sem cliente DHCP e o build morre em «Temporary failure
resolving 'archive.ubuntu.com'» (um erro que parece de rede do host, e o host tem rede); com o
kernel a `0600` morre em `cp: cannot open`. O chmod **baixa uma fronteira de segurança**, por
isso é opt-in, avisa alto e diz como reverter — o mesmo tratamento do `--low-ports`.

`--production` aplica os limites que só se atingem em CARGA, cada um por um modo de falha
concreto: `nf_conntrack_max` (todo o dataplane é nftables com conntrack — cheio, o kernel DROPA
ligações novas e do lado da aplicação parece perda aleatória), `neigh gc_thresh` (a tabela ARP
tem 1024 entradas), `ip_local_port_range`, `pid_max`, `file-max`, backlogs, `swappiness`. O
`hashsize` do conntrack vai por `modprobe.d` porque **não é um sysctl**. `LimitNOFILE`/`TasksMax`
vão para um drop-in do `user@.service`: em rootless os containers são filhos dele, e os limites
de uma sessão PAM/SSH não lhes chegam.

## Decisões registadas, e o que fica por fazer

Dois ADRs em `Proposed`: **0008** (backend Proxmox VE) e **0009** (provisionador TrueNAS), ambos
em crate próprio — `delonix-vm` tem QUATRO dependências e `delonix-volume` TRÊS, e trazer
`reqwest` (tokio + hyper + TLS) para um crate de motor de um runtime de containers é uma decisão
de cadeia de fornecimento. O 0008 traz um achado estrutural: o `VmBackend` é público e
implementável de fora, mas o `backend_for` é um `match` PRIVADO com `_ => CloudHypervisorBackend`
— nada de fora consegue registar um terceiro.

**Por fazer, e assinalado como tal**: publicar as appliances em
`ghcr.io/angolardevops/delonix-vm-appliances` (o workflow existe; falta o PAT com
`write:packages`). E `scripts/appliances/opn_install.py` está **INCOMPLETO**, commitado pelo
diagnóstico e assim marcado no próprio cabeçalho: instala o OPNsense com GPT+ESP que o EDK2
arranca, mas o kernel não encontra o pool ZFS — em QEMU **e** em Cloud Hypervisor, o que prova
que a falha é do particionamento do script e não do hipervisor. Não tratar como funcional.

**Um NO-GO revisto**: o OPNsense não arranca em Cloud Hypervisor a partir da imagem `nano`
porque essa é MBR/BIOS-only (`Disklabel type: dos`, zero ESP) e o CH só arranca UEFI. O controlo
que torna isto uma conclusão e não um palpite: a golden Linux, com o MESMO firmware EDK2,
responde em ~15s. Corrige também uma nota antiga do `AGENTS.md` que dizia que a golden é
«libvirt-only» — com o EDK2 `CLOUDHV.fd` ela arranca em Cloud Hypervisor.

## 15 de 30 containers perdiam a rede, e o erro não dizia nada

Apanhado pelo arnês de caos, não por leitura: o cenário `scale` passou a falhar
com «só 15 de 30 containers ganharam IP», e o erro era

```
error system call `ingress control` failed:
```

— nada depois dos dois pontos.

**Não era saturação do host, e mede-se**: escala com a concorrência.

| attaches concorrentes | falhas |
|---|---|
| 10 | 0 |
| 20 | 3 |
| 30 | **15** |

O `handle_control` é *o* ponto de serialização do holder — todo o comando que
muta rede corre lá, um de cada vez — e o cliente esperava **5 segundos** pela
resposta. Quem entrava na fila atrás de vinte attaches não era servido a tempo.

O que escondeu isto durante todo esse tempo foi uma linha: o cliente fazia
`let _ = s.read_to_string(&mut resp)`, **descartando o erro**. Um timeout de
leitura ficava indistinguível de um holder que respondesse nada, e os dois
imprimiam um erro sem sujeito.

Corrigido nos dois lados. O cliente lê o erro, distingue timeout de qualquer
outra falha, e diz o que fazer («o plano de controlo serializa cada operação de
rede, por isso uma rajada de `run`s concorrentes fica em fila atrás de si
própria — repete, ou arranca-os em lotes mais pequenos»); o tecto passou a 30s,
generoso de propósito porque sob rajada a espera **é a fila**, não um hang — mas
continua limitado. O holder, por seu lado, deixou de fechar a ligação mudo
quando desiste de ler o comando. O `SO_PEERCRED` recusado continua mudo de
propósito: esse é o caso hostil, e não leva oráculo.

Depois: **30/30 em 21 segundos**, e o arnês de volta a 20/20.

A nota de método vale mais que a correcção: a primeira leitura foi «o servidor
fecha mudo», e estava **errada** — o servidor não fechou nada, fomos nós que
desistimos. Foi o erro descartado que o escondeu.

### E a varredura pelo mesmo padrão, que achou um falso sucesso

Corrigido o socket de controlo, uma varredura pelo **padrão** (`let _ =` sobre um
`read`) e não por subsistema encontrou mais dois. O segundo é pior que o
original.

`slirp_add_hostfwd` tinha **500 ms** de tecto — no slirp ÚNICO que todo o ingress
partilha — e a seguir ao read fazia `if resp.contains("\"error\"")`, caindo em
`Ok(())` de outra forma. Um timeout deixa `resp` vazio, uma string vazia não
contém `"error"`, e a função devolvia **sucesso** para um publish que pode nunca
ter sido aplicado. No socket de controlo o sintoma era um erro sem sujeito; aqui
é um falso sucesso, que é a classe que a v0.37.0 existiu para apagar.

`slirp_api` devolvia `Ok("")` no mesmo caso, e o `slirp_remove_hostfwd` parseia
isso como JSON `Null` — conclui que não há entradas, e o **unpublish reporta
sucesso sem ter removido nada**, com a porta do host presa por uma entrada que o
registo já não conhece.

Os dois passam a ler o erro. O tecto do hostfwd sobe para 10s, e a mensagem diz
o que NÃO se sabe em vez de fingir uma das duas hipóteses:

```
port 18234: no reply from the slirp api-socket (...) - the publish may or may not
have been applied; check with `delonix net ingress ls`
```

Validado ao vivo: publish real numa porta, `ss` confirma o binding, `container
port` mostra-o, `--publish-rm` tira-o, `ss` fica a zero.

A lição de método é a mesma que os ficheiros temporários já tinham dado: **a
varredura por padrão apanha o que a divisão do trabalho por ficheiros deixa
cair.** O segundo `let _ =` estava noutro crate, que a investigação do `scale`
nunca teria tocado.


---

## v0.46.0 — a auditoria que se faz antes de alguém a fazer por nós

Seis revisões adversariais em paralelo sobre os oito crates, cada uma apontada a
uma classe de ataque que o Docker, o Kubernetes, o runc, o CRI-O ou o Podman já
sofreram em produção. Depois, uma varredura por padrão que apanhou aquilo que a
divisão do trabalho por ficheiros deixou cair — e foi lá que estava o pior
achado.

**Nenhum achado crítico.** A fronteira rootless→root aguentou: o mapeamento de
uid/gid nunca aponta para o uid 0 real do host em nenhum dos três caminhos, o
socket de controlo do holder verifica `SO_PEERCRED` antes de qualquer dispatch, e
o bloqueio de user namespaces aninhados (`clone` filtrado **mais** `clone3`→
`ENOSYS` sempre instalado) está no estado da arte. Catorze achados corrigidos, um
ALTO e seis MÉDIOS entre eles, cada um com teste de regressão.

## Onde estamos face aos CVE que os outros já levaram

Isto foi lido no código, não deduzido da documentação:

| Ataque | Estado |
|---|---|
| CVE-2019-5736 (runc, sobrescrever `/proc/self/exe`) | protegido — o re-exec usa o binário do host, antes de existir qualquer rootfs |
| CVE-2024-21626 «Leaky Vessels» | protegido — `close_range` nos forks, CLOEXEC, e o `chdir` do `exec` corre já dentro do mount namespace do container |
| CVE-2022-0811 «cr8escape» (sysctls) | protegido — allowlist de sysctls e `/proc/sys` remontado read-only |
| CVE-2022-0492 (`release_agent` do cgroup v1) | não aplicável — o motor é cgroup v2-only e não existe `release_agent` |
| userns aninhado, incluindo o desvio por `clone3` | protegido |
| tar-slip / zip-slip | protegido |

## O pin de digest não estava a segurar nada

`delonix image pull imagem@sha256:X` verificava cada *blob* contra o digest que o
manifesto declarava. Nunca verificava o **manifesto** contra o `X` que o
utilizador pediu.

Um registo comprometido — ou um mirror hostil, ou um registo servido em HTTP —
respondia ao pedido de `sha256:X` com um manifesto completamente diferente, a
apontar para os blobs do atacante. Como esse manifesto era internamente coerente,
todas as verificações de blob passavam e a imagem instalava sem um único erro.

Que é precisamente o contrário do que um pin existe para garantir. Está fechado
nos dois caminhos de pull e também no sub-manifesto de um índice multi-arch,
antes de se descarregar um byte de conteúdo. O teste de regressão falha com a
correcção revertida: sem ela, o pull devolve a imagem substituída.

## Uma VM podia mentir sobre quem era

Os `veth` dos containers levam, desde sempre, uma regra anti-spoofing: o que
entrar por esta interface com um endereço de origem que não é o teu é deitado
fora. O `tap` de uma VM nunca a levou.

E é na VM que ela mais falta faz, porque o kernel do convidado não é nosso — nada
lá dentro nos impede de pôr no fio o endereço que apetecer. Como **toda** a
política deste motor decide pelo endereço de origem (o corte cross-namespace
casa `@dlxall`, um `kind: Dependency` autoriza um endereço concreto), uma VM
forjava um `saddr` fora do conjunto — ou o de um vizinho da namespace-alvo — e
atravessava a fronteira.

A regra passou a ter uma definição só, partilhada pelos três sítios que a
emitem, e é limpa no teardown: nomes de `tap` são reutilizados entre reinícios, e
uma regra órfã fixada no endereço da VM anterior calaria a seguinte.

## Um `--device` chegava para truncar um ficheiro do host

O `bind_devices` era o último caminho de montagem do motor sem confinamento de
destino: construía-o por concatenação de strings e criava o ponto de montagem com
`File::create`, que **trunca**. Um `spec.devices: ["/dev/null:/../../etc/x"]` num
manifesto saía do rootfs e zerava o ficheiro do outro lado — medido, ficava com
zero bytes.

O padrão seguro estava a poucas linhas dali, em uso pelo `-v` desde há muito.
Fechou-se também, de caminho, o `/dev/mem`, o `/dev/kmem` e o `/dev/port`: são
*character devices*, por isso passavam pelo filtro que só recusava *block
devices*. Em rootless são inertes; no caminho root/CRI são o host inteiro.

## O container via mais do host do que devia

O motor mascarava o que dá **controlo** do host (`/proc/sysrq-trigger`,
`/proc/kcore`) mas não o que vaza **informação**. O caminho do CRI estava bem — o
kubelet manda sempre a sua lista — e só a CLI ficava a descoberto.

Medido dentro de um container real, antes: `/proc/interrupts` com 109 linhas
legíveis (já serviu de canal lateral para temporização de teclas) e
`/sys/firmware` com quatro entradas. Agora zero, com a lista por omissão do runc
— `timer_list` e `sched_debug` incluídos, que imprimem ponteiros do kernel e
ajudam a derrotar o KASLR. Uma lista explícita continua a mandar, e
`--privileged` continua a desligar tudo, como no Docker.

## O `docker-api` podia ficar preso a arrancar containers

O `clone()` do motor documenta, em comentário, que assume um chamador
single-threaded. No `delonix serve docker-api` isso é falso: o servidor é um
runtime tokio multi-thread. E `clone()`, ao contrário de `fork()`, **não** corre
os handlers `pthread_atfork` que repõem o lock do alocador no filho — se outra
thread o tivesse na mão naquele instante, a primeira alocação do
`container_init` bloqueava para sempre. Container que nunca arranca, API que já
respondeu «criado».

Passou a arrancar por re-exec, num processo fresco onde a premissa volta a ser
verdadeira — o mesmo que o CRI já fazia. A especificação viaja em ficheiro
(`0600`, `O_EXCL`) e não em argv: o `RunOpts` tem dezenas de campos e reconstruir
uma linha de comando perderia em silêncio tudo o que não tem flag.

**O bug que a correcção destapou já estava previsto no código.** O comentário do
reaper de zombies avisava que um `waitpid(-1)` cego «corromperia o estado de
saída» de qualquer caminho que fizesse o seu próprio `waitpid` — e o re-exec é
esse caminho. Apareceu na primeira validação ao vivo: o container arrancava bem e
o `create` devolvia um erro de I/O. O reaper passa agora a espreitar sem consumir
e só colhe filhos que ninguém reclamou. Validado com o ciclo de vida completo e
oito criações concorrentes.

## Escalada de privilégio local por um ficheiro em `/tmp`

Este é o achado que nenhuma das seis revisões viu, porque o ficheiro não estava
na superfície atribuída a nenhuma delas. Apareceu numa varredura por padrão feita
no fim.

O `delonix net flow` materializava o seu objecto eBPF no caminho **fixo**
`/tmp/delonix_flow.bpf.o`, com `fs::write`, e entregava-o ao `bpftool prog
loadall` — um processo com `CAP_BPF`/root. O `/tmp` é escrevível por toda a
gente, o `fs::write` segue symlinks, e — o pior — quem criasse o caminho primeiro
ficava **dono** do ficheiro: num `/tmp` com sticky bit nem sequer o podemos
apagar, o que lhe deixava a janela para lhe trocar o conteúdo entre a nossa
escrita e a leitura do `bpftool`.

Ou seja: um utilizador local sem privilégio nenhum conseguia que um processo
privilegiado carregasse o programa eBPF **dele** dentro do kernel.

Outros dois sítios (a config do `kubeadm` e a do HAProxy, ambas enviadas por
`scp`) tinham nome derivado do pid — igualmente adivinhável, com o mesmo vector
de redirecção. Os três passaram a usar um helper único: nome irrepetível,
`O_EXCL` (que recusa um caminho existente e não segue symlinks) e `0600` desde a
criação.

## A golden trazia credenciais conhecidas e não o dizia

`root/delonix` e `delonix:delonix`, com `sudo` sem password. São fixas, estão na
receita de build deste repositório aberto e, portanto, são públicas — mas não
havia uma linha sobre isso no README.

Existem por uma razão boa: uma VM que ficou sem rede ainda se alcança pela
consola série. Todo o resto entra por chave (o cloud-init injecta a tua, o
`cluster kubeadm` gera a dele), pelo que as imagens passam a sair com o **login
por password desligado no SSH**. A password serve na consola, que é onde faz
falta, e não pela rede.

Está escrito no README, com o que fazer se correres uma destas imagens algures
onde se lhe chegue.

## O resto

`reap_orphan_hostfwds` deixou de aceitar um conjunto de portas qualquer: exige
agora um tipo cuja única função é obrigar quem chama a **afirmar** que conhece o
ingress inteiro. Foi um chamador externo com uma lista parcial que fez as portas
publicadas morrerem sozinhas, e custou várias sessões a diagnosticar — a forma da
API era o convite.

O `CredVault` passou a escrever como o cofre de segredos irmão já escrevia
(temporário por escritor, `fsync`, modo na criação): num blob AEAD uma escrita
rasgada não é um ficheiro corrompido que se recupera, é uma credencial
indecifrável para sempre.

E ainda: o `fetch_kubeconfig` — o `mode()` só se aplica na criação, por isso um
`cluster apply` repetido sobre um kubeconfig deixado por uma build antiga
reescrevia as credenciais e mantinha o `0644` de lá; `--` antes dos posicionais
do `mount` e do `qemu-img convert`; a mesma guarda de `-` inicial que o token do
túnel já tinha, agora também no hostname; um tecto no tamanho dos downloads; e o
`personality(2)` restrito aos valores que não enfraquecem o ASLR nem o NX, como
no perfil por omissão do Docker.

## O que ficou deliberadamente por mudar

O `--cap-add` continua a aceitar qualquer capability e o `--device` continua a
ser opt-in do operador. É paridade com o Docker e o containerd, e é decisão de
quem escolhe correr o `delonix-cri` como root e aceitar pods privilegiados —
mudá-lo seria alterar a semântica esperada, não corrigir um defeito.

O checksum lateral de um `FROM https://…` no VMfile continua a vir da mesma
origem que a imagem, o que só protege contra corrupção de transporte e não contra
uma origem hostil. É opt-in e o URL é escolha de quem constrói; fica registado
como risco aceite, ao lado dos binários do Cloud Hypervisor.

## Verificação

`cargo build`, `cargo test --workspace` (zero falhas), `clippy` e `fmt` limpos.
Validado ao vivo neste host: o mascaramento dentro de containers reais com musl e
com glibc, o `docker-api` de ponta a ponta com criações concorrentes, o `net
flow`, e o `container run` normal sem regressão.

**Mudança de API pública** (`delonix-net`): `reap_orphan_hostfwds` passou a
receber `AuthoritativeLivePorts` em vez de `&HashSet<u32>`. Quem a chamava tem de
passar a construir o tipo — que é o objectivo.

---

## v0.45.0 — o que se descobre ao correr aquilo que se acabou de escrever

A v0.44.0 publicou o `VMfile`. Esta release é o resultado de o usar como um
utilizador o usaria, num host que não é a máquina de quem o escreveu. Quatro
achados, todos reproduzidos antes de corrigidos, nenhum deles visível a um
`cargo test`.

## O esqueleto do `vm init --vmfile` não construía

O VMfile gerado dizia, em comentário, «*Builds as written*». Não construía: o
primeiro `RUN` era `apt-get update && apt-get install nginx`, e o `vm build`
passa **sempre** `--no-network` ao `virt-customize`. A primeira coisa que um
utilizador faz com o comando novo — correr o que ele acabou de escrever — não
podia funcionar.

O `--no-network` está certo como omissão (um build que vai à internet dá uma
imagem diferente conforme o dia em que correu). Mas a coisa mais comum que se
quer fazer numa imagem é instalar um pacote, e um motor que a torna impossível
não está a oferecer uma escolha: está só a recusar. Por isso:

* **`delonix vm build --network`** — opt-in explícito, com o custo de
  reprodutibilidade dito no `--help`. Ligado aos **três** caminhos (`vm build`,
  `image vm build`, `image --vm build`); na receita dourada é recusado com um
  erro que aponta para o `--offline`, que é quem decide isso lá.
* **O esqueleto passou a construir tal como está escrito**, offline, e mostra o
  `apt-get` como o exemplo comentado do que fazer *com* `--network`.

## «No such file or directory» quando o que falta é um pacote

Num host sem libguestfs, o `vm build` descarregou 600 MB, verificou o
`SHA256SUMS`, redimensionou o disco — e depois disse:

```
error invalid argument: running virt-customize: No such file or directory (os error 2)
```

O `ENOENT` de um `Command::status()` é a ferramenta não existir, mas a frase
lê-se como um **ficheiro** em falta e manda o leitor procurar um caminho.
Agora nomeia o pacote, nas duas famílias:

```
error invalid argument: `virt-customize` is not installed. Install it with
`sudo apt install libguestfs-tools` (Debian/Ubuntu) or
`sudo dnf install guestfs-tools` (Fedora/Rocky).
```

Vale para `virt-customize`/`virt-sparsify`/`virt-copy-out`, `qemu-img`,
`cloud-localds` e `virsh`. A tabela é pura e testada, porque é ela que carrega
o valor todo.

## `vm create --url-img` — validado de ponta a ponta, e o utilizador errado

Correu inteiro: descarregou o qcow2 de um URL absoluto (com o aviso honesto de
que não há `.sha256` publicado ao lado, logo a confiança é só do TLS), montou o
overlay, gerou o seed NoCloud, arrancou em libvirt, e o cloud-init aplicou
hostname e chave. Entrei na VM por SSH e confirmei `Ubuntu 24.04.4 LTS`,
1 vCPU, ~1 GiB, `cloud-init status: done`.

O que falhou foi eu adivinhar o utilizador. Numa cloud image de Ubuntu o palpite
óbvio é `ubuntu`, essa conta **existe**, e não tem a chave — por isso responde
`Permission denied (publickey)`, que se lê como chave partida e não como nome
errado. A chave vai para `delonix`, e nada no output o dizia. O bloco de
próximos passos passou a incluir a linha, com o IP quando já é conhecido:

```
Next steps:
  delonix vm console urltest2    # open the serial console (back to host: Ctrl+])
  ssh delonix@<ip>               # log in with the key you injected
  ...
```

Só no caminho em que o seed é gerado por nós — quem trouxe o seu próprio
`--seed` decidiu as contas e nós estaríamos a adivinhar.

## O `vm build` completo continua por validar aqui, e digo porquê

Este host não tem `libguestfs-tools` e não posso instalá-lo. Fica provado tudo
até à fronteira: download, verificação de `SHA256SUMS`, achatamento para qcow2,
`SIZE` aplicado antes de qualquer `RUN`, e a recusa clara na ferramenta em
falta. O que falta exercitar é o `virt-customize` em si — um
`sudo apt install libguestfs-tools` e o `delonix vm build -t x:1.0 .` do
esqueleto fecha-o.

## Validação

669 testes verdes em duas corridas independentes, clippy e `fmt` limpos. Os
caminhos novos verificados também em `--l18n=pt`.

---

## v0.44.0 — VMfile, e um diagnóstico de cgroup que estava a mandar editar o /etc por nada

Duas metades. A primeira é funcionalidade nova: construir a imagem qcow2 de uma
microVM a partir de um ficheiro declarativo, ao estilo do `Delonixfile`. A
segunda começou por ser uma pergunta do utilizador — *o que faz `sudo delonix
system setup --delegate`?* — e acabou a corrigir o comando, o instalador e a
documentação, porque a resposta que eu tinha dado estava errada.

## `VMfile` — `vm init` e `vm build`

O caminho para uma imagem VM própria era `image --vm build` com uma lista de
flags (`--extra-package`, `--extra-run`) por cima de uma receita fixa em Rust.
Serve para a golden do k8s, que é nossa; não serve para alguém que queira a
SUA imagem, com o seu cloud-init, publicada no seu repositório.

`VMfile` é essa forma, com a gramática que toda a gente já sabe ler:

```dockerfile
FROM ubuntu:24.04
SIZE 20G
HOSTNAME app
VCPUS 2
MEMORY 4G

RUN apt-get update && apt-get install -y nginx
COPY ./site /var/www/html
ENV APP_ENV=production
USER delonix
SSHKEY @~/.ssh/id_ed25519.pub
CLOUDINIT ./cloud-init/user-data
```

* **`FROM` é uma cloud image**, não uma imagem OCI — `ubuntu:24.04`,
  `debian:12`, `rocky:9`, ou um URL absoluto para o qcow2 que quiseres. É a
  distinção que dá sentido ao resto: uma cloud image traz cloud-init, e é o
  cloud-init que faz o primeiro boot aplicar hostname, chaves e utilizadores.
* **Multi-stage** com `COPY --from=<estágio>`, como no `Delonixfile`.
* **`delonix vm init`** escreve o esqueleto completo (VMfile + `cloud-init/`
  com `user-data`/`meta-data` comentados), para não se começar de uma página
  em branco.
* **`delonix vm create --url-img https://…`** cria a microVM directamente de um
  qcow2 publicado — o caminho de quem já construiu a imagem e a pôs no seu
  repositório.

## O `--delegate` que eu recomendei não era a correcção

Ao explicar o comando, afirmei que ele resolvia o `cpu` em falta neste host.
**Medi antes de publicar e a afirmação estava errada nos três pontos:**

1. O Ubuntu 24.04 já traz `Delegate=pids memory cpu` no `user@.service`. O
   drop-in que o `--delegate` escreve pedia exactamente o que já lá estava.
2. O `subtree_control` do `user@1000.service` já era propriedade do utilizador.
   A delegação não estava em falta — estava feita.
3. O que faltava o `cpu` era o slice de onde o comando corria (`app.slice`, o
   scope do editor), que não o passa para baixo. Um drop-in no `/etc` não lhe
   toca.

E `cpuset`/`io` **nunca** podiam aparecer: o `user.slice`, que é da root, só
passa `cpu memory pids` para os descendentes. Pedi-los num drop-in seria pedir
uma coisa que o antepassado não tem para dar.

### O que mudou por causa disso

**O `cpu` e o `cpuset`/`io` deixaram de ser o mesmo facto.** Faltar o `cpu` é
um nó Kubernetes que não arranca; faltar `cpuset`/`io` é o estado normal de um
Ubuntu de fábrica, onde tudo funciona. A lista única tratava os dois como o
mesmo problema e mandava para o `/etc` por causa do segundo:

```
controllers: memory pids
missing:  cpu  ← a Kubernetes node CANNOT boot without this
absent:   cpuset io  ← optional; nothing here needs them
```

**A ordem dos remédios inverteu-se.** O scope delegado — sem root, sem
reiniciar sessão, efeito imediato — é agora a correcção **1**, e o drop-in no
`/etc` é a **2**, apresentada como o que fazer *se a primeira ainda disser que
falta o `cpu`*. A ordem anterior mandava alterar a configuração global da
máquina antes de tentar a coisa gratuita que resolve o caso comum.

**O preflight do `cluster create` diz o mesmo.** Recusa antes de descarregar
425 MB, como na v0.43.1, mas o comando que oferece é o scope, não o `sudo`.

**O `install.sh` não escreve o drop-in num host que já delega o `cpu`.** Lê o
`Delegate=` efectivo do `user@.service` e salta quando o `cpu` já lá está.
Escrever no `/etc` de toda a máquina para repetir o que a distro já faz não é
inofensivo: dá a impressão de ter resolvido um problema que o utilizador
continua a ter.

## Documentação

O README e o site descreviam a ordem antiga. Passaram a explicar as duas
correcções pela ordem certa, e a dizer porque é que `cpuset`/`io` aparecem como
*absent* sem que isso seja um problema a resolver. O laboratório 2 (limites de
recursos) foi reescrito no mesmo sentido — era o sítio onde um leitor novo
aprendia a começar pelo `sudo`.

## Validação

Testes verdes, clippy e `fmt` limpos. Os dois caminhos do `system setup`
verificados ao vivo (dentro do scope do editor e dentro de um scope delegado),
e o `cluster create` confirmado a passar o preflight e a avançar para o pull
dentro do scope delegado. Teste novo a fixar a distinção fatal/opcional, que
falha se a lista voltar a ser uma só.

---

## v0.43.1 — o nó do cluster morria por delegação de cgroup, e ninguém dizia porquê

Bug report real, com o ecrã inteiro como prova:

```
✗ Preparing nodes (1)
error invalid argument: timeout waiting for containerd on node 'kaeso-control-plane' (90s)
```

O containerd nunca chegou a arrancar porque o **nó já tinha morrido**. Os logs
do nó tinham a resposta em duas linhas:

```
INFO: running in a user namespace (experimental)
ERROR: UserNS: cpu controller needs to be delegated
```

Entre a causa e o sintoma havia um download de 425 MB e 90 segundos de espera
por um serviço que nunca teve hipótese.

### O `system setup` reportava sucesso sobre a limitação

Pior: o comando que existe para diagnosticar isto dizia que estava tudo bem.

```
limits:   APPLY — --memory/--cpus/--pids-limit take effect
Nothing to do.
```

E estava certo naquilo que verificava — «consigo criar um filho e escrever o
`subtree_control`?», sim. **Só que nunca olhou para QUAIS controladores existem.**
Neste host havia `memory pids` e mais nada; o `cpu`, que é o que um nó
Kubernetes exige, não estava delegado. Um diagnóstico que responde à pergunta
ao lado da que interessa é pior do que nenhum, porque é acreditado.

Agora lista-os, nomeia os que faltam, e liga-os à consequência:

```
controllers: memory pids
missing:  cpu cpuset io  ← a Kubernetes node needs these

Container limits work, but `cpu`, `cpuset`, `io` is not delegated.
`delonix cluster create` (kind mode) will fail: the node's entrypoint refuses
to boot without it (`UserNS: cpu controller needs to be delegated`).
```

O drop-in que o `--delegate` escreve passou a **nomear os controladores** em vez
de `Delegate=yes`: neste host o `yes` produziu apenas `memory pids`, o que passa
em todas as verificações do motor e mata um nó kind na mesma.

### Falhar em milissegundos, não em 90 segundos

`cluster create` (modo kind) ganhou um preflight. Sem o `cpu` delegado recusa
ANTES de puxar a imagem, com a causa e o remédio:

```
error invalid argument: the `cpu` cgroup controller is not delegated, and a
Kubernetes node cannot boot without it (its entrypoint exits with `UserNS: cpu
controller needs to be delegated`). Delegated here: memory pids. Run `delonix
system setup` for the diagnosis and `sudo delonix system setup --delegate` for
the fix, then log out and back in.
```

Só o `cpu` bloqueia. O `cpuset`/`io` faltam em muitos hosts onde o nó arranca
bem, e recusar por causa deles rejeitaria clusters que teriam funcionado.

E o timeout, quando acontece, deixou de ser a mensagem inteira: passa a dizer se
o nó ainda está vivo e a mostrar as últimas linhas do log dele. Era o que faltava
para ligar `timeout waiting for containerd` a `cpu controller needs to be
delegated`.

## `vm create` não descarregava a imagem oficial

`delonix vm create <nome>` sem `--disk`, num host sem imagens VM locais,
respondia:

> no local VM images — run `delonix image --vm build` (or `pull`) first

O `cluster kubeadm` já descarregava sozinho nessa mesma situação. A imagem
dourada é publicada como artefacto OCI precisamente para não ter de ser
construída à mão, por isso a mesma imagem em falta era um download prestável num
comando e um beco sem saída no outro.

Fechado com **um helper partilhado** pelos dois: `vm create` passa a descarregar
a oficial, e quando o download não é possível o erro **lista o que existe** em
vez de mandar correr outro comando para descobrir:

```
error invalid argument: could not download 'ghcr.io/angolardevops/delonix-vm-k8s:9.99':
no such image. Published there: 1.34, 1.35 — pick one with
`delonix vm pull ghcr.io/angolardevops/delonix-vm-k8s:<tag>`, or build your own
with `delonix image --vm build`.
```

O erro de «várias imagens locais» também passou a nomeá-las. A resposta à
pergunta «qual delas?» tem três palavras e estava a um comando de distância sem
razão nenhuma.

## Conformidade CRI: 77 → 79

Três specs fechados, e um deles confirma o que a v0.43.0 publicou como por medir
(o perfil seccomp `localhost`).

* **`ReopenContainerLog` era um no-op que devolvia sucesso.** O kubelet roda o
  log renomeando o ficheiro e chamando isto; responder «feito» sem fazer nada
  mandava cada linha seguinte para um inode que ninguém voltaria a ler — em
  silêncio, e só para contentores que vivem o suficiente para serem rodados.
  Duas metades: o shim passa a SEGUIR O CAMINHO (compara inodes antes de cada
  lote) e a chamada recria o ficheiro, que é o que o chamador verifica.
* **`list_container_stats` deitava fora o `label_selector`.** Um filtro que
  devolve mais do que foi pedido é pior que um que dá erro: as linhas a mais
  parecem respostas verdadeiras.
* **`ContainerStatus` devolvia `mounts` vazio** — lê-se como «este contentor não
  tem volumes», o oposto da verdade para tudo o que um kubelet monta.

## O motor corre dentro de um user namespace aninhado

**TERCEIRO sítio** onde `geteuid()` foi confundido com «root a sério»:
`write_userns_maps` mapeava um intervalo de 65536 uids quando euid era 0, o que
num userns aninhado — onde possuímos exactamente um uid — falha com EPERM e
nenhum contentor arranca. Com as correcções irmãs em `is_rootless` e
`runtime_dir`, o motor corre inteiro sob `unshare --user --map-root-user`.

Propagação de mounts medida nesse ambiente, com a origem `rshared` e uma
montagem feita DEPOIS de o contentor arrancar: `rslave` e `rshared` vêem-na,
`rprivate` não. A via normal mantém o intervalo de subuid intacto (`--user 101`
continua a funcionar).

## Documentação

O README estava na 0.38.0 — cinco releases atrás. Alinhado, com a conformidade
CRI publicada e três linhas novas na tabela comparativa que só valem por serem
honestas. O `--help` do `--security-opt` ainda descrevia só o que existia antes
da v0.43.0; como a documentação embebe o help real, a lacuna era no código.
Mais 18 entradas no catálogo `pt.po`.

## Correcção a um teste, não ao motor

`iotune_das_vms_e_opt_in_e_so_no_disco_raiz` falhava por ordem de escalonamento:
o teste irmão faz `env::set_var` e os testes correm em paralelo no MESMO
processo, por isso o opt-in lia o que o vizinho tinha acabado de pôr. O
comentário do próprio teste avisava disto enquanto o vizinho o fazia. A
composição do `<iotune>` passou a função pura e nenhum teste toca no ambiente.

## Validação

658 testes verdes, clippy e `fmt` limpos. Conformidade CRI 79/103 num root
limpo. Os dois bugs deste relatório reproduzidos antes e verificados depois, no
host onde foram encontrados.

---

# v0.43.0 — conformidade CRI medida: 65 → 77 de 103

Esta série começou por uma comparação honesta com o Docker e o Podman
(`docs/paridade-docker-podman.md`) e acabou a correr a suite de conformidade de
upstream a sério. **É o resultado dessa medição que dá o valor à release**:
«serve um kubelet» é uma alegação, «77 de 103 specs nomeados» é um facto que
outra pessoa verifica — `tests/compat/cri-conformance.sh` reproduz.

O detalhe completo, incluindo o que falha e porquê, está em
[docs/cri-conformance.md](../cri-conformance.md).

## O bug que estava escondido há mais tempo

O `delonix-cri` invocava **`delonix netns attach`**. A reorganização da CLI da
v0.30.0 moveu esse comando para **`delonix net netns attach`**, com corte limpo
e sem aliases — decisão deliberada e registada — e este chamador nunca foi
actualizado. **A criação de pod em rootless estava partida desde essa versão.**

Duas coisas o esconderam, e a segunda é a mais grave: o `delonix_detached`
mandava o **stderr para `/dev/null`** e devolvia um `bool`, por isso a mensagem
que chegava ao kubelet nomeava a vítima e escondia o assassino. Uma linha de
correcção levou a corrida de **19 para 65 passes**.

## `euid 0` não é ser root

`is_rootless()` era `geteuid() != 0`. **euid 0 é uid 0 nalgum user namespace**, e
num aninhado não compra nada no host: nem escrita no cgroup do host, nem `/run`,
nem montagem privilegiada. O motor tomava o caminho ROOT exactamente onde tem
menos poder, e o mesmo erro estava num segundo sítio independente
(`delonix-net::runtime_dir`, que resolvia `/run/delonix-net` e falhava com
`Permission denied`).

Corrigido na raiz, com o predicado num só sítio
(`delonix_runtime_core::in_initial_userns`, lido de `/proc/self/uid_map`) e
testes puros do parsing.

**Consequência prática**: o motor passa a correr inteiro sob
`unshare --user --map-root-user` — continua rootless e daemonless, mas com
`CAP_SYS_ADMIN` sobre os seus PRÓPRIOS namespaces, que é o tecto honesto do
modelo. O holder degrada em vez de pendurar (o `--map-auto` precisa de
`newuidmap`, que num userns aninhado é recusado) e o seu stderr deixou de ir
para `/dev/null`.

## O log do CRI dizia que tudo era `stdout`

O `log_shim` escrevia `format!("{ts} stdout {stream} …")` — com **`stdout`
literal**. Os dois fluxos do contentor iam por UM pipe e saíam todos com a mesma
etiqueta; a variável chamada `stream` era afinal a etiqueta F/P de linha
completa/parcial, confusão que ajudou o bug a durar.

Não é só conformidade: **qualquer `kubectl logs` sobre este motor afirmava que
tudo era stdout.** Corrigido com um segundo pipe (só em modo CRI — o formato
não-CRI não tem etiqueta e separar lá só interlaçaria pior) e um `poll()` sobre
os dois.

## Metade do `ContainerConfig` não estava ligada

`ContainerConfig.mounts` era lido por ninguém. Nem uma linha. Um kubelet a montar
configMaps, secrets, emptyDirs ou hostPaths não punha nada dentro do contentor —
em silêncio, porque nada dava erro. O mesmo padrão apareceu em `dns_config` e
`port_mappings` do sandbox.

Vale grepar cada campo do `PodSandboxConfig`/`ContainerConfig` antes de assumir
que está ligado.

## Uma linha que valeu quatro specs

O `exec` juntava-se a `user/uts/net/pid/mnt` — **faltava o `ipc`**. Um `exec`
deve parecer o contentor visto de dentro; sem o IPC vê os objectos System V do
HOST e os `kernel.shm*`/`fs.mqueue.*` do host, que são precisamente os knobs que
o `--sysctl` mexe. Um contentor criado com `kernel.shm_rmid_forced=1` reportava
`0` através do `exec`, porque o valor é resolvido na ipc-ns de QUEM LÊ. Lia-se
como «o sysctl não foi aplicado» e mandou investigar um caminho que estava
correcto desde o início.

## `NetworkReady` era um impasse permanente

A condição era `infra.up`, e este motor é daemonless: a netns de infra arranca A
PEDIDO. Num nó acabado de arrancar isso dava `NetworkReady: false` → o kubelet
marcava-o NotReady → não agendava pod nenhum → nada trazia a infra acima →
NotReady **para sempre**. A verificação existia para apanhar uma falha real de
SDN e descrevia o estado normal de repouso do motor.

Passou a distinguir pelo ref-count: infra em baixo COM workloads é falha, em
baixo sem nenhum é ócio. O teste de regressão que a guardava passou a codificar
a regra refinada, com a decisão extraída para uma função pura.

---

# Funcionalidades

## Perfis seccomp OCI — a última recusa fail-closed do motor

`--security-opt seccomp=<perfil.json>` e o `localhostProfile` do CRI. Formato
runc/Docker/containerd. O motor recusava-os com um erro claro — honesto como
marcador, mas era uma lacuna.

362 syscalls resolvidas **por arquitectura** (de `libc::SYS_*`, não literais: os
números diferem em aarch64 e uma tabela fixa estaria silenciosamente errada num
dos dois). Fail-closed no que não percebe — regras com `args` são **recusadas**,
não alargadas: uma regra que só queria bloquear `clone(CLONE_NEWUSER)` viraria
«bloquear clone» e partia todo o programa com threads lá dentro.

## Health check contínuo, sem daemon

`--health-cmd`, `--health-interval`, `--health-timeout`, `--health-retries`,
`--health-start-period`; o `STATUS` do `ps` passa a `Up 21 seconds (healthy)`.

**Quem monitoriza** era a decisão de desenho: é o supervisor do contentor
destacado — já existe, um por contentor, sobrevive à CLI e morre com aquilo que
vigia. Sem processo de frota. O Podman precisa de timers do systemd para o
mesmo; aqui funciona num contentor, num chroot, num host sem systemd.

**O probe limita-se a si próprio** dentro do contentor: o motor não o consegue
matar de fora sem deixar o processo vivo no pid-namespace, e isso repetir-se-ia
a cada intervalo, para sempre.

## `run --wait`

Bloqueia até o `HEALTHCHECK` da imagem passar. Medido: 64 ms sem a flag (serviço
ainda a arrancar) contra 6086 ms com ela e o serviço pronto à saída. Substitui o
`until curl …; do sleep; done` que toda a gente acaba por escrever mal.

## Segurança e namespaces

* **`--add-host`**, persistido (o `/etc/hosts` é reescrito em cada arranque).
* **`--masked-path`** (ficheiro tapado com `/dev/null`, directório com tmpfs
  vazio read-only — a técnica do runc) e **`--readonly-path`**.
* **`--group-add`**, aplicado mesmo com o contentor a correr como root, e também
  no `exec`.
* **`--security-opt no-new-privileges=[true|false]`**. O default do motor
  continua ON, mais apertado que o Docker e o Podman.
* **`-v src:dst:rprivate|rslave|rshared`** — propagação de mounts. A raiz do
  namespace passa a `MS_SLAVE` **só** quando alguma montagem o pede.
* **`--dns`, `--dns-search`, `--dns-option`.**

## Ergonomia e contrato

* **Atalhos de topo**: `ps`, `run`, `exec`, `logs`, `rm`, `images`. Por
  reescrita de argv, não por variantes clap duplicadas — são o MESMO comando por
  construção, `--help` incluído. `stop`/`start` ficam de fora de propósito: este
  motor também pára VMs e pods.
* **`system setup [--delegate]`** — diagnostica e corrige a delegação de cgroup,
  com os DOIS remédios: um `session-N.scope` de login é IRMÃO do
  `user@.service` e não herda nada dele.
* **`serve docker-api --matrix`** — as 14 rotas implementadas e as 7 ausentes
  com a razão. Um teste lê o código-fonte do próprio dispatch e falha se existir
  um braço sem linha na tabela.
* **[docs/cli-stability.md](../cli-stability.md)** — o que é estável dentro de
  0.x, o que é estável em conteúdo mas não em formato, e o que não promete nada.

## Correcções

* `commit_flat_rootfs_from_tar` **descartava o `HEALTHCHECK`** no caminho
  rootless (o normal) enquanto o caminho overlay o honrava — o mesmo Dockerfile
  dava imagens diferentes conforme o modo do motor.
* `apply_sysctls` **deitava fora o erro da escrita**.
* Um membro de pod levava **dois caminhos de publicação** — a netns é do pod, e
  o slirp por-contentor reclamava a mesma porta que o ingress já publicara.
  Pelo caminho, a quinta ocorrência da armadilha antiga: `c.ip` nunca era
  atribuído a um membro de pod.
* **ALTO, apanhado em revisão**: o `--add-host` armou um primitivo de escrita
  fora do rootfs. O `write_etc_files` usava `format!` + `fs::write`, ambos
  seguindo symlinks, e em rootless a árvore pertence ao uid mapeado — o próprio
  contentor podia plantar `etc/hosts -> ~/.ssh/authorized_keys`. Já existia como
  truncar-com-conteúdo-fixo; o `--add-host` tornou o conteúdo escolhido pelo
  atacante. Fechado com `safe_bind_target` + validação na fronteira.

## O que NÃO fecha, e porquê

* **AppArmor (9 specs)** — carregar perfis exige `CAP_MAC_ADMIN` no user
  namespace INICIAL. Medido: um aninhado responde `You need policy admin
  privileges`. **O Docker e o containerd têm exactamente o mesmo limite.**
* **`nil profile = unconfined`** — divergência **deliberada**: sem perfil
  declarado aplica-se o allowlist embutido. Somos mais restritos do que a
  especificação pede e isso não muda para ganhar um spec.
* **`shareProcessNamespace`** e o `chown_tree_once` que tira o dono ao binário
  setuid são compromissos de desenho por decidir, não esquecimentos.
* O port mapping só com container port precisa que o IP do pod seja alcançável
  do host, o que o modelo rootless não dá.

## Validação

658 testes verdes, clippy e `fmt` limpos. Conformidade CRI 77/103 num root
limpo (correr sempre assim: com estado acumulado, três specs falham por poluição
e não por conformidade). Smoke da API Docker com o SDK oficial de Python —
`create` → `start` → `inspect` → `rename` → `stop` → `remove`, a sequência que
o Testcontainers usa de facto: 14/14.

---

## v0.42.2 — `system info` mentia sobre a delegação de cgroup

Varredura pedida sobre uma CLASSE de bug, não sobre um caso: cinco defeitos desta
série eram todos a mesma frase — **X não é Y**. Um ficheiro de socket não é um
listener; `/sys/class/net` não é a netns do processo; `capture()` devolver `Ok`
não é o comando ter passado; uma label não é o estado persistido; `None` no pid
do controlo não é ausência de controlo.

A varredura encontrou mais um da mesma família, e no pior sítio possível.

### `delonix system info` respondia sempre «yes»

A linha `cgroup2 delegated:` — a que se lê exactamente para perceber porque é que
`-m`/`--cpus`/`--pids-limit` não estão a pegar — era decidida assim:

```rust
Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
    && read_to_string("/sys/fs/cgroup/cgroup.subtree_control").contains("memory")
```

Os dois ficheiros são do cgroup **raiz do host**. Em qualquer máquina com systemd
o segundo contém `memory` (medido aqui: `cpuset cpu io memory pids`). O comando
respondia portanto **`yes` incondicionalmente** — incluindo na sessão SSH onde
este projecto já tinha medido os cinco limites silenciosamente inertes.

É a mesma lição que o `install.sh` já tinha aprendido («ler `cgroup.controllers`
não chega») e que o `system info` nunca recebeu.

### A função certa existia e ninguém a chamava

`delonix_runtime::cgroup_limits_apply()` já fazia a pergunta certa — mas só tinha
um chamador (`cluster create`) e, pior, **só testava o caminho root**
(`delonix.slice`, que num host rootless nem existe). Em rootless o `spawn` usa o
cgroup **actual**. É o mesmo erro de base-estática-vs-dinâmica que o
`update_limits` fez com `container.cgroup()` em vez de `live_cgroup()`.

Agora é consciente do modo, e o `system info` chama-a.

### Qual é a sonda que discrimina (medido, não deduzido)

Testaram-se os três candidatos no cgroup real desta sessão:

| sonda | scope delegado | sessão SSH |
|---|---|---|
| criar um filho | OK | OK ← não discrimina |
| `cgroup.subtree_control` gravável | OK | **recusa** |
| activar `+memory` | **falha** | falha ← falso negativo |

Criar um filho é possível nos dois. Activar `+memory` falha mesmo num scope
genuinamente delegado, porque a regra de *no internal processes* do kernel a
recusa enquanto o nosso próprio processo estiver no cgroup (o motor contorna-o
movendo-se para um `dlx-mgr` primeiro — demasiado invasivo para um diagnóstico
só-de-leitura). **O que separa os dois casos é a posse do
`cgroup.subtree_control`**: o systemd faz chown dele para o utilizador num
`Delegate=yes`, e num `session-N.scope` fica `root:root`. Confirmado neste host:
o scope delegado é `walter:walter`, o `session-2.scope` real é `root`.

### O que a varredura NÃO encontrou

Nenhum outro `capture(...)` a ser lido pelo `Result` em vez da saída, e os
`unwrap_or_default()` restantes são todos «listar para decidir o que
acrescentar», onde vazio leva a criar (idempotente) e nunca a apagar.

Fica registada uma armadilha de API, sem bug vivo: `reap_orphan_hostfwds` é
público e falha ABERTO com uma lista vazia (lista vazia ⇒ tudo é órfão ⇒ apaga
tudo). O único chamador deste repo é seguro — propaga o erro do `store.list()`
em vez de passar um conjunto vazio, e o comentário raciocina sobre isso — mas a
função continua a ser uma armadilha para um consumidor externo, e foi exactamente
assim que as portas publicadas morriam sozinhas.

---

## v0.42.1 — «sem pid de controlo» e «sem plano de controlo» deixam de se ler igual

Follow-up directo da v0.42.0, encontrado ao testar o caminho que mais interessa a
quem já tem um nó a correr: um **upgrade in-place sobre um holder pré-split**.

Esse caminho funciona, e foi validado ao vivo com o binário v0.41.0 real a segurar
a netns: o v0.42.0 reconhece o holder antigo como pin, encontra o socket de
controlo alcançável, e **não toca em nada** — holder e container mantêm o PID, a
rede fica intacta, e o binário novo cria containers novos contra ele sem
problema.

O que estava mal era o relato. O `status` mostrava `control pid —`, porque um
holder pré-split não tem ficheiro de pid do controlo — é um só processo a fazer
os dois trabalhos. Só que um plano de controlo genuinamente **morto** mostrava
exactamente a mesma coisa. Dois estados opostos, a mesma linha:

```
ingress UP — pin 98880 · control — · slirp 98900 …   ← tudo bem
ingress UP — pin 100733 · control — · slirp 100755 … ← sem plano de controlo
```

`InfraStatus` ganhou `control_reachable` (decidido por um `connect`, não por um
pidfile nem pela existência de um ficheiro) e o `status` passou a nomear os três
estados:

```
control 100736   → processo de controlo próprio, vivo
control in-pin   → holder pré-split: um só processo faz os dois trabalhos
control DOWN     → não há ninguém a servir o socket
```

É a mesma lição que esta base de código já pagou três vezes — `holder_pid.is_some()`
não é «o holder é alcançável», `container.userns` não é «está num userns
diferente do meu», e um ficheiro de socket não é um listener. Aqui era `None` a
significar duas coisas contrárias.

---

## v0.42.0 — o holder deixa de ser um ponto único de falha da rede

Até aqui, um só processo fazia duas coisas sem relação: **segurava** os
namespaces (userns/netns/mountns) e **corria** o plano de controlo (socket de
controlo, DNS, RA, servidores de DHCP). Como consequência, reiniciar o plano de
controlo — um crash, um `kill`, um upgrade in-place — destruía a netns e
desligava permanentemente todos os workloads do nó.

A v0.41.0 tratou o sintoma (recuperar por reinício). Esta trata a causa.

### O que a medição mostrou

Antes de escrever código, matei o holder com uma VM Cloud Hypervisor viva e fui
ver o que restava:

| | |
|---|---|
| processo da VM | **vivo** |
| netns | **a mesma** (mesmo inode) |
| `delonix0` + o `tap` da VM | **presentes, UP** |
| `tap0` do slirp | **presente**, `10.0.2.100/24`, rota default |
| ruleset `nft` | **intacto**, com a chain de isolamento da VM |
| processo `slirp4netns` | **vivo** |

E entrei nessa netns órfã **sem privilégio nenhum**, através do processo da VM
(`nsenter -t <pid> -U -m -n`).

**Nada disto estava partido por limitação de kernel.** O que matava a rede era o
`ensure_up` seguinte deitar fora uma netns perfeitamente funcional para construir
outra. Isto corrige, de passagem, uma afirmação demasiado larga que estava
registada: o que é impossível em rootless é `ip netns attach` a partir do host
(precisa de CAP_SYS_ADMIN sobre o userns do holder morto) — **entrar** na netns a
partir de um membro vivo é outra operação, e essa funciona.

### A separação

- **`delonix netns pin`** — faz o `unshare` e adormece. Sem sockets, sem threads,
  sem estado. Só morre por `kill` ou com a máquina.
- **`delonix netns control`** — corre lá dentro por `nsenter` e é livre de ir e
  vir: socket de controlo, DNS, Router Advertisements e o DHCP por bridge.

O `ensure_up` passou a ter três casos: pin vivo e controlo alcançável (nada a
fazer); pin vivo e controlo ausente (**repõe só o controlo** — o caso que esta
versão existe para criar); pin morto (reconstrução completa + recuperação por
reinício, exactamente como antes).

O ficheiro de pid do pin mantém o nome histórico de propósito: é o pid que todos
os `nsenter -t <holder>` da árvore visam (`join_argv`, `infra_join_argv`,
`disable_ipv6_live`, …) e agora é o pid que **nunca muda**. Renomeá-lo seria
mexer em todos os consumidores para dizerem o mesmo.

Efeito lateral valioso: o pin não tem comportamento versionado, logo **pin antigo
+ controlo novo é seguro por construção** — a armadilha do upgrade in-place que
custou a v0.34.2 deixa de existir para o caminho normal.

### Três bugs que só a validação ao vivo revelou

**`/sys/class/net` não reflecte a netns do processo.** A primeira sonda de «esta
netns já está construída?» lia `/sys/class/net/delonix0`. O sysfs reporta a netns
de quem o **montou**, e o pin nunca remonta `/sys` — de dentro do controlo aquele
directório continua a ser o do host. Resultado: a sonda dizia «vazia» para uma
netns que tinha bridge, e o controlo morria em `ip link add delonix0: File
exists`. Passou a perguntar por netlink (`ip link show`).

**`capture()` devolve `Ok` mesmo quando o comando falha** — não olha para o exit
status de todo. A segunda versão da sonda usava `.is_ok()` e era portanto
*sempre* verdadeira: numa netns virgem o controlo tomava o caminho de reattach,
não construía nada, e o `net netns up` anunciava `ingress UP` sobre uma netns sem
bridge nenhuma. Agora lê a **saída**, que é o sinal que interessa.

**Um ficheiro de socket sobrevive ao processo que o criou.** `wait_for_control_sock`
era `path.exists()` — a terceira aparição nesta base de código do mesmo erro,
depois do `status()` por pidfile («`holder_pid.is_some()` não é "o holder é
alcançável"») e do `container.userns`. Passou a doer no momento em que o split deu
ao plano de controlo uma forma de morrer sozinho: com o ficheiro órfão a passar, o
`ensure_up` devolvia um alegre `ingress UP` sobre um nó **sem plano de controlo
nenhum** — dataplane bem (é o objectivo do split), mas sem attach, sem publish,
sem DNS, e sem um aviso. Agora faz um `connect` real. A função que tinha ficado
sem chamadores foi **apagada**, não deixada à espera do primeiro.

### Reattach não repete os passos destrutivos

Re-correr a construção sobre uma netns viva não é só desperdício, é destrutivo, e
cada um destes foi verificado e não assumido:

- `mount -t tmpfs none /run` montaria um **segundo** tmpfs por cima, escondendo
  `/run/netns` — ou seja, a netns nomeada de cada pod e de cada container
  `--net <custom>` do nó, inalcançável por nome num instante;
- `ip link add` / `ip addr add` devolvem `File exists` e, sendo propagados com
  `?`, abortariam o arranque do controlo;
- reaplicar o ruleset base reacrescenta as regras de dispatch do `fwcont` a cada
  reinício (o ruleset **funde**-se na tabela existente — não tem `flush`, que é a
  razão de as firewalls dos containers sobreviverem de todo).

No reattach reconstrói-se só o que é **local ao processo**: os servidores de DHCP,
que são threads e morreram com o controlo anterior — o ingress default mais cada
rede privada.

### Validação

Ao vivo, com um **pod**, um **container em rede custom** e uma **VM Cloud
Hypervisor** a correr ao mesmo tempo: `kill -9` no controlo →

```
pin      77461 → 77461      (intocado)
control  77464 → 77722      (reposto)
VM       77610 → 77610      pod 77528 → 77528      container 77573 → 77573
```

Zero reinícios, rede intacta nos três, chains de isolamento preservadas, e o
controlo a aceitar trabalho novo de imediato. Matar o **pin** continua a cair na
reconstrução completa com recuperação por reinício, como antes.

**Cenário de caos novo** (`control_restart`), que compara **PIDs** e não só
conectividade — uma recuperação por reinício também deixaria a rede a funcionar e
seria indistinguível de outra forma. Arnês completo: 17 PASS · 0 FAIL · 0 SKIP.

### Ainda em aberto

Se o **pin** morrer, os workloads continuam a ser recuperados por reinício — a
adopção da netns por um holder novo continua impossível em rootless, e agora é
uma janela muito mais estreita (o pin não faz nada que possa falhar). As **VMs**
continuam fora dessa reconciliação: só o `tap` morre com o pin e nada o repõe.

---

## v0.41.0 — um pod voltava do respawn do holder sem rede, em silêncio

Continuação directa da v0.40.0, que trouxe pods e VMs para dentro do isolamento
de namespace. Trazê-los para dentro tornou visível a pergunta seguinte: e quando
o holder morre e volta?

A resposta, medida antes de escrever código:

```
recovered 1 container(s) stranded by the previous holder (restarted)

pa-c0   Up 32 seconds   →  ping: Network unreachable
```

O container foi recuperado. O pod ficou vivo, sem rede, sem a sua chain de
isolamento, **e sem uma linha a dizê-lo**. Uma recuperação que reporta sucesso
por cima de um workload que acabou de abandonar é pior do que uma que não faz
nada — porque a primeira é lida como "está tratado".

### A raiz: a associação ao pod nunca era persistida

`Container` tem um campo `pod` desde sempre. O `describe` sempre o imprimiu.
**Nada alguma vez lho atribuiu** — o único traço de pertença em disco era uma
label. É a quarta vez que este repositório paga exactamente esta armadilha (o
`-v` não persistido, o `-p` numa rede custom, as redes extra perdidas no
restart), e a regra continua a mesma: *estado necessário para RECONSTRUIR o
recurso tem de ser persistido, não só usado uma vez na criação.*

Duas consequências, ambas reproduzidas:

- `container describe` num membro de pod não mostrava pod nenhum;
- `container restart` de um membro morria com `clone failed: EPERM` e deixava-o
  **`Dead`, sem caminho de volta** — o `cmd_start` reconstrói o spec a partir do
  registo e não tinha como saber que devia re-entrar na netns partilhada.

### O que passou a funcionar

- **`container restart` de um membro de pod** volta a entrar na netns do pod,
  com o mesmo IP, sem tocar nos peers. Se a netns já não existir (o holder foi
  substituído), é recriada — com a namespace do membro, portanto o isolamento
  volta com ela.
- **Um respawn do holder recupera pods** como já recuperava containers.

### Dois bugs apanhados pelo caminho, ambos só visíveis ao vivo

**`reexec_start` ignorava o seu próprio parâmetro `netns`** e usava o `id`.
Funcionava por coincidência: o único chamador passava um netns igual ao id. Um
membro de pod é o primeiro caso em que diferem — com o código antigo teria
tentado entrar numa netns com o nome do container, que não existe. Mesma família
dos ajudantes públicos, mortos e com defeito à espera do primeiro chamador que
este repo já teve de apagar duas vezes. O caminho de falha ganhou também um
`owns_netns`: só se desmonta uma netns que seja nossa — a de um pod é partilhada,
e derrubá-la porque um membro não voltou tiraria a rede aos outros.

**A guarda de idempotência da recuperação estava a saltar membros.** Perguntava,
dentro do ciclo, «o holder serve esta netns?» — e a resposta passa a *sim* assim
que o **primeiro** membro recupera, pelo que todos os seguintes eram tratados
como saudáveis enquanto continuavam dentro da netns morta. Foi a primeira versão
desta correcção, e só um pod de **dois** containers a mostrou:

```
recovered 2 container(s)     pa-c0 → Network unreachable
```

A pergunta passou a ser feita **uma vez, antes do ciclo**. Aí é a pergunta certa:
no início da passagem, ou o holder servia a netns do pod (nada morreu — saltam-se
todos os membros) ou não servia (estão todos encalhados — reiniciam-se todos).
Para um container, cuja netns é só sua, snapshot e consulta ao vivo são
equivalentes.

### Validação

Ao vivo, no sandbox isolado: `restart` de um membro com um peer a correr (o peer
não perde um pacote, o membro volta ao mesmo IP do pod); respawn do holder com um
pod de dois membros + um container em rede custom — **os três recuperados**, os
dois membros no mesmo IP, e o isolamento reconstruído (cross-namespace bloqueado
nos dois sentidos, mesmo-pod aberto).

**Cenário de caos novo** (`pod_holder_respawn`), deliberadamente com **dois**
containers no pod: com um só, a guarda partida ainda passava. Falha com a
correcção revertida (`c0=down c1=down`) e passa com ela. Arnês completo:
16 PASS · 0 FAIL · 0 SKIP.

### Ainda em aberto

A recuperação continua a ser **por reinício**, não por adopção: adoptar a netns
viva para dentro do holder novo é impossível no kernel em rootless (`ip netns
attach` precisa de CAP_SYS_ADMIN sobre o userns do holder que morreu), e isso
está medido e documentado desde a v0.39. VMs continuam fora da reconciliação — o
`tap` morre com o holder e nada o repõe.

---

## v0.40.0 — pods e VMs entram no isolamento de namespace

O isolamento por `--namespace` cobria containers simples. Pods e VMs ficavam de
fora — cada um por uma razão diferente, e a dos pods era a pior das duas.

### Pods estavam meta-ligados, o que é pior que desligados

`pod create` já passava a namespace ao attach, por isso o IP do pod **entrava**
nos sets `@dlxall`/`@dlxns_<ns>`: as chains dos outros workloads já recusavam
ligações vindas dele. O que nunca existiu foi chain **própria** — e as regras de
isolamento vivem na chain de cada workload. Sem chain, nada dropava o tráfego a
**entrar** no pod. Uma fronteira aberta num sentido só é uma fronteira aberta.

Medido antes da correcção, três pods de um container na bridge default:

```
podA(teamA) → podB(teamB)    REACHABLE    ← devia estar bloqueado
podA(teamA) → podA2(teamA)   reachable    ← correcto
```

…com os sets do holder perfeitamente correctos (`@dlxall = {.2,.3,.4}`,
teamA = `{.2,.4}`, teamB = `{.3}`) e o `@fwmap` **vazio**. A metade da
composição estava lá; a metade que aplica, não.

Corrigido no `create_pod`, chaveado pelo **nome da netns do pod** e não pelo id
de um container membro: a netns é que segura o endereço, todos os membros a
partilham, e o verdict map do dataplane é chaveado por IP — uma entrada é tudo
o que caberia de qualquer forma. O teardown já estava coberto (`pod rm` →
`detach_container` → `unfirewall`).

### VMs não tinham namespace — e havia um obstáculo real

O IP de uma VM vem por **DHCP**, portanto no momento do attach ainda não se sabe
qual é. A saída veio de o servidor DHCP ser nosso: `dhcp_serve` corre em Rust
dentro do holder e o lease é **determinístico do MAC**. `infra::dhcp_lease_ip`
calcula-o do lado do host, antes de o guest sequer arrancar — e é isso que
permite registar a membership e instalar a chain no momento certo, em vez de
esperar por um lease que ninguém observa.

Essa aritmética estava **duplicada em dois sítios** (`dhcp_serve` e
`dhcp_ip_for_mac`) e esta sessão quase acrescentou uma terceira. Agora há uma só
função, e os três consumidores — o servidor que entrega o lease, o `vm ls` que o
reporta, e o attach que o isola — passam todos por ela. Duas cópias divergiriam
no dia em que a pool mudasse, e o sintoma seria o pior possível: uma VM com
firewall num endereço que ninguém usa, reportada como isolada.

Novidades de superfície: `vm create --namespace <ns>`, `metadata.namespace` no
`kind: Vm`, e a namespace visível no `vm describe`. `Vm.namespace` é persistido
(registos antigos ficam em `default`, que é exactamente o que eram) e
reconstruído pelo `config_from`, com teste dedicado — a namespace desaparecer no
primeiro `vm start` seria a quarta ocorrência de uma armadilha já documentada
neste repo (`-v` não persistido, `-p` em rede custom, redes extra perdidas no
restart).

### Só o backend `cloud-hypervisor` — e a recusa é explícita

Uma VM libvirt vive na `virbr0`, no netns do **host**: outro L2, que este motor
não programa. `--namespace` nesse backend é **recusado com erro dirigido**, nunca
aceite-e-ignorado. Aceitar uma opção de isolamento e não fazer nada é a armadilha
que este projecto já teve de corrigir três vezes (`--security-opt seccomp=`,
`-v …:z`, `--network-alias`), e num campo de segurança custa mais do que não ter
a opção.

### Compatibilidade de holder

A linha de controlo `vmtap` só cresce para 6 tokens quando há mesmo namespace a
aplicar (`vmtap_line`, pura e testada) — o mesmo idioma que `attach` e
`attach-extra` já usavam. Contra um holder de uma build anterior, uma VM
namespaced falha **alto** (`invalid control command`) em vez de arrancar sem
isolamento em silêncio. Confirmado ao vivo.

### Validação

**Pods**, ao vivo no sandbox isolado: cross-namespace bloqueado nos **dois**
sentidos, same-namespace aberto, gateway intacto, uma chain por pod no `@fwmap`.

**VMs**, ao vivo: `vm create --backend cloud-hypervisor --namespace teamA` real,
chain instalada em `10.200.254.20` — o lease previsto no attach, e o mesmo
endereço que o `vm ls` reporta. Com tráfego verdadeiro contra esse endereço,
através da chain instalada, os contadores do kernel nomeiam a regra que decidiu:

```
ct state established,related          counter packets 1 bytes 84   accept
ip daddr … ip saddr @dlxnse20c4037    counter packets 1 bytes 84   accept   ← same-ns
ip daddr … ip saddr @dlxall ct new    counter packets 4 bytes 336  drop     ← cross-ns
```

**Cenário de caos novo** (`pod_namespace_isolation`), que falha com a correcção
revertida e passa com ela. Arnês completo: 15 PASS · 0 FAIL · 0 SKIP.

**O que não foi provado com um convidado a sério**: nenhuma imagem deste host
arranca em Cloud Hypervisor (a golden é libvirt-only e falta o `hypervisor-fw`),
por isso o alvo no endereço da VM foi um veth real na bridge do holder e não o
guest. O que fica por confirmar é só o troço `tap`→convidado, que é idêntico ao
de qualquer VM CH sem namespace nenhuma; a chain, o endereço e a decisão do
kernel foram exercitados com pacotes verdadeiros.

### Ainda em aberto

O isolamento continua a **não ser reconstruído num respawn do holder** (os
sets/chains voltam vazios e os workloads vivos não se re-atacham sozinhos;
reiniciar cada um repõe). É agora a última limitação conhecida do modelo.

---

# v0.39.3 — SEGURANÇA: o isolamento por namespace estava desligado desde a v0.39.0

**Actualize imediatamente se corre containers de mais do que um inquilino no mesmo
host.** As versões v0.39.0, v0.39.1 e v0.39.2 aplicam `metadata.namespace` /
`--namespace` mas **não instalam a firewall que o faz valer**. Um container de uma
namespace alcança containers de outra.

## O que aconteceu

Tornar o supervisor universal (v0.39.0, para capturar exit codes) fez com que
**todos** os containers desanexados passassem a sair de `cmd_run` mais cedo — antes
do bloco que aplica o isolamento por namespace. Enquanto o supervisor estava
condicionado a `--restart`, isso não tinha efeito prático; deixou de estar.

E **nada falhou**. Uma firewall que nunca é instalada não devolve erro: o tráfego
simplesmente passa. Foi preciso atravessar a fronteira para o descobrir.

Medido, mesmo cenário, dois binários:

```
v0.38.2 (supervisor só com --restart)   teamA → teamB   bloqueado
v0.39.0 (supervisor universal)          teamA → teamB   ALCANÇA
```

## A correcção

O isolamento passou a ser aplicado **antes** do ramo do supervisor, onde o IP já é
conhecido, por isso os dois caminhos o recebem. Verificado nos dois sentidos:
cross-namespace bloqueado, same-namespace aberto — porque um isolamento demasiado
agressivo é outro bug, não uma correcção.

É a **terceira** ocorrência da mesma armadilha estrutural nesta série: estado
necessário depois da criação vivia depois de um `return` antecipado. As duas
anteriores foram `ip`/`network` (v0.39.0) e o rootfs no `rm` (v0.39.1). A lição que
fica no código: quando um `return` novo é acrescentado a `cmd_run`, tudo o que vem
depois dele deixa de acontecer para esse caminho — e nada avisa.

## O que impede a repetição

Cenário novo no arnês de caos, `namespace-isolation`, que **atravessa a fronteira**
em vez de verificar configuração: cross-namespace tem de estar bloqueado, e
same-namespace aberto. Uma firewall ausente falha o cenário.

Bateria: **14 PASS · 0 FAIL · 0 SKIP**.

## Também nesta versão

**O último wedge do holder.** `handle_control` saiu da thread do accept para um
worker dedicado que mantém a serialização da fábrica de netns/veth/nft. Um `nft`/`ip`
preso deixou de pendurar o holder inteiro:

- o accept continua a aceitar, e um chamador recebe `holder busy` ao fim de 60 s em
  vez de nunca receber nada;
- os verbos de leitura (`ping`, `has-netns`, `fwstats`, `egress-show`) continuam a
  ser servidos, por isso o nó permanece observável — e a reconciliação do
  `net netns up` continua a poder perguntar que containers estão servidos.

**Dito com todas as letras**: isto não torna um `handle_control` preso inofensivo. O
worker é único por desenho, logo uma mutação encravada continua a bloquear as
mutações seguintes — que agora falham com erro limitado e diagnosticável em vez de
nunca voltarem. Tornar isso progresso real exige a fábrica ser interrompível, que é
outro trabalho.

## Validação

`clippy --workspace --all-targets` a zero avisos; **622 testes**; arnês 14/14.

---

# v0.39.2 — o instalador morria em servidores headless; e os limites de recurso precisam de cgroup delegado

Dois achados de uma corrida do arnês de caos **dentro de uma VM** criada com o
próprio `delonix vm create` — um host limpo, alcançado por SSH, que é como se chega
a um servidor a sério. Nenhum dos dois aparece num host de desenvolvimento.

---

## 1. O instalador morria em silêncio em qualquer host sem GPU

```sh
GPU_INFO=$(lspci | grep -Ei 'vga|3d controller' | ...)
```

Sob `set -euo pipefail`, um `grep` que **não encontra nada** sai 1, o `pipefail`
propaga-o, a atribuição falha, e o `set -e` mata o instalador — logo a seguir à
linha `preparing the host`, **sem imprimir erro nenhum**.

Acontece em todo o host sem dispositivo VGA: praticamente toda a VM e todo o
servidor headless. Reproduzido numa VM Ubuntu 24.04 limpa, onde o instalador morria
antes de instalar seja o que for.

Uma etiqueta cosmética de GPU nunca pode poder falhar uma instalação.

## 2. Limites de recurso são inertes numa sessão SSH

`-m` / `--cpus` / `--pids-limit` só tomam efeito quando o processo que arranca o
container **possui um cgroup delegado**. Medido na VM, com `-m 128M --cpus 0.5`:

```
cgroup: /user.slice/user-1000.slice/session-40.scope   (partilhado com o sshd)
memory.max=max   cpu.max=max   pids.max=max   memory.swap.max=max
```

**É regra do cgroup v2, não limitação do motor** — o Podman rootless tem
exactamente o mesmo requisito. Um scope de sessão SSH é **irmão** de
`user@<uid>.service`, não filho, e migrar um pid entre os dois exige escrever o
`cgroup.procs` do antepassado comum (`user-<uid>.slice`), que é da root:

```
mkdir  user@1000.service/probe                  → ok
echo $$ > user@1000.service/probe/cgroup.procs  → EACCES
```

Derivar a fronteira do uid em vez de a procurar no caminho **foi tentado e medido a
não funcionar**; o fallback foi apagado em vez de deixar código que só cria um
directório onde nada entra. (O comentário antigo afirmava que essa migração era
permitida. Não é.)

**O isolamento de namespace e seccomp não é afectado** — só os tectos de recurso.

### O que mudou

- **`install.sh` TESTA a delegação de verdade**: cria um cgroup filho e tenta
  activar `+memory`. Ler `cgroup.controllers` não chega — o controlador pode estar
  listado e a migração continuar proibida. Quando falta, imprime o remédio exacto,
  verificado literalmente nesta VM.
- **`loginctl enable-linger`** para o utilizador, pré-requisito de qualquer uso
  não interactivo (cron, CI, unidade de utilizador).
- **Arnês** distingue os dois casos em vez de reportar ambiente como bug de código:
  `aggregate-ceiling` reconhece um `session-*.scope` e faz SKIP com o remédio;
  o novo `delegated-scope` **verifica** que sob um scope delegado os cinco limites
  (`memory.max`, `cpu.max`, `pids.max`, `memory.swap.max`, `memory.oom.group`)
  aplicam mesmo.
- **README** ganhou a secção *Resource limits need a delegated cgroup*, e a linha
  «Daemon» da tabela comparativa foi corrigida: desde a v0.39.0 há um supervisor
  curto por container (o modelo do `conmon`), que é o que torna o exit code de um
  container desanexado conhecível. Dizer «nenhum monitor por container» passou a ser
  falso.

O remédio, verificado::

```
systemd-run --user --scope -p Delegate=yes -- \
  delonix container run -d --name t -m 128M alpine sleep 60
# memory.max = 134217728, não "max"
```

---

## Validação

`clippy --workspace --all-targets` a zero avisos; **621 testes**, todos verdes.
Arnês: **VM 12 PASS · 0 FAIL · 1 SKIP** (o SKIP é o relato honesto do ambiente),
**host 13 PASS · 0 FAIL · 0 SKIP**. O instalador corrigido foi corrido de ponta a
ponta numa VM Ubuntu 24.04 limpa, e o conselho que imprime foi seguido à letra e
confirmado.

---

# v0.39.1 — o `rm` deixava o rootfs inteiro em disco

**Actualização recomendada a todos.** Uma fuga de disco silenciosa, encontrada pelo
arnês de caos e não por nenhum teste, mais quatro cenários novos de caos — um dos
quais prova ao vivo, pela primeira vez, uma correcção anterior.

---

## A fuga

Remover 30 containers devolvia **zero bytes** ao disco. Medido: 39 MiB por
`redis:7-alpine`, ~1,2 GiB por 30 containers — com o `rm` a reportar **sucesso** e
o registo já apagado, por isso nada, em lado nenhum, apontava para o órfão.

É exactamente a classe do incidente que já taxou um nó deste host com
`disk-pressure` (49 rootfs órfãos, ~45 GiB).

**Pré-existente**, não regressão da v0.39.0: confirmado reproduzindo com o binário
publicado da v0.38.2, com a mesma configuração. *(Uma primeira análise concluiu o
contrário porque comparou a v0.38.2 com `--net none` contra a build local com rede
custom — configurações diferentes não comparam nada.)*

**Causa-raiz: EACCES.** A árvore de um container rootless tem directórios que a
extracção deixou só-leitura e ficheiros escritos dentro de um userns mapeado como
SUBUIDs; o uid chamador não consegue desligar através deles. E o erro do
`remove_dir_all` era descartado com `let _ =`, por isso a falha era invisível.

**O remédio já existia dos dois lados e nunca tinha sido ligado.**
`ImageStore::container_path` foi acrescentado — o doc-comment dele diz literalmente
*"so `rm` can remove it in a mapped userns (subuid files) via the runtime"* — e
`remove_tree_mapped` é o helper que o faz, já usado pelo `volume rm` e pelo `system
prune`. O `container_path` tinha **zero chamadores**: a mesma armadilha do "API
pública à espera do primeiro chamador" que esta base de código já pagou três vezes
(`mount_live`, `set_net_rate`, `update_limits`) — e aqui custou uma fuga de disco.

Novo `purge_container_dir`: remoção simples primeiro (barata, sem fork, e o
suficiente quando não há subuids), re-exec mapeado só como recurso, e aviso
**apenas se ambos falharem**. O `remove_container_dir` deixou de avisar por conta
própria — anunciava uma fuga que estava prestes a ser limpa, que é o mesmo relato
desonesto a apontar para o outro lado.

## Arnês de caos: 7 → 12 cenários

- **`holder-wedge`** — a **primeira prova ao vivo** da correcção do wedge do socket
  de controlo da v0.38.1. Até aqui só tinha teste unitário, porque não havia um
  holder seguro para prender. Um par liga-se ao socket, nunca escreve, e o attach
  seguinte é servido em 5s em vez de nunca.
- **`slirp-kill`** — matar o slirp não pode deixar o motor a afirmar saúde que não
  tem.
- **`scale`** — 30 containers em paralelo: IPs únicos, tecto agregado de pé, e o
  disco devolvido na limpeza. Foi este que apanhou a fuga.
- **`write-failure`** — store só-leitura a meio: falhar alto, zero temporários
  órfãos.

**Resultado: 12 PASS · 0 FAIL · 0 SKIP** (o `scale-cleanup` passou de 1168 MiB
retidos para 0).

### Dois bugs do próprio arnês, corrigidos

1. O `scale-cleanup` media `df` do **filesystem inteiro** — partilhado com a
   produção e com os builds — em vez de `du` do sandbox. O número da "fuga" incluía
   o que outros processos escreviam ao mesmo tempo.
2. Vários cenários assumiam o holder de pé. A infra é ref-contada: remover o último
   container fá-la descer, **por desenho**. Isso fez o `holder-wedge` fazer skip em
   silêncio na bateria completa enquanto passava isolado — um bug de arnês
   disfarçado de socket em falta.

---

## Validação

`clippy --workspace --all-targets` a zero avisos; **621 testes**, todos verdes;
arnês 12/12. Ao vivo: `rm` de um container em rede custom passa de 39 MiB retidos
a 4 KiB, sem aviso nenhum porque já não há nada a avisar.

---

# v0.39.0 — o motor a ser partido de propósito: recuperação, exit codes e um arnês de caos

Cinco frentes fechadas, e a mais valiosa não é código: é um **arnês de caos** que
parte o motor a sério, isolado da produção. Foi ele que apanhou a única regressão
desta série — e um bug pré-existente que nenhuma das auditorias anteriores viu,
porque todas leem código.

---

## 1. Containers largados por um respawn do holder

Medido ao vivo num sandbox isolado:

| | comportamento |
|---|---|
| holder **morre** | containers mantêm L3 (o netns sobrevive porque o `slirp4netns` ainda o referencia); perdem só DNS e plano de controlo |
| holder **volta** | netns NOVO, o antigo morre com todos os veths — `Network unreachable` **para sempre** |

O dano nunca foi o holder morrer: foi o holder voltar. Um upgrade in-place, que é
a forma normal de actualizar isto, cai exactamente aí.

**A reparação óbvia é impossível, e é regra do kernel — não feature em falta.**
Ambas as vias foram implementadas e testadas ao vivo antes de serem descartadas:

* adoptar o netns vivo (`ip netns attach <nome> <pid>`) →
  `Bind /proc/<pid>/ns/net -> /run/netns/<n>: Permission denied`. O bind de um
  ficheiro de namespace exige `CAP_SYS_ADMIN` sobre o userns que o **possui**, e
  o netns do container pertence ao userns do holder **morto**;
* fixar (pin) os namespaces à partida → falha ainda antes: o bind tem de aterrar
  num caminho visível ao host, e isso exige privilégio no mount namespace do
  host, que é precisamente o que o rootless não tem.

Em rootless, os namespaces do holder não podem sobreviver ao processo. O código de
adopção foi **apagado**, não deixado a apodrecer.

**O que fica** é o que o kernel permite: `net netns up` deteta exactamente quais
os containers largados (verbo `has-netns`) e recupera-os por restart — a única via
que reconstrói o netns correctamente. `DELONIX_NO_AUTO_RECOVER=1` reporta com o
comando exacto e não toca, para quem quer escolher quando uma base de dados
reinicia.

## 2. O exit code de um container `-d` passa a ser sempre capturável

`waitpid` é a única fonte de um estado real e o kernel só o dá ao pai. Sem pai
duradouro, `ps -a` só sabia dizer `Exited (unknown)`.

O supervisor **já existia** — estava condicionado a uma política de restart. Sem
política ele faz uma coisa só: regista o código real, emite `die`, sai.

**Não atravessa a linha do daemonless.** Daemonless aqui é *sem daemon central*.
Um supervisor por container é o desenho daemonless padrão — o Podman é daemonless
e corre um `conmon` por container por esta exacta razão. Este motor já mantém
processos persistentes por-nó e já fazia fork deste mesmo supervisor.

`serve docker-api` opta por fora: é multi-thread e o `fork()` assume chamador
single-threaded — a mesma razão pela qual já recusava `--restart`.

## 3. Tecto absoluto de I/O

`--device-read-bps`/`--device-write-bps`/`--device-read-iops`/`--device-write-iops`
por-container, com paridade Docker/Podman. A sintaxe `<device>:<rate>` é aceite e
o device **ignorado**: as escritas de um container só chegam ao disco do store, e
aceitar outro seria aceitar uma instrução que o motor não pode honrar.

`<iotune>` no disco raiz das VMs (`DELONIX_VM_IO_MAX_BPS`/`_IOPS`), **opt-in** —
ao contrário da memória não há valor "generoso" seguro; depende do dispositivo.

Nota medida: em rootless o systemd delega `cpu memory pids` e **não** `io`, por
isso nenhum motor sem privilégio — Podman incluído — impõe `io.max` ali.

## 4. Auditoria E2E: a dívida era de documentação

O `AGENTS.md` dizia «27 dos 35 achados continuam em aberto». **Não continuavam.**
Re-triados um a um contra o código actual, praticamente todos já estavam fechados,
cada um com o seu comentário e teste de regressão. O `AUDITORIA-E2E.md` é que
nunca foi actualizado à medida que as correcções entravam.

Único resíduo real: `SecretStore::save` não fazia `fsync` e criava o ficheiro sob
o umask antes de o apertar para `0600` — a mesma janela TOCTOU que o kubeconfig já
tinha fechado. Novo `write_atomic_mode` define o modo **atomicamente na criação**.

*Uma tabela de achados que não é actualizada passa a mentir nos dois sentidos: fez
27 problemas resolvidos parecerem dívida viva durante semanas.*

## 5. Arnês de caos (`scripts/chaos.sh`)

7 cenários — `holder-kill`, `idempotent-up`, `oom`, `concurrent-attach`,
`abrupt-kill`, `aggregate-ceiling`, `disk-full` — contra um motor **totalmente
isolado**: `DELONIX_ROOT` + `DELONIX_NET_RUNTIME_DIR` redirigem ambas as raízes,
e o sandbox corre o SEU holder ao lado do de produção sem nenhum dar por isso
(verificado ao vivo, com 4 containers de produção de pé durante toda a bateria).

**Resultado: 7 PASS · 0 FAIL · 0 SKIP.**

### O que ele encontrou, e nenhum teste tinha encontrado

Um bug **pré-existente**: o bloco que persiste `network`/`ip` vive depois do
return do supervisor, por isso o caminho supervisionado nunca lá chegava. Medido
num `--restart always --net <rede>`, antes desta série tocar no supervisor:

```
ip persistido: None   network: None
```

…com o container a ter endereço a funcionar na SDN. Mesma família do bug do `-v`
não persistido: um `start` depois de um `stop` não re-atacha uma rede de que não
tem registo, o DNS não tem endereço, a firewall não tem IP para governar.

### E dois bugs do próprio arnês, ambos instrutivos

1. O gateway estava fixo em `10.254.0.1`. Não é constante — cada rede recebe o seu
   `/16` do alocador — por isso todos os cenários faziam skip com "a rede não
   subiu" contra uma rede perfeitamente funcional.
2. O cenário de OOM enchia `/dev/shm` com `dd` e reportava **falha contra um motor
   correcto**: o `/dev/shm` de um container é ele próprio limitado, logo o `dd`
   batia em ENOSPC muito antes do `memory.max`.

*Um teste que falha pela razão errada é tão perigoso como um que passa pela razão
errada.*

---

## Validação

`clippy --workspace --all-targets` a zero avisos; **621 testes**, todos verdes;
arnês de caos 7/7. Ao vivo: exit codes (`Exited (42)`, `wait` → 42), recuperação
de containers largados, rede custom com supervisor, persistência de `ip`/`network`
nos dois caminhos.

**Por provar ao vivo**: o locale do `virsh` (este host tem zero catálogos de l10n
do libvirt instalados — provado o mecanismo, não o sintoma) e os tectos de VM
(exigem arrancar uma VM real; as correcções de XML são função pura coberta por
teste).

## Migração

- Um `run -d` passa a deixar um processo supervisor por container, que morre com
  ele. É o modelo do Podman/conmon.
- `net netns up` passa a **reiniciar** containers largados por um holder anterior.
  `DELONIX_NO_AUTO_RECOVER=1` mantém o comportamento anterior (reportar e não
  tocar).

---

# v0.38.2 — contenção: o host deixa de poder ser derrubado pelas suas próprias cargas

**Actualização recomendada a quem corre mais do que um workload por host.** Esta versão
fecha seis lacunas encontradas numa avaliação dirigida ao uso do kernel — cgroups,
namespaces e a integração com o libvirt — com uma pergunta só: *o que impede uma carga
de levar o host?*

A resposta, antes desta versão, era «em rootless, nada». Todas as lacunas foram medidas
no host real antes de corrigidas, e cada correcção foi revertida com o teste a disparar
antes de ser reposta.

---

## 1. O `virsh` era invocado com o locale do utilizador

`virsh` é um programa gettext — confirmado: o binário exporta
`bindtextdomain`/`dcgettext` e carrega `"shut off"` como msgid traduzível. E este crate
decidia se um domínio estava vivo comparando o output contra **literais ingleses**:

```rust
libvirt_poweroff:            state == "shut off"
LibvirtBackend::is_running:       s == "running"
```

Num host com os catálogos de l10n do libvirt instalados e `LANG=pt_PT` — um host de
produção angolano ou português perfeitamente normal, ou seja o mercado deste produto —
o `virsh domstate` responde em português e as **duas** comparações passam a falso em
silêncio. Uma VM a correr reporta-se parada (`vm ls` mente, o `wait_for_boot` nunca
converge) e o `libvirt_poweroff` dispara `destroy` num domínio já desligado, que é
exactamente a falha de stderr cru que a v0.11 corrigiu pelo outro lado.

**Corrigido** com `stable_cmd`, que fixa `LC_ALL=C`/`LANG=C` em toda a ferramenta cujo
output este crate parseia — `virsh`, `qemu-img`, e as que vierem. O locale fica pinado
na camada certa: torna a saída da ferramenta uma interface de **máquina**, em vez de
ensinar cada call site a reconhecer N traduções.

Não foi reproduzido ao vivo: este host tem **zero** catálogos de libvirt instalados. Está
provado o mecanismo, não o sintoma.

## 2. Em rootless não havia tecto agregado nenhum

`ensure_delonix_slice()` — a função que dimensiona o tecto agregado a partir do host —
só era alcançada **depois** do `return` do caminho delegado, e escreve em
`/sys/fs/cgroup/delonix.slice`, que é root-only. Ou seja, no modo **rootless, o modo
bandeira do motor**, não existia tecto agregado de todo.

Medido ao vivo antes da correcção, no pai de todos os containers rootless deste host:

```
dlx-containers:  memory.max=max   pids.max=max   cpu.max=max
```

Limites por-container não substituem isto: N containers a M somam N×M, e o `-m` tem
default `max` — na prática, N containers sem limite nenhum debaixo de um pai sem limite
nenhum. Uma fuga levava o host, que é precisamente o que o comentário do próprio
`setup_cgroup` diz que a delegação existe para impedir.

**Corrigido** com `apply_aggregate_ceiling`, aplicado **só** à base que o Delonix cria
para si (`<user@uid.service>/dlx-containers`) — nunca a um cgroup herdado, porque limitar
o scope do chamador estrangularia em silêncio o editor ou a shell dele. Medido ao vivo
depois, num host de 30,5 GiB e 32 cores com a reserva por omissão de 85 %:

```
dlx-containers:  memory.max=27878420480 (25,9 GiB)   cpu.max=2720000/100000 (27,2 cores)
                 pids.max=131072                     memory.swap.max=0
```

## 3. `memory.max` não limitava a memória; e o OOM matava meio container

Duas escritas em falta na leaf delegada, ambas presentes há muito no caminho root:

- **`memory.swap.max`** — sem ela, um container que bate no `memory.max` **faz swap** em
  vez de ser reclamado. O tecto que o operador pediu valia só para a memória residente.
  Medido: `memory.swap.max = max` numa leaf de um container arrancado com `-m 256M`.
- **`memory.oom.group`** — com o default `0` do kernel, o OOM mata **um** processo do
  cgroup. Para um container o cgroup **é** a unidade de falha: matar um filho do pid 1 e
  deixar o resto vivo produz um container meio-morto que continua a reportar-se Running.
  `1` torna a morte atómica sobre o cgroup inteiro, que é o que o runc e o systemd fazem.

Escapatória: `DELONIX_SWAP_MAX=max` repõe o comportamento antigo para quem meça o seu
workload e queira a pista de aterragem.

## 4. As VMs tinham alocação, não contenção

O XML gerado tinha `<memory>`, `<vcpu>` e (opcional) pinning. Nada disso é um tecto que o
**host** imponha:

- **`<memtune><hard_limit>`** — `<memory>` dimensiona a visão do *guest*. O RSS real do
  QEMU é isso **mais** device models, buffers de vídeo/migração e o heap dele, e uma fuga
  empurra-o arbitrariamente longe sem nada a travar. É a mesma falha que o caminho de
  container fecha com `memory.max`.
- **`<cputune><period>/<quota>`** — `<vcpu>N` limita as *threads de vCPU* a N cores, mas
  as threads de emulador e de I/O do QEMU correm fora dessa conta e ficavam sem tecto.

**Corrigido**, com margens deliberadamente **generosas**: a documentação do próprio
libvirt avisa que um `hard_limit` apertado leva o host a matar o domínio, e uma VM que
morre ao calhas é pior que uma VM meramente ilimitada. O tecto de memória é
`guest + max(1 GiB, 25 %)`; o de CPU é `(vcpus + 1) × período` — o core extra não é folga,
é requisito: exactamente `vcpus × período` faria as vCPUs e o emulador competir pelo mesmo
orçamento, e um VM com todas as vCPUs ocupadas esfomearia a sua própria thread de I/O,
num precipício de desempenho que se lê como problema de disco.

Tudo afinável: `DELONIX_VM_MEM_OVERHEAD_PCT`, `DELONIX_VM_CPU_QUOTA_CORES` (fraccionário —
é assim que se dá a um tenant 8 vCPUs para paralelismo mas só 2 cores de débito), e `off`
em qualquer um para desligar.

## 5. O default de `-m` fica em `max` — e passa a avisar quando isso é perigoso

O default **não** mudou: `max` é paridade com o Docker e é o que os utilizadores esperam;
impor um tecto por-container em silêncio surpreenderia todos os workloads existentes. O
que o tornava perigoso nunca foi o default — foi não haver tecto agregado por baixo dele.

Com o ponto 2 no lugar, o caminho comum está protegido. O que resta é tornar **visíveis**
as configurações onde ainda não há protecção nenhuma (um scope `Delegate=yes` que não
criámos, ou o caminho best-effort sem delegação): um aviso único por processo, com os dois
comandos exactos de correcção. Silenciável com `DELONIX_NO_CGROUP_WARN`.

## 6. Leafs de cgroup órfãs

`try_delegated_base` cria `<base>/dlx-<id>` **antes** de saber se a base é delegável, e
todos os `return false` a seguir deixavam o cgroup vazio para trás, para sempre. Medido ao
vivo: seis directórios `dlx-*` órfãos sob um scope do systemd onde a delegação tinha
falhado — um por cada container alguma vez tentado ali. Não são inócuos: fazem qualquer
ferramenta baseada em `cgroup.procs` ver containers que não existem.

---

## Validação

`cargo clippy --workspace --all-targets` a zero avisos; **613 testes** (+9), todos verdes.

Ao vivo neste host: um container real confirmou `memory.swap.max=0`, `memory.oom.group=1`,
`memory.max`/`cpu.max`/`pids.max` aplicados na leaf, e o tecto agregado no pai com a
aritmética a bater com os 32 cores e 30,5 GiB reais (CPU e pids exactos). A leaf é removida
com o container.

**Por provar ao vivo**: o ponto 1 (sem catálogos de l10n instalados neste host) e o ponto 4
(exige arrancar uma VM real, e as correcções de XML são função pura coberta por teste).

## Nota de migração

- Containers passam a correr **sem swap** por omissão. Um workload que dependia de swap
  para sobreviver a picos passa a levar OOM no limite que declarou — que é o
  comportamento correcto, mas é uma mudança. `DELONIX_SWAP_MAX=max` repõe o anterior.
- VMs passam a ter tecto de memória e de CPU. As margens são generosas, mas um domínio
  afinado ao milímetro pode precisar de `DELONIX_VM_MEM_OVERHEAD_PCT` mais alto.

---

# v0.38.1 — durabilidade, medição honesta de disco, e o holder que já não pendura

**Actualização recomendada a todos.** Seis correcções encontradas numa avaliação
dirigida às quatro áreas de carga, memória, volumes e redes. Nenhuma é uma feature
nova: são todas casos em que o motor fazia algo diferente do que a sua própria
documentação afirmava, e três delas afectam decisões que um operador toma a partir
destes números.

Todas foram corrigidas com o teste a falhar PRIMEIRO — cada correcção foi revertida
e o teste correspondente disparou, antes de ser reposta.

---

## 1. O holder podia ficar preso para sempre num par que não escreve

`control_loop` serve **uma ligação de cada vez** (deliberado — é a fábrica de
netns/veth/nft e essas operações não podem intercalar-se) e fazia `read_line`
**sem prazo nenhum**. Um cliente que ligasse ao socket de controlo e não
completasse a linha bloqueava o holder para sempre, e com ele o plano de controlo
de **todos** os containers do nó: sem attach, sem detach, sem publish, sem
firewall, sem `cni-add`. Nada o recuperava — não havia timeout para expirar nem
segunda thread para progredir.

Não é preciso um par malicioso (o `SO_PEERCRED` já restringe ao uid do próprio
motor). O gatilho realista é banal: o `control_query` faz `connect` e só depois
`write`, por isso qualquer CLI descalendarizado, `SIGSTOP`ado ou estrangulado por
OOM nessa janela tranca o nó.

**Corrigido** com `CONTROL_IO_TIMEOUT` (5 s) na leitura E na escrita da resposta —
a mesma classe de bloqueio para a qual o `recv_fd` já tinha ganho um `SO_RCVTIMEO`,
e que o holder simplesmente nunca recebeu.

**Residual, deliberadamente em aberto**: um `handle_control` que ele próprio
pendure (um `nft`/`ip` bloqueado num lock de netlink) ainda pára o ciclo. Fechar
isso exige tirar o dispatch desta thread, o que quebra a serialização de que a
fábrica depende — é uma mudança com desenho próprio, não um timeout.

## 2. Fuga de threads sem limite quando o I/O pendura mesmo

A colheita cara de métricas (rede + disco) tinha um tecto de 120 s e deixava a
thread presa de propósito — o Rust não cancela uma thread bloqueada numa syscall.
O que passou despercebido é que o chamador é um **ciclo periódico infinito**: 120 s
de prazo + 30 s de espera, ou seja **uma thread vazada a cada ~150 s**, ~576 por
dia, todas paradas na mesma syscall, a crescer sem limite até o processo ficar sem
threads.

A nota original argumentava que «vazar mais uma thread por tentativa é o problema
menor». Isso vale para uma paragem transitória; não vale para uma **permanente** —
e permanente é o caso realista: o gatilho documentado é uma montagem NFS que não
responde, e volumes NFS/CIFS/WebDAV são uma funcionalidade de primeira classe deste
motor. Um NAS que desaparece é um evento operacional normal.

**Corrigido** com um disjuntor (`run_bounded` + `InFlight`): enquanto o worker
anterior continuar preso, nenhum novo é lançado. A fuga fica limitada a
**exactamente uma thread**, dure o bloqueio o que durar, e no momento em que a
operação desbloquear a colheita retoma sozinha. A latch é limpa por `Drop`, para
que um pânico no worker não a deixe presa para sempre — o que trocaria uma fuga de
threads por uma métrica morta.

`collect_with_timeout` passou a devolver `Bounded<T>` (`Done`/`TimedOut`/`Skipped`).
A distinção importa: dizer «não terminou em 120 s» quando nada chegou a ser
tentado seria mentira.

## 3. A medição de disco não era `du`, e é ela que decide quotas

Três cópias privadas da mesma caminhada de directórios (`delonix-volume`,
`delonix-mgmt`, `cmd/system.rs`) somavam o **tamanho aparente** (`m.len()`) e nunca
deduplicavam hardlinks — enquanto se descreviam a si próprias como `du`. Dois erros
independentes, em direcções opostas, sobre o número que **É** a quota em rootless (a
única imposição que o modo rootless tem, já que o `losetup` precisa de
`CAP_SYS_ADMIN`):

- **Hardlinks contados N vezes** — uma árvore com ligação pesada (caches de pacotes,
  `node_modules`, layers OCI deduplicadas) sobre-reporta, e o volume estoura a quota
  a segurar muito menos do que parece.
- **Tamanho aparente, não blocos** — ficheiros esparsos contam pelo comprimento
  nominal, incluindo a imagem de quota que este próprio crate cria com
  `truncate -s <quota>`: um volume **vazio** com quota de 100 GB reportava 100 GB
  usados.

Medido ao vivo neste host, por área:

| área | walk antigo | real (`du`) | erro |
|---|---:|---:|---:|
| `layers` | 28 929 220 KiB | 31 352 956 KiB | **−7,7 %** |
| `containers` | 30 298 438 KiB | 33 524 288 KiB | **−9,6 %** |
| `volumes` | 1 983 186 KiB | 2 068 748 KiB | −4,1 % |

**5,7 GiB sub-reportados** no total das cinco áreas — num host cujo incidente
documentado foi precisamente o kubelet a aplicar `disk-pressure`.

**Corrigido** com blocos alocados (`st_blocks × 512`) e deduplicação por
`(dev, ino)`. Só inodes com `nlink > 1` são memorizados, para que o pico de memória
seja proporcional ao número de hardlinks e não à contagem de ficheiros.

As **três cópias foram consolidadas numa só**: `delonix-mgmt` e `cmd/system.rs`
delegam agora em `delonix_volume::measure`. A nota antiga justificava a duplicação
com «o `-mgmt` não pode depender do crate `-bin`» — verdade, mas ao lado da questão:
a caminhada correcta vive no `delonix-volume`, de que os dois já dependiam e que
exporta `measure` exactamente para isto.

Blocos também põem os dois modelos de quota de acordo: o tecto duro é uma imagem
ext4, e o ext4 dá ENOSPC sobre blocos alocados, não sobre bytes aparentes.

## 4. O tráfego de um container multi-homed contava só metade

`container_net_bytes` lia `/sys/class/net/**eth0**/statistics/*` — uma interface
fixa. Um container ligado a uma segunda rede por `container update --net-connect`
(funcionalidade de primeira classe) leva-a em `eth1`, e **todos** esses bytes eram
invisíveis.

Pior do que invisíveis: a função devolvia `Some` na mesma, por isso o colector
contava o container como medido com sucesso e `network_unmeasured_containers` ficava
a zero. A gauge saía **falsamente completa** — exactamente o modo de falha que esse
campo tinha sido criado para evitar no caso `--net host/none`.

É o mesmo ponto cego que a firewall e o isolamento de namespace já tiveram (a lição
do «IP primário» no `AGENTS.md`), aplicado desta vez às métricas.

**Corrigido** lendo `/proc/net/dev` e somando todas as interfaces excepto `lo`
(tráfego de loopback não é uso de rede — neste host mede 487 GB contra 13 GB da
interface real). Sai mais barato de caminho: **um** `nsenter`+`cat` por container em
vez de dois, e isto corre por container a correr em cada colheita cara.

## 5. Nenhuma escrita de estado era durável

Todos os stores do workspace escreviam com temp + `rename` e chamavam-lhe «atomic
write». É meia verdade, e a metade que falta é a que importa depois de uma quebra de
energia: o `rename(2)` é atómico face a *leitores* concorrentes, mas publica uma
entrada de directório que pode apontar para um ficheiro cujo **conteúdo** o kernel
ainda não escreveu. `grep -rn 'sync_all|fsync'` sobre os nove crates devolvia
exactamente um resultado, e era a constante `SYS_fsync` da allowlist do seccomp.

O pior caso é o registo de leases do IPAM: perdê-lo leva o alocador de volta ao hash
puro, que o doc do próprio módulo mede como colidindo com ~50 % de probabilidade aos
~300 containers — dois containers no mesmo IP, com as regras de firewall e DNAT
indexadas no errado.

**Corrigido** com `delonix_runtime_core::write_atomic` (temp → `fsync` → `rename` →
`fsync` do directório), partilhado pelos quatro sítios. Provado ao vivo com `strace`
sobre uma escrita real:

```
openat(".alvo.json.<pid>.0.tmp", O_WRONLY|O_CREAT|O_TRUNC) = 3
write(3, ...)                                              = 33
fsync(3)                                                   = 0   ← o conteúdo
rename(tmp → alvo.json)                                    = 0
fsync(3)                                                   = 0   ← o directório
```

**Apanhado pelo caminho**: o `meta.json` dos volumes não era sequer atómico — eram
três `fs::write` crus, que **truncam** e só depois escrevem. Um crash a meio deixava
um `meta.json` truncado, que não desserializa; o `list()` salta em silêncio todo o
volume cuja metadata não parseia e o `inspect()` responde `NotFound`. O volume
desaparecia de `volumes ls`, do `system df` e das verificações de quota — com cada
byte dos seus dados ainda em disco, e o nome livre para o `create` seguinte o
entregar a outro inquilino. É exactamente a forma da fuga cross-tenant que este crate
já corrigiu uma vez, vinda do outro lado.

## 6. Três correcções menores da mesma família

- **IPAM: o lock falhava ABERTO.** `IpamLock::acquire` era infalível — em caso de
  falha do `open` devolvia um lock com fd `-1` e os chamadores (`let _lock = …`)
  seguiam **sem lock nenhum**, a correr precisamente o read-modify-write
  dessincronizado que o módulo existe para impedir. Passou a `Option`, e os três
  pontos (`allocate`/`reserve`/`release`) recusam a operação. O erro nomeia a
  consequência — «pode entregar o mesmo IP a dois containers» — e não só a falha.

- **Overflow silencioso no cálculo da quota.** `used * 100 >= q * pct` transborda
  `u64` dos dois lados; `q * pct` a partir de ~182 PB com os 90 % por omissão — e
  `parse_size_bytes` aceita explicitamente `1024t` (1,15 EB), com teste a afirmá-lo.
  O `[profile.release]` não activa `overflow-checks`, por isso em release a
  multiplicação **envolvia em silêncio** e o veredicto de alerta saía arbitrário; em
  debug entrava em pânico. Reescrito em `u128`. O `alert_pct` passou também a ser
  limitado a 100: é um `u8`, e nada impedia um 200 — que significava «avisa ao dobro
  da quota», ou seja um aviso que só dispara depois do limite já estourado.

- **`Store::load` construía um `Container` inteiro por registo.** Uma procura por
  nome ou prefixo desserializava mounts, env, labels, portas e regras de firewall de
  **todos** os registos só para comparar duas strings, e deitava tudo fora menos um.
  Passou a parsear apenas `{id, name, created_unix}` na varredura e a desserializar
  só o vencedor. A ordenação não muda: mais recente primeiro, primeiro match ganha.
  **Deliberadamente não corrigido**: a varredura O(N) em si — torná-la O(1) exige um
  índice nome→id, isto é, um segundo estado persistente a manter sincronizado entre N
  processos concorrentes, o que num motor daemonless é pior falha do que uma
  varredura linear.

---

## Validação

`cargo clippy --workspace --all-targets` a zero avisos; **604 testes** (+12 novos),
todos verdes. Ao vivo neste host: `system df` conferido contra `du` real por área
(tabela acima), colheita de rede real (487 MiB rx / 753 MiB tx dos containers vivos),
e a ordem `fsync`→`rename`→`fsync` confirmada ao nível de syscall.

**O que NÃO ficou provado ao vivo, e porquê:**

- **Achado 1 (holder)** — o `control_loop` corre dentro do processo do holder, e este
  host tem 4 containers vivos com o holder UP (refcount 5). Respawná-lo derrubaria a
  SDN de todos. Provado por teste unitário e leitura; **só toma efeito num respawn do
  holder**.
- **Achado 4, a metade do multi-homing** — os containers vivos deste host são todos
  single-homed (confirmado: só `eth0`). Provar o `eth1` exigiria criar uma rede e um
  `--net-connect` num host com carga real. O teste unitário usa o formato verbatim de
  um `/proc/net/dev` real; ao vivo está provada a ausência de regressão no caminho
  comum.
- **Durabilidade contra perda de energia** não é testável em unidade. O `strace` prova
  a ordem das syscalls; a garantia que daí decorre é do kernel.

## Nota para quem consome as métricas

Os números de `system df`, do `dash` e das gauges `delonix_storage_bytes_*` **vão
mudar** com esta versão — passam a reflectir o consumo real de disco em vez do
tamanho aparente sem deduplicação. Um salto nos gráficos ao actualizar é esperado e é
a correcção, não uma regressão.

---

# v0.38.0 — Universal Runtime: kind: Workload, snapshots de VM, e uma API `-o json`

A maior release de superfície desde o início do **Runtime Abstraction Layer**. Um único
objecto declarativo passa a descrever os quatro tipos de computação, o motor de VMs ganha
snapshots de sistema de 1.ª classe, e **todos** os comandos de listagem ganham saída JSON
máquina-legível — a fundação para GitOps/CI/observabilidade por cima do runtime. Inclui
também o endurecimento de segurança do caminho de dados IPv6 (antes marcado v0.37.1, nunca
lançado em separado).

Cada peça foi validada ao vivo num host real, não só com `cargo test` — e as limitações
conhecidas estão incluídas, como sempre.

## `kind: Workload` — um objecto para os 4 tipos de computação (ADR-0001, ADR-0006)

O começo do Universal Runtime: `kind: Workload` + `spec.type: container | vm | pod | microvm`,
com um bloco nomeado pelo tipo (`spec.container`/`spec.vm`/`spec.pod`/`spec.microvm`) que é
**exactamente** a `ContainerSpec`/`VmSpec`/`PodSpec` do Kind autónomo — não redefine um único
campo, logo não pode divergir.

```yaml
apiVersion: delonix.io/v1
kind: Workload
metadata: { name: web }
spec:
  type: container          # container | vm | pod | microvm
  container:
    image: nginx:alpine
    ports: ["8080:80"]
```

- **Açúcar que baixa no `manifest::load`** — um `kind: Workload` é reescrito num
  `kind: Container`/`Vm`/`Pod` sintético (herda `metadata`) e segue o apply por-Kind normal,
  tal como um filho de `kind: Stack`. `apply`/`stack apply`/`--dry-run`/`ls`/`describe` e o
  `apply -f` por-Kind vêem o filho **sem wiring novo**.
- **`type: microvm`** (ADR-0006) baixa para `kind: Vm` com o **backend forçado a
  `cloud-hypervisor`** (o VMM de microVM). Um bloco que peça outro backend (`backend: libvirt`)
  é contradição → erro dirigido. Precisa de CH instalado + imagem CH-bootável (não o golden k8s,
  que é libvirt-only).
- **Fail-closed** em todo o lado: cada tipo tem de trazer exactamente o seu bloco (os outros
  ausentes), tipo desconhecido/em falta → erro claro. Já não há tipos reservados.

Ver `examples/workload.yaml`.

## `delonix workload {ls,describe,stop,rm}` — day-2 unificado (ADR-0002)

O lado imperativo da unificação (a criação é declarativa, via `kind: Workload`). Um trait
`ComputeDriver` com adaptadores para os motores de container e VM:

- **`workload ls`** mostra containers **E** VMs numa só tabela (e com `-o json`, ver abaixo).
- **`describe`/`stop`/`rm`** fazem routing por nome exacto, **fail-closed**: zero donos →
  `no such workload`; um container E uma vm com o mesmo nome → `ambiguous` (aponta para o
  comando específico, nunca adivinha).

## Snapshot/restore de VM de 1.ª classe

O `VmBackend` ganha snapshot/restore como métodos de 1.ª classe:

- **`vm snapshot <vm> <nome>`** — no libvirt, de uma VM **a correr** é um checkpoint de
  **sistema** (memória **+** disco).
- **`vm restore <vm> <nome>`** — volta ao checkpoint (`virsh snapshot-revert`).
- **`vm snapshots <vm>`** — lista os snapshots.
- **Cloud Hypervisor**: fail-closed (o restore do CH relança um vmm novo — ciclo diferente —
  e precisa de `ch-remote`; deferido, com erro claro a apontar para o backend libvirt).
- **Armadilha a saber**: `vm stop` faz *undefine* do domínio (para não deixar órfãos), por isso
  o snapshot exige a VM **a correr**.

## `-o json` — saída estruturada em todos os comandos de listagem (ADR-0005)

O runtime deixa de ser só-CLI. **Todos os 10 comandos de listagem** aceitam
`-o/--output json` (default continua `table`), emitindo um array JSON com **chaves estáveis,
independentes de língua** (nunca os headers traduzidos) — a "API" para automação/GitOps/SRE:

```
delonix workload ls -o json | jq '.[] | select(.status | startswith("Up"))'
```

Cobertura: `workload · container ps · vm · pod · network · volumes · secret · storage ·
sharevolume · image` (incl. imagens VM pelos três pontos de entrada `image ls`/`image --vm
ls`/`image vm ls`).

- **`secret ls -o json` nunca expõe valores** — só nomes de chaves + contagem (os valores só
  saem por `secret inspect --reveal`).
- Valores máquina-crus onde faz sentido: `created_unix`/`size_bytes` numéricos,
  `running`/`total` de pods, bytes/booleans de quota (com `used_complete`/`measured` para
  distinguir medição incompleta de valor real).

## Segurança: o caminho de dados IPv6 não filtrado (era v0.37.1)

**Actualização de segurança recomendada.** A SDN atribuía um IPv6 ULA a cada container e a
firewall inteira é `table ip` (v4) — um segundo caminho de dados com zero política, que
contornava `ingress`/`egress`, `policy deny`, isolamento de namespace, `kind: Dependency` e o
guarda L4. Corrigido com **duas camadas independentes**: `disable_ipv6` no attach (tira ULA +
link-local) e `table ip6 dlxing` (`forward policy drop`) no holder, mais uma chain `fwguard`
que nega `169.254.0.0/16` e `127.0.0.0/8`. Endurecimento a quente dos containers já a correr
(`net netns up`), sem reiniciar. Escapatórias ruidosas: `DELONIX_ENABLE_IPV6=1`,
`DELONIX_ALLOW_LINK_LOCAL=1`. Detalhe em `docs/releases/v0.37.1.md`.

## Qualidade e processo

- **Infra de testes** (dev-only, stable Rust): robustez via `proptest` ("fuzz on stable" — o
  `cargo-fuzz` exige nightly), micro-benchmarks `criterion`, e cobertura via `cargo-llvm-cov`
  (`scripts/coverage.sh`, `make bench`/`make coverage`). Tudo confinado a dev-dependencies — a
  árvore de release continua limpa.
- **6 ADRs** (`docs/adr/`) registam as decisões estruturais: Workload (0001), driver de
  computação (0002), modelo de capacidades sem-tenancy (0003, *proposed*), checkpoint/CRIU
  (0004, *proposed*), saída `-o json` (0005), `type: microvm` (0006). Mais a descoberta de
  arquitectura em `docs/runtime/`.

## Compatibilidade

- **Aditivo, sem breaking changes de CLI.** O default de todos os `ls` continua a tabela
  humana; `-o json` é opt-in. `kind: Container`/`Vm`/`Pod` continuam a funcionar tal e qual —
  `kind: Workload` é açúcar por cima, não um substituto.
- `type: microvm` é host-dependente (precisa de Cloud Hypervisor) de uma forma que `type: vm`
  não é — por desenho, e fail-closed no boot se o CH faltar.

## Limitações conhecidas

- Snapshot/restore de VM só no backend **libvirt** (o Cloud Hypervisor recusa com erro claro).
- A validação end-to-end do snapshot cobre as operações libvirt numa VM real; o rollback de
  estado *dentro* do convidado não foi exercitado.
- ADR-0003 (capacidades) e ADR-0004 (checkpoint) ficam *proposed* — por desenho, à espera de um
  consumidor/necessidade concreta, para não construir abstracção prematura.

---

# v0.37.1 — versão de SEGURANÇA: o caminho IPv6 não filtrado

**Actualização recomendada a todos.** Esta versão fecha um contorno completo do modelo
de política de rede. Quem corre containers de mais do que um inquilino no mesmo host
deve actualizar antes de qualquer outra coisa.

Bloco 0 do plano `33_delonix_runtime_ingress_egress_hardening` — RF-NET-11 (mitigação)
e RF-NET-02. O relatório de discovery que o motivou está em
[`docs/discovery/33_GAPS_ENCONTRADOS.md`](../discovery/33_GAPS_ENCONTRADOS.md).

---

## O problema (RF-NET-11)

A SDN atribuía a cada container um endereço IPv6 ULA derivado do seu IPv4
(`fd00:<2º octeto>::<o3>:<o4>`), enquanto **toda** a firewall vive em `table ip` — IPv4
apenas. Isso é um segundo caminho de dados, completamente sem política, para todos os
containers do host.

Reproduzido ao vivo, dois containers na mesma rede:

```
# a firewall a NEGAR em IPv4
$ delonix container exec cli wget -T3 -O/dev/null http://10.216.133.231/
wget: download timed out

# o mesmo alvo, o mesmo porto, por IPv6
$ delonix container exec cli wget -T3 -O/dev/null 'http://[fd00:216::5081:c3ff:fe63:8bd1]:80/'
                                                            → 200
```

**O que era contornável:** regras `ingress`/`egress`, `policy deny`, isolamento de
namespace, `kind: Dependency` e o guarda L4 — todos são `table ip`.

**Facilidade de exploração:** trivial. O endereço deriva do IPv4, e um único
`ping -6 ff02::1%eth0` enumera todos os vizinhos da bridge numa passagem (medido: três
respostas, incluindo o alvo). As imagens modernas escutam em `[::]` por omissão — o
`nginx:alpine` activa `listen [::]:80` no seu próprio entrypoint.

**Privilégio necessário:** nenhum. Um container normal chega a qualquer outro.

## A correcção

IPv6 passa a ser **explicitamente recusado** na SDN, em duas camadas independentes:

1. **Sem endereços** — `disable_ipv6` dentro do netns de cada container, no `attach`.
   Remove a ULA *e* o link-local que o kernel atribui sozinho. Não depende de nenhuma
   configuração do host.
2. **Sem encaminhamento** — `table ip6 dlxing` com `forward policy drop` no holder.
   Apanha o que ainda assim rotear v6, por exemplo um container **privilegiado** que
   remonte `/proc/sys` em escrita e volte a ligar o v6.

São duas e não uma de propósito: a camada 2 depende de `bridge-nf-call-ip6tables`, que
um host pode não ter; a camada 1 não depende de nada.

**Nada se perdeu.** Não havia uplink v6 (`slirp4netns` corre sem `--enable-ipv6`, e a
saída v6 sempre respondeu `Network is unreachable`) e o resolvedor interno só alguma vez
respondeu registos A. A ULA servia tráfego leste-oeste que nenhuma política governava —
que é exactamente o problema.

Validado ao vivo com holder fresco: zero endereços `inet6` no container, `nginx` arranca
e serve normalmente em IPv4, e o próprio `ping -6` falha na origem
(`Cannot assign requested address`).

## Destinos sensíveis negados por omissão (RF-NET-02)

Chain nova `fwguard`, a `forward priority -20` — antes do `fwdeny` (-10), do dispatch
por container (-5) e da política por omissão (0). Nenhuma regra de utilizador se lhe
pode pôr à frente:

- `169.254.0.0/16` — metadados de instância cloud. Num host cloud é o *endpoint* de
  credenciais da instância, a um `GET` de distância de qualquer container. Não existia
  negação nenhuma na árvore.
- `127.0.0.0/8` — loopback do host. Já inalcançável na prática pelo
  `--disable-host-loopback` do slirp, mas isso é uma flag num caminho de arranque à
  distância de uma regressão, e a regra custa uma linha.

Confirmado ao vivo pelo contador da própria regra: `packets 4 bytes 240` após uma
tentativa de alcançar `169.254.169.254`.

`fe80::/10` fica coberto pela recusa de v6 acima — não há endereços v6 para filtrar.

**Fora desta versão, deliberadamente**: os sockets de gestão (`serve api`/`cri`/
`docker-api`) são UNIX por omissão, logo não há endereço para um container alcançar;
proteger os serviços que o próprio holder expõe (DNS interno, proxy L7) exigiria uma
chain `input`, que este netns não tem. Fica registado como seguimento em vez de meio
feito.

## Escapatórias (opt-in, ruidosas)

| Variável | Efeito |
|---|---|
| `DELONIX_ENABLE_IPV6=1` | Repõe a SDN IPv6 anterior: ULA + rota v6, e a tabela de recusa não é instalada |
| `DELONIX_ALLOW_LINK_LOCAL=1` | Remove as negações do `fwguard` |

Ambas emitem aviso de segurança no arranque do holder e existem para depuração. Mesma
forma do `DELONIX_FORWARD_POLICY=accept` já existente. As duas foram validadas ao vivo
(com `DELONIX_ENABLE_IPV6=1` o container volta a ter 3 endereços v6 e não há
`table ip6`).

## Compatibilidade

- **Sem alterações de CLI, de manifesto ou de esquema do `Store`.**
- Um container que dependesse de alcançar outro por IPv6 dentro da SDN deixa de o
  conseguir. Não era uma funcionalidade documentada nem governável por política; quem
  precise dela tem a escapatória acima, e deve saber que nenhuma regra de firewall se
  lhe aplica.
- **Containers já a correr são endurecidos A QUENTE — sem reiniciar nenhum.** A recusa
  entra no `attach`, o que só cobria containers criados a partir daqui; mandar reiniciar
  os outros seria a resposta errada neste motor, onde o dataplane não pertence ao ciclo
  de vida do processo (`container update` já troca portas, volumes e redes com o PID
  inalterado). `delonix net netns up` — idempotente — varre os netns vivos e desliga-lhes
  o IPv6 no lugar:

  ```
  $ delonix net netns up
  IPv6 refused on 2 running container netns (no restart needed)
  ingress UP — holder pid 771156 · slirp pid 771174 · bridge delonix0 (10.200.0.1)
  ```

  Validado ao vivo contra containers criados **antes** da correcção, com o bypass aberto:
  o PID ficou igual (`771209` antes e depois), o uptime continuou a contar, os endereços
  v6 passaram a zero, o bypass fechou (`Cannot assign requested address`) e o serviço
  IPv4 nunca piscou.

  A varredura entra nos namespaces do holder por `nsenter` em vez de usar um verbo do
  socket de controlo, **de propósito**: o caso que interessa é o upgrade in-place, em que
  o holder ainda corre o binário ANTIGO e não conheceria um verbo acrescentado hoje.

## Como confirmar depois de actualizar

```bash
# 1. sem endereços v6 dentro do container
delonix container exec <c> ip -6 addr show          # deve vir vazio

# 2. tabela de recusa presente no holder
delonix net netns status                            # obtém o pid do holder
nsenter -t <pid> -U -n --preserve-credentials -- nft list tables
#   table ip dlxing
#   table ip6 dlxing        ← esta

# 3. as negações incondicionais, com os seus contadores
nsenter -t <pid> -U -n --preserve-credentials -- nft list chain ip dlxing fwguard
```

---

# v0.37.0 — auditoria sistemática dos 208 subcomandos: 4 caminhos de perda de dados fechados

Release de **integridade de dados e honestidade de relato**. Nasceu de uma auditoria
comando a comando de toda a superfície da CLI (208 subcomandos, mapeados por dump
recursivo de `-h`), com testes ao vivo num host real em vez de só leitura de código.
Rendeu 23 achados confirmados, **todos reproduzidos antes de serem corrigidos** e
re-verificados depois.

O tema é um só: comandos que **destruíam dados dizendo que falharam**, ou que
**diziam ter destruído sem destruir**, ou que **reportavam sucesso sobre uma falha**.
Nenhum destes aparece num `cargo test` — só olhando para o estado real do host
depois de cada comando.

> **Esta release muda comportamento.** Um `container run` passa a devolver o código
> de saída do container (era sempre 0); um `volumes rm` recusa-se a apagar um volume
> em uso; um `image rm` recusa uma imagem em uso; um `system prune` pede confirmação.
> Scripts que dependiam do comportamento antigo precisam de revisão — ver
> "Compatibilidade" no fim.

## Perda de dados (4 caminhos, todos reproduzidos ao vivo)

### `compose` colapsava todos os projectos num só

`default_project_name` derivava o nome do projecto do directório — mas recebia um
caminho **relativo** (`find_compose_file` devolve `docker-compose.yml` nu, e
`-f docker-compose.yml` também é relativo). `Path::new("docker-compose.yml").parent()`
é `Some("")`, cujo `file_name()` é `None`, por isso **toda** a invocação normal caía
no literal `"default"`. O único teste que existia passava caminhos absolutos, logo
nunca apanhou nada — pior, o teste **codificava o bug** (`assert_eq!(…, "default")`).

Medido num host real: dois projectos compose em directórios diferentes tornavam-se
ambos o projecto `default`, partilhando a rede `default_default`, o volume
`default_<nome>` e o container `default-<serviço>`. Um `up` no segundo directório
**adoptava** o container do primeiro ("already exists, nothing to do") e lia os dados
dele; um `down -v` ali **destruía o volume nomeado do primeiro projecto**.

Agora o caminho é absolutizado contra o cwd antes de se derivar o nome — que é
exactamente o directório que o Docker Compose usa. Regressão coberta por um teste
que exige o comportamento certo para caminhos relativos.

### `volumes rm` apagava um volume montado num container a correr

Sem verificação de referências, sem `--force`, rc=0. Medido: o `/data` de um container
vivo passou de 30 MiB a vazio. O Docker recusa isto há sempre.

`volumes rm` passa a recusar enquanto um container (ou um `kind: ShareVolume`) o
referenciar, nomeando quem o segura e o seu estado. `--force` continua a existir para
quem realmente quer destruir. A verificação usa `c.mounts` — estado que já era
persistido para o `start` reconstruir os mounts — e compara **mountpoints
resolvidos**, por isso um bind do mesmo caminho conta tanto como uma referência
nomeada.

### `volumes rm` do Storage pai destruía os dados de todos os tenants

Um `kind: Storage` com N `ShareVolume`s dentro: `volumes rm <storage>` apagava a
árvore toda, rc=0, sem aviso. Os registos dos shares **sobreviviam** a apontar para um
caminho apagado, e o `sharevolume ls` continuava a mostrá-los saudáveis a `USED 0 B`.
No caso NAS real é pior: `storage rm` desmonta o export e o mountpoint do share passa
a resolver para o disco **local** por baixo — os tenants continuam a escrever, para o
sítio errado, sem um único erro.

`volumes rm` e `storage rm` passam a recusar enquanto houver shares.

### `volumes rm` parcial ressuscitava os dados de outro tenant

`VolumeStore::remove` era um `fs::remove_dir_all` cru. Em rootless, o `_data` escrito
por um container em userns mapeado pertence a um **subuid**, e qualquer base de dados
gerida faz `chmod 700` ao seu directório — logo o `remove_dir_all` levava EACCES, mas
só **depois** de já ter desligado o `meta.json` (apaga entradas à medida que percorre).

Resultado medido: o `rm` reportava `Permission denied`, o volume **desaparecia** de
`ls`/`inspect`/`system df`, e todos os bytes ficavam no disco. Um `create` do **mesmo
nome** a seguir dizia `usage: 0 bytes` e entregava os dados do dono anterior a quem o
montasse. Num PaaS onde o nome do volume deriva do nome da app/addon, o tenant B
herdava a base de dados do tenant A — e o operador não via nada de errado.

Três volumes exactamente neste estado foram encontrados no host de desenvolvimento
(110 GB de um cluster Postgres entre eles), invisíveis ao `volumes ls`.

Agora: **dados primeiro, contabilidade em último, e nada é desligado se os dados não
saírem**. Um `rm` falhado deixa o volume inteiramente visível e inteiramente do seu
dono. E o `remove_tree_mapped` (o mesmo helper que o `system prune` já usava) passa a
ser injectado, para que dados subuid sejam de facto removíveis em vez de deixarem um
volume que nenhum comando conseguia apagar.

### `vm rm` apagava o disco antes de verificar se a VM existia

O bloco que remove `<nome>.qcow2` e o directório de seed corria **antes** do teste de
existência, por isso um `vm rm <nome>` sem registo nem domínio libvirt apagava o disco
e **depois** devolvia `no such VM` com rc=1. Quem lê esse erro conclui razoavelmente
que nada aconteceu. Reproduzido com um qcow2 real. Mesma forma que o `volumes rm`
acima: não destruir nada antes de saber que o objecto é nosso para destruir.

### `sharevolume rm --purge-data` afirmava apagar sem apagar

Era `let _ = std::fs::remove_dir_all(...)`. Em rootless o directório do tenant
pertence a um subuid, o `remove_dir_all` falha com EACCES, e o comando imprimia
`removed (data deleted)` com todos os bytes intactos no NAS. É o **espelho** dos bugs
acima: reportar destruição sem destruir — o que importa muito num offboarding de
tenant ou numa resposta a pedido de apagamento.

Agora tenta a remoção simples, cai no userns mapeado se preciso, e se a árvore
**ainda** sobreviver é **erro** (com o registo do share preservado, para não se perder
o rasto), nunca um rodapé.

## Códigos de saída: `container run` era sempre 0

`exit 42`, `exit 1` e um `execve` falhado do entrypoint davam todos `$? = 0`. E um
container que **nunca arrancou** (`failed to prepare the rootfs`, que o filho reporta
como 126) também. Qualquer job one-shot — migração de schema, `pg_dump`, health probe,
backup — reportava sucesso ao falhar. Um backup que falha em silêncio só se descobre
no restore.

A causa era um `Ok(())` a descartar o `Status` que o `waitpid` do primeiro plano já
calculava. Agora:

| | antes | agora |
|---|---|---|
| `run --rm sh -c "exit 42"` | 0 | **42** |
| `run --rm sh -c "exit 1"` | 0 | **1** |
| `run --rm /no/such/binary` | 0 | **127** |
| rootfs impossível de preparar | 0 | **126** |
| `run --net <rede> sh -c "exit 42"` | 0 | **42** |

O caminho `--net <rede>` merece nota própria: corre numa 2.ª passagem de re-exec, e o
pai achatava qualquer saída não-zero num erro genérico ("o container não arrancou na
rede"). Agora propaga o código exacto; um erro **real** de arranque continua a ser
rc=1 com a mensagem que explica.

### `wait` deixou de inventar 137

Um container `-d` **sem** `--restart` era registado como `Crashed` com
`crash_reason=process_gone` e o `wait` devolvia `137` (128+SIGKILL) — mesmo para um
`exit 43` limpo. O motor não é o pai real do processo em modo desanexado, por isso o
código **não existe** para ser lido; inventá-lo é pior que admiti-lo, porque um
orquestrador não consegue distinguir sucesso, falha aplicacional e OOM-kill.

- `ps -a` mostra `Exited (unknown)` em vez de `Dead` (que se lê como "morto à força");
- `wait` recusa-se a imprimir um número, e diz como obter o real;
- `Dead` fica reservado para o que **realmente** foi morto (OOM, SIGKILL externo).

Com um supervisor (`--restart on-failure`/`always`/`unless-stopped`) o código real
continua a ser capturado — `Exited (43)`, `wait` → 43. **O limite arquitectural
mantém-se**: fechá-lo por omissão exigiria um processo supervisor por container (o
modelo do conmon do podman), o que é uma decisão de filosofia, não um bug fix.

## Segurança

### `--secret` anulava o cofre cifrado

Os valores decifrados eram escritos em **claro** no registo do container
(`<root>/containers/<id>.json`) e impressos verbatim pelo `container
inspect`/`describe` — enquanto o `secret inspect` redige cuidadosamente os mesmos
valores e os esconde atrás de `--reveal`. O cofre cifra em repouso e o primeiro
consumidor desfazia isso, permanentemente, num ficheiro que sobrevive ao container.

Agora só os **nomes** são persistidos; os valores são resolvidos no spawn, como o
`--secret-files` já fazia (e é por isso que esse modo nunca teve o bug). Efeito
secundário bem-vindo: um segredo rodado passa a aplicar-se no `start` seguinte, em vez
de o registo fixar o valor capturado na criação. O `exec` também os resolve, para não
ver um ambiente diferente do processo principal (paridade docker), e um `-e` explícito
continua a ganhar.

### Token de streaming do CRI podia ser previsível

`random_token` partia de `[0u8; 16]`, abria `/dev/urandom` dentro de um `if let Ok` e
descartava o resultado do `read_exact` com `let _ =`. Sem entropia, o token tornava-se
a constante `00000…0` — e como estas URLs dão execução de código arbitrário dentro de
um pod, qualquer processo local poderia sequestrar uma sessão pendente; dois execs
concorrentes também colidiriam na mesma chave. Agora falha fechado (`Status::internal`),
incluindo o caso improvável de uma leitura toda a zeros.

### Credenciais do NAS sobreviviam ao `storage rm`

`store.remove(name)` só toca em `<root>/volumes/<name>/`, mas o utilizador e a password
vivem em `<root>/storage/<name>.cifs-credentials`. A credencial ficava no disco
indefinidamente, e só era sobreposta se alguém recriasse um storage com o mesmo nome.
Agora sai com o storage.

### `--username`/`--password` eram ignorados em `webdav`

O help documenta-os como "(cifs/webdav)" e o ficheiro de credenciais era escrito para
ambos, mas só o ramo `cifs` o referenciava — para `webdav` e `nfs` as credenciais eram
aceites e caladamente descartadas, deixando a montagem falhar com um erro de
autenticação opaco. Agora recusa com uma mensagem que diz onde as pôr (o davfs2 lê
`/etc/davfs2/secrets`, configuração de host que este motor não deve escrever).

## Medição de uso: `0 B` sobre dados reais

`dir_usage`/`dir_size` engoliam qualquer erro de `read_dir` e devolviam `0` —
indistinguível de um volume vazio. E em rootless esse é o caso **normal**, não uma
extremidade: qualquer base de dados gerida faz `chmod 700` ao seu directório sob um
userns mapeado. Consequências medidas: um `describe` a reportar `Usage: 0 B` sobre
20 MiB reais, um `system df` a dizer `volumes 0 B` num disco a encher, e a quota
rootless — documentada como o limite MONITORIZADO, a única imposição que o rootless
tem — a nunca poder disparar.

Novo tipo `Usage { bytes, unreadable }` e `QuotaState { …, measured }`: uma medição
incompleta é **desconhecida**, nunca zero. E um novo re-exec mapeado (`__duusage`,
mesmo idioma do `__volsnap`/`__buildtar` já existentes) mede de dentro do userns onde
somos donos dos subuids. Validado: um volume com 20 MiB que reportava `0 bytes` passou
a reportar `20971520`.

## Outros achados corrigidos

- **`volumes create --quota <inválida>`** criava o volume e **depois** validava a
  quota: rc=1 mas com um volume real, **sem quota**, no disco. Um control-plane que
  retentasse reutilizava-o sem limite. Agora valida antes de criar nada.
- **`parse_size_bytes` saturava em silêncio**: `--quota 99999999999t` virava
  `u64::MAX` (o cast `f64 as u64` é saturante em Rust) — uma quota que o `inspect`
  mostra como definida mas que nenhum volume atinge, i.e. quota nenhuma. Agora é erro.
- **`volumes snapshot restore`** limpava o `_data` **antes** de validar o arquivo: um
  tar corrompido ou truncado destruía os dados vivos sem nada para pôr de volta.
  Agora descodifica o arquivo inteiro primeiro (uma leitura extra de um ficheiro que
  vai ser lido de qualquer forma) e recusa com os dados intactos.
- **`image rm`** apagava a imagem de um container a correr, rc=0. O container
  sobrevive (o rootfs já está materializado), por isso nada parece errado — mas o
  workload deixa de poder ser recriado ou escalado, e num nó air-gapped essa imagem
  simplesmente desapareceu. Agora recusa, com `--force` para quem insiste.
- **`system prune`** removia **todos** os containers parados (incluindo os apenas
  `Created`) sem qualquer confirmação, apesar de o help liderar com "unused images".
  Agora pergunta, listando os que vão desaparecer; sem TTY exige `--force`, para que
  um prune não-vigiado seja sempre explícito.
- **`compose up -d`** era rejeitado com rc=2 — a invocação mais comum do compose,
  presente em todo o tooling e CI existente. Aceito como no-op (os serviços já
  arrancam desanexados: não há daemon a segurá-los).
- **`compose down -v`** engolia os erros de remoção de volume com `let _ =`,
  imprimindo sucesso. Agora respeita a mesma verificação de referências e reporta.
- **Pânico em SIGPIPE**: `delonix image ls | head` terminava em
  `panicked at … failed printing to stdout: Broken pipe` com nota de backtrace. O
  runtime do Rust ignora SIGPIPE, por isso o `println!` fazia unwrap a um EPIPE.
  Restaurada a disposição por omissão — o processo termina em silêncio num pipeline,
  como qualquer outra ferramenta UNIX.
- **Quota de `ShareVolume`** era medida pelo caminho directo (logo `0 B` para qualquer
  tenant real); passa pela medição mapeada e distingue "não medido" de "dentro da
  quota".
- **`system events` era cego a dados**: só emitia eventos de `container`. Agora
  `volume`, `storage`, `image`, `secret` e `sharevolume` também emitem — um incidente
  de perda de dados deixa rasto.

## Compatibilidade

O que pode quebrar um script existente, e o que fazer:

| Mudança | Impacto | Ajuste |
|---|---|---|
| `container run` devolve o código do container | um `run` de um job que falha já não é 0 | é o comportamento do docker; corrigir o script que assumia 0 |
| `volumes rm` recusa volume em uso | automação que apagava volumes de containers vivos falha | parar o container primeiro, ou `--force` |
| `storage rm` recusa com shares | idem | remover os shares primeiro |
| `image rm` recusa imagem em uso | idem | `--force` |
| `system prune` pede confirmação | prune em cron/CI passa a falhar | acrescentar `--force` |
| `compose` deriva o nome do projecto do directório | recursos passam a chamar-se `<dir>_<nome>` em vez de `default_<nome>` | usar `-p default` para manter os nomes antigos, ou recriar |
| `wait` erra em vez de devolver 137 | script que lia o 137 | usar `--restart` para capturar o código real, ou primeiro plano |
| `storage create --username` em `webdav` | passa a erro em vez de ser ignorado | pôr as credenciais em `/etc/davfs2/secrets` |

## Verificação

Todos os 23 achados foram reproduzidos ao vivo **antes** da correcção e re-verificados
depois, num host rootless real: 25/25 reprodutores passam. `cargo clippy --workspace
--all-targets` com zero avisos, `cargo fmt` limpo, 571 testes a passar — incluindo
testes de regressão novos para a ordem do `remove`, a medição incompleta, o overflow
da quota, a preservação dos dados de um share externo, e o nome de projecto do compose
a partir de um caminho relativo.

Uma armadilha encontrada e documentada no caminho: `remove_tree_mapped` re-executa
`current_exe()`, o que num **binário de teste** re-entra no harness — que lê
`__rmtree <path>` como filtros de nome, corre zero testes e sai **0**. Esse falso
sucesso suprimia qualquer fallback. Onde importa, tenta-se agora a remoção simples
primeiro (mais rápida e sem fork) e o userns mapeado só como recurso.

## Não incluído

`--format json` nos comandos de listagem continua em falta — a automação tem de
parsear tabelas alinhadas. É uma superfície de API nova em ~10 comandos, não um bug, e
merece ser desenhada de propósito em vez de acrescentada às pressas.

---

# v0.36.1 — instalador endurecido e cadeia de assinatura das releases

Release de **segurança do canal de distribuição**. O binário é funcionalmente
idêntico ao da v0.36.0 (nenhum ficheiro `.rs` mudou) — o que mudou foi a forma como
ele chega à tua máquina.

## O instalador executava a meio se o download fosse cortado

O uso documentado é `curl … | bash`, e o **bash executa à medida que lê**. Uma
transferência truncada — rede a cair, proxy a cortar — corria **metade** do
instalador. E a primeira metade não é inócua: instala pacotes com `sudo`, acrescenta
a `/etc/subuid`, carrega perfis AppArmor e escreve em `/etc/sysctl.d`. O host ficava
meio-configurado **sem um único erro**.

O corpo do `install.sh` passou a estar dentro de `{ … }`: o bash tem de ler até à
chaveta final antes de executar seja o que for, por isso um ficheiro truncado morre
em `syntax error: unexpected end of file` e **nada** corre. Verificado
empiricamente, com o instalador cortado a 60% e a 80%: zero linhas executadas.

## `SHA256SUMS` sozinho não protegia contra uma release adulterada

O instalador sempre verificou os binários contra o `SHA256SUMS`. Isso prova
**integridade de transferência** — não **autenticidade de origem**. O `SHA256SUMS`
vem da mesma URL que o binário, por isso quem consiga publicar uma release adulterada
(conta comprometida, token de CI roubado) publica também o `SHA256SUMS` a condizer: a
verificação passava e instalava o artefacto adulterado.

Esta release traz a cadeia completa, pronta a activar:

- o workflow assina o `SHA256SUMS` com **minisign** e publica `SHA256SUMS.minisig`;
- o `install.sh` traz a chave pública embutida e verifica a assinatura **antes** de
  qualquer verificação de hash (sem um `SHA256SUMS` autêntico, os hashes que ele
  contém não valem nada);
- **fail-closed**: hash adulterado, assinatura de outra chave, `.minisig` em falta ou
  `minisign` que não se consegue instalar — todos abortam. `--insecure-skip-signature`
  existe como escape documentado.

O mecanismo foi validado ponta a ponta com uma chave descartável: assinatura legítima
verifica, `SHA256SUMS` adulterado dá *Signature verification failed*, e assinado por
outra chave dá key id a não bater.

> **Esta release ainda sai SEM assinatura.** A chave de assinatura é gerada pelo
> mantenedor, na máquina dele, e ainda não está configurada — ver
> [docs/SECURITY-RELEASES.md](../SECURITY-RELEASES.md). Até lá o instalador avisa e
> verifica só integridade. Assim que a chave existir, a release seguinte é assinada
> automaticamente e o instalador passa a exigir a assinatura.

## `--low-ports`: publicar 80/443 em rootless

Sem isto, `-p 80:80` e um `kind: HTTPRoute` com `entrypoints: [{port: 80}]` falham com
`slirp_add_hostfwd failed`: quem liga a porta do lado do host é o `slirp4netns`, um
processo sem privilégios, e o kernel reserva as portas <1024. Não é limitação deste
motor — o podman e o docker rootless têm o mesmo muro e documentam o mesmo contorno.

`install.sh --low-ports` escreve `/etc/sysctl.d/99-delonix-lowports.conf` com
`net.ipv4.ip_unprivileged_port_start = 80`.

**Deliberadamente opt-in e fora do bloco `--no-tune`**: os sysctls que já lá estavam
afinam limites (inotify, `max_map_count`) e não mexem em nenhuma fronteira de
privilégio; este baixa uma, para o host inteiro — a partir daí qualquer programa de
qualquer utilizador local pode ligar-se às portas 80-1023. Num portátil de
desenvolvimento é um compromisso razoável; numa máquina partilhada, a alternativa sem
baixar nada é um proxy na porta 80 a correr como root.

## Actualizar

```bash
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash
```

---

## v0.36.0 — the `-p` / ingress-firewall flow: 5 bugs, 2 of them security

Real bug report from a live host: *"the browser should not block when a container is exposed via
`-p`"*. The symptom was real — and following it surfaced four worse problems sitting next to it.
Every bug below was **reproduced live before being fixed**, and the fixes were re-verified against
a real holder afterwards.

Three of the five share one root cause worth stating on its own: **`c.ip` is not "the container's
address", it is "the container's address on the primary network"**. Every control-plane path that
took a single IP from the record was blind to any additional network.

### SECURITY — a firewall rule silently ignored its port when the proto was `any`

`fw_chain_body` only emitted the `dport` **inside** the `proto != "any"` branch. Since the CLI
defaults `proto` to `any` whenever a bare port is written (`ingress allow <c> 9999` — the common
form), the generated rule collapsed to `ip daddr <ip> accept`: **the whole container**.

Measured live against a published port, not inferred:

| command | what it says | what it did |
|---|---|---|
| `policy deny` + `ingress allow <c> 9999` | open port 9999 | **opened port 18099 too** |
| `policy allow` + `ingress deny <c> 9999` | close port 9999 | **closed the whole container** |
| `ingress deny <c> tcp/9999` (explicit proto) | close port 9999 | correct |

The explicit-proto form was always correct, which is why every prior E2E test — all of which use
`tcp/…` — passed over it.

**Fix**: `any` + a port now emits `meta l4proto { tcp, udp } th dport <port>` (`th` = transport
header, valid for both protocols, ranges included). Regression test:
`fw_body_keeps_the_port_when_proto_is_any`.

### SECURITY — multi-homing bypassed both the firewall and namespace isolation

Two independent holes, one cause. `apply_firewall` was keyed on `c.ip` alone, so the `fwdeny`
jumps (`ip daddr <ip> jump fw<hash>`) never existed for an additional network's IP; and
`do_attach_extra` never called `ns_set_join`, so the extra IP stayed outside `@dlxall` — and the
cross-namespace drop only fires for sources inside that set.

Reproduced live, both halves:

```
ingress policy deny on B   →  A→B via primary IP  = blocked
                              A→B via extra IP    = REACHABLE     ← firewall bypassed

namespace teamA / teamB    →  A→B via primary IP  = blocked
                              A→B via extra IP    = REACHABLE     ← isolation bypassed
```

`ingress`, `egress`, `Dependency` and namespace isolation were **all** bypassable by connecting
the target to a second network.

**Fix**: new `apply_firewall_all` (control line `firewall <id> <ip1,ip2,…> <hex>`), with
`do_firewall` emitting one rule body per IP and a jump pair per IP; `attach-extra` carries the
namespace (6 tokens = `default`, 7 = namespaced, the same compatibility rule `attach` already
used) and joins the sets.

**Corollary fixed on the way**: `fwdeny` jumps are now **rebuilt**, not added-if-missing. A
container leaving an additional network used to leave the released IP's jump behind — and IPAM
hands that address to another container later, which silently inherited this one's firewall.
`--net-connect` / `--net-disconnect` re-apply the firewall.

### Additional networks vanished on `stop` + `start`

`cmd_start` re-attached only the primary network. `c.extra_networks` was persisted all along —
nothing ever replayed it. Live: `eth1` present before, gone after, while `describe` went on
reporting `Extra: dlx-dev2 (10.239.… on eth1)`. A service reachable only over that second network
broke on its first restart, in silence.

Same family as the `-v` bug fixed in v0.35.0, with one difference worth recording: there the state
was never saved; here it *was* saved and simply never replayed. Both failure modes are worth
checking for when wiring any `start`/`restart` path.

**Fix**: `cmd_start` re-attaches each `ExtraNet` (same container id into IPAM, so the address
normally comes back identical; if it differs, the record is corrected rather than left pointing at
an address that no longer exists).

### `unpublish` was proto-blind while `publish` was not

`-p 53:53/tcp` and `-p 53:53/udp` are two distinct publications, but `slirp_remove_hostfwd` matched
on `host_port` alone and `do_unpublish` used a bare `dport <n>` needle — both tore down every
protocol. Worse, `unpublish_live` removed only ONE record entry. Live result: `--publish-rm 18100`
left the record claiming `18100:53/tcp` with **zero** bindings and `curl` returning nothing — and
the host port stayed reserved in `port_owner` for a container that no longer served it.

**Fix**: `slirp_remove_hostfwd_proto` / `do_unpublish(port, proto)` / `unpublish_port_proto`, and
`unpublish_live` clears every record entry for that host port. The 2-token control form is
unchanged for teardown.

### `-p [hostIp:]hostPort:contPort` — the original report

Published ports bind to `127.0.0.1`. That default is correct, but it was the **only** behaviour
reachable from the CLI: the Docker `-p <ip>:<host>:<cont>` form was rejected outright as `invalid
port`, and the sole way to widen the bind was the undocumented `DELONIX_PUBLISH_ADDR`. A browser
on another machine — or the host itself via its LAN address — got connection-refused with nothing
explaining why.

```
-p 8080:80             →  binds 127.0.0.1 only   (unchanged, still the default)
-p 0.0.0.0:8080:80     →  every interface
-p 192.168.1.10:8080:80 → one interface
-p 192.168.122.1:8080:80 → the libvirt gateway (reachable from VMs — see `delonix vm reach`)
```

`publish_bind_addr` concentrates the precedence (spec > env > `127.0.0.1`) in one place so the two
publish datapaths — the per-container slirp and the single ingress slirp — cannot diverge. IPv4
only: the value is interpolated into the slirp api-socket's JSON, and an IPv6 literal would also
collide with the `:` splitting. A non-IPv4 host is **rejected**, never silently discarded — which
was precisely the compose bug (`127.0.0.1:9000:80` dropped, publishing on every interface instead).

Validated live: `ss` confirms the `0.0.0.0` bind and `curl http://<lan-ip>:18099/` returns **200**.

### `ingress ls` stopped lying

Two honesty fixes in the table people read specifically to decide whether something is exposed:

- Every publish was printed as `allow / DNAT` unconditionally. Under `policy deny`, `curl` returned
  nothing while the table still said `allow`. It now resolves the verdict the way the dataplane
  does (`published_verdict`) and prints `BLOCKED` plus the exact recovery command. `ingress policy
  deny` warns at the moment it blocks a published port.
- `--net host` / `--net none` containers were listed as `allow (default)` — which reads as
  "governed and open" — when `require_sdn_ip` rejects *any* firewall mutation for them. They now
  read `n/a (host net)`.

A related fact that was nowhere documented and is genuinely counter-intuitive: **a rule must name
the CONTAINER port, never the host port**, because DNAT runs at `prerouting` and the per-container
chain sees the already-translated destination. The new warnings say so explicitly. Before the
`proto: any` fix above this went unnoticed — the rule ignored the port and "worked" by accident.

### Known limitations

- `infra::publish_port_allow` (publish with a CIDR allowlist applied *before* the DNAT) still has
  **zero callers** — `ingress allow --from <cidr>` only writes the per-container chain. It is the
  same dead-public-API trap as `mount_live` / `set_net_rate` / `update_limits` before their first
  real caller found a latent bug. Wiring it or deleting it deserves its own session; it is
  documented rather than rushed.
- The holder-side fixes take effect only after a holder respawn, and a respawn does not self-heal:
  live containers do not re-attach on their own and must be restarted. That is pre-existing
  behaviour, not new here, but it is what an upgrade on a busy host has to plan for.

---

## v0.35.1 — `cluster load` now actually reaches the kubelet, and `image save`/`image load` land

**v0.35.0's `cluster load` did not work end to end.** `ctr` reported a successful import and
`crictl images` listed the image, so it looked right — but a pod requesting that image failed with
`ErrImageNeverPull`. Found by running the test that actually matters (a pod with
`imagePullPolicy: Never`, which no registry can rescue) instead of trusting the import's own exit
code. Three independent defects, each of which alone breaks the feature:

1. **The image was registered under the short name.** `ctr` recorded `nginx:alpine` — the name as
   typed — while the kubelet resolves the docker-normalized `docker.io/library/nginx:alpine` and
   found nothing. New pure, tested `containerd_ref()` applies the docker default
   (`docker.io/library/…`, or `docker.io/<org>/…`) and deliberately leaves an explicit registry
   alone: rewriting `10.232.67.14:5000/app:1` would point the node somewhere else entirely.
2. **The wrong snapshotter.** `ctr images import` does not use the CRI plugin's snapshotter — it
   uses containerd's global default (`overlayfs`), which cannot mount inside a rootless userns
   (`failed to mount … fstype: overlay … invalid argument`). The node's configured snapshotter
   (`fuse-overlayfs`) is now read from its containerd config and passed explicitly, exactly as
   real `kind` does.
3. **containerd 2.x's transfer service refuses to unpack.** Once the snapshotter was right, the
   import failed with `unable to initialize unpacker: no unpack platforms defined`. The classic
   client-side path (`--local`) unpacks correctly with the same archive, snapshotter and platform.
   `--local` does not exist on containerd 1.6, so support is **probed**, not assumed. Also
   `--platform linux/<arch>` (the image's own) instead of `--all-platforms`, which is what
   triggers that unpacker error in the first place.

**Validated the right way this time**: `delonix cluster load nginx:alpine` → `kubectl run
--image-pull-policy=Never` → pod **Running**, on a rootless kind-mode cluster with containerd
2.1.3.

### New: `delonix image save` / `delonix image load`

The counterparts of `docker save`/`docker load` — moving an image to another machine with **no
registry**, which is what a remote Ansible deploy needs (build here, `save`, copy, `load` there).

```bash
delonix image save delonix-web:v1.2.3 -o /tmp/web.tar     # `-o /dev/stdout | gzip` also works
delonix image load -i /tmp/web.tar
```

The archive is an OCI layout **with** the legacy `manifest.json`, exactly as `docker save` still
emits — so one file is readable by `delonix image load`, `docker load`, `podman load` **and**
`ctr images import`. `load` wires up `delonix_image::load_docker_archive`, a library function that
had existed with no CLI caller.

**Validated live**: `save` → `image rm` → `load` → the restored image runs and serves HTTP 200,
with the same image id.

### Note

`image save` writes progress to **stderr**, never stdout — `-o /dev/stdout` is a supported
destination and a status line there would corrupt the archive.

---

## v0.35.0 — `cluster load`: the `kind load docker-image` equivalent, with no registry — and volumes no longer vanish on `container start`

Feature release, from a real report: a `make push` that ends in `kind load docker-image` fails on a
host that has no `kind` binary — and installing it would not help, because `kind` is a Docker
client and needs a Docker/Podman provider this engine deliberately replaces.

### `delonix cluster load <IMAGE>... [--name <cluster>]`

Takes an image from the **local store** and imports it into the containerd of **every running
node** of a kind-mode cluster. No registry, no `docker save`, no second engine.

```bash
delonix build -t delonix-web:v1.2.3 .
delonix cluster load delonix-web:v1.2.3            # one cluster → no --name needed
kubectl set image deploy/web web=delonix-web:v1.2.3
```

Use `imagePullPolicy: IfNotPresent` in the manifests — the image is already on the node and there
is nothing to pull it from.

- **New `delonix_image::write_oci_archive`** (`save.rs`, the inverse of the existing
  `load_docker_archive`): writes an OCI image layout archive reusing the SAME manifest
  `registry::build_manifest` publishes to a registry. Store blobs are re-packed verbatim —
  nothing is recompressed or re-hashed, and the digests the node ends up with are byte-identical
  to the local ones. Not to be confused with `image export`, which produces a *runtime* bundle
  for `runc`/`crun`.
- **The channel into the node already existed**: the `cluster_dir` ↔ `/kind/delonix` bind mount
  that `cluster create` uses to exchange `kubeadm.conf`/`kubeconfig`. The archive crosses as a
  plain file — no stdin plumbing, no second copy of the rootfs.
- `--all-platforms` on the import (without it `ctr` filters by *its* platform string and can
  import nothing while reporting success), the archive is deleted right after (it is a full extra
  copy of the image on disk), and a node that is **not running is reported, never skipped in
  silence**.
- Cluster resolution follows the existing rule: with one cluster `--name` is optional; with
  several, the error names them instead of picking blindly.

**Validated live** on a real cluster: `delonix-web` (94 MiB) and `delonix-server` (45 MiB)
imported into `dev-control-plane`, visible both in `ctr -n k8s.io images ls` and in the kubelet's
own `crictl images` (as `docker.io/library/<name>`, the same normalization real `kind load`
produces).

### Bug fix (severe): `-v` mounts were never persisted — volumes were LOST on `container start`

Found while landing the above, because a restarted cluster node had lost `/kind/delonix`.

`cmd_run` put the resolved mounts only into the `RunSpec` (applied at spawn) and **never into the
record**, while `cmd_start` rebuilds its `RunSpec` from `c.mounts` — a field that was therefore
always empty. A `container start` of anything created with `-v` came back **Running with no bind
mounts and no named volumes**, and writes that belonged in the volume went silently to the
container's rootfs instead. A restarted database "works" and stores its data in the wrong place.

Fixed by persisting the resolved mounts at creation. CDI mounts (`--gpus`) are included
deliberately: `start` never re-resolves a CDI spec, so leaving them out would silently drop GPU
access on the first restart.

**Validated live**: a host file visible inside the container both before and after a
`stop` + `start` (before the fix it disappeared).

**Recovering a container that already lost its mounts** — the record cannot invent what was never
saved, so re-attach them once with the hot path (no downtime, and it persists):

```bash
delonix container update <name> --volume-add /host/path:/container/path
```

### The pattern behind three bugs in two days

`-p` on a custom network (v0.34.3), volumes on restart (this release), and the already-documented
`vm start` flag loss share one root: **state needed to RECONSTRUCT a resource must be persisted,
not merely used once at creation.** When reviewing any `start`/`restart` path, compare field by
field what creation *uses* against what the record *stores* — whatever only creation sees
disappears on the first restart, silently.

---

## v0.34.3 — fixes a v0.34.1 regression: publishing a port on a custom network was broken

Real bug report from a live host, one command after the v0.34.2 recovery: bringing up a container
on a custom SDN network with a published port (`kaeso-odoo`, port 8069) failed with

```
delonix: system call `slirp api-socket` failed: No such file or directory (os error 2)
error invalid argument: the container did not start inside the network '...' (exit Some(1))
```

**Everything with `-p` on a custom network has been broken since v0.34.1** — `container run --net
<custom> -p <port>` and `container start` of such a container. Containers with no published ports
(and everything on `--net host`/`--net none`, which uses its own per-container slirp) were never
affected, which is why it took a real workload with a port to surface it.

### Root cause

v0.34.1 (`a112754`) moved the ingress sockets out of `DELONIX_ROOT` into `runtime_dir()`
(`/tmp/delonix-net-<uid>`) to stop a deep `DELONIX_ROOT` from blowing the 108-byte `sun_path`
limit. That introduced a **second uid-derived path** — and only the first one was being pinned
across the privilege boundary.

`--net <custom>` publishes ports from the **2nd re-exec pass**, which runs inside the holder's
userns via `nsenter -U … ip netns exec`, where our uid is mapped to **0**. `reexec_into_netns`
passed `DELONIX_ROOT` explicitly (exactly because `base_root()` consults `geteuid()`), but nothing
pinned the new socket dir. So the re-exec'd process resolved `runtime_dir()` for uid 0 —
`/run/delonix-net` — and `slirp_add_hostfwd` spent its retry budget on a directory that does not
exist. Before v0.34.1 the sockets were `ingress_dir()`-derived, i.e. covered by the
`DELONIX_ROOT` that was already being passed; pinning the root alone had silently stopped being
enough.

The failure was invisible to the test suite for a structural reason worth recording: the divergence
only exists **across a userns boundary**, in a child process, and no unit test can map a uid.

### Fix

- New `infra::runtime_dir_env() -> (&'static str, PathBuf)` — the single accessor for pinning
  `runtime_dir` on a child that runs with a different uid view. Returned as one pair so no caller
  can pass a var/value mismatch, and so `grep runtime_dir_env` finds every child that needs it.
  `start_holder` (which had this right already) now uses it too, so there is one source of truth.
- `cmd::container::reexec_env(id, ip)` — one env list shared by **both** re-exec sites
  (`reexec_into_netns` for `run`, `reexec_start` for `start`), so a third one cannot be added with
  half of it missing. Regression test asserts both uid-derived paths are pinned, and that the
  runtime dir is pinned to *our* value rather than left to the child.

### Validated live

Reproduced on the reporting host with v0.34.1 (`run --net <custom> -p 18069:80` → the exact error),
then with the fix: the container starts, `curl 127.0.0.1:18069` returns **HTTP 200**, and a
`stop` + `start` cycle (the second re-exec site) serves **HTTP 200** again.

### Known, unchanged

The ingress `refcount`/`refs` leak is still open — a container whose start fails mid-flight can
leave a ref behind for an id that no longer exists. It is harmless to traffic (it only delays the
infra teardown) and predates this release.

---

## v0.34.2 — a holder left behind by an in-place upgrade now says so (instead of a bare `ENOENT`)

Bug fix release, from a real report on a live host: `delonix cluster create --name dev` failed at
`✗ Preparing nodes (1)` with nothing but

```
error system call `control socket` failed: No such file or directory (os error 2)
```

The runtime itself was fine. What was broken was the *state of the host*: the ingress netns holder
running there had been started by a **pre-v0.34.2 binary**, and v0.34.1 moved the control socket
(`a112754`, "decouple the ingress control/slirp sockets from `DELONIX_ROOT`'s length") from
`<DELONIX_ROOT>/ingress/control.sock` to `/tmp/delonix-net-<uid>/control.sock`. Upgrading the
binary in place — the normal `install.sh` flow — leaves the *old* holder alive, bound to a path
the *new* binary never looks at.

### Why nothing caught it

`status()` decides the infra is `up` by reading **pidfiles**, never by checking reachability. So:

- `ensure_up()` saw `up` and returned early — no respawn, no complaint.
- `control_query()`'s fast-fail saw `holder_pid = Some(...)` and proceeded, then burned its 50 ×
  40 ms retry budget connecting to a path that would never appear, and reported the raw
  `ENOENT` from the last attempt.

Both were reasonable in isolation, and together they turned "your holder is from the previous
build" into an error message with no subject, no path, and no recovery.

### Fix

`stale_holder_message` (pure, tested): when the holder is **alive but its control socket is
absent**, the error now names the pid, the socket that is missing, the likely cause, and the exact
recovery — and, when the pre-v0.34.2 socket is still on disk (which *proves* an in-place upgrade,
rather than guessing at it), it says that outright and names both paths:

```
error system call `control socket` failed: ingress holder (pid 17552) is alive but
`/tmp/delonix-net-1000/control.sock` does not exist: it is bound to
`/home/w/.local/share/delonix/ingress/control.sock` instead — the path the control socket used
BEFORE v0.34.2, so this holder was started by an older delonix build (in-place upgrade). Restart
the infra to recover: `delonix net netns down` (kills holder + slirp by pidfile, so it works
whatever build started them; the next command respawns both), then
`delonix container restart <name>` for each container on the SDN — they keep running but lose
their veth along with the old netns.
```

Checked in two places, because they are reached by different paths: `ensure_up()` (every setup
path — fails at the entry point instead of at the first attach, with a ~2s grace for the
legitimate startup race where another process spawned the holder microseconds ago and hasn't
`bind`ed yet) and `control_query()` after its retries are exhausted (the teardown paths don't go
through `ensure_up`, and a socket that never *appeared* is a different failure from one that
exists and refuses the connection).

**Deliberately not auto-healing.** Killing a live holder frees its netns and drops the network of
every container attached to the SDN — an operator decision, not something a `cluster create`
should do behind your back. So this release makes the condition *loud and actionable*, it does not
"fix" it by itself.

`teardown()` (i.e. `delonix net netns down`) now also removes the pre-v0.34.2 socket paths: it is
the command that recovers a host from exactly this situation, so it must not leave a socket behind
from the build it just killed — a leftover legacy file would make a *later* diagnosis blame an old
binary that is no longer running.

### Recovering a host that already hit this

```bash
delonix net netns down                 # kills the stale holder + its slirp (works on any build)
delonix container restart <name>       # for each container with an SDN IP (`delonix container ls`)
delonix cluster create --name dev      # the original command now proceeds
```

Containers on `--net host`/`--net none` are unaffected: their published ports come from their own
per-container slirp, which has no relationship with the holder.

---

## v0.34.1 — ingress control/slirp sockets no longer break under a deep `DELONIX_ROOT`

Bug fix release, found live while validating an unrelated feature under a deliberately deep
`DELONIX_ROOT` (a nested test/scratch path): `container run --net <custom>` (and anything else
that brings up the rootless ingress infra) failed with `system call "control socket" failed:
path must be shorter than SUN_LEN`.

### Root cause

`slirp_sock_path`/`control_sock_path` nested their AF_UNIX sockets directly under `ingress_dir()`
— itself derived from `DELONIX_ROOT`. Linux caps a bound socket's `sun_path` at 108 bytes
(`SUN_LEN`); `DELONIX_ROOT` itself (a regular directory) has no such limit (`PATH_MAX`, ~4096).
The two were sharing a length budget they never should have — the exact separation Docker/
Podman/containerd already make (`/run/docker.sock`, `/run/podman/podman.sock`, never nested
under `--data-root`).

### Fix

New `runtime_dir()`, used only by the two sockets — `/tmp/delonix-net-<uid>` for rootless,
`/run/delonix-net` for real root (currently unreachable by this specific code path — real root
never goes through this module's holder at all). `DELONIX_ROOT` and everything under it (VMs,
containers, images) is completely unaffected; pidfiles/status/lock stay exactly where they were.

A second real bug was found landing this fix, before settling on `/tmp`: the more conventional
`$XDG_RUNTIME_DIR`/`/run/user/<uid>` was tried first — `setup_infra_netns()` remounts `/run` as a
fresh, empty tmpfs *inside* the holder's own mount namespace (so containers get a private
`/run/netns`), which makes anything under `/run` that the *parent* created invisible to the
*holder* afterwards. Confirmed live via `ENOENT` binding a socket in a directory that
demonstrably existed on the host's real `/run`. `/tmp` is a separate mount, untouched by that
remount.

Same shape as the existing `DELONIX_ROOT` fix already in this file: the parent resolves
`runtime_dir()` once and passes it explicitly via `DELONIX_NET_RUNTIME_DIR` to the holder, since
the holder's uid maps to 0 inside its own userns — an independent `geteuid()`-based computation
there would diverge from the parent's.

### Validação

Build/clippy/test limpos no workspace inteiro. Validado ao vivo: reproduziu-se a falha original
com um `DELONIX_ROOT` construído especificamente para exceder o `SUN_LEN`, confirmou-se que
`container run --net <custom>` passa a funcionar — o container ganha um IP real da SDN e faz
ping ao gateway — com os sockets a viverem no caminho curto `/tmp` enquanto pid/status/lock
continuam sob o `DELONIX_ROOT` (profundo) como antes.

---

## v0.34.0 — `container run -w/--workdir`, `compose` gets `working_dir:` and random host ports

Continuação directa da revisão de gaps Docker/Podman/Delonix — dois itens genuinamente "dívida
real, sem tocar em rootless/daemonless" fechados nesta versão.

### `container run -w/--workdir` (nova flag — gap do motor inteiro, não só do compose)

`container run` ganha `-w`/`--workdir`. Isto fecha um gap que afectava o motor inteiro, não só o
`compose`: `c.workdir` já era aplicado correctamente no `chdir()` do processo de init (antes do
`execve`, depois do `pivot_root`) — só nunca havia forma de o **definir** a partir de fora da
imagem no momento do `run`; só `exec -w` tinha um override, e só por-chamada. Com o fix, `compose`'s
`working_dir:` (antes aceite mas ignorado, com aviso) passa a usar exactamente o mesmo caminho —
`RunOpts.workdir`.

Validado ao vivo: `container run -w /tmp alpine pwd` → `/tmp`; um serviço compose com
`working_dir: /opt/app` → `pwd` dentro do container confirma `/opt/app`.

### `compose`: porta sem host explícito já não é recusada

`ports: ["80"]` (forma curta, sem `:`) ou a forma longa com `published` omitido — antes recusados
("random assignment not supported in v1") — passam a ganhar uma porta livre real do host
(`free_host_port`: bind à porta 0, o kernel escolhe, liberta-se de imediato — a mesma técnica que
qualquer atribuição aleatória de porta usa). Limitação inerente e aceite: há uma janela TOCTOU
entre encontrar a porta livre e o container a publicar de facto — o mesmo compromisso que
qualquer sistema com esta técnica aceita, dentro ou fora deste motor.

Validado ao vivo: `compose up` com `ports: ["80"]` publicou numa porta real e alcançável,
confirmado por `container port`.

### Não tentado nesta versão, documentado porquê

Volumes anónimos do compose (o outro gap desta categoria) ficam de fora deliberadamente — a
semântica de limpeza (um `down` simples remove um volume anónimo, ou só `down -v`?) merece ser
pensada com calma, não decidida às pressas antes de uma publicação.

### Validação

Build/clippy/fmt/test limpos no workspace inteiro (304 testes em `delonix-runtime-bin`, +1 desde
v0.33.0 — o novo teste de `resolve_ports` para porta aleatória). Validado ao vivo (ver acima).

---

## v0.33.0 — `container update --memory/--cpus`, network create rollback, `delonix-vm` locking

Três itens da lista de dívida arquitectural documentada saíram do "dívida conhecida" para "feito"
— a mesma fonte de trabalho que a v0.32.x já vinha fechando ao testar sistematicamente cada grupo
de comandos antes da publicação. Os três seguintes eram genuinamente correcção de engenharia, sem
tocar em nenhuma fronteira de privilégio/namespace.

### `container update --memory/--cpus` (nova funcionalidade — e um bug real corrigido ao ligá-la)

`delonix container update` ganha `--memory`/`-m` e `--cpus`/`-c`, ao lado das flags já existentes
(`--publish-add`/`--net-connect`/`--net-rate`/...): reescreve o limite de memória/CPU de um
container **a correr**, sem o parar.

A função do motor (`runtime::update_limits`) já existia — rotulada "`docker update`" no seu
próprio doc-comment — mas nunca tinha um único chamador em todo o histórico do repositório.
Ligá-la revelou um bug real, exactamente o mesmo padrão já visto com `mount_live`/`set_net_rate`
noutra sessão: `update_limits` calculava o cgroup alvo por `Container::cgroup()`, a fórmula
ESTÁTICA só válida em modo root; em rootless delegado (o caminho normal), o cgroup real vive num
caminho descoberto em runtime via `/proc/<pid>/cgroup` — exactamente a razão de existir de
`live_cgroup()`, já usada correctamente por `pause`/`unpause`. Sem o fix, `container update
--memory ...` dizia "actualizado", o registo mudava, mas o cgroup REAL do container ficava
intocado — só um `restart` aplicava o novo limite de facto.

Corrigido trocando `container.cgroup()` por `live_cgroup(container)`. Validado ao vivo: `run -m
64M --cpus 0.5` → `update --memory 128M --cpus 1.0` muda `memory.max`/`cpu.max` do cgroup real de
imediato, sem `restart`.

### `network create` já não deixa registo órfão numa falha parcial

O driver `bridge` fazia `store.create(name)?` (declarativo) e só DEPOIS `infra::
network_create_with(...)?` (físico); se o segundo falhasse, o registo do primeiro ficava órfão —
`network ls` mostrava a rede, nada conseguia anexar-se, e um retry falhava com "already exists"
até um `network rm` manual. Corrigido: uma falha na realização física agora remove o registo
recém-criado antes de propagar o erro. `overlay` mantém a sua limitação pré-existente e separada
(não coberta por esta correcção).

### `JsonStore<T>` ganha um `update` genérico — fecha a janela de escrita perdida do `delonix-vm`

Novo `JsonStore::update` (mesmo padrão do já existente `Store<Container>::update`: `flock`,
re-leitura sob o lock, aplica a mutação, grava), generalizado por tipo. `delonix_vm::status()` —
que fazia `load`→mutar(IP)→`save` sem lock nenhum, a correr concorrentemente com o refresh de
métricas em background do dash/`delonix-mgmt` a par de um `vm start/stop/create` da CLI — passou a
usar este primitivo. Provado com um teste de concorrência real (threads + janela de corrida
explícita): sem lock perdem-se escritas, com lock não.

### Validação

Build/clippy/fmt/test limpos no workspace inteiro (306 testes em `delonix-runtime-core`+`-bin`
juntos, +3 desde v0.32.2 — os novos testes de concorrência do `JsonStore::update`). Validado ao
vivo: `container update --memory/--cpus` a mudar o cgroup real sem `restart`; `network create`
(caminho feliz) inalterado; `vm ls` idêntico depois da mudança em `status()`.

---

## v0.32.2 — i18n: 380+ strings de UI que ainda vazavam PT em EN por omissão

Achado a testar sistematicamente cada grupo/subcomando da CLI antes da publicação pública: para
além do `manifest.rs` isolado corrigido na v0.32.1, uma varredura completa do binário revelou que
a mesma classe de bug — texto português hardcoded fora do catálogo `pt.po`, visível mesmo com
`delonix` a correr na língua por omissão (EN) — estava espalhada por **26 ficheiros** de
`crates/delonix-runtime-bin/src/cmd/`. Nada disto muda comportamento; é 100% superfície de texto.

### O que estava mal

Exemplos reais confirmados ao vivo antes da correcção:

- `delonix cluster apply` sem manifesto: `error invalid argument: sem manifesto: passa -f
  <ficheiro> ou cria um ./delonix-manifest.yaml` — em EN por omissão, sem `--l18n=pt`.
- `delonix volumes inspect <nome>`: toda a saída em português (`nome:`/`criado:`/`uso:`).
- `delonix network inspect <nome>`: idem (`nome:`/`driver:`).
- `delonix net httproute ls`: `httproute: proxy parado (nenhum HTTPRoute activo)`.
- `delonix cluster kube generate --help`: o próprio texto de `--help` só existia em português no
  código-fonte — nunca haveria versão EN, com ou sem `--l18n`.

### A correcção

**380 strings** (376 numa varredura de agentes em paralelo + 4 encontradas numa 2.ª passagem
manual nas respostas HTTP do reverse-proxy L7) movidas para o padrão já estabelecido do projecto:
string EN na fonte, envolvida em `po::t`/`po::tf`, com a tradução portuguesa original relocada
para `crates/delonix-runtime-bin/data/pt.po` (352+ entradas novas no catálogo, sem colisões de
`msgid`). Duas armadilhas de concordância de género apanhadas e evitadas antes de entrarem no
catálogo (ex.: "created" → *criada* quando o sujeito é "a rede", mas *criado* quando é "o
volume" — a partilhar a mesma chave teria mistraduzido um dos dois em silêncio).

**Não tocado, por desenho**: nomes de função/asserts só usados em `#[cfg(test)]` (nunca vistos
por um utilizador), doc-comments `///`/`//!` (já traduzidos automaticamente via
`po::translate_help`), e o conteúdo dos ficheiros GERADOS por `delonix stack init`/`vm init`/etc.
(`scaffold.rs`) — esses são exemplos/config em português deliberadamente, para o utilizador editar,
não texto de interface.

### Documentação

Aproveitando a varredura, três gaps reais na doc pública (`docs/gen.py` → `docs/comandos/`):

- **`delonix compose` não tinha página nenhuma** — um grupo de comandos inteiro (suporte nativo a
  `docker-compose.yml`, desde a v0.29.0) invisível no site. Adicionado.
- **`delonix serve docker-api`** descrito como "só leitura" — desde a v0.26.0 suporta o ciclo de
  vida completo dum container (`create`/`start`/`stop`/`kill`/`wait`/`restart`/`rename`/`rm`).
  Corrigido.
- **`delonix cluster kubeadm`** descrito como sem suporte a HA multi-control-plane — o
  provisionamento automático de HAProxy existe e está validado ao vivo desde a v0.13.0. Corrigido,
  com nota também sobre `--etcd-cluster`.
- `delonix network` descrito como só realizando fisicamente o driver `bridge` — o `overlay`
  (VXLAN+WireGuard) também é realizado fisicamente desde há várias versões, só `macvlan`/`ipvlan`
  ficam por implementar. Corrigido.
- `delonix dash` sem menção aos KPIs de RAM/rede/disco, `--json`, ou `/metrics` Prometheus
  (trabalho da v0.31.0/v0.32.0). Corrigido.

### Validação

Build/clippy/fmt/test limpos no workspace inteiro (303 testes em `delonix-runtime-bin`, mesma
contagem da v0.32.1 — só texto mudou, zero lógica). Validado ao vivo neste host: `cluster apply`,
`volumes inspect`, `network inspect`, `net httproute ls`, `cluster kube generate --help` — todos
em EN por omissão e em português exacto via `--l18n=pt`, incluindo o alinhamento de colunas do
`volumes inspect` (as etiquetas mudaram de comprimento entre línguas; cada template mantém o seu
próprio espaçamento).

---

## v0.32.1 — `secret create` via stdin, mensagens "no such X" correctas, doc do `stack init` actualizada

Continuação directa da revisão v0.32.0: enquanto se testava sistematicamente cada grupo de
comandos da CLI antes da publicação pública, três problemas reais apareceram — dois bugs de
utilizador visíveis logo no primeiro uso, e uma doc gerada desactualizada.

### `secret create` — stdin já funciona (bug real, achado ao testar o próprio cheatsheet)

O cheatsheet dos docs mostrava `printf 's3nha' | delonix secret create db-pass` como forma
"segura" de criar um segredo sem o valor passar pelo argv/histórico do shell — mas `secret
create` só aceitava `--from-literal KEY=value` ou `--from-env-file <caminho>`, sem nenhuma via de
stdin. O comando do próprio exemplo falhava com "segredo vazio". Corrigido: `--from-env-file -`
lê de stdin (convenção `-` = stdin, a mesma de dezenas de outras CLIs), interpretado no mesmo
formato `KEY=value` de um ficheiro `.env` normal. Cheatsheet actualizado (`docs/gen.py`) para o
exemplo real e testado: `printf 'password=s3nha' | delonix secret create db-pass
--from-env-file -`.

### Mensagens de erro "no such X" com o substantivo correcto

`Error::NotFound`, partilhado por secrets/redes/volumes/imagens/imagens-VM/clusters, tinha o
texto de exibição fixo em `"no such container: {0}"` independentemente do recurso — um `delonix
secret rm <inexistente>` respondia literalmente **"no such container: secret X"**. Cada um dos
outros stores já embutia o seu próprio prefixo correcto na string (`"secret {name}"`, `"network
{name}"`, ...) — só os dois pontos de `Store<Container>` dependiam da formatação fixa da antiga
Display. Corrigido na raiz (`Error::NotFound` passou a `"no such {0}"`, genérico) + os dois
call-sites de `Store<Container>` passaram a fornecer o prefixo `"container: "` deles, preservando
byte-a-byte a mensagem já existente para containers. Validado ao vivo: `secret rm`/`volumes
inspect`/`network inspect` num recurso inexistente agora nomeiam o recurso certo; `container
rm`/`stop` num container inexistente continua igual (sem regressão).

### `stack init` já não descreve `network:` como limitado a root

O manifesto gerado por `delonix stack init` continha um comentário a dizer que `network: <rede>`
"tem uma limitação CONHECIDA em rootless — o `setns` falha... só funciona como root". Essa
limitação foi fechada há várias versões (re-exec `nsenter ... ip netns exec` para dentro da netns
do holder, ver `reexec_into_netns`/AGENTS.md) — o comentário ficou desactualizado e estava a
empurrar utilizadores novos para `--net host` sem necessidade. Confirmado ao vivo antes de
corrigir (`container apply` com `network: <rede-existente>` ganhou IP real em rootless) e o
comentário do scaffold foi reescrito para reflectir o estado actual: `network:` funciona em
rootless, `--net host` continua a ser o default do scaffold só por simplicidade (zero passo
`network create` extra num projecto que tem de funcionar à primeira).

### Validação

Build/clippy/fmt/test limpos no workspace inteiro. Validado ao vivo neste host: `secret create`
via stdin (criar + inspect --reveal + rm), `secret rm`/`volumes inspect`/`network inspect` num
recurso inexistente com a mensagem certa, `container apply -f` com `network: dlx-dev` a ganhar IP
real em rootless.

---

## v0.32.0 — revisão ampla de código/arquitectura: 7 bugs reais corrigidos

Pedido explícito antes da publicação pública: revisão de código E arquitectura sobre todo o
repositório (não só segurança), com foco redobrado no código mais recente (dashboard/métricas,
`compose.rs`, a reorganização da CLI). Quatro auditorias independentes em paralelo encontraram e
corrigiram 7 bugs reais.

### `compose.rs` — 4 correcções

- **`depends_on: condition: service_completed_successfully` já não hangs para sempre.** Ganhou um
  tecto generoso (30 min) + heartbeat de progresso a cada 30s — antes era um `loop {}` sem saída a
  não ser Ctrl-C se a dependência nunca terminasse (condição errada, ou `restart:always` a
  reciclar de volta para Running).
- **Porta `host_ip:host:container` (ex.: `127.0.0.1:9000:80`) já não descarta o IP em silêncio.**
  Passa a recusar explicitamente — o motor já recusa o mesmo em `-p`; descartar em silêncio
  publicava a porta em TODAS as interfaces, o oposto de um bind a loopback.
- **Nomes `<projecto>_<chave>` de rede/volume já não colidem entre projectos diferentes.** Uma
  codificação livre de prefixo (`compose_scoped_name`) substitui a concatenação simples — antes,
  `compose down` de um projecto podia apagar o recurso de OUTRO projecto com nomes que colidissem.
- **`shlex_split` segue agora a regra POSIX exacta para backslash dentro de aspas duplas** — só
  `$ \` " <newline>` são especiais; qualquer outro carácter mantém o backslash (antes, TODOS eram
  tratados da mesma forma, mudando em silêncio o argv de comandos com padrões tipo `\d+`).

### `firewall.rs` — corrida real de lost-update corrigida

Todos os pontos de mutação (`ingress`/`egress` allow/deny/rm/clear/policy, manifesto, Dependency)
faziam `load` → mutar → aplicar no kernel → `save` sem NUNCA passar pelo `flock` do `Store` —
`Store::update` existe precisamente para isto, só nunca tinha sido usado aqui. Corrida real: dois
comandos de firewall contra o mesmo container aplicavam ambos no kernel com sucesso, mas só o
último `save` sobrevivia — a regra "perdedora" ficava viva no `nft` mas desaparecia do registo, e
some silenciosamente no próximo `container start`. Corrigido com um `update_locked` novo (envolve
`Store::update` para um closure que pode ele próprio falhar). Validado ao vivo ponta-a-ponta.

### Dashboard/métricas — dois bugs do v0.31.0 corrigidos

- **Timeout na colheita cara** (rede+disco): nem a thread do TUI nem a tarefa de refresh do
  `delonix-mgmt` tinham qualquer tecto — uma operação de I/O genuinamente presa (NFS pendurado,
  `nsenter` preso) congelava o refresh para sempre. `collect_with_timeout` (120s) resolve, com
  leak deliberado da thread presa em vez de um hang permanente.
- **Tráfego de containers `--net host`/`--net none` já não conta como "0 bytes" em silêncio** —
  novo campo `network_unmeasured_containers`, visível no tile do dash, no JSON, e numa gauge
  Prometheus nova, em vez de somado silenciosamente como zero.

### Consolidação

`peer_uid()` (extracção `SO_PEERCRED`) estava duplicado verbatim em 4 crates — consolidado em
`delonix_runtime_core::peer_cred::peer_uid`.

### Dívida documentada (não corrigida nesta versão)

Três achados de arquitectura sem bug ao vivo reproduzido — documentados em AGENTS.md em vez de
corrigidos apressadamente antes da publicação: rollback em falha parcial de `network create`,
`delonix-vm`'s store sem lock/update, e `spawn()` como função de ~405 linhas.

### Validação

Build/clippy/fmt/test limpos no workspace inteiro (303 testes, +10 desde v0.31.1). Validado ao
vivo neste host: firewall allow→deny→rm→policy→clear ponta-a-ponta com estado persistido a
conferir com o `nft` real; `--cpu-weight` confirmado na leaf do cgroup; `/metrics` com a gauge
nova; `dash --json`/`--once`.

---

## v0.31.1 — cpuset/cpu.weight/io.weight passam a aplicar-se no cgroup rootless-delegado

Fecha o único gap "importante" que continuava genuinamente aberto na análise Docker/Podman: o
cgroup rootless-delegado (o modo NORMAL em rootless, via `systemd --user` com `Delegate=yes`)
ignorava `--cpuset`/`--cpu-weight`/`--io-weight`.

### Corrigido

`try_delegated_base` (`crates/delonix-runtime/src/lib.rs`) já activava os controladores `+cpuset`/
`+io` no `subtree_control` da base delegada, mas nunca escrevia `cpuset.cpus`/`cpu.weight`/
`io.weight` na leaf do container — só `memory.max`/`pids.max`/`cpu.max`. O caminho não-delegado
(root) já aplicava os três correctamente; só o delegado ficava a meio. Corrigido com os mesmos
três `fs::write` best-effort que o caminho root já usa.

**Validado ao vivo** (host real, sessão `user@1000.service`): um `container run --cpu-weight 500`
confirmou `cpu.weight=500` na leaf real do cgroup — o controlador `cpu` está delegado neste host.
`cpuset`/`io` continuam sem confirmação ao vivo aqui especificamente: este host não delega esses
dois controladores ao `user@.service` (confirmado tentando forçar com `systemd-run --user --scope
-p Delegate=cpuset`, que devolveu só `cpu memory pids`) — limite de systemd/distro, não do código.
O `fs::write` fica best-effort nesse caso, tal como o caminho root já aceita.

Novo teste de regressão puro (`try_delegated_base_aplica_cpu_weight_cpuset_e_io_weight_na_leaf`) —
`try_delegated_base` só faz `fs::write`/`fs::create_dir_all` contra o `base` recebido, por isso
testa-se com um directório temporário simples, sem precisar de um cgroupfs real.

### Validação

Build/clippy/fmt/test limpos no workspace inteiro. `docs/COMPARACAO-DOCKER-PODMAN.md` actualizado —
já não há nenhum gap "importante" de cgroups em aberto, só o `--format` Go-template (bloqueante
isolado de scripting/CI) e a triagem dos 10 achados candidatos menores da auditoria original.

---

## v0.31.0 — KPIs de RAM/rede/storage no dashboard, Prometheus, `dash --json`

Pedido directo do utilizador ao ver o `delonix dash`: dashboard bonito mas sem KPIs dinâmicos de
consumo (RAM, tráfego, disco), a barra de actividade sem informação de uptime, texto em PT por
omissão (bug), e nenhuma forma de alimentar Grafana/outras ferramentas de plataforma.

### Novo — KPIs de recursos + `GET /metrics` (Prometheus) + `GET /v1/dash` (JSON)

Um único colector novo, `delonix-mgmt::dashstats::collect`, alimenta o TUI, o `delonix dash --json`
e o scrape Prometheus — nunca divergem na aritmética.

- **Tiles novos** no `delonix dash`: MEMORY (uso real do cgroup slice), TRAFFIC (soma de bytes
  rx/tx por-container) e STORAGE (uso de disco por área: imagens/volumes/VM-images/containers).
- **`delonix-mgmt` (`delonix serve api`)** ganhou `GET /v1/dash` (o mesmo resumo em JSON) e gauges
  novas em `GET /metrics`: `delonix_containers_running/total`, `delonix_vms_running/total`,
  `delonix_memory_bytes_used/limit`, `delonix_network_rx/tx_bytes`,
  `delonix_storage_bytes_{images,volumes,vm_images,containers}` — o caminho nativo para o Grafana
  (Prometheus/REST, não gRPC — este último não é consumido nativamente pelo Grafana).
- **Coluna `UP`** na tabela de recursos do dash — uptime real por-container (mesmo mecanismo do
  `container ls`).
- **Sparkline com alternância** — tecla `m` troca entre "containers a correr" e "memória usada"
  no gráfico dos últimos 2 minutos (antes só mostrava a contagem de containers).
- **`delonix dash --json`** — um snapshot só, sem TUI, para scripts/CI ou um datasource JSON.

### Corrigido — dois bugs reais encontrados a validar ao vivo antes de publicar

1. **Bug de custo**: a soma de uso de disco percorre `containers/` — em rootless cada container
   tem uma cópia FLAT completa do rootfs. Medido neste host (49 containers, vários nós
   `kindest/node` completos): **68 GiB, mais de um minuto** de I/O. Calcular isto em linha
   bloquearia o TUI a cada tick e estouraria o timeout de scrape do Prometheus (10s por omissão).
   Corrigido com colheita separada: o TUI faz a 1ª snapshot só com os campos baratos (instantâneo)
   e delega os campos caros a uma thread própria em background (refrescada a cada 15s); o
   `delonix-mgmt` faz o mesmo com uma tarefa `tokio` a cada 30s — o scrape `/metrics` fica sempre
   rápido (confirmado ao vivo: ~0.15s).
2. **Bug de i18n**: `cmd/dash.rs` tinha 100% do texto de utilizador hardcoded em PT no código-fonte
   — nunca usava o catálogo `pt.po`, ao contrário do resto da CLI. O dashboard aparecia em
   português mesmo sem `--l18n=pt`. Corrigido: fonte 100% EN + traduções no `pt.po` (incluindo um
   gap pré-existente e não relacionado, o about-text do `docker-api`).

### Validação

Build/clippy/fmt/test limpos no workspace inteiro. Validado ao vivo neste host: `dash --json`/
`--once` correctos (~57s, colheita completa incluindo o scan de disco); TUI a arrancar em segundos
(confirmado pelo estado do processo, não mais bloqueado em I/O); `delonix serve api` real com
`/metrics` (~0.15s, gauges caras preenchidas após a 1ª passagem em background) e `/v1/dash` (JSON
completo, ~33s) via socket unix real.

---

## v0.30.1 — acabamentos do v0.30.0: tradução pt + site de docs actualizado

Duas correcções de acompanhamento à reorganização da CLI do v0.30.0, sem mudança nenhuma de
comportamento — ficaram de fora do commit da tag anterior por serem passos posteriores do mesmo
trabalho (tradução + regeneração do site), não código novo.

### i18n — `delonix net`/`delonix serve`/`delonix cluster kube` traduzidos em `--l18n=pt`

As strings de about-text dos dois grupos novos (`net`/`serve`) e do texto actualizado de
`cluster`/`cluster kube` degradavam para EN sob `--l18n=pt` por falta de entrada no `pt.po`.
Acrescentadas as 5 entradas em falta — incluindo `docker-api`, que nunca tinha tido tradução
própria mesmo antes desta reorganização (gap pré-existente, fechado de caminho).

### Site de documentação (`docs/*.html`) regenerado para os novos caminhos

O gerador (`docs/gen.py`) invocava o `--help` de vários grupos pelo nome antigo e plano
(`netns`/`cri`/`kube`/...), que deixou de existir na raiz depois da reorganização — as páginas
`docs/comandos/{netns,flow,ingress,egress,httproute,tunnel,boot,cri,docker-api,kube}.html`
mostravam `error: unrecognized subcommand`. Corrigido com um mapa `GROUP_PATH` (nome da página →
argv real de hoje); títulos e exemplos hardcoded (cheatsheet, tutorial) também actualizados para
`delonix net ...`/`delonix serve ...`/`delonix cluster kube ...`.

### Validação

Build/clippy/fmt/test limpos no workspace inteiro (sem mudança de código de motor/CLI — só
catálogo `pt.po` e o gerador Python do site). `--l18n=pt` confirmado a mostrar texto real para os
três grupos novos; site regenerado contra o binário desta release, zero `unrecognized subcommand`
em qualquer página.

---

## v0.30.0 — Reorganização da CLI (BREAKING), progresso no `pull`, `ls-remote` em tabela

Três pedidos directos do utilizador, todos vindos de uso real do binário: a raiz do `delonix`
tinha crescido demasiado plana, o `pull` não dava feedback nenhum durante o download, e o
`ls-remote` imprimia as tags cruas sem coluna nenhuma.

### BREAKING — raiz da CLI reorganizada (agrupamento profundo, corte limpo, sem aliases)

A raiz do `delonix` tinha 26 subcomandos lado a lado — fácil invocar por engano um sub-comando de
baixo nível (`netns`, `cri`, `kube`, ...) como se fosse um comando principal. Reorganizado em dois
grupos novos + um existente que ganhou uma faceta:

- **`delonix net <x>`** — plumbing de rede/infra de baixo nível: `netns`, `flow`, `ingress`,
  `egress`, `httproute`, `tunnel`, `boot`.
- **`delonix serve <x>`** — os três "serve um protocolo num socket unix": `cri`, `api`,
  `docker-api`.
- **`delonix cluster kube generate`** — o antigo `delonix kube generate` passou a viver dentro de
  `cluster`, ao lado de `cluster apply`/`cluster kubeadm`.

Pura reorganização de roteamento — cada comando delega exactamente na mesma função `run()` de
sempre, **zero mudança de comportamento**, só o caminho da CLI para lá chegar. `delonix
ingress-proxy` (subcomando OCULTO, o processo interno do proxy L7) ficou de fora de propósito —
não aparece no `--help` e mexer no seu argv arriscava o mecanismo de re-exec que já usa.

**Sem aliases de retrocompatibilidade** (pedido explícito do utilizador: "corte limpo") — um
script/pipeline que invoque a forma antiga falha com `unrecognized subcommand`, nunca em silêncio.

Mapeamento completo antigo → novo:

| Antigo | Novo |
|---|---|
| `delonix netns ...` | `delonix net netns ...` |
| `delonix flow ...` | `delonix net flow ...` |
| `delonix ingress ...` | `delonix net ingress ...` |
| `delonix egress ...` | `delonix net egress ...` |
| `delonix httproute ...` | `delonix net httproute ...` |
| `delonix tunnel ...` | `delonix net tunnel ...` |
| `delonix boot ...` | `delonix net boot ...` |
| `delonix cri ...` | `delonix serve cri ...` |
| `delonix api ...` | `delonix serve api ...` |
| `delonix docker-api ...` | `delonix serve docker-api ...` |
| `delonix kube generate ...` | `delonix cluster kube generate ...` |

**Mecanismo interno confirmado intocado**: o re-exec de `--net <rede-custom>`
(`container::reexec_into_netns` → `nsenter … ip netns exec`) e o holder netns usam interceção de
`std::env::args()` crua em `main()`, ANTES do parsing `clap` — não passam pelo enum `Cmd` público
de todo. Validado ao vivo neste host: um `container run --net <rede-existente>` continua a
ganhar IP real na SDN depois da reorganização.

### Melhorias de UX

- **`delonix image pull`/`vm pull`/`image --vm pull` ganharam progresso por-layer** (estilo
  `docker pull`): uma barra por layer com bytes transferidos/total e percentagem, em vez do único
  log inicial que fazia o download parecer preso. Validado ao vivo com um pull real de 7 layers.
- **`delonix vm ls-remote`/`image vm ls-remote`/`image --vm ls-remote`** passaram a imprimir uma
  tabela (`output::Table`, coluna `TAG`) em vez de uma tag crua por linha.

### Validação

Build/clippy/fmt/test limpos no workspace inteiro. Validado ao vivo neste host: `--help` da raiz
mostra 18 comandos (antes 26+); os três grupos novos (`net`/`serve`/`cluster kube`) renderizam
correctamente; um container real correu com `--net <rede-existente>` e obteve IP na SDN, provando
que o re-exec do holder continua intacto.

---

## v0.29.0 — `docker-compose.yml` nativo + fecho da Fase 0 de segurança

Fecha o último gap "importante" (não bloqueante, mas de alto valor) da análise Docker/Podman
([docs/COMPARACAO-DOCKER-PODMAN.md](../COMPARACAO-DOCKER-PODMAN.md)) — suporte nativo a
`docker-compose.yml` — e conclui as DUAS auditorias de segurança adversariais independentes que
faltavam desde 2026-07-23, fechando por completo a "Fase 0" do roadmap de produção.

### Novo — `delonix compose up|down|ps|logs|config`

Suporte nativo ao Compose Spec v2.x (`cmd/compose.rs`), um tradutor de esquema estrangeiro da
mesma família de `container::pod_to_run_opts` (Pod k8s) e `dockerapi::docker_config_to_run_opts`
(API Docker) — parser tipado à mão (zero dependência nova), traduzido directamente para `RunOpts`
(containers, via o mesmo `cmd_run` do CLI) ou para `ManifestDoc`s que reaproveitam
`image`/`network`/`volume::apply` tal-e-qual (mesma idempotência, mesmo hardening de input, zero
lógica de criação duplicada).

- **`depends_on`** com as 3 condições reais (`service_started`/`service_healthy`/
  `service_completed_successfully`) — ordenação topológica do grafo de serviços (ciclo → erro
  claro nomeando os serviços envolvidos, nunca uma ordem arbitrária) + espera pelo healthcheck
  real (inline do serviço, ou o da própria imagem se o serviço não declarar um). Zero mudança ao
  schema do motor/store.
- **Projecto** (`compose down`/`ps`/`logs`, escopados) — cada container ganha a label
  `delonix.io/compose-project=<nome>` (mesma ideia de `pod.rs`); redes/volumes (sem campo de
  labels) usam nomeação determinística `<projecto>_<nome>`, a mesma convenção do `docker compose`
  real. `down` reconstrói os mesmos nomes a partir do ficheiro compose reanalisado — sem registo
  próprio, mesma filosofia do `stack describe`/`cluster ls`.
- Cobre `image`/`build`/`environment`/`env_file`/`ports`/`volumes`/`command`/`entrypoint`/
  `healthcheck`/`restart`/`networks`/`labels`/`user`/`cap_add`/`cap_drop`/`privileged`/`tmpfs`/
  `deploy.resources.limits`/`container_name`/`hostname`/`read_only`, top-level `networks:`/
  `volumes:` (incl. `external: true`).
- **Por fazer, sempre com erro claro (nunca silencioso)**: `profiles`/`extends`/`configs`/
  `secrets` top-level (usa `kind: Secret` em vez disso)/multi-ficheiro (`-f a -f b`/`include:`),
  `build.target`, `deploy.replicas≠1`, `networks.*.ipv4_address` fixo, volumes anónimos, porta
  sem host explícito. `working_dir:` é aceite mas AVISA e é ignorado — gap pré-existente do motor
  inteiro, não introduzido por este módulo.

### Validado ao vivo, de ponta-a-ponta (host real, Postgres + app)

`compose up` com um `web` a depender de `db` via `condition: service_healthy`: `web` só arrancou
depois do `pg_isready` do `db` ter sucesso REAL (visível no output do próprio healthcheck); `ps`
mostrou os 2 containers correctos; re-`up` foi idempotente ("already exists, nothing to do");
`logs`/`logs <serviço>` funcionaram (incluindo o log completo de arranque do Postgres); `down -v`
removeu os 2 containers + a rede + o volume sem deixar nada para trás. Um bug real de CLI foi
encontrado e corrigido durante esta validação: `compose logs` tinha um `-f` a colidir entre
`--file` e `--follow` (um `panic` do `clap` só disparado em runtime, nunca detectado por
build/clippy/test — só a validação ao vivo o apanhou).

### Segurança — as duas auditorias independentes que faltavam, ambas feitas nesta janela

1. **Núcleo de syscalls (`delonix-runtime/lib.rs`, 104 `unsafe`) + `delonix-net/infra.rs`** — uma
   correcção de registo importante: uma versão anterior deste doc afirmava estes ficheiros
   "nunca terem tido revisão adversarial", o que estava desactualizado (o AGENTS.md já
   documentava uma auditoria de 2026-07-23 que os cobriu, com 2 CRITICAL + 3 HIGH corrigidos
   nesse dia). A auditoria FRESCA desta janela é a confirmação independente de fora para dentro
   que faltava para essa ronda — **zero achados novos CRITICAL/HIGH**.
2. **Os 6 HIGH da auditoria original de 2026-07-21** — nunca tinham tido um 2º par de olhos
   genuinamente externo. 5/6 confirmados sólidos ao tentar reconstruir activamente cada exploit
   original. O 6º (kubeconfig cluster-admin) tinha um **TOCTOU residual real**: `fs::write`
   cria o ficheiro local no modo do umask (664, medido ao vivo neste host) e só DEPOIS aplica
   `chmod 600` — uma janela real em que outro utilizador local podia ler as credenciais
   cluster-admin. **Corrigido**: `OpenOptions::mode(0o600)` define o modo atomicamente na
   criação (`cmd/cluster.rs::fetch_kubeconfig`), o mesmo padrão que `ensure_libvirt_network` já
   usa noutro ponto do código.

Com isto, a Fase 0 do roadmap de segurança (docs/COMPARACAO-DOCKER-PODMAN.md) está fechada por
completo — não há mais nenhuma peça "por confirmar de fora para dentro" em aberto.

### Validação

Build/clippy/fmt/test limpos no workspace inteiro. 299 testes em `delonix-runtime-bin` (+9
novos: parsing/tradução do compose, ordenação topológica, tokenizador `shlex_split`, parsing de
duração Go, nomes de projecto).

---

## v0.28.0 — GPU real via CDI (`--gpus`/`--device nvidia.com/gpu=...`)

Fecha o último dos 4 gaps "bloqueantes" da análise Docker/Podman
([docs/COMPARACAO-DOCKER-PODMAN.md](../COMPARACAO-DOCKER-PODMAN.md)): passagem de GPU real
(injecção das libs de driver NVIDIA, não só os nós `/dev`), via **CDI (Container Device
Interface, `cdi.k8s.io`/CNCF)** — o mesmo mecanismo que Docker/Podman/containerd/CRI-O reais usam.

### Novo

- **`cmd/cdi.rs`** — um CONSUMIDOR de CDI: parseia specs JSON/YAML já gerados por `nvidia-ctk cdi
  generate` (`/etc/cdi/*.json|yaml`, `/var/run/cdi/*.json|yaml`) e traduz o `containerEdits` de
  cada dispositivo (`deviceNodes`/`mounts`/`env`) para o MESMO `Vec<Mount>`/`Vec<String>` que
  `-v`/`--device` já alimentam — aplicados pelo motor via `bind_volume`/`bind_devices` tal-e-qual.
  Deliberadamente **não** o modelo legacy do hook `nvidia-container-cli configure --pid=<pid>`
  (um 2.º processo a `setns` para o userns/mntns de OUTRO por PID — precisaria de CAP_SYS_ADMIN
  nesse userns alheio, o mesmo problema de privilégio cross-namespace que o `--net <rede-custom>`
  já resolve por re-exec, não por attach externo): aqui os mounts são feitos pelo PRÓPRIO init do
  container, antes do `pivot_root`, **zero modelo de privilégio novo** — o mesmo mecanismo
  rootless que `-v`/`--device` já usam. A descoberta/versão do driver fica 100% dentro do
  `nvidia-ctk`, tal como em qualquer runtime real — este motor nunca reimplementa isso.
- **`--gpus nvidia|all`** (upgrade do flag existente) e **`--device nvidia.com/gpu=<nome|all>`**
  (extensão do `--device` para o formato `vendor.com/class=name` do Docker/CDI real, ao lado do
  formato `/dev/...` já existente). `--gpus dri` fica **inalterado** (bind cru de `/dev/dri/*` —
  Mesa/VAAPI é open-source e já vem no pacote da própria imagem, não é o gap que isto fecha).
- **Erro claro e accionável, nunca silencioso**: sem spec CDI nem `nvidia-ctk` no `PATH`, um
  `--gpus nvidia`/`--device nvidia.com/gpu=...` recusa ANTES de criar seja o que for, com o
  comando exacto para corrigir (`nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml`) — nunca
  cai em silêncio para o bind cru de `/dev/nvidia*` antigo, que falharia a meio com um erro
  confuso do próprio CUDA.
- **`ldconfig -r <rootfs>`** best-effort logo após os mounts em `setup_rootfs` (engine,
  `crates/delonix-runtime`), ainda antes do `pivot_root` — substituto deliberadamente mais simples
  do hook `createContainer` real de um spec CDI (que precisaria do protocolo OCI-hook-stdin-state,
  não implementado); um spec que declare `hooks` avisa (não silencioso) que não foram executados.

### Validado ao vivo (host real, sem GPU)

`--gpus nvidia`/`--device nvidia.com/gpu=all` sem CDI disponível recusam correctamente ANTES de
criar o container (confirmado: nenhum leftover no `container ls -a`); `--gpus dri` continua a
funcionar sem alterações (0 dispositivos encontrados, comportamento normal sem GPU). O parsing e
tradução `containerEdits → Mount/devices/env` está coberto por teste unitário com um spec JSON
real (formato `cdi.k8s.io`).

### Por confirmar num host GPU real (impossível neste sandbox)

A precedência exacta `/etc/cdi` vs `/var/run/cdi`; se `ldconfig -r` chega como substituto
suficiente dos hooks `createContainer` reais de um spec `nvidia-ctk`-gerado.

### Validação

Build/clippy/fmt/test limpos. 290 testes em `delonix-runtime-bin` (+2 novos em `cmd::cdi`).

### Fecha o roadmap dos 4 gaps "bloqueantes"

Com este, os 4 gaps identificados na análise Docker/Podman original (mutações da Docker Engine
API, BuildKit-lite, GPU/CDI, e paridade de verbos CLI) estão todos FEITOS — cada um como uma
fatia v1, com limitações documentadas honestamente em vez de escondidas.

---

## v0.27.0 — BuildKit-lite: `RUN --mount=secret` e `--platform`

Fecha mais um dos gaps "bloqueantes" da análise Docker/Podman
([docs/COMPARACAO-DOCKER-PODMAN.md](../COMPARACAO-DOCKER-PODMAN.md)): segredos de build sem os
bakear numa layer, e builds cross-arch — o mínimo que um pipeline de CI a sério precisa de um
build system moderno.

### Novo

- **`RUN --mount=type=secret,id=<nome>[,target=<caminho>][,required=true|false]`** — o segredo
  (`--secret id=<nome>,src=<ficheiro>` no `delonix build`) é bind-montado AO VIVO no container de
  trabalho só durante a janela desse `RUN` (`runtime::mount_live`/`unmount_live` — o mesmo
  primitivo já provado por `container update --volume-add`, nunca antes exercitado contra o
  container de trabalho do `build`). Como o mount vive só no namespace de montagem já próprio do
  container, é INVISÍVEL do lado do host que o `commit_flat_rootfs`/a cache de layers leem — o
  valor do segredo estruturalmente não pode chegar a uma layer ou a um snapshot de cache.
  Default `target`: `/run/secrets/<id>` (convenção Docker). `required=false` (default, como o
  Docker): um segredo em falta é ignorado em silêncio; `required=true`: erro claro ANTES de
  qualquer trabalho no container. `type=ssh`/`type=cache`/`type=bind` (e qualquer outra flag
  `RUN --xxx=`) dão erro claro — nunca viram texto literal passado ao shell (armadilha que a
  gramática antiga tinha: `RUN --mount=... cmd` sem suporte nenhum virava um "comando não
  encontrado" confuso).
- **`--platform linux/<arch>`** (CLI e `kind: Image`'s `spec.build`) — resolve a imagem base do
  arch pedido (`resolve_or_pull_platform`: só reaproveita uma imagem local se o seu
  `config.architecture` gravado bater com o pedido; caso contrário força um pull fresco da rede —
  conservador de propósito, para nunca resolver em silêncio para o arch errado), e carimba esse
  arch tanto no config OCI como na imagem resultante. `ImageConfig` ganhou o campo `architecture`
  (`#[serde(default)]` para imagens já persistidas antes deste campo existir). **Preflight claro**:
  antes de sequer tentar um `RUN` cross-arch, verifica `/proc/sys/fs/binfmt_misc/qemu-<arch>` — se
  não estiver registado/activo, erro imediato a apontar para `qemu-user-static`/
  `tonistiigi/binfmt --install <arch>` (o mesmo pré-requisito de HOST que o buildx real também
  tem — este motor não instala nem gere binfmt/QEMU, só confirma que já está lá).

### Validado ao vivo (host real)

Um Dockerfile `FROM alpine:3.20` + `RUN --mount=type=secret,id=x cat /run/secrets/x > /marker`:
o valor do segredo foi lido correctamente durante o `RUN` (`/marker` mostra-o), e a imagem final
NÃO tem `/run/secrets/x` (nem um ficheiro vazio — o placeholder que o `mount_live` cria antes de
montar é removido depois do `unmount_live`). Um `required=true` sem `--secret` correspondente
falha com um erro claro, antes de tocar no container. `--platform linux/riscv64` (sem QEMU
registado neste host) falha com o erro de preflight esperado, apontando para a correcção certa.

### Por fazer (deliberadamente fora desta fatia, documentado)

`type=ssh`/`type=cache`/`type=bind`; heredocs; `--cache-from/to`; manifest-list multi-arch no
push (constrói só UM arch por invocação); GPU real via CDI/nvidia-container-toolkit — o último
gap "bloqueante" do roadmap, já com plano desenhado.

### Validação

Build/clippy/fmt/test limpos nos crates tocados (`delonix-image`, `delonix-runtime-bin`) — 288
testes em `delonix-runtime-bin` (+3 novos: `parse_build_secrets`/`parse_platform`/
`valid_secret_id`), 45 em `delonix-image` (+4 novos: parsing de `--mount=type=secret`).

---

## v0.26.0 — mutações na Docker Engine API (`delonix docker-api`)

Fecha mais um dos 4 gaps "bloqueantes" identificados na análise Docker/Podman
([docs/COMPARACAO-DOCKER-PODMAN.md](../COMPARACAO-DOCKER-PODMAN.md)): a API
docker-compatível (`delonix docker-api`) tinha só leitura (`/_ping`/`/version`/`/info`/
`/containers/json`/`/images/json`, v.2026-07-23) — sem `docker run`/`docker compose up`
funcionarem, o `DOCKER_HOST=unix://...` de um `docker` CLI real não servia de nada além de `ps`.

### Novo

- **Mutações de ciclo de vida** — `POST /containers/create|start|stop|kill|wait|restart|rename`,
  `DELETE /containers/{id}`, `GET /containers/{id}/json`. Cada rota delega na MESMA
  `cmd_run`/`cmd_stop`/`cmd_kill`/`cmd_wait`/`cmd_restart`/`cmd_rename`/`cmd_rm` que o CLI já usa —
  zero lógica de motor duplicada. `docker_config_to_run_opts` traduz o `ContainerConfig` JSON do
  Docker (`Image`/`Cmd`/`Entrypoint`/`Env`/`Labels`/`HostConfig.{Binds,PortBindings,RestartPolicy,
  Memory,NanoCpus,Privileged,CapAdd,CapDrop}`) para o `RunOpts` interno.
- **Simplificação deliberada, documentada**: `create` já arranca de imediato (o motor não tem um
  estado "created" dormente à parte) — `start` numa já-a-correr devolve o **304** idempotente que o
  docker real também devolve nesse caso, o que mantém o par `create`→`start` (o que `docker compose
  up` de facto usa) a funcionar sem precisar de um estado dormente novo.
- **`exec`/attach interactivo (HTTP hijacking) continua fora de escopo** desta fatia (não muda o
  scope já documentado na v anterior). **`--restart` é recusado com um erro claro**: a política
  precisa do supervisor `run_supervised`, que faz um `fork()` cru assumindo um chamador
  single-threaded — verdade só para o CLI, não para este servidor `tokio` multi-thread; arriscar
  esse fork podia deadlockar silenciosamente (um lock de outra thread, ex. do alocador, ficaria
  preso para sempre no filho). Usa `delonix container run --restart ...` do CLI para isso.

### 2 bugs reais corrigidos, encontrados a validar ao vivo contra um `docker` CLI real

1. **Zombie permanente** — um container desanexado morto (`docker kill`) ficava `<defunct>` para
   sempre (`ps`), e `docker inspect` continuava a dizer `Running` indefinidamente. Causa-raiz:
   `spawn()` só devolve sem `waitpid` quando `detach: true` — inofensivo no CLI normal (o processo
   sai a seguir, o órfão é reparentado ao `init` real do host, que o reapa sozinho), mas este
   servidor NUNCA sai — é o pai real do container para sempre e nunca chamava `waitpid`, e uma
   zombie ainda ocupa a entrada na tabela de processos (`kill(pid, 0)` continua a suceder).
   **Corrigido**: thread dedicada (`spawn_zombie_reaper`) que faz `waitpid(-1, ...)` em loop —
   confirmado seguro contra o resto do motor (as únicas chamadas `waitpid` directas num pid
   específico, em `reexec_mapped`/`remove_tree_mapped`, servem `build`/`volsnap`/`prune`, nenhuma
   das quais esta API expõe hoje).
2. **Fuga de file descriptors no shim de logs** — `log_shim` (um `fork()` que nunca faz `execve`,
   corre para sempre a copiar o pipe do container para o ficheiro de log) só fechava o stdio
   herdado (fds 0/1/2). Num servidor long-lived, herdava TAMBÉM os sockets de outras ligações HTTP
   vivas nesse instante e ficava a segurá-los abertos por toda a vida do container. **Corrigido**:
   fecha tudo excepto o fd de origem logo a seguir ao fork (`libc::close_range`, sem alocação —
   seguro tão cedo depois de um fork de processo multi-thread).

### Validado ao vivo contra um `docker` CLI real (27.3.1)

`docker create`+`start`+`inspect`+`kill`+`wait`+`restart`+`rename`+`stop`+`rm` — todos correctos e
instantâneos (é exactamente o caminho que `docker compose up/down` usa, sem passar pelo `run`).
`wait` bloqueia e devolve o exit code real só com supervisor `--restart` (limitação arquitectural
pré-existente, não desta fatia — sem supervisor mostra `137`, documentado desde a v0.25.0).

**Limitação encontrada, documentada, não bloqueante**: o subcomando de conveniência `docker run`
(create+start num só comando) não devolve o controlo ao terminal de forma fiável contra este
servidor — o container fica correcto e a funcionar (confirmado via `inspect`/`describe` nativo),
mas o processo `docker` cliente não termina. A causa aparenta ser um comportamento interno do
próprio CLI Go (sinalização/cleanup), não reproduzido com `create`+`start` em separado. Recomendação:
usar `docker create`+`docker start` (ou `docker compose`, que já usa esse caminho) em vez de
`docker run` directamente contra este servidor.

### Validação

`cargo build`/`clippy --all-targets -D warnings`/`fmt --check` limpos nos 4 crates tocados
(`delonix-runtime`, `delonix-runtime-bin`). Testes existentes continuam verdes (285 em
`delonix-runtime-bin` + suite de `delonix-runtime`).

### Por fazer

BuildKit-lite (`RUN --mount=secret`, `--platform`) e GPU real via CDI/nvidia-container-toolkit —
os 2 gaps "bloqueantes"/maiores restantes do roadmap, já com plano desenhado, próximas fatias desta
mesma série.

---

## v0.25.0 — paridade de CLI de operação com Docker/Podman

Fecha a Fase 5 do roadmap de paridade Docker/Podman
([docs/COMPARACAO-DOCKER-PODMAN.md](../COMPARACAO-DOCKER-PODMAN.md)): os verbos de operação que
faltavam no `delonix container`. Nenhum destes precisou de tocar no princípio daemonless/
rootless — são extensões da superfície de CLI/motor já existente, não um subsistema novo.

### Novo

- **`container kill [-s <signal>]`** — envia um sinal arbitrário (nome ou número, ex.: `KILL`,
  `SIGKILL`, `9`, `TERM`) ao processo init do container. Ao contrário de `stop`, NÃO espera nem
  força o estado a `Stopped` — o resultado real (`Crashed` se o sinal matar mesmo o processo) só
  se reflecte na próxima observação, o que é honesto: um `kill -s TERM` num PID 1 sem handler
  próprio (comportamento normal do Linux em namespaces de PID) pode não ter efeito nenhum, tal
  como aconteceria num Docker real com o mesmo entrypoint.
- **`container wait`** — bloqueia até o container sair, depois imprime o exit code real. Só é
  REAL quando o container tem um supervisor `--restart` (o único caso em que o motor é o pai
  verdadeiro do processo e por isso tem um `waitpid` genuíno); sem supervisor, a morte continua a
  aparecer como `Crashed`/137, uma limitação arquitectural pré-existente (documentada, não nova).
- **`container restart`** — `stop` seguido de `start`, reaproveitando os dois tal-e-qual (imprime
  2 linhas em vez de 1 — trade-off aceite para não duplicar a lógica de rede/namespace de
  nenhum dos dois).
- **`container rename <id> <novo-nome>`** e **`container port <id>`**.
- **`container exec -e/-w/-u`** — overrides por-chamada (nunca persistidos no registo do
  container). **Bug real corrigido pelo caminho**: `exec` fazia `chdir("/")` incondicionalmente,
  ignorando o `workdir` próprio do container mesmo sem `-w` nenhum — agora usa o workdir
  configurado por omissão, só `-w` o substitui.
- **`container logs --tail/--since/--timestamps`** — só funcionam em containers corridos com
  `--log-cri` (o único formato de log com timestamps reais por linha, `<rfc3339nano> stdout F
  <linha>`); sem isso, erro claro em vez de uma coluna de timestamp em branco. `--since` aceita
  um timestamp Unix (segundos); comparação lexicográfica de strings RFC3339 (sem `chrono`,
  mesma disciplina de supply-chain do resto do projecto).
- **`container attach`** — reaproveita o mecanismo de `logs -f`. Deliberadamente **só saída**:
  este motor não guarda nenhum conduíte de stdin vivo para um container já arrancado em
  detached (ao contrário de um shim persistente por-container, como o `containerd-shim`);
  `-i`/`--interactive` é recusado com um erro claro, apontando para `exec -it` em vez disso.

### Validado ao vivo (host real)

Todos os verbos acima testados contra containers reais: `wait` mostrou o exit code real (7) com
supervisor e `137` sem ele (comportamento esperado, não um bug); `kill` (SIGKILL) matou o
processo, `kill -s TERM` corretamente NÃO matou um `sleep` sem handler (mesmo comportamento que
um Docker real teria); `exec -e/-w/-u` mostrou o workdir/env/user aplicados corretamente;
`logs --timestamps`/`--tail` formataram as linhas certas; `attach -i` recusou com o erro
esperado; `rename`/`restart`/`port` funcionaram de ponta a ponta.

### Validação

21 testes novos/actualizados (parsing de sinal, parsing de linhas de log CRI, conversão
Unix→RFC3339 cross-validada contra `date -u` real). `cargo test --workspace`, `clippy
--all-targets -D warnings` e `fmt --check` limpos.

### Por fazer

Mutações da API Docker-compatível (`/containers/create|start|stop|exec`), BuildKit-lite (`RUN
--mount=secret`, `--platform`) e GPU real via CDI/nvidia-container-toolkit — os 3 gaps
"bloqueantes"/maiores restantes do roadmap, cada um já com plano desenhado, próximas fatias
desta mesma série.

---

## v0.24.0 — etcd externo dedicado (`cluster apply`/`cluster kubeadm`)

Fecha o último item do backlog da auditoria E2E ([docs/AUDITORIA-E2E.md](../AUDITORIA-E2E.md)):
`etcd.mode: "external"` — delonix passa a provisionar e gerir o seu PRÓPRIO cluster etcd
dedicado, em vez de apenas o `stacked` (co-localizado nos control-planes, default do kubeadm).
Era o item explicitamente deixado para o fim, por precisar de uma sessão de planeamento própria
(subsistema de PKI novo, `kubeadm init` a mudar de flags simples para `--config` YAML).

### Novo

- **`etcd.hosts` no manifesto (`mode: ssh`) ou `cluster kubeadm --etcd-cluster <N>`** (auto-provisiona
  N VMs extra, mesmo `create_and_wait` das outras roles). Delonix gera a sua própria CA (`rcgen`,
  API de baixo nível — `self_signed`/`signed_by`, primeira vez que este código assina uma CADEIA
  em vez de um único leaf self-signed) + um leaf por membro etcd (reutilizado para TLS de peer E
  client/server, reduzindo a superfície de PKI de um subsistema novo) + um leaf
  `apiserver-etcd-client` para o kube-apiserver.
- **Bootstrap paralelo**: instala+arranca o `etcd` real (binário oficial `etcd-io/etcd`,
  descarregado e verificado por `SHA256SUMS` — nunca por `apt`, a versão fica sob o nosso
  controlo, mesmo padrão de `vmimage::download_cri_bin`) em TODOS os membros ao mesmo tempo
  (`std::thread::scope`) — o bootstrap estático do etcd precisa de todos os membros iniciais
  alcançáveis em conjunto, não é só uma questão de velocidade. Espera o quórum ficar saudável
  (`etcdctl endpoint health --cluster`) antes de avançar para o `kubeadm init`.
- **`kubeadm init --config=...`**: como o caminho de flags simples não consegue exprimir
  `ClusterConfiguration.etcd.external`, o caminho externo passa a gerar um YAML de 2 documentos
  (`cmd/kubeadm_config.rs`, `serde_yaml`) e corre `kubeadm init --config=...` em vez de
  `--pod-network-cidr=...` etc. **O caminho `stacked` (default) fica byte-a-byte inalterado** —
  zero risco de regressão para quem não usa etcd externo.
- **Quórum**: `validate()` exige `etcd.hosts` não vazio e um número ÍMPAR de membros (excepto
  exactamente 1, aceite para dev/teste com um aviso alto de "sem HA" — um só nó etcd é um ponto
  único de falha).
- **Achado não validado, contornado em vez de assumido**: não se confirmou se o
  `--upload-certs`/`--certificate-key` do kubeadm já redistribui o cert `apiserver-etcd-client`
  para CADA control-plane no caso externo, da mesma forma que faz para o `stacked`. Em vez de
  depender disso, `etcd::push_etcd_client_pki` reenvia `ca.crt` + `apiserver-etcd-client.{crt,key}`
  a cada control-plane (o do `kubeadm init` e cada `kubeadm join --control-plane`) antes do
  respectivo comando — a correcção fica independente do comportamento nativo do kubeadm.
  Confirmar isso ao vivo (e possivelmente simplificar) fica como follow-up, não bloqueia esta
  versão.
- **Armazenamento**: CA + certos ficam em `<root>/clusters/<nome>/etcd/` (directório `0700`,
  ficheiros `0600`) — estende a convenção de subdirectório por-cluster que `id_ed25519` já usa,
  em vez de reaproveitar o `SecretStore`/`CredVault` (esses são o gestor de segredos
  *user-facing*; um segredo gerado internamente pelo sistema não pertence lá).

### Deliberadamente fora desta versão

Adicionar/remover membros etcd depois do bootstrap inicial, rotação de certificados, migrar um
cluster `stacked` já vivo para `external`, e `mode: vm` (manifesto) auto-provisionar etcd — só o
`cluster kubeadm --etcd-cluster` o faz por agora (`validate()` recusa `etcd.mode: external` fora
de `mode: ssh` de propósito, para não descartar `etcd.hosts` em silêncio nesse modo).

### Validação

`pki::generate_ca`/`issue_leaf`, `etcd::build_etcd_unit`/`etcd_release_asset_url`,
`kubeadm_config::render_init_config` e as 6 novas ramificações de `validate()` têm teste de
regressão dedicado — 21 testes novos/actualizados. O URL/formato do `SHA256SUMS` do
`etcd-io/etcd` foi confirmado AO VIVO (não suposto) antes de escrever código, e o YAML real
gerado por `render_init_config` foi inspeccionado visualmente. Este sandbox não tem hosts SSH
reais — o bootstrap etcd real (formação de quórum, `kubeadm init --config=...` até um
control-plane `Ready` contra um cluster etcd dedicado) precisa de validação no host do
utilizador. `cargo test --workspace`, `clippy --all-targets -D warnings` e `fmt --check` limpos.

---

## v0.23.0 — os 3 gaps menores por fechar (Fase 5, fecha o backlog restante)

Fecha os 3 gaps menores documentados no `AGENTS.md` que ficaram deliberadamente de fora das
fases 1-4 da mesma auditoria ([docs/AUDITORIA-E2E.md](../AUDITORIA-E2E.md)). Só fica por fazer o
etcd externo (`--etcd-cluster`), subprojecto de PKI maior, deixado para o fim.

### Correcções

- **Selecção por omissão em `Pull`/`LsRemote` para os repositórios `delonix-vm-base`** — a
  golden `--no-k8s` (Ubuntu 24.04, só `delonix` + rootless) já tinha o repositório definido
  (`OFFICIAL_VM_BASE_IMAGE`) mas nenhuma forma de a seleccionar sem escrever a referência OCI
  completa à mão. Nova flag `--no-k8s` em `vm pull`/`vm ls-remote` (e os 2 caminhos irmãos,
  `image vm pull`/`image --vm pull`) — sem `source`, escolhe entre a golden Kubernetes e a
  golden sem Kubernetes consoante a flag, tal como o `vmimage build --no-k8s` já fazia para
  construir a imagem. Nova `default_pull_source(no_k8s)` partilhada pelos 3 pontos de entrada da
  CLI (mesmo padrão triplo do resto do grupo `vm`/`image --vm`).
- **Preparação de hosts em `cluster apply` corria sequencial** — cada host é completamente
  independente (sessão SSH própria, sem estado partilhado), mas `apply_ssh` preparava-os um a um,
  pagando N vezes o `apt update` + instalação de pacotes + deploy do `delonix-cri` em série. Agora
  corre em paralelo via `std::thread::scope`. Mudança de comportamento deliberada: ao contrário do
  loop sequencial antigo (parava no primeiro host mau, os restantes nunca eram tentados), agora
  TODOS os hosts são preparados independentemente do resultado dos outros e TODAS as falhas são
  reportadas juntas — mais útil para quem está a corrigir um manifesto multi-host do que descobrir
  um host partido de cada vez que corre o comando.
- **Mensagens de erro dos crates de motor nunca traduzidas em `--l18n=pt`** — dois bugs
  distintos na mesma funcionalidade:
  1. O `t_dyn` (o mecanismo já desenhado para isto, conforme o `AGENTS.md` já documentava) fazia
     um lookup de string EXACTA contra o texto TOTALMENTE renderizado do erro — mas esse texto é
     sempre um prefixo EN fixo (`"invalid argument: "`, `"no such container: "`, ...) colado à
     mensagem real. Como o catálogo tinha só a mensagem interna semeada, o texto embrulhado nunca
     batia com NADA, mesmo havendo uma entrada `pt.po` perfeitamente boa à espera — todo o erro de
     um crate de motor saía em inglês, sempre. Corrigido: `t_dyn` reconhece os 6 moldes
     `#[error(...)]` traduzíveis de `delonix_runtime_core::Error` (excluindo deliberadamente
     `Io`/`Json`/`Runtime`, cujo texto vem de um errno/serde do SO, não é nosso para traduzir),
     separa prefixo/sufixo/interior, e traduz cada parte de per si.
  2. O caminho de erro principal do `main.rs` (`run()` → `cmd::output::error(...)`) nunca sequer
     chamava `t_dyn` — só os 4 caminhos escondidos de re-exec (`netns run`, `__rmtree`,
     `__volsnap`, `__buildtar`) o faziam. Isto é o caminho que a esmagadora maioria dos erros
     visíveis ao utilizador atravessa. Também descoberto pelo caminho: `for_each_id`
     (`container stop/rm/...` com vários ids) tinha o seu PRÓPRIO `eprintln!` que também nunca
     passava por `t_dyn` — um `container stop <id-inexistente>` batch ficava em inglês mesmo com
     um `container stop` de 1 id só já traduzido. Ambos corrigidos.

### Validação

`default_pull_source`/`combine_host_prep_errors`/`split_error_wrapper` têm teste de regressão
dedicado, puro (sem SSH real nem mutação do estado global `output::is_pt()`, que teria risco de
instabilidade entre testes em paralelo — ver `split_error_wrapper`, extraído exactamente para
poder ser testado sem tocar nesse estado). Validado ao vivo: os 3 pontos de entrada de
`pull`/`ls-remote --no-k8s` resolvem para o repositório certo (`--help` + tentativa real contra o
registo); `network create --driver overlay --vni 99999999 --l18n=pt`, `vm stop
<inexistente> --l18n=pt` e `container stop <inexistente> --l18n=pt` (via `for_each_id`) saem
totalmente em português, antes e depois comparados lado a lado. `cargo test --workspace`, `clippy
--all-targets -D warnings` e `fmt --check` limpos em todo o workspace.

### Por fazer

Só falta o etcd externo (`--etcd-cluster <N>`) — subprojecto de PKI maior (CA+certificados via
`rcgen`, `kubeadm init --config` em vez de flags simples), deixado para o fim por desenho desde o
início desta série de correcções.

---

## v0.22.0 — os últimos 5 bugs da auditoria E2E (Fase 4 do backlog)

Continuação directa da v0.19.0/v0.20.0/v0.21.0 — mesma auditoria
([docs/AUDITORIA-E2E.md](../AUDITORIA-E2E.md)). **Fecha o backlog dos 24 achados confirmados +
achados unverified relevantes** — só ficam por fazer os gaps menores já documentados e o etcd
externo (subprojecto à parte, deixado para o fim).

### Correcções

22. **Buffering ilimitado no pull de blobs de imagem VM (OOM via `Content-Length` malicioso)** —
    `blob_with_progress` fazia `Vec::with_capacity(content_length)` confiando directamente no
    header cru do registo: um valor gigante/mentiroso abortava o processo por falha do
    alocador, e o loop de leitura não tinha nenhum teto independente — um servidor que apenas
    continuasse a enviar dados (sem se importar com o que o header dizia) esgotava a memória do
    host. Corrigido com `blob_with_progress_capped`: a reserva inicial é limitada por
    `max_bytes` (8 GiB), e o loop aborta assim que os bytes REALMENTE lidos ultrapassam o
    limite — independente do que o header alegava.
23. **Processo de `exec`/`attach` do CRI vazava quando o cliente desligava** — nos três
    caminhos (SPDY, WebSocket com TTY, WebSocket sem TTY), quando a ligação do `kubectl
    exec`/`crictl exec` caía (rede, Ctrl-C, timeout do cliente), o `delonix exec`/child spawnado
    nunca era morto — uma shell interactiva sem cliente nenhum ligado corria para sempre,
    vazando um processo (e tudo o que ele segura: pty, fds de netns, referências ao namespace do
    container) por cada sessão de exec abandonada. Corrigido: os três caminhos agora mandam
    SIGKILL ao pid da criança assim que detectam que foi o CLIENTE que desapareceu (não o
    processo a terminar sozinho).
24. **`ContainerStatus` (CRI) fabricava `started_at`/`finished_at` a cada poll** —
    `started_at` era sempre `created_at` (nunca o instante real de arranque); `finished_at` era
    `now_ns()` recalculado em CADA chamada — como o kubelet faz poll repetido deste RPC, o
    instante de fim reportado avançava indefinidamente muito depois do container já ter
    morrido, partindo tudo o que depende de "há quanto tempo isto morreu" (temporização do
    crash-loop backoff, heurísticas de rotação de log). Corrigido: os dois instantes são
    persistidos no registo do container UMA vez (no `StartContainer` real e na primeira vez que
    um exit é observado) e servidos estáveis daí em diante.
25. **Resolvedor DNS interno: scan O(n) por query + forward bloqueante serializava todo o DNS
    do nó** — `dns_resolve` fazia uma varredura completa do directório + parse JSON de TODOS os
    registos de container/VM em CADA query (incluindo domínios externos, já que
    `parse_internal_name` nunca devolve `None` para um hostname nu), e cada VM sem IP estático
    disparava o seu próprio `ip neigh show` (um exec por VM por query). Pior: o loop de aceitação
    era single-threaded — `forward_dns` pode bloquear até ~6s (2 upstreams × timeout de 3s) num
    resolvedor externo lento/morto, o que travava a resolução de DNS do NÓ INTEIRO (incluindo
    lookups de containers/VMs vivos, que nem tocam a rede) pela duração desse timeout. Corrigido
    em duas frentes: (1) o directório é agora indexado num cache com refresh a cada 2s (bem
    abaixo do TTL de 30s já usado nas respostas `A`), reduzindo o scan de "uma vez por query"
    para "uma vez por intervalo"; a tabela `neigh` também passa a ser lida uma vez por refresh em
    vez de uma vez por VM por query; (2) cada query passa a ser tratada na sua própria thread
    (com um teto de 64 em voo, para uma inundação de UDP não gerar threads sem limite) — um
    forward lento bloqueia só o seu próprio cliente, nunca o nó inteiro.
26. **Accept loop de gestão/CRI morria com um erro transitório de `accept()`** — tanto o
    `delonix-mgmt` (API de gestão) como o `delonix-cri` propagavam QUALQUER erro de `accept()`
    para fora do loop — incluindo `EMFILE`/`ENFILE`/`ECONNABORTED`, todos transitórios e
    auto-recuperáveis (a própria manpage do `accept(2)` diz para simplesmente tentar de novo).
    Um esgotamento passageiro de file descriptors noutro sítio do host derrubava o processo
    inteiro, matando todos os pedidos em curso por uma condição que nada tinha a ver com eles.
    Corrigido: o erro é registado e o loop continua a aceitar em vez de propagar.

### Validação

Achados #22, #24 e #25 (as partes puras — chaves do índice, tabela `neigh`) têm teste de
regressão dedicado. #23 e #26 não têm — mesmo precedente já aceite nesta série para correcções
de baixo nível de syscall/concorrência sem infra real disponível neste sandbox (fuga de fd no
`exec`, permissões do ficheiro de spec, corrida de config do proxy). `cargo test --workspace`,
`clippy --all-targets -D warnings` e `fmt --check` limpos em todo o workspace.

### Por fazer

Só ficam os gaps menores já documentados (selecção por omissão em `Pull`/`LsRemote` para os
repositórios `delonix-vm-base`, paralelizar a preparação de host em `cluster apply`, i18n das
mensagens de erro dos crates de motor) e o etcd externo (`--etcd-cluster`) — subprojecto de PKI
maior, deixado para o fim.

---

## v0.21.0 — mais 4 bugs da auditoria E2E (Fase 3 do backlog)

Continuação directa da v0.19.0/v0.20.0 — mesma auditoria
([docs/AUDITORIA-E2E.md](../AUDITORIA-E2E.md)).

### Correcções

18. **Primeiro arranque concorrente do proxy L7 podia fazer double-spawn** — `ensure_running`
    fazia o `running_pid()`-check → `spawn_proxy()` sem lock nenhum. Quando nenhum proxy ainda
    existe, dois chamadores concorrentes (`httproute apply` + `container run --expose`, ou dois
    `--expose` em simultâneo) viam ambos `running_pid() == None` e ambos arrancavam um proxy —
    os dois tentam ligar às mesmas portas, um ganha, o outro cai; se o pidfile ficar com o pid do
    que caiu, o sobrevivente fica órfão (SIGHUP/SIGTERM já não o alcançam). Corrigido serializando
    toda a decisão check-then-spawn sob um `spawn.lock` dedicado.
19. **`kube generate` emitia YAML sem escape** — `quote()` era um `format!("\"{s}\"")` cru. Um
    comando com aspas embutidas (`sh -c 'echo "hi"'`) produzia YAML inválido que o `kubectl apply
    -f -` recusa; uma quebra de linha embutida injectaria uma linha literal no documento,
    potencialmente com chaves extra não intencionais. Corrigido com escape correcto de scalar
    YAML entre aspas duplas (`\`, `"`, `\n`, `\t`, `\r`).
20. **Dashboard reconciliava cada VM DUAS VEZES por tick** — `delonix_vm::list` já reconcilia
    cada VM internamente (chama `status` por dentro), mas o dashboard fazia um `.map(status)`
    extra a seguir — dobrando os `virsh` que arranca por segundo no backend libvirt, sem
    propósito. Removido o `.map` redundante.
21. **Versão patch do k8s aceite pela validação mas rejeitada pelo repositório** —
    `valid_version` aceita explicitamente `1.31.2` (o seu próprio doc-comment di-lo), mas o
    `pkgs.k8s.io` só publica directórios ao nível MINOR (`stable:/v1.31`, nunca
    `stable:/v1.31.2`). Um `spec.k8sVersion: "1.31.2"` passava a validação e só falhava muito
    mais tarde, com um 404 no `apt-get update` de TODOS os hosts do cluster. Corrigido truncando
    para major.minor só na construção do URL do repositório — `--kubernetes-version` do kubeadm
    continua a receber a versão completa, correctamente.

### Validação

Cada achado tem teste de regressão dedicado, excepto o #18 (correcção de concorrência sem infra
de proxy real disponível neste sandbox). `cargo test --workspace` (263 testes só em
`delonix-runtime-bin`), `clippy -D warnings` e `fmt --check` limpos.

### Por fazer

5 achados restantes da mesma auditoria: buffering ilimitado no pull de imagens (OOM via
Content-Length malicioso), fuga de processos exec/attach do CRI quando o cliente desliga,
timestamps fabricados no `ContainerStatus` a cada poll, performance do resolvedor DNS interno
(scan O(n) + forward bloqueante serializa TODO o DNS do nó), e resiliência do accept loop de
gestão/CRI a erros transitórios (EMFILE derruba o processo inteiro). Mais os gaps menores já
documentados e o etcd externo, deixado para o fim.

---

## v0.20.0 — 7 mais bugs da auditoria E2E (Fase 2 do backlog)

Continuação directa da v0.19.0 — mesma auditoria ([docs/AUDITORIA-E2E.md](../AUDITORIA-E2E.md)),
próximos 7 achados confirmados directamente no código actual.

### Correcções

11. **`kubeadm join` duplicava a porta num `controlPlaneEndpoint` com porta explícita** —
    `kubeadm_init` interpola o endpoint tal-e-qual em `--control-plane-endpoint=` (aceita
    `host[:porta]`), mas `kubeadm_join` anexava `:6443` incondicionalmente. Um LB/VIP HA numa
    porta não-6443 (cenário real, `valid_endpoint` sempre aceitou porta) gerava `kubeadm join
    host:8443:6443` — endereço malformado, todos os joins de control-plane/workers falhavam.
    Corrigido: só anexa a porta por omissão quando o endpoint ainda não tem uma.
12. **`pick_route` (HTTPRoute) ignorava a fronteira de segmento no path prefix** — AGENTS.md
    alinha explicitamente a semântica ao Kubernetes Gateway API (`PathPrefix` de `/foo` casa
    `/foo` e `/foo/bar`, NUNCA `/foobar`), mas o matcher usava `starts_with` cru. Duas rotas
    `/api` (backend interno) e `/` (público) no mesmo host: um pedido a `/api-docs` (destinado
    ao público) era silenciosamente encaminhado para o backend interno.
13. **Config composta do proxy L7 escrita fora do flock** — `auto_register`/`auto_deregister`
    faziam o read-modify-write de `auto.json` sob flock, mas chamavam `rebuild()` (compõe +
    escreve `config.json` + SIGHUP) DEPOIS de largar o lock. Duas registações `--expose`
    concorrentes podiam interleaving de forma a que o `config.json` final (o que o proxy
    realmente serve) reflectisse um snapshot desactualizado — uma rota adicionada com sucesso a
    `auto.json` nunca chegava ao proxy vivo, sem erro nem novo trigger para recompor.
14. **Gate de admissão de CVE falhava aberto com um valor de política desconhecido** —
    `DELONIX_SCAN_ON_PULL=criticl` (erro de escrita de "critical") caía em
    `Severity::parse` → `None` → `admission_rejects` devolve `false` → a imagem era ADMITIDA,
    só avisada DEPOIS do facto. Um gate documentado como "fail-closed" degradava-se
    silenciosamente para consultivo. Corrigido: a política é validada ANTES de scanear.
15. **Password CIFS exposta em argv world-readable + injecção por vírgula** — `mount.cifs`
    corre como root com a password como argumento LITERAL do processo
    (`/proc/<pid>/cmdline` legível por qualquer utilizador local durante o mount), derrotando o
    propósito do `--password-secret`/`kind: Secret`. Além disso as opções CIFS são delimitadas
    por vírgula sem escape — uma password com vírgula truncava a credencial ou, vinda de um
    Secret não-confiável, injectava opções de mount arbitrárias. Corrigido com
    `credentials=<ficheiro 0600>` em vez de `username=`/`password=` inline.
16. **`SecretStore`/`CredVault` sem validação de nome em `load`/`remove`/`get`/`exists`** — só
    `save`/`put` validavam; os outros construíam o caminho a partir do nome cru. Um nome como
    `../../../etc/passwd` em `container run --secret <nome>` ou num manifesto não-confiável era
    uma primitiva de leitura/eliminação arbitrária de ficheiros.
17. **`SecretStore::save` com ficheiro temporário fixo e sem lock** — exactamente o padrão que
    `Store::save` (estado de containers) já tinha sido desenhado para evitar: dois
    `secret set` concorrentes do MESMO nome escreviam no mesmo temp, produzindo um blob
    encriptado corrompido (permanentemente ilegível, já que o valor é AEAD-selado) — perda de
    dados, não só desactualização. Corrigido com o mesmo padrão de temp único por-escritor
    (pid+sequência) do `Store::save`, mais um novo `SecretStore::update` (read-modify-write sob
    flock) que `secret set`/`unset` agora usam em vez de load+mutate+save sem protecção.

### Validação

Cada achado tem teste de regressão dedicado (excepto o #13, uma correcção de concorrência sem
infra de proxy real disponível neste sandbox — validado por leitura cuidadosa + o padrão já
provado noutros locais deste código). `cargo test --workspace` (258 testes só em
`delonix-runtime-bin`, mais os das crates de motor), `clippy -D warnings` e `fmt --check`
limpos.

### Por fazer (próximas fatias do mesmo backlog)

7 achados restantes: buffering ilimitado no pull de imagens (OOM), fuga de processos
exec/attach do CRI em disconnect, timestamps fabricados no `ContainerStatus`, performance do
resolvedor DNS (scan O(n) + forward bloqueante), double-spawn do proxy em primeiro arranque
concorrente, YAML sem escape do `kube generate`, dashboard a reconciliar VMs 2x por tick, e
resiliência do accept loop de gestão/CRI a erros transitórios (EMFILE). Mais os gaps menores já
documentados (selecção por omissão em `Pull`/`LsRemote`, paralelizar `cluster apply`, i18n dos
crates de motor) e o etcd externo (subprojecto maior, deixado para o fim).

---

## v0.19.0 — 10 bugs fechados da auditoria E2E (Fase 1 do backlog completo)

Início de uma passagem sistemática pelo backlog inteiro da auditoria E2E ampla
([docs/AUDITORIA-E2E.md](../AUDITORIA-E2E.md)) — 24 achados confirmados + 11 por-verificar,
registados numa sessão anterior e nunca fechados. Esta primeira fatia cobre 10 achados reais,
confirmados directamente no código actual (não só na descrição do relatório) antes de qualquer
correcção.

### Segurança / correcção real

1. **`container run --rm` fugia o rootfs inteiro em rootless** (ambos os caminhos: foreground e o
   watcher detached) — `unmount_rootfs` preserva deliberadamente o rootfs FLAT (para o `start`
   reaproveitar); só `remove_container_dir` o apaga de facto, e `--rm` nunca a chamava. É o MESMO
   leak de disco já corrigido para `rm`/`rm -f` (49 directórios, 45 GiB, `disk-pressure` no
   kubelet) — só nunca tinha sido aplicado ao `--rm`. Validado ao vivo: `container run --rm
   alpine echo ...` já não deixa nenhum directório órfão.
2. **`egress` global apagava as regras de egress POR REDE (fail-open)** — o loop de limpeza do
   `do_egress` global casava com `oifname "tap0"` + `drop`, mas as regras por-rede
   (`apply_egress_from_state`) também têm essa forma (`iifname "<bridge>" oifname "tap0" ...
   drop`). Um `egress allow`/`deny` global não relacionado apagava silenciosamente o `deny`/
   `allowlist` de uma rede já restringida, reabrindo egress total para a Internet sem erro nem
   aviso — um caminho de exfiltração de dados reaberto por um comando não relacionado. Corrigido
   excluindo linhas com `iifname` do loop de limpeza global.
3. **Endereço PCI (VFIO) interpolado no XML do libvirt sem validação hex nem escape** —
   `parse_pci_addr` devolvia os 4 componentes crus, sem confirmar que eram hex; um
   `spec.devices` malicioso injectava atributos XML. Corrigido: cada componente tem de ser hex da
   largura certa (domain 4, bus 2, slot 2, func 1), e `xml_escape` aplicado no ponto de
   interpolação como defesa em profundidade.
4. **Ficheiro de spec do re-exec ficava world-readable com segredos em claro** — `reexec_into_netns`
   (usado por TODO `--net <custom>`/`--pod`) escrevia `-e KEY=VALUE` num JSON com o umask
   ambiente (tipicamente 0644); num container em foreground, ficava assim durante TODA a vida do
   container. Corrigido com o mesmo padrão `create_new`+0600 já usado para o XML da rede libvirt.
5. **Escape por symlink no alvo de bind-mount** — confirmado JÁ CORRIGIDO (achado desactualizado):
   `safe_bind_target` já resolve o alvo componente-a-componente, sem seguir symlinks.
6. **`read` bloqueante depois de um `poll` falhado podia pendurar `run` para sempre** — em DOIS
   pontos (`start_slirp` e `slirp_attach`), se o `slirp4netns` nunca sinalizasse pronto E nunca
   fechasse o write-end (um neto a herdá-lo chega), o `read` seguinte bloqueava
   indefinidamente — reintroduzindo exactamente o deadlock que o `poll` existia para evitar.
   Corrigido: só lê quando o `poll` confirma que há dados; fecha o fd sempre.
7. **`system prune` podia derrubar TODA a infra de ingress a meio de um `run`** — `attach_container`
   escreve o marcador de referência ANTES do registo do container ser guardado na Store; um
   `system prune` a correr exactamente nessa janela via o marcador como órfão e, se fosse o
   último, desligava o holder+slirp+nft inteiros — afectando também todos os OUTROS containers
   (o netns do holder é partilhado). Corrigido com um período de graça de 15s no `mtime` do
   marcador, sem precisar de reordenar a criação do container.
8. **Prefixo de id ambíguo resolvia silenciosamente para o container mais recente** — `find`
   (usado por `stop`/`rm`/etc.) devolvia o PRIMEIRO match (ordem created-desc) em vez de recusar,
   ao contrário do Docker/Podman ("multiple IDs found"). Um comando destrutivo com um prefixo
   ambíguo podia acertar no container errado sem aviso. Corrigido: match exacto de id/nome ganha
   sempre; múltiplos matches por prefixo são erro explícito, listando os candidatos.
9. **`exec` fugia os fds das namespaces no processo-pai** — só o filho fechava os seus fds (via
   `OwnedFd`); o pai nunca fechava os seus, mantendo vivas as namespaces (mnt/net/pid/user) mesmo
   depois do container morrer, além do esgotamento de fds em chamadores de longa duração.
   Corrigido: fecha explicitamente no ramo `ForkResult::Parent`, simétrico com `mount_live`/
   `unmount_live`.
10. **`--sysctl net.*` escrevia no netns do HOST quando o container o partilha** — `--net host` (ou
    rede custom em rootless, que partilha o netns do holder) não tem `CLONE_NEWNET`; `/proc/sys/
    net/*` reflecte então a namespace PARTILHADA, não uma isolada do container. O Docker recusa
    exactamente esta combinação. Corrigido: `net.*` só é permitido quando o container tem mesmo a
    sua própria netns.

### Validação

Cada achado teve um teste de regressão novo (excepto os dois de baixo nível de syscall — fd
leak no `exec` e a permissão do ficheiro de spec — que replicam exactamente o padrão já
estabelecido no código sem teste dedicado, mesma classe do fix já existente para o XML da rede
libvirt). `cargo test --workspace` (252 testes só em `delonix-runtime-bin`, mais os das crates de
motor), `clippy -D warnings` e `fmt --check` limpos. `--rm` validado ao vivo neste host real
(nenhum directório órfão após `container run --rm`).

### Por fazer (próximas fatias do mesmo backlog)

14 achados restantes da lista confirmada/por-verificar — HTTPRoute path-prefix, proxy config fora
do flock, admission gate fail-open, credenciais CIFS, `SecretStore`/`CredVault` traversal +
concorrência, buffering ilimitado no pull de imagens, fuga de processos exec/attach do CRI,
timestamps fabricados no `ContainerStatus`, performance do resolvedor DNS, `kubeadm join` com
porta duplicada, double-spawn do proxy, YAML sem escape do `kube generate`, dashboard a
reconciliar VMs 2x, e resiliência do accept loop de gestão/CRI — seguem em fatias posteriores.

---

## v0.18.0 — golden VM images: `--distro rocky` (Fase 3 de 3, a última)

Terceira e última fatia da sequência de variantes de golden image (Ubuntu 26.04 já funcionava de
graça; Ubuntu 24.04 `--no-k8s` na v0.16.0; Debian na v0.17.0). Rocky Linux era o maior salto —
primeira família RPM/dnf a entrar neste código, tudo o resto até agora era apt/dpkg.

### Escopo: só `--no-k8s`

Ao contrário do Debian (que já suportava k8s e `--no-k8s` de graça), o Rocky **só suporta
`--no-k8s`** por agora — `k8s_recipes` (o repositório `pkgs.k8s.io`, `dpkg -i`/`apt-mark hold`) é
apt-only; o equivalente RPM do `pkgs.k8s.io` tem URL/GPG diferentes e fica fora desta fase.
`cmd_build` rejeita `--distro rocky` sem `--no-k8s` com erro claro, antes de tentar correr
`apt-get`/`dpkg` contra um guest dnf.

### Três descobertas confirmadas ao vivo antes de escrever código (não assumidas)

1. **Árvore do cloud image diferente da do Debian**: `pub/rocky/<major>/images/x86_64/
   Rocky-<major>-GenericCloud.latest.x86_64.qcow2` — sem o segmento `images/cloud/`. O `<major>`
   é literal (8/9/10), sem tradução de codinome.
2. **Um TERCEIRO formato de checksum** neste módulo: Rocky publica um `.CHECKSUM` PER-FILE (não
   uma `SUMS` de directório) no formato BSD `SHA256 (<ficheiro>) = <hash>` — diferente do
   `<hash>  <ficheiro>` GNU que Ubuntu/Debian usam. `parse_bsd_checksum` novo, testado contra a
   linha real capturada ao vivo.
3. **Nomes de pacote RPM** confirmados contra as listagens reais do repositório Rocky 9:
   `shadow-utils` (não `uidmap`), `iproute` (não `iproute2`), `conntrack-tools` (não
   `conntrack`) — todos em BaseOS/AppStream, sem EPEL.

### `shared_account_steps` ganhou consciência de distro

Três pontos que divergem por família, todos confirmados ao vivo: o grupo sudo-equivalente é
`wheel` no Rocky (não `sudo`); o ficheiro bash interactivo é `/etc/bashrc` (não
`/etc/bash.bashrc`); a limpeza de cache é `dnf clean all` em vez de `apt-get clean`. Ubuntu/Debian
mantêm o output exactamente como antes (teste de regressão dedicado).

### Um bug apanhado antes de publicar

O passo do perfil AppArmor tinha `printf ... > /etc/apparmor.d/delonix && (apparmor_parser ... ||
true)` — só a chamada ao `apparmor_parser` estava guardada, não a ESCRITA do ficheiro. Rocky não
tem `/etc/apparmor.d/` (usa SELinux); correr este passo lá faria o `virt-customize` inteiro
falhar. Corrigido gating o passo a `distro == Ubuntu` — mais correcto também para o Debian, que
nunca precisou dele (o sysctl que este perfil contorna é uma patch exclusiva do kernel Ubuntu
23.10+), só nunca tinha sido revisto porque no Debian o passo degradava em silêncio sem partir
nada.

### Validado ao vivo

- `--distro rocky` sem `--no-k8s` rejeitado com erro claro, antes de qualquer rede.
- `--rocky-release 7` (fora da whitelist 8/9/10) rejeitado de imediato.
- URL + redirect confirmados com `curl -I` contra Rocky 8, 9 e 10.
- `parse_bsd_checksum` testado com a linha `.CHECKSUM` real capturada de
  `dl.rockylinux.org` (Rocky 9 GenericCloud).
- `cargo test --workspace` (249 testes só em `delonix-runtime-bin`, incluindo 5 novos
  específicos de Rocky), `clippy -D warnings` e `fmt --check` limpos.

### Limitação conhecida

Tal como o Debian na v0.17.0, o download real do `.qcow2` Rocky (~600 MiB) não foi validado de
ponta a ponta neste sandbox — a mesma ligação de saída lenta já documentada. A verificação SHA256
está coberta por teste unitário com dado real; a build `virt-customize` completa corre no CI
(`vm-image.yml`, input `rocky_release` novo) ou no host do utilizador, como qualquer golden image
deste projecto.

---

## v0.17.0 — golden VM images: `--distro debian` (Fase 2 de 3)

Segunda fatia da sequência de variantes de golden image pedida pelo utilizador (Ubuntu 26.04 já
funcionava de graça; Ubuntu 24.04 `--no-k8s` saiu na v0.16.0; falta Rocky Linux, dnf, o maior
salto). Esta fase acrescenta o Debian como segunda distro suportada por `delonix image --vm
build`.

### `--distro <ubuntu|debian>`

`download_ubuntu_base` generalizou-se por trás de um novo `Distro` (`clap::ValueEnum`, omissão
`ubuntu` — zero mudança de comportamento para quem não usa a flag). Antes de escrever código,
confirmado ao vivo contra `cloud.debian.org` (não suposto):

- O cloud image Debian vive em `images/cloud/<codinome>/latest/debian-<major>-genericcloud-
  amd64.qcow2` — variante `genericcloud` (kernel só-virtio, mais pequena, cloud-init mantido), não
  `generic` (que também traz drivers legados desnecessários aqui). O directório usa o CODINOME
  (`bookworm`), o nome do ficheiro usa o NÚMERO MAJOR (`12`) — sem alias numérico no directório,
  daí uma whitelist explícita (`bullseye`→11, `bookworm`→12, `trixie`→13), erro claro para
  qualquer outro codinome.
- Debian publica **`SHA512SUMS`, não `SHA256SUMS`** — confirmado com `curl -I` (404 num, 200
  noutro). Mesmo formato de linha (`<hash>  <ficheiro>`), algoritmo diferente. `hex_sha512_file`
  novo, mesma crate `sha2` já na árvore (zero dependência nova).

O resto do pipeline já era 100% distro-agnóstico e não precisou de nenhuma mudança: o repositório
`pkgs.k8s.io` usa formato "flat" (sem codinome/suite no URL, funciona em qualquer dpkg), a conta
`sudo`/`/etc/bash.bashrc` são convenções idênticas em Debian e Ubuntu (mesma linhagem de
empacotamento). Por isso `--distro debian` já funciona **com e sem `--no-k8s`**, sem código extra
para o lado do Kubernetes.

### Schema

`VmImage` ganhou `distro: Option<String>` (`#[serde(default)]` — metadados de imagens antigas
continuam a carregar sem erro). `ubuntu_release` manteve o NOME do campo — renomeá-lo quebraria o
`.json` já em disco de qualquer imagem existente, que não tem `#[serde(default)]` nesse campo —
mas passou a guardar o identificador de release de qualquer distro, não só Ubuntu. `image --vm
ls`/`describe` mostram agora `<distro>/<release>` (ex.: `debian/bookworm`) quando ambos os campos
existem, degradando com graça para metadados pré-v0.17.0.

### CI

`vm-image.yml` ganhou os inputs `distro`/`debian_release` (só para builds `--no-k8s`, mesmo
escopo do `no_k8s` já existente da fase anterior).

### Validado ao vivo

- `--distro rocky` (ainda não implementado) rejeitado por `clap` com erro claro antes de tocar em
  qualquer código.
- `--debian-release sid` (codinome desconhecido) rejeitado de imediato por `debian_major_version`,
  sem tentar nenhum download.
- URL + redirecionamento (`cloud.debian.org` → mirror) e formato do `SHA512SUMS` confirmados com
  `curl` real contra `bookworm` e `trixie`.
- `hex_sha512_file` testado contra o vector oficial NIST de `SHA-512("abc")`.
- `cargo test --workspace` (242 testes só em `delonix-runtime-bin`), `clippy -D warnings` e `fmt
  --check` limpos.

### Limitação conhecida

O download real do `.qcow2` Debian (~300-600 MiB) não foi validado de ponta a ponta neste
sandbox — a ligação de saída até `cloud.debian.org` mostrou-se muito lenta (confirmado de forma
independente com um `curl` simples, não é bug do código deste release; um build real corre no CI
via `vm-image.yml`, como toda a golden image). Uma build `--offline` para Debian também não está
implementada nesta fase (só o caminho online e `--no-k8s` foram cobertos) — `download_k8s_debs`
já é distro-agnóstico do lado do host, mas essa combinação específica ainda não foi exercitada.

---

## v0.16.1 — `tunnel expose --provider pinggy` já não fica sem URL

Bug report real (host kaeso-sys-01): `delonix tunnel expose --provider pinggy --local-port 8181`
respondia sempre `running — (URL ainda não confirmada — ver \`delonix tunnel describe\` / o log)`,
nunca uma URL real, mesmo esperando os 15s do poll.

**Causa-raiz, confirmada independentemente do delonix** (correndo o `ssh` real à mão, fora do
binário): `free.pinggy.io`'s geo-DNS, a partir deste host, resolvia sempre para um PoP regional
partido (`br.free.pinggy.io` → `lin.br.1.a.pinggy.click`) — a ligação SSH era aceite, o
`-R0:localhost:<porta>` era alocado, e o servidor fechava a ligação segundos depois, sem imprimir
nenhuma URL. Um 2.º comportamento, também reproduzido: sob `setsid`/detached (exactamente como o
`spawn_and_capture` do tunnel lança o processo), o cliente `ssh` às vezes **nem sequer sai** depois
do servidor fechar a ligação — fica pendurado indefinidamente sem progredir. Nenhum dos dois é
detectável de forma fiável só por "o processo morreu" — o processo pode ter morrido cedo, OU pode
ter ficado vivo mas parado.

### Corrigido

`spawn_pinggy` tenta primeiro `free.pinggy.io` (o endpoint DOCUMENTADO pela pinggy, mantido como
omissão), e se não sair nenhuma URL do poll — morto ou pendurado, não importa qual — mata o
processo se ainda estiver vivo (nunca deixa 2 túneis vivos para o mesmo `TunnelRecord`) e tenta
UMA vez `a.pinggy.io` (endpoint próprio da pinggy, não documentado à parte, mas que ligou com
sucesso nas mesmas condições). `spawn_and_capture` também passou a sair do poll assim que o
processo morre, em vez de esperar sempre os 15s completos — falha mais rápido quando há mesmo uma
falha a detectar.

### Validado ao vivo

- Reproduzido o bug de forma isolada (3× consecutivas, `ssh` a correr à mão, fora do delonix) contra
  `free.pinggy.io` — confirma que não é um bug de scraping de log, é uma falha real da ligação.
- Com o fix: `delonix tunnel expose --provider pinggy --local-port 8181` imprime o aviso do
  fallback e devolve uma URL pública real (`https://dlbll-105-174-64-18.free.pinggy.net`).
- `curl` ao endpoint local (`127.0.0.1:8181`, `200`) e à URL pública devolvida (`200`) — o túnel
  encaminha tráfego real de ponta a ponta, não é só uma URL capturada do log.
- `cargo test --workspace` (todos os crates, 238 testes só em `delonix-runtime-bin`), `clippy -D
  warnings` e `fmt --check` limpos.

---

## v0.16.0 — golden image sem Kubernetes: `delonix image --vm build --no-k8s`

Pedido real (host kaeso-sys-01): golden VM images para tenants que não precisam de Kubernetes —
só uma VM com o `delonix-runtime` pronto a usar. O pedido cobria quatro variantes (Ubuntu 26.04,
Ubuntu 24.04 sem k8s, Debian, Rocky Linux); por decisão explícita, a entrega é incremental. Esta
release é a **fase 1 de 3**: Ubuntu 24.04 sem Kubernetes. Debian e Rocky ficam para releases
seguintes (Rocky, dnf-based, é o maior salto arquitectural).

### Ubuntu 26.04 já funcionava — sem código novo

Confirmado ao vivo: `--ubuntu-release 26.04` contra o pipeline existente já funciona
(`cloud-images.ubuntu.com/releases/26.04/release/SHA256SUMS` redirecciona correctamente, mesma
forma que a 24.04). Não fazia parte desta release por não precisar dela.

### `--no-k8s`: o gap real que fechou

`k8s_version: None` **não** produzia uma imagem sem k8s — `k8s_repo_version` caía sempre em
`stable:/v1.31` e instalava kubeadm/kubelet/kubectl na mesma. Não havia forma de desligar.

`delonix image --vm build --no-k8s --delonix-bin <caminho>` é um caminho novo e mutuamente
exclusivo com `--k8s-version`/`--offline`/`--cri-bin` (erro claro se combinados, nunca ignorado em
silêncio). Instala:

- Os pacotes rootless que o `install.sh` exige num host normal: `slirp4netns`/`uidmap`/
  `nftables`/`iproute2`/`conntrack`.
- O binário **`delonix`** em si (não `delonix-cri` — um shim de CRI para o kubelet não serve para
  nada sem kubelet). Sem unidade systemd: o motor é invocado por CLI, é daemonless por desenho.
- O intervalo subuid/subgid da conta `delonix` (mesma lógica do `ensure_subid` do `install.sh` —
  sem isto o userns rootless só mapeia 1 uid).
- O perfil AppArmor `unconfined+userns`, necessário em hosts Ubuntu 23.10+-family com
  `kernel.apparmor_restrict_unprivileged_userns=1` — sem ele o `unshare(CLONE_NEWUSER)` falha logo
  no arranque, e uma imagem dourada "pronta a usar" que falha ao primeiro `delonix run` não seria
  pronta a usar nenhuma.

Conta/sudoers/bash-completion/limpeza de apt/reset de machine-id continuam **partilhados** com o
caminho Kubernetes (`common_customization_steps` foi dividido em `install_cri_steps` + o novo
`shared_account_steps`, sem alterar o output do caminho k8s existente — verificado pelos testes já
existentes, que continuam a passar sem alteração).

Publica em `ghcr.io/angolardevops/delonix-vm-base` (repositório novo, tag `ubuntu-24.04`;
Debian/Rocky publicarão no mesmo repositório sob `debian-12`/`rocky-9`). Sem selecção por omissão
em `Pull`/`LsRemote` nesta fase — o chamador passa sempre a fonte explícita.

### `vm-image.yml`

Novo input `no_k8s` (boolean) — troca o passo de build (`--no-k8s --delonix-bin`) e o
repositório/tag de destino.

### Validado

- `cargo build`/`clippy -D warnings`/`fmt --check`/`test` no workspace inteiro, incluindo os
  testes novos de `rootless_customization_steps` (pacotes rootless, binário `delonix` sem CRI,
  subuid/subgid, perfil AppArmor) e da validação `--no-k8s` (rejeita `--k8s-version`/`--offline`/
  `--cri-bin`; rejeita `--delonix-bin` sem `--no-k8s`).

### Limitação conhecida

Este sandbox não consegue correr `virt-customize` de ponta a ponta (`libguestfs-common` em falta,
o mesmo bloqueio já documentado noutras releases). A validação real do build acontece via
`vm-image.yml` (`no_k8s: true`) ou no host do utilizador.

---

## v0.15.0 — a golden image k8s já não redescarrega tudo em cada `kubeadm init`

Bug report real (host kaeso-sys-01, na mesma corrida que validou o HAProxy automático da
v0.13.0): um `kubeadm init` REAL redescarregava sempre TODAS as imagens core
(apiserver/controller-manager/scheduler/etcd/coredns/pause) do zero, em CADA VM criada — lento o
suficiente para estourar o próprio deadline interno do rate-limiter do kubeadm e fazer o
`wait-control-plane` falhar a meio (`client rate limiter Wait returned an error: rate: Wait(n=1)
would exceed context deadline`).

### Causa-raiz: o CAS nunca era consultado antes de descarregar um blob

`delonix_image::registry::pull_from_registry_with_creds` (usado pelo `delonix image pull` E pelo
`delonix-cri` a cada `PullImage` do kubelet) descarregava sempre cada blob da rede, mesmo quando o
conteúdo exacto já estava no `Cas` local — o método `Cas::has` já existia, simplesmente nunca era
chamado. **Corrigido**: verifica-se `has` antes de cada `GET` de blob (config + cada layer); só
descarrega o que ainda não tem. Isto sozinho já ajuda qualquer reexecução de um pull parcial ou
qualquer imagem partilhada entre pulls — mas é também o pré-requisito sem o qual pré-semear a
imagem dourada não adiantaria nada (o `delonix-cri` continuaria a redescarregar tudo na mesma).

### `image --vm build --offline` pré-semeia as imagens do `kubeadm`

Com o CAS corrigido, o modo `--offline` passa a:

1. Extrair o binário `kubeadm` do `.deb` já descarregado/verificado no HOST (`dpkg-deb -x`, sem
   instalar nada);
2. Correr `kubeadm config images list --kubernetes-version=vX.Y.Z` localmente (sem rede — é uma
   tabela interna estática do próprio binário);
3. Descarregar cada imagem no HOST através do mesmo `pull_from_registry_with_creds`, para um
   `ImageStore` de trabalho;
4. Injectar as suas 4 subpastas (`images`/`layers`/`containers`/`blobs`) em `/var/lib/delonix` do
   convidado via `virt-customize --copy-in` — o mesmo caminho que `delonix-cri` já lê em runtime.

Melhor esforço em toda a cadeia: qualquer falha (imagem em falta, `dpkg-deb` ausente, etc.) só
avisa e o build segue sem pré-semear — nunca chumba o build inteiro por isto. Só disponível no
modo `--offline` (o caminho online já resolve pacotes/imagens dentro do próprio convidado, sem o
mesmo encaixe host-primeiro que o `--offline` já usa para os `.deb`).

### Validado ao vivo

- `kubeadm config images list` a partir de um `kubeadm` extraído (sem instalar) devolveu as 7
  imagens reais da v1.34.
- Um `pull_from_registry_with_creds` real contra `registry.k8s.io/pause:3.10.1` confirmou que o
  layout do `ImageStore` resultante bate exactamente com o que o `--copy-in` espera.
- Um novo teste com um mock de registo local (contador de `GET` de blobs) confirma que um 2.º
  pull da mesma referência não toca a rede para nenhum blob já presente.

### Limitação conhecida

Não valida o build `virt-customize` completo de ponta a ponta nesta sessão (bloqueado neste
sandbox de desenvolvimento, `libguestfs-common` em falta — o mesmo bloqueio já documentado para
outras sessões; o CI real, que já publicou a `delonix-vm-k8s:1.34`/`1.35`, é onde isto corre de
verdade). Uma reconstrução real da golden image (`vm-image.yml`) é o próximo passo para confirmar
o ganho de tempo ao vivo num `kubeadm init` real.

---

## v0.14.0 — `cluster kubeadm` ganha progresso ao estilo `kind`

Pedido directo do utilizador: o log de `cluster kubeadm` (uma `println!` por VM criada, por
recipe aplicada em cada host, por passo do `kubeadm init`/`join`) ficava verboso e pouco elegante
num cluster de várias VMs — o pedido foi trazer o mesmo formato limpo do `kind create cluster`,
com uma animação por etapa.

### Um spinner por etapa lógica, não por VM/comando

`cmd::kindmode.rs` (o modo `kind`) já tinha exactamente este mecanismo — `output::Progress`, cujo
próprio comentário já dizia "like kind/spinnies". `cluster kubeadm`/`cluster apply` (via
`provision_and_apply`/`apply_ssh`) passaram a usá-lo:

```
info Creating cluster "ngolacloudlab" (kubeadm, 1.34)...
 ✓ Provisioning 2 control-plane(s) 📦
 ✓ Provisioning 3 worker(s) 📦
 ✓ Provisioning the HAProxy load balancer ⚖️
 ✓ Preparing 6 host(s) 🔧
 ✓ Bootstrapping control-plane (kubeadm init) 🕹️
 ✓ Joining 1 more control-plane(s) 🎮
 ✓ Joining 3 worker(s) 🚜
 ✓ Fetching kubeconfig 📇
```

Cada linha anima (braille spinner) enquanto a etapa inteira corre — não uma vez por VM — e fecha
com `✓`. Sem TTY (pipe, CI, `2>&1 | tee`), degrada sozinho para uma linha por etapa só quando ela
termina, sem nada durante — exactamente o que um log de CI quer, validado ao vivo. Uma falha a
meio fecha a etapa aberta com `✗` automaticamente antes do erro propagar.

### Também mais silencioso onde importa

- `ssh-keygen -q` tira o banner "Generating public/private ed25519 key pair..." que aparecia
  sempre, mesmo sem interesse nenhum na maioria das corridas.
- Os erros dentro de cada etapa ganharam contexto explícito (`[<host>] <recipe>: <erro>`) — sem
  o log granular de antes, um erro auto-contido importa mais do que nunca para diagnosticar.

Sem mudanças de comportamento — só de apresentação; `kubeconfig: ...`/`export KUBECONFIG=...`
continuam a imprimir-se no fim, como sempre.

---

## v0.13.2 — `cluster kubeadm` já não exige um checkout do código-fonte para o `delonix-cri`

Bug report real, na mesma corrida que validou o HAProxy automático da v0.13.0 (host kaeso-sys-01):
depois de provisionar as VMs e o LB com sucesso, `delonix cluster kubeadm` falhava com `não
encontrei o binário delonix-cri: usa --cri-bin <caminho>, instala-o ao lado do delonix, ou corre a
partir do checkout do código-fonte` — o utilizador tinha instalado via `install.sh` **sem**
`--with-cri` (o comportamento por omissão) e corria o comando fora de um checkout, os dois únicos
casos que a resolução do `delonix-cri` sabia tratar.

**Corrigido**, dois gaps da mesma família:

- `resolve_cri_bin` descarrega agora o `delonix-cri` (verificado contra o `SHA256SUMS` da própria
  release — o mesmo não-negociável de qualquer download deste projecto) do release do GitHub que
  bate com a versão do PRÓPRIO `delonix` a correr, já que os dois se publicam sempre juntos, na
  mesma tag. Cache em `<root>/bin/<versão>/delonix-cri` — um download por versão instalada, nunca
  mais que isso. Detecta a variante `-v3` (AVX2/BMI2/FMA) com o mesmo critério do `install.sh`.
- `dist/delonix-cri.service` (o unit systemd) é estático e não depende de versão nenhuma — passou
  a vir embutido no próprio binário (`include_str!`), escrito para a mesma pasta de cache na
  primeira vez que falta, sem precisar de rede.

Validado ao vivo com um binário isolado (sem `delonix-cri` ao lado, fora de qualquer checkout,
estado limpo) contra a v0.13.1 real publicada: descarrega, verifica, cacheia, e `cluster
apply`/`cluster kubeadm` avançam correctamente até à fase real de preparação SSH dos hosts.

---

## v0.13.1 — `install.sh` já não engole falhas de download em silêncio

Bug report real: `curl -fsSL .../install.sh | bash` mostrava `curl: (56) Failure when receiving
data from the peer` (uma falha de rede transitória) e acabava em `error SHA256 verification
FAILED for delonix-x86_64-v3-linux — corrupted or tampered download, aborting` — uma mensagem
enganosa, que implica adulteração/MITM para o que era só uma transferência que falhou.

**Causa-raiz**: `fetch_asset`/`dl_main` corriam sob `set -e`, mas terminavam sempre com `echo`
(mascarando um `curl` falhado) ou sem verificar explicitamente o `curl` do `SHA256SUMS` — e como
as duas correm dentro de `spin ... || die`, o `errexit` fica SUSPENSO para toda a árvore de
chamadas aninhada sob esse `||` (comportamento documentado do bash: uma falha só dispara o
`set -e` se NÃO estiver a ser testada por `&&`/`||`/`if`, e essa suspensão propaga-se para dentro
de funções chamadas nesse contexto). O download podia falhar por completo sem o script alguma vez
o detectar — só a verificação SHA256 (mais tarde, contra um `SHA256SUMS` em falta) apanhava o
sintoma, com uma mensagem errada.

**Corrigido**: `|| return 1` explícito em cada `curl` que tem de ser fatal — controlo de fluxo
explícito, que não depende do estado (in)consistente do `errexit` aninhado. `verify_asset` ganhou
também uma verificação separada: `SHA256SUMS` em falta agora diz claramente "could not download
SHA256SUMS — check your network and re-run (this is a download failure, not a corrupted/tampered
file)", distinta da mensagem de hash genuinamente errado.

Validado com testes funcionais isolados das funções reais do script (curl mockado a falhar
sempre) — confirma que a falha agora propaga correctamente através de `dl_main`/`spin`/`die`, e
que a mensagem de "SHA256SUMS em falta" já não se confunde com "ficheiro adulterado".

---

## v0.13.0 — `cluster kubeadm` provisiona HAProxy automaticamente para HA multi-control-plane

Até aqui, `delonix cluster kubeadm --control-plane <N>` com `N > 1` recusava sempre com erro
claro: kubeadm HA exige um endpoint estável (LB/VIP) à frente dos control-planes, e o comando não
provisionava um — o utilizador tinha de preparar um LB externo à mão e usar `delonix cluster
apply` com `controlPlaneEndpoint` já definido.

**Corrigido**: com `--control-plane > 1`, o comando provisiona automaticamente uma VM extra
(`<nome>-lb`) a correr HAProxy como balanceador TCP (L4 — a TLS do apiserver termina sempre no
control-plane real, nunca no LB), aponta o `balance roundrobin`/`option tcp-check` para a porta
6443 de cada control-plane, e usa o IP dessa VM como `controlPlaneEndpoint`. Um único comando
produz agora um cluster HA a funcionar — sem flag nova, dispara sozinho a partir de `N > 1`.

Nada mudou a jusante: `kubeadm_init`/`kubeadm_join` já suportavam multi-control-plane
(`--control-plane-endpoint`/`--upload-certs`/`--certificate-key`) desde a v1 original; a única
lacuna era não termos NENHUM endpoint a apontar-lhes. `delonix cluster apply` continua a aceitar
um `controlPlaneEndpoint` externo/manual, para quem já tem o seu próprio LB.

Novo módulo `cmd/lb.rs`: `build_haproxy_cfg` (função pura, testada) gera o `haproxy.cfg`;
`ensure_haproxy` instala o haproxy via apt se preciso, escreve a config (mesmo idioma de
`prepare_host` para o `delonix-cri`: tmpfile local → scp → `mv` privilegiado) e reinicia o
serviço — sempre reescreve + reinicia, idempotente-simples (o mesmo compromisso já aceite no
resto de `cluster apply`), seguro em qualquer re-execução porque o HAProxy é um proxy L4 sem
estado.

### Limitação conhecida

A VM do LB reaproveita o mesmo perfil (`--vcpus`/`--memory`) e a mesma imagem dourada das
restantes VMs do cluster — sem flags próprias de dimensionamento nesta versão. `--etcd-cluster
<N>` (etcd externo dedicado, isolado dos control-planes) fica para uma sessão de planeamento à
parte — ver `AGENTS.md`, secção "Próximas fases".

---

## v0.12.0 — `vm start`/`vm restart`, `cluster kubeadm` sem `--name` e com auto-pull

Pedido directo de um utilizador real, no seguimento do fix do `vm console` em v0.11.1: depois de
uma VM ficar `Stopped`, a única forma de a trazer de volta era `delonix vm create <nome>` de
novo — idempotente/auto-heal, mas exigindo as MESMAS flags (`--vcpus`/`--memory`/`--disk`/...)
que o `create` original, ou o "auto-heal" arrancaria silenciosamente com os defaults do clap
(1 vCPU, 1G) em vez da configuração real da VM.

### `delonix vm start <nome>` / `delonix vm restart <nome>`

`start` arranca uma VM parada — idempotente (já a correr = sem efeito). `restart` força sempre um
reboot real (pára primeiro se estiver a correr). Os dois reconstroem a configuração de arranque a
partir do PRÓPRIO registo persistido da VM (disco base, vcpus, memória, rede, backend,
`restart_policy`, dispositivos, e — só para libvirt — o modo de rede), sem pedir nada ao
utilizador. O overlay (e portanto o disco) é sempre reaproveitado, nunca recriado.

**Limitação honesta**: o registo de uma VM nunca guardou tudo o que o `vm create` completo aceita
— kernel/initrd/firmware/cmdline de boot directo, seed de cloud-init próprio, volumes 9p, IP
estático, VNC, e os campos avançados de libvirt (machine/cpu model/topology/TPM/video/boot
order/discos ou NICs extra/XML cru) só existem como flags do `vm create` e não sobrevivem depois
dele terminar. Uma VM que precise de algum destes continua a precisar do `vm create` original
(também idempotente) — `start`/`restart` cobrem o caso comum (imagem dourada, sem flags
avançadas), não substituem `create` para o resto.

Validado: build/clippy/fmt/testes completos do workspace, mais os dois casos novos
(`config_from_recovers_libvirt_net_mode_from_the_tap_field`,
`config_from_leaves_net_mode_none_for_cloud_hypervisor`) que fixam o comportamento exacto de
recuperação do modo de rede libvirt a partir do campo `Vm.tap`.

### `delonix cluster kubeadm` — `--name` opcional, e já não desiste quando falta a imagem

Dois bugs reais, mesmo host: (1) `--name` era obrigatório, sem a mesma analogia do nome
automático angolano que containers e `cluster create` (modo kind) já têm; (2) `--vm-image
<v>`/`--k8s-version <v>` sem correspondência local local dava sempre erro — mesmo quando a golden
é um artefacto OCI publicado precisamente para não precisar de pull manual, e mesmo quando a
imagem já estava local mas só sob o nome de convenção completo (`delonix-vm-k8s:1.34`), que um
`--vm-image 1.34` abreviado nunca batia certo.

**Corrigido**:

- Sem `--name`, `random_kubeadm_cluster_name` gera um nome livre `<rei>-<lugar>-NN` (partilha o
  gerador com `kindmode::random_cluster_name` via `names::random_name`), verificado contra as VMs
  já existentes (um cluster kubeadm é as suas próprias VMs `<nome>-cp1`/`<nome>-w1`).
- `resolve_vm_image` prefere agora o nome de convenção local (`delonix-vm-k8s:<v>`) quando o
  valor explícito não bate por si só com nenhuma imagem local.
- Sem imagem local nenhuma mesmo assim, `provision_and_apply` descarrega-a do repositório oficial
  (`ghcr.io/angolardevops/delonix-vm-k8s:<v>`) antes de continuar, em vez de recusar.

Validado ao vivo: `--vm-image 1.34` (local, sob `delonix-vm-k8s:1.34`) resolve sem tentar nenhum
download; `--vm-image 1.35` (ausente) inicia o pull real do repositório oficial; sem `--name`,
gera um nome (`nzinga-cacuaco-19` numa corrida real) e prossegue.

---

## v0.11.1 — `vm console` já não fica preso num "Active console session exists"

Bug report real (host kaeso-sys-01): depois de um `delonix vm console dev` terminar de forma não
limpa (ligação SSH caída, Ctrl-C a atingir o `virsh` em primeiro plano, terminal fechado), toda
tentativa seguinte de `delonix vm console dev` falhava com `error: operation failed: Active
console session exists for this domain` — sem saída a não ser reiniciar o `libvirtd` do host.

`delonix vm console` é um comando de um único operador ("volta a ligar-me a esta VM"); uma sessão
presa da tua PRÓPRIA ligação anterior é o caso esmagadoramente comum, não um segundo espectador
real a proteger. Corrigido com a flag `--force` do `virsh console` (feita exactamente para isto —
"disconnect already connected sessions"), em vez de recusar para sempre.

---

## v0.11.0 — `ls-remote` para imagens VM douradas

Feature pontual: descobrir que versões da imagem VM dourada estão publicadas num registo
remoto, ANTES de fazer `pull`. Faltava — `image vm ls` só mostra o que já está local.

### `delonix vm ls-remote` / `delonix image vm ls-remote` / `delonix image --vm ls-remote`

Lista as tags do repositório OCI (`GET /v2/<repo>/tags/list`) — sem argumento, o repositório
OFICIAL da Delonix (`ghcr.io/angolardevops/delonix-vm-k8s`). Reutiliza inteiramente o `Client`
já usado por `pull`/`push` (`crates/delonix-image/src/registry.rs`) — o mesmo fluxo de
autenticação 401→`WWW-Authenticate`→token→retry, por isso funciona contra ghcr.io/Docker Hub/
qualquer registo v2 tal como o `pull` já funciona, sem código novo de auth.

Como o `pull`, os TRÊS pontos de entrada (CLI dedicada `vm`, `image vm`, `image --vm`) convergem
no mesmo `VmImageCmd::LsRemote` — o mesmo padrão triplo que o `pull` já seguia, para os três
caminhos ficarem consistentes desde o início (ao contrário do `pull`, que só ganhou essa
convergência num fix posterior).

**Limitação conhecida**: uma só página (sem paginação por `Link` header) — adequado para o
punhado de tags que um repositório de imagem dourada realisticamente tem; um repositório com
centenas de tags só veria a 1.ª página do registo.

Validado ao vivo contra o ghcr.io real: `delonix vm ls-remote` (sem argumento) devolve a tag
`1.34`, hoje a única publicada.

---

## v0.10.2 — `image --vm pull`/`image vm pull` sem argumento voltam a funcionar

Fix pontual, encontrado ao vivo por um utilizador num host real: `delonix image vm
pull --name delonix-vm-k8s:1.34` (sem `source`) dava `error: the following required
arguments were not provided: <SOURCE>` — ao contrário do que a própria ajuda do
comando promete ("com nenhum argumento, a imagem OFICIAL da Delonix"), comportamento
que só `delonix vm pull` (uma definição de CLI irmã, separada) tinha mesmo.

Três pontos de entrada partilham o mesmo `VmImageCmd::Pull` por baixo, e os TRÊS
precisavam do fix (cada um alcançável independentemente e independentemente
partido): `delonix vm pull` já funcionava; `delonix image vm pull` e `delonix image
--vm pull` tinham `source`/`image` tipados como `String` obrigatória ao nível do
clap, recusando a invocação sem argumento antes de qualquer código correr. Os três
passam agora pelo mesmo `source.unwrap_or_else(|| OFFICIAL_VM_IMAGE.to_string())`
dentro de `vmimage::run`. `ImageCmd::Pull.image` também serve o caminho (não
relacionado) de pull de imagens de container, que não tem default sensato — esse
handler passa a exigi-lo explicitamente com um erro claro em vez de depender do clap.

Validado ao vivo: os 3 caminhos tentam agora o pull real em vez de errar de
imediato; um `image pull` simples (sem `--vm`, sem referência) continua,
correctamente, a exigi-la.

---

## v0.10.1 — 2 CRITICAL + 3 HIGH corrigidos (revisão adversarial completa)

Patch de segurança urgente. Pedida uma revisão de código completa ao runtime — bugs,
gaps, erros de design/arquitectura que pudessem comprometer o sistema em ambientes
críticos. Correram 4 auditorias adversariais em paralelo: (1) re-verificação dos 35
achados da auditoria anterior (`docs/AUDITORIA-E2E.md`) contra o código actual, (2)
primeira auditoria de sempre aos 104 blocos `unsafe` de `delonix-runtime/lib.rs`, (3)
auditoria fresca ao holder/control-socket de `delonix-net/infra.rs`, (4) auditoria de
todo o código novo da sessão anterior (Tunnel, ShareVolume, `cluster.rs`, specs
agrupados). Dois achados CRITICAL e três HIGH — todos já em produção no v0.10.0 — foram
corrigidos de imediato em vez de só reportados.

### 2 CRITICAL

- **`kind: ShareVolume` com `name: ".."` escapava para o Storage pai inteiro.** O
  charset do `VolumeStore::valid_name` aceitava um nome composto SÓ pelo carácter `.`
  (`".."` passava). Juntar esse nome ao caminho do Storage pai resolve, sem normalizar,
  para o próprio directório de dados do pai — bypass total do isolamento, e
  `sharevolume rm --purge-data` nessa fatia apaga o NAS partilhado inteiro. Corrigido na
  raiz: `valid_name` passa a recusar qualquer nome a começar por `.` ou a conter `..`,
  protegendo todos os consumidores do store, não só o ShareVolume.
- **Injecção de argv SSH via token do `kind: Tunnel`.** O token do provider `pinggy`
  era embutido como o ÚLTIMO argumento posicional do `ssh`, sem `--` a separar. Um
  token a começar por `-` (ex.: `-oProxyCommand=<comando>`) é interpretado pelo `ssh`
  como uma OPÇÃO, executando o comando do atacante via `/bin/sh -c` antes de qualquer
  ligação de rede — RCE local como quem corre `tunnel apply/expose`. Corrigido no único
  ponto de resolução do token (protege pinggy E ngrok) mais um `--` no argv como defesa
  em profundidade.

### 3 HIGH

- **Nomes de container nunca validados** — um `container run --name registry.npmjs.org`
  vulgar (sem manifesto, sem privilégio) sequestra a resolução DNS desse hostname para
  TODOS os outros containers/VMs do nó, em qualquer namespace. Corrigido com
  `valid_container_name` (exclui `.` deliberadamente).
- **`cluster kubeadm --copy-kubeconfig` confiava no `admin.conf` remoto por inteiro** —
  um `users[].user` pode legalmente ter um `exec:` (execução de comando arbitrário LOCAL
  da próxima vez que o `kubectl` usar o contexto). Um control-plane comprometido depois
  do provisionamento vira execução de código na máquina do operador. Corrigido:
  constrói-se um `cluster`/`user` NOVO só com os campos que o `admin.conf` real do
  kubeadm tem, nunca clonando o bloco bruto.
- **Bind-mounts seguiam symlinks plantados pela imagem, antes do `pivot_root`** — um
  `mount_target_safe` só lexical (rejeita `..`) não chega: a criação do destino
  (`create_dir_all`/`open`, ambos seguem symlinks) corre com `/` ainda a ser o
  filesystem real do host. Uma imagem com `/etc -> /root` redirecciona a criação de
  ficheiros/directórios reais para o host, como o uid do motor. Corrigido com
  `safe_bind_target`: resolve o caminho componente a componente, recusando qualquer
  symlink — o equivalente, do lado do motor, ao `confine_to` já usado no `COPY` do build.

### Estado da auditoria anterior

Reverificados os 6 HIGH da auditoria de segurança do v0.9.0: continuam corrigidos. Os
outros 29 achados MEDIUM/LOW/por-verificar de `docs/AUDITORIA-E2E.md` continuam em
aberto — não tocados nesta release, ficam como lista de trabalho priorizada para uma
próxima sessão dedicada.

### Nota de honestidade

Todas as correcções foram validadas ao vivo contra o exploit real (não só testes
unitários): o `..` do ShareVolume recusado antes de tocar em disco, um token malicioso
bloqueado via `tunnel apply -f` real sem efeito lateral, um nome de container com ponto
recusado via `container run --name` real, um `admin.conf` malicioso com `exec:`/
`insecure-skip-tls-verify` removido enquanto os campos legítimos sobrevivem, e a recusa
de symlink do `safe_bind_target` cobre tanto uma componente intermédia do caminho como o
próprio alvo final.

---

## v0.10.0 — kind: Tunnel, kind: ShareVolume, e um `cluster kubeadm` finalmente real

O caminho `delonix cluster kubeadm`/`cluster apply` (modo `vm`) nunca tinha corrido de
ponta a ponta antes desta release — cada tentativa real de o levar até um cluster
Kubernetes a funcionar encontrou um bug novo, corrigido no acto. Também dois Kinds novos
(`Tunnel`, `ShareVolume`), ambos validados ao vivo com tráfego/isolamento reais, não
simulados.

### `kind: Tunnel` — expor um serviço à internet pública

`delonix tunnel apply|expose|ls|describe|rm`: leva tráfego da internet pública até UMA
porta local, sem conta, sem IP público, sem tocar no router. Três providers, cada um o
mecanismo REAL desse serviço:

- **pinggy** — zero binário extra (`ssh` puro, já uma dependência do projecto). Grátis,
  efémero.
- **ngrok** — precisa do agente `ngrok` no PATH; a URL pública sai da API local do
  próprio agente (não de scraping de logs).
- **cloudflare** — precisa de `cloudflared`; por agora só o quick-tunnel efémero
  (`*.trycloudflare.com`, sem conta). Um tunnel NOMEADO com domínio próprio precisa da
  API do Cloudflare (accountId/zoneId/token) — desenhado mas não implementado, ver
  limitações abaixo.

Junta-se ao `kind: HTTPRoute` (já existente) apontando `localPort` para onde o proxy L7
escuta — uma só URL pública, routing por Host para tantos backends quantos precisares.
**Validado ao vivo**: tráfego HTTPS real da internet chegou a um servidor local através
de um tunnel pinggy (HTTP 200); `rm` confirmado a matar o processo agente a sério.

### `kind: ShareVolume` — multi-tenant num só NAS

`delonix sharevolume apply|ls|describe|rm`: várias cargas a partilhar UM export
NFS/CIFS/WebDAV (`kind: Storage`), cada uma com o seu ponto de montagem ISOLADO e a sua
QUOTA. Sem mecanismo de montagem novo: cada `ShareVolume` é um subdirectório real da
árvore já montada, registado como o seu próprio volume — a isolação é confinamento de
caminho puro e o consumo usa `-v <nome>:/destino` de sempre, zero código novo do lado do
container/vm/pod. Quota SOFT (uso medido + alerta) — o caminho HARD (loopback ext4) não
compõe com um subdirectório de um mount de rede. **Validado ao vivo**: dois tenants no
mesmo NAS, escrita num nunca visível no outro, alerta a mudar para OVER ao passar a
quota, um container real a ler/escrever por `-v` normal.

### `delonix cluster kubeadm` — 6 bugs reais, cada um encontrado a correr o comando a sério

Este caminho (provisiona VMs + faz o bootstrap kubeadm) nunca tinha sido validado de
ponta a ponta. Persistir até um cluster real a funcionar encontrou, um a um:

1. **cloud-init só chegava ao utilizador `ubuntu`**, nunca ao `delonix` que a imagem
   dourada cria e que o comando usa como login SSH — corrigido (`users:` scoped no
   `user-data`).
2. **`known_hosts` obsoleto** bloqueava a recriação de uma VM no mesmo IP (o
   `StrictHostKeyChecking=accept-new` recusava, correctamente, uma chave de host
   diferente) — purga automática antes da 1.ª tentativa.
3. **`/etc/machine-id` partilhado entre VMs clonadas** — o `virt-customize` deixava um id
   real gravado (uma imagem cloud normal vem vazia de propósito); o DUID do DHCP deriva
   dele, e o dnsmasq via 3 VMs como o MESMO cliente, movendo o lease de uma para a
   outra. Corrigido: `truncate -s 0 /etc/machine-id` como o último passo do build.
4. **O loop de espera de SSH fixava-se no 1.º IP visto** e nunca voltava a verificar —
   se o DHCP da VM mudasse a meio do boot (observado ao vivo), o loop martelava um
   endereço morto até ao `--boot-timeout`.
5. **`virsh domifaddr` lista leases obsoletas em ordem nenhuma** — apanhado a escolher o
   IP errado tanto pela primeira como pela última linha. Corrigido: `virsh
   net-dhcp-leases` tem um `Expiry Time` real e ordenável; a resolução de IP passa a
   filtrar pelo MAC da própria VM e escolher o mais recente.
6. **`kubeadm init`/`join` nunca passavam `--cri-socket`** — o kubeadm só auto-detecta
   entre um punhado de caminhos conhecidos (containerd/CRI-O), tentava o socket do
   containerd (que não existe nesta imagem) e falhava logo no preflight, antes de tocar
   no `delonix-cri` de todo.

Com os 6 corrigidos, o cluster passou a chegar consistentemente a `kubeadm init` a
gerar certificados, kubeconfig e a arrancar o kubelet — o preflight do CRI, que falhava
sempre antes do fix #6, passa a verde. (A validação completa até um nó `Ready` ficou
limitada por pressão de memória do sandbox onde isto foi corrido, não por nenhum destes
bugs — cada um tem prova ao vivo independente do resultado final.)

Também novo: **`cluster kubeadm --copy-kubeconfig`** espera por todos os nós `Ready`
antes de tocar em `~/.kube/config`, e passa a MERGE o cluster novo como o seu próprio
contexto em vez do comportamento antigo (`fs::copy` simples, que só copiava na
primeira vez — o 2.º cluster nunca aparecia). E **`--k8s-version 1.35`** passa a
seleccionar automaticamente `delonix-vm-k8s:1.35` quando `--vm-image` é omitido.

### `delonix vm ls` — mais colunas, `image --vm ls` mais claro

`vm ls` ganha **UPTIME** (desde o boot actual, não desde a criação — distinto para uma
VM reiniciada), **ROLE** (control-plane/worker, lido da convenção de nomes do `cluster
kubeadm`), **GPU** (dispositivos PCI passthrough, agora persistidos no registo da VM) e
um `--ports` opt-in (sonda TCP a um punhado de portas conhecidas). `image --vm ls`:
coluna `UBUNTU` renomeada para `DISTRO`, mais uma coluna `KERNEL` nova (a versão do
kernel instalado, lida via `virt-cat` sem nunca arrancar a imagem).

### Layout YAML agrupado para `kind: Vm`/`kind: Container`

Os specs destes dois Kinds tinham crescido para 30-40 campos sem estrutura nenhuma além
de comentários. Passam a aceitar uma forma AGRUPADA (`resources:`/`network:`/`boot:`/
`cloudInit:`/`libvirt:` na Vm; `resources:`/`network:`/`security:`/`storage:`/`env:`/
`limits:` no Container) — a forma plana antiga continua 100% suportada, sem quebrar
nenhum manifesto existente; os `examples/` passam a mostrar a forma nova.

### Documentação

Site regenerado: `httproute`/`tunnel`/`sharevolume`/`dash`/`docker-api` estavam
completamente ausentes da referência (sem página, sem entrada na navegação). Exemplos
passam a poder mostrar o RESULTADO real de um comando, não só o comando. Novo projecto
completo (`examples/delonix-temp/` + tutorial): uma API FastAPI de tempo real, corrida a
sério — build multi-stage → `container run` → `tunnel expose` até uma URL pública real,
confirmada com `curl` de fora da máquina.

### Limitações conhecidas

- `Tunnel` com provider `cloudflare`: só o quick-tunnel efémero — um tunnel nomeado com
  domínio próprio precisa da API do Cloudflare (accountId/zoneId/token), ainda por
  implementar.
- `cluster kubeadm`: validação completa até um nó `Ready` não foi possível fechar nesta
  sessão por pressão de recursos do ambiente de desenvolvimento (não um bug do código —
  cada um dos 6 fixes tem prova ao vivo independente).
- Núcleo de syscalls do motor continua sem auditoria de segurança adversarial (ver
  release anterior).

---

## v0.9.0 — segurança fechada, build de produção (multi-stage/ARG/cache) e API Docker (leitura)

A maior release em superfície desde o extraction do monorepo: fecha os 6 achados de
segurança HIGH da auditoria adversarial, e resolve a maior parte da "Fase 2" do plano de
paridade com Docker/Podman (`docs/COMPARACAO-DOCKER-PODMAN.md`) — build multi-stage,
`ARG`, cache de camadas — mais uma primeira fatia (leitura) da API Docker Engine.

### Segurança — 6 HIGH corrigidos (auditoria de 2026-07-21)

Todos confirmados por 2 céticos adversariais independentes, nenhum corrigido antes desta
release (`docs/AUDITORIA-E2E.md`):

- **Path traversal em whiteouts OCI** — uma imagem maliciosa apagava ficheiros fora do
  rootfs no `container run` rootless por omissão. Corrigido: `safe_rel` no ramo de
  whiteout + confinamento contra symlink plantado por uma layer anterior.
- **IDs do CRI sem validação** — um kubelet comprometido apagava/lia `*.json`
  arbitrário via `../`. Corrigido: whitelist centralizada em `write_rec`/`read_rec`.
- **Nome de VM ainda escapava o fix anterior** — `generate_seed_iso` escrevia antes de
  `create()` validar o nome. Corrigido na origem.
- **kubeconfig cluster-admin exposto** em `/tmp` a modo 0644. Corrigido: `sudo cat` para
  stdout do SSH, nunca toca em disco remoto.
- **`COPY` do build contornável por symlink** — reabria leitura/escrita arbitrária de
  ficheiros do host. Corrigido com confinamento canonicalizado + teste de regressão.
- **Socket de gestão sem autenticação de peer** — condições comuns davam `container
  exec` (execução arbitrária em qualquer container) a qualquer processo local. Corrigido
  com `SO_PEERCRED` + modo 0600, também aplicado ao socket do `delonix-cri`.

**Nota de honestidade**: os fixes foram testados por quem os fez, não confirmados por
uma 2.ª auditoria independente; o núcleo de syscalls (104 blocos `unsafe`) continua sem
revisão adversarial nenhuma. Ver a comparação pública para o estado de segurança
actualizado: [delonix vs Docker/Podman](https://angolardevops.github.io/delonix-runtime/comparacao.html).

### Build multi-stage (`FROM ... AS` + `COPY --from`)

Cada estágio ganha o seu próprio container/rootfs; um estágio pode construir sobre outro
(`FROM <estágio-anterior>`, clonado via `cp -a --reflink=auto` — preserva symlinks/
permissões, ao contrário de uma cópia recursiva ingénua). Único limite conhecido: em modo
root (overlay), o estágio final ainda tem de ser uma imagem real (sem lineage OCI para um
estágio clonado) — erro claro, não silencioso; sem essa restrição em rootless.

### `ARG`/`--build-arg`, e `USER`/`ENTRYPOINT` já sobrevivem ao build

`ARG NAME[=default]` com substituição `${NAME}`/`$NAME` (incluindo antes do 1.º `FROM`,
para `FROM alpine:${VERSION}`); `--build-arg`/manifesto `buildArgs` só têm efeito num
nome que o Dockerfile declare, como no Docker. `USER`/`ENTRYPOINT` deixam de se perder no
commit rootless (antes só o `ENTRYPOINT` do modo root sobrevivia; `USER` perdia-se
sempre, nos dois modos, e nem chegava ao JSON de config OCI).

### Cache de camadas por instrução (rootless)

Um `RUN`/`COPY` repetido não volta a executar — cadeia de hash por instrução,
`--no-cache`/manifesto `noCache` para saltar. **Dois bugs reais apanhados a testar, não a
rever código**: sincronizar um cache-hit no rootfs de um container já activo corrompia os
mounts de `/proc`/`/sys`/`/dev` (corrigido: um cache-hit clona sempre para um container
novo, nunca escreve por cima de um já vivo); e uma fuga de rootfs **pré-existente em
todos os builds rootless desde sempre** (o `unmount_rootfs` preserva deliberadamente o
rootfs — certo para um container real, errado para o container de trabalho efémero de um
build — `remove_container_dir` agora corre também). Modo root continua sem cache
(`commit_upper` precisa de um `upper/` real que um clone plano não tem).

### API Docker Engine — fatia de leitura

`delonix docker-api` (socket próprio, `/run/delonix-docker.sock` por omissão):
`/_ping`, `/version`, `/info`, `/containers/json`, `/images/json` — o suficiente para
`docker version`/`ps`/`images`/`info` apontados via `DOCKER_HOST=unix://<socket>`
funcionarem contra o estado real do delonix. **Validado contra um `docker` CLI real**
(27.3.1) — o protocolo (negociação de versão via o header `Api-Version` da resposta ao
`/_ping`) foi capturado ao vivo antes de escrever código, não adivinhado da
especificação. Mesma postura de segurança do socket de gestão: 0600 + `SO_PEERCRED`
(só o próprio utilizador). **Por fazer**: as mutações (`create`/`start`/`exec`) — o que
falta para `docker run`/`docker compose up`; qualquer rota ainda não implementada dá 404
claro em vez de um erro confuso do lado do cliente.

### Limitações conhecidas

- Núcleo de syscalls do motor sem auditoria de segurança adversarial (ver acima).
- API Docker Engine só de leitura — sem `docker compose`/testcontainers ainda.
- Sem BuildKit real (`RUN --mount=secret`, `--platform`).
- `container run` não aplica automaticamente o `USER` guardado numa imagem — só um
  `--user` explícito o faz (gap separado, encontrado ao validar esta release).

---

## v0.8.0 — diagnóstico de crash (razão + forense) e re-supervisão de `--restart` no `start`

Motivado por uma investigação real a containers a aparecerem como **"Dead"** sem
explicação (`kaeso-odoo` em produção). A causa-raiz exacta ficou em aberto — um teste
controlado mostrou que processos órfãos do `exec` não reparentam necessariamente para o
PID 1 do container, mas sim para o subreaper mais próximo na árvore REAL de processos
(tipicamente `systemd --user`, se estiver na cadeia de ancestrais) — mas duas melhorias
de resiliência ficaram claras independentemente da causa exacta, e é isso que esta
release traz.

### Diagnóstico automático de crash

`reconcile_status` grava agora **porquê** um container passou a `Crashed`, no momento
em que deteta:

- `crash_reason`: `process_gone` (o pid do init já não existe) ou `pid_reused` (o kernel
  reciclou o pid para um processo não relacionado antes de darmos por isso).
- `crashed_at`: timestamp Unix.

Ambos aparecem em `container describe`/`ls`/`inspect`, e são limpos automaticamente no
próximo arranque bem-sucedido (não ficam a apontar para uma causa já resolvida).

Na primeira deteção, é também gravado um **snapshot forense best-effort**:
`containers/<id>/crash-<ts>.log` (razão + as últimas ~8 KiB do log do container) e um
evento `container crashed` em `delonix system events`. **Limitação honesta**: o engine
nunca é o pai real do processo do container (é reparented no arranque — arquitectura
daemonless), por isso não há `waitpid` possível aqui e nunca há exit code/sinal
capturado — só esta pista indirecta. Registos de crashes ANTERIORES a esta versão não
são anotados retroactivamente.

### `container start` volta a supervisionar `--restart`

Até agora, se o **supervisor** de um container `run -d --restart always|unless-stopped|
on-failure` morresse junto com ele (reboot do host, `kill -9` no supervisor), o
container ficava "Dead" **para sempre** — a política ficava persistida mas sem ninguém
a aplicá-la. `container start` agora reconhece a `restart_policy` guardada e volta a
entrar em `run_supervised`, fechando esse gap.

**Continua por fazer** (âmbito desta release): não há forma de definir/mudar
`--restart` num container já existente sem recriar (`container update` ainda não tem
essa flag); e não há nenhum processo de fundo que note sozinho um crash não
supervisionado sem alguém correr `start` — coerente com a arquitectura daemonless
documentada, não um bug.

### Validado ao vivo (sandbox, sem tocar em containers de produção)

`odoo:16` + `postgres:15` de teste: `kill -9` ao PID 1 → "Dead" com razão + evento +
ficheiro forense correctos; `start` limpa a razão. `alpine --restart always`: matar só
o supervisor → fica "Dead" sem recuperar sozinho (confirma o gap); `start` → volta a
supervisionar; matar só o PID 1 depois disso → recupera sozinho em segundos.
`cargo test --workspace`: 275 testes, 0 falhas.

---

## v0.7.21 — pods reais multi-container (`kind: Pod`), netns + IPC + UTS partilhados

Culminação (e correcção + validação E2E) da série de **pods reais multi-container**
iniciada em v0.7.19/v0.7.20: N containers a partilhar as namespaces de um pod, como
no Kubernetes.

### `delonix pod` / `kind: Pod` — N containers, namespaces partilhadas

Um pod agrupa N containers que **partilham as namespaces do pod** e vivem/morrem como
uma unidade:

- **Rede (netns)** — mesmo IP, `localhost` entre si (Fase 1, v0.7.19).
- **IPC** (System V/POSIX shm/queues) + **UTS** (hostname) — reais e privadas ao pod
  (Fase 2, v0.7.20).
- **PID** (`shareProcessNamespace`) — o campo está no schema; a implementação é a fatia
  seguinte.

Superfície: `delonix pod create -f <manifesto>` / `ls` / `describe` / `rm` / `logs`, e
**`kind: Pod`** no manifesto (mesmo schema `spec.containers[]` do `kind: Container`, mas
com N containers) + grupo `pods:` no `kind: Stack` + `--dry-run`.

**Como funciona** (rootless, daemonless): o pod tem uma netns SDN nomeada no holder
(`pod-<nome>`, com IP na `delonix0`); cada container junta-a via o re-exec `nsenter …
ip netns exec` (`--pod`). O 1.º container segura o ipc/uts; os restantes fazem `setns`
de `/proc/<pid>/ns/{ipc,uts}` — possível em rootless **porque o re-exec já os põe no
userns do holder, onde o `setns` tem privilégio**. Membership derivada dos labels
(`delonix.io/pod=<nome>`), sem registo novo — como `cluster`/`stack`. Tapa também o gap
do CRI root-mode (que chamava um `delonix pod create/rm` inexistente).

### Validação E2E (rootless, real)

Pod de 2 containers `alpine`, leitura directa de `/proc/<pid>/ns/*` no host:

| namespace | container a | container b | host | |
|---|---|---|---|---|
| net | `4026533752` | `4026533752` | `4026531833` | **partilhada** |
| ipc | `4026533818` | `4026533818` | `4026531839` | **partilhada** |
| uts | `4026533817` | `4026533817` | `4026531838` | **partilhada** |
| pid | `…819` | `…822` | `…836` | separada (Fase 3) |

hostname e IP iguais nos dois; `pod rm -f` limpa tudo sem tocar noutros containers. Ou
seja: o `setns` de IPC/UTS através do userns do holder **funciona mesmo em rootless**.

### Correcção (o que motivou esta release)

- **`pod rm` propaga falhas** em vez de sucesso silencioso — apanhado pelo E2E: `pod rm`
  (sem `-f`) dizia `removed` mas os containers continuavam a correr (o `cmd_rm` sem force
  recusa um container a correr e o erro era engolido). Agora reporta a falha com erro
  claro (aponta para `pod rm -f`) e só desmonta a netns partilhada quando **todos** os
  membros saem — coerente com o invariante "sem falha silenciosa".

### Limitações conhecidas

- **PID partilhado (`shareProcessNamespace`)** ainda não implementado — obriga a
  reestruturar o `container_init` (`setns(pid)` + `fork`), fatia dedicada seguinte.
- `delonix container exec` **não entra na ipc-ns** do container (gap pré-existente do
  `exec`, não dos pods) — a partilha de IPC do pod é real na mesma (validada host-side).
- `--expose` (auto-registo no proxy L7) por-pod ainda não ligado aos membros.

---

## v0.7.18 — `vm bridge`: VM↔container por IP directo (EXPERIMENTAL, root, opt-in)

### VM — `delonix vm bridge`/`unbridge`

A última fronteira que o modelo rootless não fecha sozinho: dar a uma VM libvirt
alcançabilidade **DIRECTA por IP** aos containers da SDN (e vice-versa). A bridge
da SDN (`delonix0`/`dlxn…`) vive dentro do netns do holder (`unshare --user
--net`), inalcançável do host sem `CAP_NET_ADMIN` no init-netns — por isso `vm
bridge` **exige root**, é a excepção deliberada ao daemonless-rootless, e usa
**dry-run por omissão** (só imprime o plano; `--apply` executa).

- **Mecanismo**: `veth` do host para dentro do netns do holder (ponta SDN
  enslaved à bridge da rede) + endereço/rota no host + `ip_forward` + rota de
  retorno das subnets das VMs dentro do holder. **Sem SNAT**: o container vê o
  IP real da VM, e o firewall `ingress`/`egress` por-container continua a
  governar o tráfego.
- **Robustez**: regras `iptables -I FORWARD` ACCEPT nos dois sentidos (contra o
  REJECT default do libvirt), e establish **idempotente** (limpa um veth órfão
  antes de criar). `vm unbridge <rede>` desfaz tudo.
- **Segurança**: abre VM↔container **só na rede indicada**; a subnet da VM é a
  NAT do libvirt (ex.: `192.168.122.0/24`), **não** a LAN externa.
- **Sob sudo** resolve o state do utilizador invocador (`$SUDO_USER`), não do
  root — encontra as tuas redes/holder na mesma.

**Validado end-to-end** num host real: de dentro de uma VM libvirt,
`ping`/`curl` a um container por IP directo → **HTTP 200** (`ttl=63`, uma hop
pelo forward do host); `unbridge` limpa. Complementa o `vm reach` (VM→container
por porta publicada, **sem** privilégio) da v0.7.15.

**Follow-ups conhecidos**: persistência (re-aplicar num respawn do holder) e
**descoberta por NOME** (a VM resolver `<container>.<ns>.delonix.internal` via o
DNS do holder — os IPs de container são dinâmicos por DHCP). As mensagens do
comando estão em EN (i18n do `pt.po` pendente para este comando experimental).

---

## v0.7.15 — `vm reach` (descoberta VM→container) + `kind: Container` forma de Pod k8s

### VM — `delonix vm reach`

Descoberta de como as VMs alcançam os serviços de container, sem dataplane novo
nem privilégio. Uma porta publicada só é alcançável de dentro de uma VM libvirt
se estiver ligada a um endereço que a VM roteia — o **gateway da rede da VM**
(ex.: `192.168.122.1`), não o loopback (o default SEGURO, que faz o VM→container
falhar em silêncio com "connection refused").

- `delonix vm reach` lista os gateways das redes de VM (`virbr*`), lê o bind
  VIVO de cada porta publicada (via `ss`) e separa **"alcançáveis a partir de
  VMs"** (endereço:porta a usar) dos **"loopback-only"**, com o comando exacto
  para os expor (`unpublish` + republish com `DELONIX_PUBLISH_ADDR=<gateway>` —
  alcançável pelas VMs dessa rede, **não** pela LAN externa, que é NAT).
- Read-only, zero privilégio, zero mudança ao default seguro.

**Provado E2E ao vivo**: de dentro de uma VM, `curl <gateway>:<porta>` → HTTP 200
para um container na SDN; o loopback-bound recusa, como esperado. `container→VM`
já funcionava nativamente (o egress por-container governa-o). O IP 10.x **directo**
VM→container (bridge virbr0↔SDN) continua a exigir um dataplane privilegiado
(veth+rotas, `CAP_NET_ADMIN` no init-netns) — trabalho opt-in, fora deste release.

### Container — `kind: Container` com a forma de um Pod k8s

O `kind: Container` passa a aceitar a FORMA de um Pod do Kubernetes quando
`spec.containers` está presente (a alternativa "flat" continua totalmente
suportada; as duas formas nunca se misturam):

```yaml
spec:
  containers:
    - image, command (ENTRYPOINT), args (CMD),
      ports: [{ containerPort, hostPort, protocol, hostIP }],
      env: [{ name, value }],
      volumeMounts: [{ name, mountPath, readOnly }],
      resources: { limits: { cpu, memory } },
      securityContext: { privileged, runAsUser, readOnlyRootFilesystem,
                         capabilities: { add, drop } },
      workingDir
  volumes: [{ name, hostPath | emptyDir | persistentVolumeClaim | source }]
  network / restartPolicy / hostname / expose   # extensões delonix
```

**v1**: exactamente UM container por Pod (erro claro se >1). Normaliza para o
MESMO `RunOpts` interno da forma flat — o motor fica intocado. 1.ª fatia do
pedido "manifestos mais parecidos ao k8s".

---

## v0.7.12 — VM com IP alcançável por omissão (`nat` inteligente + `--ip` estático)

### VM — rede

Do bug report real: `vm create dev` mostrava `IP <none>` para sempre. Sem
`--net-mode` e em rootless, o backend libvirt caía em `qemu:///session`
user-mode (SLIRP), cujo IP é invisível ao `domifaddr` e inalcançável do host.

- **Default inteligente `nat`**: sem `--net-mode`, se a conexão de SISTEMA do
  libvirt é utilizável (utilizador no grupo `libvirt`), a VM passa a receber
  **IP por DHCP da rede libvirt** — visível no `vm ls` e alcançável. Só quando
  o system libvirt não está disponível fica user-mode, e aí o `create` **avisa
  alto** ("no reachable IP — join the `libvirt` group, or pass `--net-mode`")
  em vez de um `<none>` silencioso.
- **`--ip <estático>`** (e `spec.ip` no manifesto) — reserva DHCP MAC→IP na
  rede libvirt (modo `nat`). O guest não precisa de config de rede no
  cloud-init; noutros modos, erro claro.
- **`vm ls`/`--wait` corrigidos**: `Vm.tap` regista o modo EFECTIVO
  (`nat`/`bridge`/`user`), por isso o `--wait` espera o lease DHCP de uma VM
  `nat` em vez de desistir aos 3s (antes desistia para qualquer VM libvirt sem
  IP imediato).

### VM — dois bloqueios corrigidos pelo caminho

- **AppArmor + golden image**: o QEMU abria o overlay mas levava `Permission
  denied` no qcow2 base (`vm-images/…`). O perfil AppArmor por-domínio
  (virt-aa-helper, Ubuntu) só autoriza caminhos presentes no XML — o domínio
  passa a declarar `<backingStore>` explícito (formato via `qemu-img info`,
  nunca pela extensão).
- **DNS interno resolve VMs `nat`**: uma VM `nat` vive na `virbr0` do HOST e o
  seu MAC nunca aparece na tabela `neigh` do holder (o único mecanismo
  anterior). O `dns_resolve` passa a usar o **IP do registo** primeiro (neigh
  como fallback para VMs Cloud Hypervisor), e o `vm status` **persiste** o IP
  aprendido por DHCP após o arranque.

### Alcançabilidade VM↔container (validado ao vivo)

Container → VM funciona nativamente (container na SDN → holder → slirp → stack
do host → `virbr0`; provado com banner SSH recebido de dentro de um container),
e o egress por-container governa-o. VM → container por IP directo continua a
passar por portas publicadas no host ou pelo proxy L7 (o NAT do slirp esconde
os IPs de container) — um dataplane que exponha IPs de container a VMs é
trabalho futuro (`delonixd`), fora do âmbito deste fix.

---

## v0.7.11 — firewall: o último comando ganha (`allow` depois de `deny` volta a abrir)

### Firewall `ingress`/`egress`

Do bug report real: `ingress deny <c> 8069` seguido de `ingress allow <c> 8069`
deixava o serviço bloqueado para sempre — as regras acumulavam e a chain nft é
first-match terminal, por isso o deny antigo (acima) ganhava sempre. Agora:

- **O último comando ganha** (semântica `ufw`): uma regra nova para o mesmo
  match (proto/porta/origem, com `""`≡`0.0.0.0/0`≡`*`) **substitui** a
  existente, e o output di-lo: `(replaces the previous deny rule for this
  match — the last command wins)`.
- **Aviso de sombra**: numa sobreposição parcial (ex.: `deny 8069` vs
  `allow tcp/8069` — matches distintos), avisa que a regra anterior continua a
  casar primeiro e dá o comando exacto para a remover.
- **`ingress rm` / `egress rm` novos** — remoção cirúrgica de regras:
  `rm <c> 8069` remove as regras tcp/udp/any dessa porta; `rm <c> tcp/8069` só
  a tcp; `--from`/`--to` filtram por CIDR; `*` remove todas. Complementa o
  `clear` (tudo-ou-nada); firewall vazia desaparece por inteiro, como no `clear`.
- **`ingress unpublish` funciona em containers parados** (sem rede custom): o
  hostfwd vive no slirp por-container, que morre com ele — não há dataplane
  para limpar; remove-se o registo (antes: erro "container is not running" e o
  publish ficava preso para sempre).

Validado end-to-end ao vivo: `deny` → porta bloqueada; `allow` → HTTP 200 com
uma só regra no `ls`; `rm` limpa as sobrepostas; `unpublish` de container
parado limpa o registo. Tudo com tradução PT (`--l18n=pt`).

---

## v0.7.10 — gestão de VM 100% nativa no libvirt: managed save, órfãos, `--force`

### VM — `vm stop`/`vm rm` à prova de managed save e de órfãos

Do bug report real: `vm rm` de uma VM com *managed save image* vazava o stderr
cru do `virsh` ("Refusing to undefine while domain managed save image exists"),
apagava o registo local NA MESMA e deixava o domínio órfão no libvirt — e o
`vm stop` seguinte respondia "no such container" (substantivo errado). Agora:

- **`undefine` leva sempre `--managed-save --snapshots-metadata --nvram`**
  (fallback para o simples em virsh antigo) — a causa-raiz da recusa; o
  `destroy` só corre se o domínio não estiver "shut off".
- **Nada do `virsh` vaza cru**: stdout/stderr capturados e transformados em
  mensagens claras (ex.: `vm: could not remove VM 'dev' from libvirt
  (qemu:///session): …`).
- **Sem órfãos em nenhum sentido**: se a limpeza no libvirt falhar, o `rm`
  **preserva o registo local** e diz como forçar; `vm rm -f/--force` descarta o
  estado local na mesma. Um domínio órfão de ANTES do fix (sem registo local) é
  reconhecido e limpo/desligado por `rm`/`stop`.
- **`no such VM: <nome> (see `delonix vm ls`)`** em `stop`/`rm`/`status` —
  e `vm rm` de um nome inexistente passa a ser **erro** (devolvia sucesso
  silencioso), como no docker.
- **Aliases**: `vm down` = `stop`, `vm delete` = `rm`.
- O `rm` também limpa o directório seed do cloud-init (`vms/<nome>/`) e o
  `.sock.lock`, que ficavam para trás.

Validado ao vivo no cenário exacto do report: um domínio "shut off" com managed
save foi removido em silêncio, e o `rm` repetido respondeu `no such VM`.

**Nota de transparência**: parte deste trabalho entrou já no v0.7.9 (dentro do
commit dos fail-closed, sem constar das notas); o v0.7.10 completa-o (rm de
inexistente é erro, limpeza do seed dir, testes de regressão) e documenta o
conjunto.

---

## v0.7.9 — consola recupera o shell + chega de falhas silenciosas

### VM

- **`vm console` regressa ao shell do host** quando a VM se desliga (poweroff) —
  antes ficava preso. Ponte bidireccional com `poll()`: sai no Ctrl-] (destacar)
  ou quando a VM fecha. (`exit`/Ctrl-D dentro da VM vão para o getty da VM.)

### Correctude — fail-closed (da análise `docs/COMPARACAO-DOCKER-PODMAN.md`)

Três opções que eram aceites e depois IGNORADAS (o utilizador julgava estar
protegido) passam a falhar/avisar de forma explícita:

- **`--security-opt seccomp=<perfil.json>`** — perfil custom era ignorado (corria
  com o allowlist embutido) → **erro** (só `seccomp=unconfined` suportado).
- **`-v host:/dst:z|:Z|:U`** — opções SELinux ignoradas (o bind falhava em
  RHEL/Fedora enforcing) → **erro** (só `:ro`/`:rw`).
- **`--network-alias`** — gravado mas o DNS não o consultava → **aviso** no `run`.

### Docs

- `docs/COMPARACAO-DOCKER-PODMAN.md` — análise de gaps vs Docker/Podman rootless.

---

## v0.7.8 — auto-login na consola + correcções de segurança da superfície VM

### Segurança (auditoria delonix-runtime-sec da superfície VM das v0.7.x)

- **ALTO — path traversal via nome da VM.** O nome (do CLI ou de
  `metadata.name` de um manifesto não-confiado via `stack apply -f`) fluía cru
  para os caminhos de seed/overlay, permitindo escrever/sobrescrever ficheiros
  fora do directório de estado como o utilizador. Corrigido com `valid_vm_name`
  no boundary do motor (fecha também argv do `virsh` e injecção no cloud-init).
- **MÉDIO — ficheiro temp da rede libvirt** com nome previsível em /tmp
  (symlink attack) → `create_new` (O_EXCL) + 0600.
- **BAIXO — `--` nos argv do `virsh`** (nome começado por `-` seria opção).
- Downloads do instalador sem checksum (cloud-hypervisor/firmware) documentados
  como risco aceite (HTTPS-mitigado; upstream não publica digest).

### VM

- **Auto-login na consola serial** — o `vm console` volta a entrar directo. O
  seed cloud-init (sempre gerado desde a v0.7.7, para a rede) reconfigurava o
  getty e a consola passava a pedir login; agora o user-data configura autologin
  do utilizador `delonix` no `serial-getty@ttyS0`.

---

## v0.7.7 — rede da VM: internet por omissão e NAT/SSH suave

Corrige os dois pontos de rede que faltavam para uma VM utilizável.

- **Internet na VM por omissão.** `vm create` sem `--hostname`/`--ssh-key` não
  gerava seed cloud-init, e a cloud image sem datasource não configurava a rede
  — a VM ficava sem IP nem rota (`ping: Network is unreachable`). Agora o seed é
  **sempre** gerado, com um network-config que faz DHCP em qualquer interface
  ethernet. A VM tem egress/internet out-of-the-box.
- **`--net-mode nat` suave (IP pingável do host + SSH).** Garante a rede libvirt
  `default`: define-a se não existir (virbr0, NAT, 192.168.122.0/24, DHCP),
  arranca-a e põe autostart. Aviso claro e accionável se faltar o grupo
  `libvirt` (`sudo usermod -aG libvirt $USER && newgrp libvirt`).

Dois fluxos:

```
# VM com internet + acesso por consola:
delonix vm create dev && delonix vm console dev

# VM pingável + SSH do host:
delonix vm create dev --net-mode nat --ssh-key ~/.ssh/id_ed25519.pub
delonix vm ls                       # IP 192.168.122.x
ssh delonix@<ip>
```

Não confundir com ingress/egress do delonix (firewall L4 da SDN de containers):
a rede da VM libvirt é a do próprio QEMU.

---

## v0.7.6 — boot da VM dinâmico (a sério desta vez)

O boot dinâmico do `vm create` (planeado para a v0.7.5) tinha ficado de fora —
um `gh pr merge` falhou em silêncio por um hiccup de rede e a v0.7.5 saiu só
com o fix da conexão do console. Esta release traz o que faltava.

- **`vm create --console`** — após arrancar, anexa à consola serial e mostra o
  boot **ao vivo** até ao login (Ctrl-] para sair).
- **`vm create --wait [--boot-timeout N]`** — spinner `a arrancar…` até a VM
  ganhar IP, depois `up — ip …`. Em rede user-mode (libvirt rootless, sem IP
  alcançável) orienta para a consola em vez de esperar o timeout em vão.
- `vnc` reconhecido no `kind: Vm` (deixa de dar falso aviso de campo desconhecido).

```
delonix vm pull
delonix vm create dev --console   # cloud image → libvirt, boot ao vivo
```

---

## v0.7.5 — boot da VM dinâmico; console/vnc na conexão libvirt certa

- **Boot dinâmico no `vm create`.** Deixava de dar sinal do arranque (só o
  nome). Agora:
  - `--console` — após arrancar, anexa à consola serial e mostra o boot **ao
    vivo** até ao login (Ctrl-] para sair).
  - `--wait [--boot-timeout N]` — spinner `a arrancar…` até a VM ganhar IP,
    depois `up — ip …`. Em rede user-mode (libvirt rootless, sem IP alcançável)
    não fica preso no timeout — orienta para a consola.
- **`vm console`/`vm vnc` usam a conexão libvirt certa** (`-c <uri>`). Davam
  `error: failed to get domain` porque o `virsh` default (session) não via um
  domínio definido em `system`. Passam a descobrir a URI e a usá-la.

Fluxo completo:

```
delonix vm pull
delonix vm create dev --console   # cloud image → libvirt, boot ao vivo até ao login
```

---

## v0.7.4 — cloud images arrancam por libvirt; console com recuperação clara

Correcções nascidas de teste real do ciclo `vm pull && vm create dev`.

- **Cloud images (a golden) preferem o backend libvirt.** No Cloud Hypervisor
  faziam kernel panic (`Unable to mount root fs`): o `rust-hypervisor-fw` não
  carrega o initrd das cloud images Ubuntu, e sem initrd o `root=LABEL=...` não
  resolve. A auto-detecção passa a escolher libvirt (UEFI/SeaBIOS completo, que
  as boota) quando a VM arranca por firmware sem kernel explícito, mantendo o
  Cloud Hypervisor para **direct-kernel boot** (nós k8s), onde é o melhor. Sem
  libvirt, cai no CH com aviso. Consequência: o IP volta a vir do
  `virsh domifaddr` (real) para a golden — resolve também o ping.
- **Erro do `vm console` com o comando exacto de recuperação** — uma VM
  arrancada por um delonix antigo (sem socket de consola) já não dá um erro
  vago; diz `vm stop <name> && vm create <name>` para a re-arrancar.

Fluxo completo (backend automático):

```
delonix vm pull
delonix vm create dev      # cloud image → libvirt automaticamente
delonix vm console dev     # boot até ao login
delonix vm ls              # IP real (ping/SSH)
```

---

## v0.7.3 — acesso à VM: console serial, IP correcto, firmware auto, VNC

Fecha o ciclo de usar uma VM criada com `vm pull && vm create dev`.

- **`delonix vm console <name>`** — terminal serial interactivo da VM, que
  funciona **sem IP** (logs de boot, login). Cloud Hypervisor via socket UNIX
  + ponte raw-tty (escape `Ctrl-]`); libvirt via `virsh console`.
- **IP correcto no `vm ls`** — deixava de mostrar `<none>` numa VM viva. O IP
  é determinístico do MAC (o servidor DHCP dá `<prefix>.254.<10+fnv32(mac)%240>`);
  passa a ser calculado com essa fórmula em vez de lido da tabela ARP (que só
  o mostrava após tráfego). É o endereço certo para SSH.
- **Firmware do Cloud Hypervisor automático** — o CH não tem BIOS, por isso uma
  cloud image (a golden) precisava de `--firmware`; agora o motor resolve o
  `rust-hypervisor-fw` (que o instalador descarrega) e `vm create dev` arranca
  sem flags.
- **VNC gráfico (`--vnc` / `vm vnc`)** — consola gráfica no backend libvirt;
  `vm vnc` imprime o endereço para um cliente VNC. O Cloud Hypervisor não tem
  display — nesse caso o comando aponta para `vm console`.
- **Barra de progresso no `vm pull`** e **default de rede `ingress`** (da v0.7.2).

Fluxo completo, sem setup:

```
delonix vm pull            # golden oficial, com barra de progresso
delonix vm create dev      # firmware + rede automáticos
delonix vm console dev     # entra na VM (mesmo sem IP)
delonix vm ls              # já mostra o IP
```

---

## v0.7.2 — VMs de ponta a ponta: pull com progresso, rede default corrigida

O fluxo `vm pull && vm create dev` corre agora sem qualquer setup manual.

- **Barra de progresso no `vm pull`** — o download da golden (~680 MiB) passou
  a streaming com uma barra animada (`[vm pull] <ref> ██████░░ 58% 393/678 MiB`),
  redesenhada em tempo real; só em tty (pipes/CI ficam limpos). Antes o pull
  parecia pendurado até acabar.
- **Default de rede corrigido** — `vm create` defaultava para `--network bridge`,
  tratada como uma rede PRIVADA a criar antes (`vm create dev` falhava com
  "ingress network 'bridge'"). Passa a `ingress`, a rede default do sistema
  (bridge `delonix0`/10.200, sempre presente). Erro de rede inexistente agora
  diz como a criar.
- Help do `vm pull`/`vm push` em inglês (fonte), traduzido via catálogo.

Fluxo completo, sem fricção:

```
delonix vm pull        # a golden oficial do ghcr, com barra de progresso
delonix vm create dev  # cria a VM sobre ela, rede ingress default
```

---

## v0.7.1 — VMs sem fricção: vm pull da imagem oficial, --disk opcional, vm init corrigido

Correcções e UX nascidas de uso real do grupo `vm`:

- **`delonix vm pull`** (novo) — sem argumento, descarrega a **imagem VM
  dourada oficial** (`ghcr.io/angolardevops/delonix-vm-k8s:1.34`: Ubuntu 24.04
  + kubeadm/kubelet/kubectl + `delonix-cri` como serviço); com argumento,
  qualquer referência OCI. **`vm push <nome> <destino>`** publica uma golden
  local. Delegam na lógica do `image --vm` (zero duplicação).
- **`vm create --disk` opcional** — sem a flag, usa a imagem dourada local
  única (0 ou várias dão erro claro com o comando para resolver). O fluxo
  completo passou a: `delonix vm pull && delonix vm create dev`.
- **`vm init` deixou de criar containers** — o menu de templates (apps em
  containers: django/nginx/...) aparecia em `vm init` e, escolhido um
  template, construía e arrancava um *container*. O menu agora aplica-se só
  a `container/stack init`; `vm init`/`cluster init` geram o scaffold do alvo.
- Exemplo do cartão `--version` corrigido para a sintaxe real (`vm create dev`).

---

## v0.7.0 — fonte 100% EN completa: mensagens do motor no catálogo pt.po

Fecha a migração i18n iniciada na v0.5.0: **todo o código de utilizador fala
inglês** — agora incluindo os 9 crates de motor (~250 mensagens convertidas:
net, runtime, image, cri, core, vm, mgmt, volume, scan).

- **Catálogo `pt.po` com 429 mensagens.** `--l18n=pt` traduz o help completo,
  as mensagens dos comandos e as mensagens ESTÁTICAS dos crates de motor —
  estas últimas traduzidas à saída, no printer de erros do binário (os crates
  de motor não dependem do catálogo).
- **Limitação documentada**: mensagens de motor com valores interpolados não
  casam no lookup e saem em inglês.
- Preservados deliberadamente: padrões de matching de stderr do CRI (lógica
  de idempotência, cobrem PT antigo e EN novo), fixtures e asserts de teste.

Resta da migração apenas os comentários do código (PT→EN), sem impacto no
utilizador.

---

## v0.6.2 — corrige o "delonix delonix" na 1.ª linha do --version

O clap prepõe o nome do binário ao `long_version`; o cartão da v0.6.1 também
o incluía e a primeira linha saía "delonix delonix 0.6.1". Só o fix.

---

## v0.6.1 — `--version` rico: identidade do build + por onde começar

- **`delonix --version`** passa a cartão de visita: versão, **commit e data de
  build** (injectados em build-time, com `SOURCE_DATE_EPOCH` respeitado para
  reprodutibilidade), a descrição do motor, um bloco **get started** com os 5
  fluxos principais (container / vm / cluster / stack / dash) e o link das
  docs. Traduzido via catálogo (`--l18n=pt`).
- **`-V`** mantém a linha curta e estável (`delonix X.Y.Z`) — scripts que
  fazem parse não partem.

---

## v0.6.0 — stack ls, stop idempotente, instalador animado

Resultado de um varrimento completo da CLI (157 comandos/subcomandos
enumerados do `--help` real + execução dos read-only): a estrutura estava sã;
as correcções são de semântica e UX.

### CLI

- **`delonix stack ls [-f]`** — lista a estrutura que o manifesto compõe
  (containers, volumes, redes, e os restantes Kinds) numa tabela única
  `KIND / NAME / PRESENT / STATUS`, confirmando cada recurso no store real.
  O stack continua sem registo próprio (por desenho) — é a vista tabular do
  `describe`.
- **`container stop` idempotente** como o docker: parar um container já parado
  é sucesso — o idioma `stop X && rm X` volta a funcionar. As mensagens de
  erro de operações multi-id deixam de sair duplicadas.
- **`vm status`** e **`volumes snapshot ls`** sem argumento listam TODOS
  (consistente com o `ingress/egress ls` da v0.5.0).
- **Aviso de morte à nascença no `run -d`**: se o init morre nos primeiros
  400ms, avisa com o nome + apontador para os logs; no caso clássico
  (rootless + default `--net host` + imagem a fazer bind de porta <1024,
  ex.: nginx), explica a causa e as saídas (`-p` ou `--net <rede>`).

### Instalador

- **Animação por passo**: spinner braille nos passos com espera real
  (instalação de pacotes, download dos binários, cloud-hypervisor estático),
  com cursor escondido/reposto e degradação limpa para as linhas estáticas
  fora de um tty (pipes/CI). Corrigido também um "a instalar" que tinha
  escapado à tradução EN da v0.5.1.

### Limitação conhecida (deliberada)

Mudar o default de rede do `run` para netns privado (como o docker) fica
para uma decisão de arquitectura à parte — por agora o aviso acima cobre a
armadilha.

---

## v0.5.1 — instalador em inglês + cloud-hypervisor por omissão (com fallback libvirt)

- **`install.sh` fala inglês por default** — alinhado com a CLI (fonte EN,
  `--l18n=pt` no binário para português). A gramática de progresso mantém-se
  (`install/delonix: preparing the host...`, `[deps] x: already satisfied (SKIP)`).
- **cloud-hypervisor instala-se SEMPRE** (é o backend preferido do motor; o
  `delonix-vm` tenta-o primeiro e cai para `virsh`/libvirt): via pacote da
  distro onde exista (Fedora/Arch/openSUSE) e, nas famílias Debian/Ubuntu
  (sem pacote), via o **binário estático oficial do upstream** para
  `/usr/local/bin/cloud-hypervisor`. O libvirt+QEMU continua a ser instalado
  como fallback.

Sem alterações de motor — os binários mudam apenas pelo bump de versão.

---

## v0.5.0 — nomes angolanos, i18n por catálogo pt.po, ingress/egress ls global

### Identidade angolana nos nomes

Containers sem `--name` deixam o `dlx-<hash>` ilegível e ganham nomes do
padrão do produto — reis/rainhas + lugares de Angola (`njinga-benguela-07`),
o mesmo dos clusters kind-mode. Determinístico do id (as 2 passagens do
re-exec de `--net` convergem), colisões avançam para a combinação seguinte.

### i18n a sério: fonte EN + catálogo gettext embutido

- O código fala **inglês** (padrão de mercado num repo público); as traduções
  vivem num **`pt.po` gettext standard embutido no binário** (171 mensagens) —
  o formato que Poedit/Weblate/Crowdin falam. Língua nova = novo `.po`.
- **`--l18n=pt`** (ou `DELONIX_L18N=pt`) traduz **o help do clap incluído**
  (reescrito em runtime antes do parse) e as mensagens de progresso/erro.
- O `tr(en, pt)` inline morreu; mensagens interpoladas usam templates com
  placeholders nomeados (`{port}`, `{owner}`) que sobrevivem à reordenação.

### UX

- `delonix ingress ls` / `egress ls` **sem argumento** listam o estado de
  firewall de TODOS os containers (overview estilo `docker ps`).
- Erro de porta ocupada estruturado como receita: o facto + três comandos
  prontos a copiar (stop do dono / outra porta / `update --publish-rm`).
- Instalador: avisa quando outro `delonix` no PATH faz sombra ao instalado
  (com as duas versões e o comando para resolver).

### Instalação

```bash
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash
```

---

## v0.4.2 — progresso do instalador profissional, na gramática do `delonix cluster`

O `install.sh` fala agora a MESMA língua do resto do produto — o formato de
progresso do `delonix cluster apply` (`[fase] passo: a aplicar... OK` /
`já satisfeito (SKIP)`), com a idempotência visível passo a passo:

```
install/delonix: a preparar o host (Zorin OS 18.1, gestor apt)...
[host] cpu: AMD Ryzen 9 8940HX with Radeon Graphics (32 cpus, x86-64-v3 (AVX2))
[host] recursos: 30GB RAM · 765GB livres em /home/walter
[host] gpu: NVIDIA Corporation Device 2d59 (rev a1) · AMD/ATI Raphael (rev d8)
[deps] slirp4netns: já satisfeito (SKIP)
[deps] uidmap: a instalar (containers rootless multi-uid)... OK
[rootless] subuid: já satisfeito (SKIP)
[kernel] sysctls: a aplicar (inotify/ip_forward/bridge-nf/max_map_count)... OK
[verificar] user namespaces: OK
install/delonix: pronto
```

- Mensagens em português, alinhadas com a voz da CLI.
- Cores só nos estados (OK/SKIP/AVISO/ERRO) e desligadas fora de um tty
  (logs de CI/pipes ficam limpos).
- GPU reportada sem o ruído do lspci.

Sem alterações de motor — os binários mudam apenas pelo bump de versão.

---

## v0.4.1 — instalador ciente do hardware, binário optimizado (LTO + x86-64-v3), tuning de kernel

### Correcção crítica do instalador

- **`install.sh` da v0.4.0 falhava com 404**: o `source /etc/os-release` esmagava
  a variável `VERSION` do script com a versão do SO ("18.1" no Zorin) e o download
  ia para uma release inexistente. A leitura do os-release passou a subshell isolada.

### Binário optimizado

- **LTO thin + `codegen-units=1`** no perfil de release — inlining entre crates
  no caminho quente (hash de layers, serde, parsing).
- **Nova variante `x86-64-v3`** (`delonix-x86_64-v3-linux`, idem `-cri`):
  compilada com AVX2/BMI2/FMA para CPUs modernos (AMD Zen 2+ — incl. Ryzen 9 HX —
  e Intel Haswell+). O genérico `x86-64` continua publicado como fallback universal.

### Instalador ciente do hardware

- **Detecção de CPU/RAM/disco/GPU** no arranque: escolhe automaticamente a
  variante do binário certa para o CPU (com fallback para releases sem ela),
  reporta a GPU presente, e avisa cedo sobre RAM <2GB e disco livre <10GB
  (o kubelet despeja pods sob disk-pressure — melhor saber antes).
- **Tuning de kernel** (novo, opt-out com `--no-tune`): sysctls e módulos que
  containers/k8s/VMs exigem — limites de inotify (o kubelet esgota os defaults),
  `ip_forward`, `br_netfilter` + `bridge-nf-call-*` (requisito kubeadm),
  módulos `overlay`/`tun`, `vm.max_map_count`, `somaxconn`, `ping_group_range`
  (ping em containers rootless). Persistido em `/etc/sysctl.d/99-delonix.conf`
  + `/etc/modules-load.d/delonix.conf`.
- Falha de autenticação sudo agora aborta cedo com mensagem clara, em vez de se
  disfarçar de "pacote indisponível".

### Instalação

```bash
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash
```

---

## v0.4.0 — instalador oficial multi-distro, observabilidade C1, conformância CRI

### Instalador (`install.sh`)

Um comando deixa uma máquina virgem 100% funcional — sem passos manuais:

```bash
curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash
```

- Instala o binário (verificado por SHA256) **e todas as dependências de runtime**:
  `slirp4netns` (rede rootless / `-p`), `uidmap` (imagens com utilizador não-root),
  `nftables`, `iproute2`, `conntrack`.
- Configura o host para rootless: intervalos `subuid`/`subgid`, perfil AppArmor
  para a restrição de userns do Ubuntu 23.10+, sysctl de userns no Debian antigo.
- Instala a stack de microVMs por omissão: libvirt+QEMU/KVM (cloud-hypervisor
  onde a distro o empacota), `qemu-img`, `cloud-localds`, grupos `kvm`/`libvirt`.
- Multi-distro: famílias Debian/Ubuntu (apt), Fedora/RHEL (dnf), openSUSE
  (zypper) e Arch (pacman) — detecção por `ID`/`ID_LIKE`, com candidatos de
  pacote por gestor.
- Verificação final com relatório claro (setuid do newuidmap, /dev/net/tun,
  userns utilizáveis, backend de VM presente).
- Flags: `--no-vm`, `--with-cri`, `--user`, `--version vX.Y.Z`, `--no-binary`.

### Observabilidade (C1)

- Logging estruturado com `tracing` em todos os crates de motor.
- Métricas Prometheus partilhadas + `GET /metrics` no `delonix-cri` e no mgmt.
- Spans OpenTelemetry/OTLP — a 3.ª perna da observabilidade.

### CRI

- `RemoveContainer`/`StopContainer` idempotentes; exec streaming (SPDY) delega
  no `delonix`; hostname do pod + `RunAsUser`/`RunAsGroup`/`RunAsUserName`;
  image `Uid`/`Username` + labels/annotations preservadas no `ContainerStatus`;
  `--pod` liga o container ao netns partilhado do sandbox.

### Motor

- Manifesto/config/índice OCI migrados para `oci-spec`; `image export` gera um
  bundle OCI conformante.
- Reaper determinístico de refs+rootfs órfãos no `system prune`; refcount do
  ingress substituído por conjunto de marcadores idempotente.

### Instalação

Ver a secção *Install* do README. Binários: `delonix-x86_64-linux`,
`delonix-cri-x86_64-linux` (+ `SHA256SUMS`, `install.sh`).

---

## v0.3.0 — paridade docker no dia a dia: -p/--publish, start, --rm, --entrypoint, inspect/stats/logs -f

## CLI (`delonix container`)
- **`-p/--publish hostPort:contPort[/tcp|udp]`** (e `ports:` no manifesto): com `--net <rede>` publica pelo ingress (hostfwd no slirp único + DNAT nft — regras trocáveis a quente); com `--net host` (default) o container passa a netns próprio com NAT userspace (slirp4netns, modelo podman rootless). Limpeza automática no stop/rm.
- **`start`** — rearranca containers parados/crashados com a spec do Store e o rootfs persistente (as escritas sobrevivem; multi-ID).
- **`--rm`** — remove à saída; em `-d` um watcher destacado (daemonless) faz a limpeza quando o container morre.
- **`--entrypoint`** — sobrepõe o ENTRYPOINT da imagem ("" limpa).
- **`inspect`** (JSON do Store), **`stats`** (CPU%/MEM/PIDS via cgroup v2, fallback VmRSS), **`logs -f`** (follow com rotação).
- **`ls`** (alias de `ps`), **`ps -q`**, **`rm`/`stop` multi-ID** com semântica docker.

## Runtime
- Fix do /sys vazio em `--privileged` + `--net host` (EPERM ao montar sysfs novo num userns sem ser dono do netns → fallback bind do /sys do host, como o runc rootless) e do mountpoint de cgroup2 criado no sítio errado pós-pivot_root — os dois bloqueadores conhecidos do arranque de nodes Kind (`kindest/node`).

Assets: `delonix-x86_64-linux`, `delonix-cri-x86_64-linux`, `SHA256SUMS`.

---

## v0.2.0 — grupos semânticos, manifesto declarativo, imagem VM dourada, cluster kubeadm

Binário `delonix` reestruturado em grupos semânticos (`container`/`image`/`build`/`vm`/`volumes`/`network`/`stack`/`cluster`), com `delonix-cri` a ganhar o seu primeiro binário standalone.

## Novidades
- **CLI reorganizado**: `delonix container run` (-v/--volume, --net <rede-custom>), `delonix image`, `delonix build` (Dockerfile/Delonixfile), `delonix vm`, `delonix volumes`, `delonix network`.
- **Manifesto declarativo** (`delonix-manifest.yaml`, estilo Kubernetes): `apply` idempotente por-Kind em cada grupo + `delonix stack apply` para todos os Kinds de uma vez.
- **`delonix image --vm ls|pull|push|build`**: imagem VM dourada (Ubuntu 26.04 LTS + kubeadm/kubelet/kubectl + `delonix-cri` pré-instalado), publicável/obtível como artefacto OCI.
- **`delonix cluster apply -f cloud.yaml`**: bootstrap `kubeadm` idempotente sobre SSH em hosts já vivos (`kind: Cluster`) — idempotência sem-estado, progresso por-etapa.
- **`delonix completion <shell>`**: autocompletion (bash/zsh/fish/elvish/powershell).
- **`delonix-cri`**: primeiro binário standalone (`dist/delonix-cri.service` incluído) — endpoint CRI para o kubelet.

## Assets
- `delonix-x86_64-linux` — CLI principal.
- `delonix-cri-x86_64-linux` — servidor CRI standalone (para a imagem VM/hosts kubeadm).
- `SHA256SUMS` — checksums de verificação.

Ver `AGENTS.md` no repositório para detalhes de arquitectura, limitações conhecidas desta v1 (só etcd `stacked`, execução sequencial em `cluster apply`) e as próximas fases já registadas.

---


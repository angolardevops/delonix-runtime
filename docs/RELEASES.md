# Delonix Runtime — features por release

> Gerado por `scripts/gen-releases.sh` a partir de `docs/releases/<tag>.md`
> (regenerado automaticamente pelo pipeline de release a cada tag publicada).
> Não editar à mão — edita a nota da release respectiva.

## v5.0.1 — duas falhas que abriam em vez de fechar: `--secret-files` e a admissão por scan

Release de correcção sobre a `v5.0.0`, cortada de um ramo de manutenção
(`release/5.0.x`) com **exactamente dois commits de código** em cima da tag — os
dois `cherry-pick -x` da `main` — mais a acção `.github/actions/protoc`, só de CI,
que o `release.yml` da `main` exige na árvore do tag. Os outros ~140 commits que entraram na `main` desde a
`v5.0.0` **não** vêm aqui: trazem funcionalidade nova, e funcionalidade não sai
numa PATCH. Quem precisa só de segurança actualiza para esta; o resto sai na
próxima MINOR.

As duas falhas são da mesma família, a que este motor persegue em todo o lado:
um caminho de erro que, em vez de recusar, seguia em frente e deixava o resultado
parecer bom.

---

### `--secret-files` deixava de fechar quando o tmpfs não montava (`3626ae70`)

Cada passo do `write_secret_files` — o `create_dir_all`, as duas chamadas
`mount(2)` do tmpfs em `/run/secrets`, e cada escrita de ficheiro — tinha o erro
descartado. Se o tmpfs não montasse (uma flag recusada por um kernel, um
`/run/secrets` já ocupado, um userns mais restrito que o esperado), o ciclo
continuava a escrever **directamente no `/run/secrets` que existisse**, ou seja na
camada persistente do container. Daí o segredo sobrevive a um `container commit`
para uma imagem publicada e a qualquer backup ou snapshot do container — a
exposição exacta que `--secret-files` existe para evitar face às variáveis de
ambiente.

Agora a montagem e a escrita são duas funções que propagam o erro, e o init do
container **aborta** (exit 126, o mesmo padrão do `setup_rootfs`) em vez de
arrancar um container cujos segredos podem estar em disco. Um container que não
arranca não fuga nada; um que escreveu em silêncio o segredo para a camada
persistente já fugiu.

**O que está provado e o que não está**: a metade de escrita (`write_secret_values`)
tem teste unitário próprio contra uma pasta temporária; a propagação do erro da
montagem é imposta pelo compilador (`?`), não por um `mount(2)` simulado. A falha
de montagem em si **não foi reproduzida ao vivo**.

### A admissão por scan deixava passar quando o próprio scan falhava (`f59c9f8a`, #778)

Com `DELONIX_SCAN_ON_PULL=<severidade>`, o `admission_scan_on_pull` tratava
**qualquer** erro do scanner como «esta imagem não tem gestor de pacotes que eu
leia» (DX-1401, o caso legítimo de uma imagem `scratch`): avisava e deixava o pull
passar. Um `advisories.json` truncado (o `scan --update` escrevia-o sem
atomicidade, e uma escrita interrompida deixava-o cortado) ou um blob de layer
ilegível seguiam o mesmo caminho. Um portão documentado como *fail-closed* passava
a não fazer nada no instante em que a sua própria base de dados se partia, com um
aviso no stderr como único sinal.

Agora só o DX-1401 deixa passar — a distinção é feita pelo número do dicionário,
não pelo texto da mensagem. Qualquer outra falha do scan **recusa o pull e remove
a imagem**. E a mensagem «Image removed» deixou de ser dita incondicionalmente:
diz se a remoção correu mesmo.

Medido ao vivo na `main`, com `DELONIX_ROOT` isolado: imagem `scratch`
(`hello-world`) avisa e entra (rc 0, inalterado); vulnerabilidade real
(`alpine:3.19`, busybox < 1.37.0) é recusada (rc 1, inalterado);
`advisories.json` corrompido à mão é agora **recusado e removido** (rc 1) — antes
entrava com rc 0.

---

### Como foi validada esta release

- Os dois cherry-picks aplicaram sem conflito sobre a `v5.0.0`.
- Neste ramo: `cargo test -p delonix-linux --lib`, os testes `scan` do
  `delonix-runtime-bin`, `cargo clippy -D warnings` dos dois crates tocados,
  `version_gate`, `arch_fitness` (o `library_prints` sobe de 89 para 90 com a
  linha de erro do init, já na linha de base do commit) e `lang_ratchet`.
- **Não corrido neste ramo**: a bateria `scripts/e2e.sh` e o arnês de caos — o
  ramo de manutenção não tem CI própria (o `ci.yml` só corre na `main`). A CI
  completa corre no PR que funde a `v5.0.1` de volta na `main`.

---

## v5.0.0 — o `USER` da imagem é quem corre, o bloco `provider` tipado, e o contrato de nó servido

Duzentos e dois commits desde a `v4.5.0` (162 sem contar os merges), de `#572` a
`#720` — medidos em `831b9601`, a ponta depois do segundo merge da `main` de
2026-10-08 (`git rev-list --count v4.5.0..831b9601`).

O número vai agora com o commit em que foi medido, porque sem isso envelhece em
silêncio: os «180 (152)» desta nota estavam certos em `ea38d89b`, onde foram
contados, e deixaram de bater com a branch assim que ela cresceu. Fixá-lo a um
sha torna-o verificável em vez de uma afirmação sobre «a ponta».

**Numerada como MAJOR por uma razão só, e está na secção seguinte**: um container
passa a correr como o utilizador que a imagem declara. Todo o resto desta release
é aditivo ou é uma recusa a substituir um sucesso que o motor não conseguia
provar — mas essa mudança quebra o contrato do `container run`, e um contrato
quebrado não sai numa MINOR.

O fio condutor é o das últimas releases, levado à superfície declarativa: **o que
o motor não consegue provar não se reporta como sucesso, e o que ele não entende
não se engole em silêncio**. Nesta release isso chega aos manifestos (um campo
desconhecido recusa o documento inteiro), ao isolamento (uma namespace que o host
não imporia é recusada em vez de aplicada), aos limites (`--device-*` sem disco
que o aceite) e ao ciclo de vida (um `rm -f` que desistiu não é ressuscitado pelo
supervisor).

---

### Quebra de contrato — ler antes de actualizar

**Um container corre como o `USER` da imagem** (ADR-0062, `#632`, `#633`, `#637`,
`#638`, `#639`). Até à v4.5.0, `container run` ignorava o `USER` que a imagem
declara e o processo corria como uid 0 — desde a v1.0.0, sem erro nem aviso.
Medido com `haproxy:3.4-alpine`, que declara `USER haproxy`: `id` dentro do
container respondia `uid=0`.

O que muda, e o que fazer:

- **Sem `--user`, o processo corre como o `USER` da imagem.** Um serviço que
  dependia de ser root sem o pedir passa a não o ser. Para o manter, `--user 0`.
- **Num host rootless sem intervalo de subuid só cabe um uid**: aí fica a 0 e o
  `run` di-lo em voz alta, em vez de falhar.
- **O `--user` deixou de entregar o sistema de ficheiros inteiro ao utilizador.**
  A varredura antiga (`chown_tree_once("/")`) dava-lhe 940 das 986 entradas de uma
  imagem de 15 MB — incluindo `/etc/passwd` e o próprio binário do serviço — e
  atravessava os mounts, re-apropriando um bind mount do HOST (um ficheiro de
  `1000:1000` passava a um subuid, e o dono deixava de conseguir escrever no seu
  próprio ficheiro). Agora o container recebe **o que a imagem lhe dá**: um índice
  de donos gravado ao extrair a layer, aplicado uma vez pelo init, nunca noutro
  sistema de ficheiros. Medido depois, na mesma imagem: 2 entradas do utilizador
  em vez de 940, camada de escrita de 56 K em vez de 13 MB, e o bind do host
  intacto. No `odoo:20.0` (~2 GB), o primeiro arranque com `-u odoo` passou de
  «ainda a varrer aos 16 minutos» para **1,7 s**.
- **Um volume nomeado VAZIO** fica do dono que a imagem dá ao ponto de montagem,
  ou do utilizador do container quando a imagem não nomeia ninguém. Um volume com
  dados e um bind mount **nunca são tocados** — um filestore escrito como root por
  uma corrida anterior não é migrado.
- **O `build` segue a mesma regra**: um `RUN` corre como o `USER` em vigor nesse
  passo (antes corria sempre como root, mesmo depois de um `USER app`), e a cache
  de layers preserva os donos — um rebuild com cache perdia-os.

**Duas assinaturas públicas do `delonix-sdn` mudaram** (`#718`):
`can_bind_host_port` e `host_port_busy` passaram a receber um `Proto`. Quem
consome o crate por `git` + tag não parte hoje, mas subir esse pin exige uma
passagem. A assinatura antiga não podia ficar porque **era** a armadilha: as duas
sondas abriam sempre um `TcpListener`, qualquer que fosse o spec, logo uma
publicação UDP era verificada contra TCP — cega por construção, com o conflito a
aparecer só dentro do slirp como JSON opaco. O enum obriga cada sítio de chamada
a dizer o transporte, e recusa nessa fronteira o `sctp` que o CRI consegue emitir
em vez de o sondar como TCP.

Efeito lateral da mesma passagem: a extracção passou a preservar **setuid, setgid
e sticky**. O `/tmp` de todos os containers era `777` em vez de `1777`, e um `su`
setuid perdia o bit. Uma layer extraída por um motor anterior é corrigida a partir
dos cabeçalhos do blob na primeira vez que uma imagem a usa.

---

### Mudanças de comportamento

**O endereço de bind de uma porta publicada passa a ser REGISTADO** (`#718`). Era
estado USADO na publicação e nunca persistido: o registo guardava a spec como foi
escrita (`51072:80`) e o `DELONIX_PUBLISH_ADDR` era relido do ambiente a cada
`start`. Quinta ocorrência da armadilha «estado necessário para RECONSTRUIR o
recurso tem de ser persistido». Medido nos dois sentidos, contra o bind lido do
kernel:

- publicada com `DELONIX_PUBLISH_ADDR=0.0.0.0`, voltava em **`127.0.0.1`** depois
  de um `start` — que é o que um unit do `net boot enable` corre, sem ambiente
  nenhum. Serviço em baixo, listagem a dizer publicado;
- e o INVERSO, que é o que pesa em segurança: uma porta publicada de propósito
  **sem** endereço (só loopback) voltava em **`0.0.0.0`**, exposta à LAN inteira,
  porque a shell que correu o `start` tinha a variável exportada.

Nos dois casos rc=0, sem aviso, com o `net ingress ls` a imprimir a linha igual
antes e depois — `51070:80` e `51072:80` eram bytes idênticos em disco com
alcance oposto. Agora a spec guardada é sempre `addr:hostPort:contPort/proto`, e
o `container ls` mostra o endereço (`0.0.0.0:55070->5070/udp`) na coluna que se
lê precisamente para decidir o que está exposto. Uma spec **sem** endereço passou
a querer dizer «registo anterior a esta versão».

Recusas novas. Em todos estes casos o motor **aceitava e não cumpria**.

**Um `hostPort` que o nó não publica (ADR-0074, `#720`)** — e a recusa é **por
caminho**, porque os caminhos diferem e medi-los foi o que o mostrou: o
`slirp4netns` recusa SCTP no `add_hostfwd` (`bad arguments.proto`), o `portmap`
do CNI publica-o ponta a ponta, e o `hostNetwork` não publica nada (o container
está na netns REAL do host e liga as portas ele próprio). Uma resposta cega teria
fechado uma porta que está aberta em modo root. A recusa chega do
`RunPodSandbox`, antes de o sandbox existir, como `failed_precondition` — que o
kubelet mostra como evento no pod em vez de um `StartContainer` retentado para
sempre. Um `containerPort` sem `hostPort` continua descartado, que é a forma que
um **Service** SCTP usa: nenhum serviço SCTP é afectado.

**Manifestos (ADR-0069, `#682`, `#687`)**

- **Um campo que o motor não entende recusa o manifesto, antes de qualquer
  efeito** — incluindo dentro dos itens de `spec.containers[]` de um Pod, onde as
  probes e o `lifecycle` não existem e até aqui desapareciam em silêncio.
  `stack validate` continua a RELATAR em vez de recusar, e
  `DELONIX_MANIFEST_LENIENT=1` é a válvula explícita.
- **Um campo que um Kind recusa pelo NOME mantém o seu próprio código.** O
  `unprivileged: false` de um `SystemContainer` respondia `DX-1000 unknown field —
  check the spelling` a um campo bem escrito; agora responde `DX-1540`, que diz o
  que o campo é.
- **O mesmo nome de Pod noutra namespace é um conflito**, não «already exists»; e
  `Container` e `Service` passaram a ser identificados por `(namespace, nome)` —
  dois homónimos em namespaces diferentes deixaram de ser um recurso no plano.
- **Um store ilegível é um erro, não uma máquina vazia** (VM, volumes, presença de
  containers). Um `plan` que lê zero por não ter conseguido ler propunha criar
  tudo de novo.

**Isolamento e rede**

- **Uma namespace com nome num host que não filtra o tráfego de bridge é recusada**
  (`DX-6305`, exit 69, decisão D5 do plano de maturidade, `#664`). Sem
  `br_netfilter` as chains são instaladas, os sets são preenchidos, todos os
  comandos reportam sucesso — e a fronteira não existe. É a forma mais cara de
  falha silenciosa que este motor pode ter, porque o que falha é uma propriedade
  de segurança que se lê como aplicada. Válvula:
  `DELONIX_ALLOW_UNENFORCED_ISOLATION=1`.
- **O anti-spoofing mudou para uma tabela `bridge`, onde realmente corta**
  (`#671`). A regra antiga (`ip … iifname <veth> ip saddr != <ip> drop`) tinha o
  contador a zero: entre duas portas da mesma bridge, o `iifname` que a camada IP
  vê é a BRIDGE, nunca a porta. Medido: um container com `NET_ADMIN` forjava a
  origem e chegava ao vizinho (3/3), e forjando o endereço de um membro de outra
  namespace furava o isolamento. Agora prende por porta o MAC, a origem IPv4 e o
  emissor ARP. **Uma lição que fica: uma regra de segurança só está provada
  quando um pacote forjado é contado a cair** — a auditoria que «fechou» isto
  leu o ruleset e viu a regra lá.
- **As concessões são do administrador, não do pedido** (`#672`):
  `--allow-source <cidr>` e `--no-source-check` existem para um container que
  encaminha, mas são PERMISSÕES na política do nó. Sem política, nada é concedido,
  e `mode: warn` não concede.

**Ciclo de vida**

- **Um `--wait` que esgota o tempo sai com 124 (`DX-8503`) e deixa a VM a correr**
  (decisão D6, `#661`). Antes devolvia 0: um `--wait` que não viu a VM responder
  reportava sucesso.
- **`--device-read-bps`/`--device-write-bps` são recusados quando não há disco que
  os aceite** (`#663`). O `io.max` passou a nomear o disco em que o rootfs do
  container vive; antes o limite era escrito e não aplicado.
- **Um `rm -f` que desistiu não é ressuscitado pelo supervisor** (`#647`). Com o
  disco saturado, um `rm -f` devolvia `DX-8101` e mantinha o registo como a regra
  manda — e quando o processo finalmente saía, o supervisor de `--restart always`
  reiniciava-o. A intenção de remoção passa a ficar gravada ANTES do sinal.
- **Um `stop` que desistiu mantém o processo registado**, e um `start` que falha
  depois de um `rm -f` leva o seu directório (`#604`, `#607`).
- **O `slirp4netns` de um container acaba com ele** (`#646`, `#648`, `#651`). Era
  lançado no meio do arranque e herdava os descritores do chamador, por isso ficava
  vivo a segurar a porta publicada depois de o container sair — e o `start`
  seguinte pendurava para sempre à espera de um EOF que só esse slirp podia dar.
  Medido: mais de uma hora de sobrevida com o disco saturado.

**Superfície declarativa**

- **O bloco `provider` é `type` + `spec`** (ADR-0070, ADR-0071). O que só um
  fabricante entende vive **inline** no recurso, tipado por provider E por
  recurso: uma chave que o `type` não conhece é recusada. `backend:` passou a
  `provider.type` numa `VirtualMachine`, e o grupo `libvirt:` e os campos planos
  normalizam para o mesmo. **As grafias anteriores continuam a carregar e a
  querer dizer o mesmo**, reportadas como superadas; repetir um campo nos dois
  sítios é recusado.
- **`kind: NetworkZone` passou a `kind: Network` com `spec.provider.proxmox`.**
  As redes que nomeiam a mesma zona fundem-se no carregamento no único documento
  que o executor já reconcilia — a zona é criada com o primeiro vnet e removida
  com o último, sem contagem de referências. O nome antigo carrega, anunciado
  como superado.
- **Um workload governado por política nasce FECHADO** (ADR-0069 D6, `#685`,
  `#686`). Um container ou Pod que uma `NetworkPolicy`, `NetworkAccessRule` ou
  `Dependency` do manifesto nomeie é criado com a chain default-deny instalada
  antes do processo, e só é aberto ao que as políticas dizem depois de elas
  estarem no lugar. Se uma camada falha, o workload fica fechado e o apply di-lo.

---

### Novidades

**O `hostPort` é publicado em modo root/CNI** (ADR-0074 D3, `#720`). Não era, para
protocolo NENHUM: os mapeamentos eram guardados e depois descartados, e o pod
subia `Running` com uma porta que não respondia — silêncio, rc=0. Os
`portMappings` passam agora como `runtimeConfig` do plugin, como o containerd
faz, e isso entrega tcp, udp **e** sctp de uma vez. O argumento de capacidade é
injectado na conflist, e é isso que dispensa plumbing novo nos dois caminhos CNI:
o root entrega a lista ao `attach_named_netns`, o rootless hex-codifica o MESMO
JSON na linha de controlo do holder (cuja forma não muda, por isso um holder
antigo continua a servir), e guardar o resultado como a conflist do sandbox
devolve ao `DEL` a configuração idêntica. A declaração é lida do
`capabilities.portMappings` do próprio plugin, nunca de uma lista de nomes; uma
cadeia que não a declare é recusada a nomear o `portmap`. Medido numa VM da
golden deste repo com o `crictl`, contra os dois binários: antes **0** regras
DNAT e porta muda, depois **3** regras, resposta pela porta do host, e **0**
regras depois do `rmp`.

E o `hostIP` passa a viajar nesse caminho: descartá-lo publicaria em todas as
interfaces uma porta que o pod pediu numa só — exposição alargada em silêncio, o
mesmo defeito que o `#718` corrigiu pela outra ponta.

**Containers de sistema no Proxmox — `kind: SystemContainer`** (ADR-0058, plano
63, `#579`, `#582`, `#583`, `#588`, `#590`–`#594`)

Um LXC do Proxmox não serve `kind: Container` — a API do nó não tem `exec`, logs
nem código de saída, e o dataplane do motor não chega lá. Entra como recurso
próprio, com semântica próxima de uma VM: o motor puxa a imagem e escreve o
arquivo OCI que o nó aceita, o nó corre o container, e uma edição à mão no nó é
**deriva** (o `plan` dá 2 e o `drift` mostra-a). `memory`, `swap` e `cores` são
quentes; o `rootfs` só cresce (encolher planeia um `Replace`). Dia 2 completo:
snapshots (com um `restore` que espera que o nó volte a responder — medido, ~40 s
em que o pveproxy dá 596 a quem espera), backup no storage do nó, clone, firewall
por `kind: NetworkPolicy` com `scope: systemcontainer`, e migração entre nós
(offline, porque o nó aborta um `restart=1` quando o init ignora o SIGTERM e
deixa o container na origem com o `pvesh` a sair 0). `exec`, logs e código de
saída ficam `unsupported-by-provider` com a razão escrita.

**O contrato de nó passou a ser servido** (ADR-0042, `#659`, `#662`, `#669`,
`#670`, `#673`, `#674`, `#680`, `#681`)

Até aqui o `proto/delonix/node/v1` era um contrato sem um único produtor em Rust.
Agora há `delonix serve node-api`: **gRPC e HTTP/JSON dos mesmos `.proto`**, num
socket unix local, só o próprio uid.

- `NodeService`: `GetApiRoot` (`GET /v1`, com `links` para tudo o que é servido),
  `GetNodeInfo`, `GetHealth`, `GetCapacity`, `ListProviders`.
- `NetworkService`: as leituras (`ETag`/`If-None-Match` com 304, `label_selector`,
  paginação com `Link rel=next`) e as duas primeiras mutações, `CreateNetwork` e
  `DeleteNetwork`.
- `VolumeService`: `GetVolume` e `ListVolumes`. O `Get` mede o uso; a `List` não
  mede, e di-lo — percorrer os dados de todos os volumes a cada listagem é um
  custo já medido no dashboard. Desconhecido nunca é zero.
- `OperationService`: **cada mutação responde com uma `Operation` persistida antes
  do trabalho**. Um registo por acabar cujo dono já não existe é terminado
  `FAILED` com razão `Interrupted` — o servidor é activado por socket e não tem
  quem o faça por ele. `Idempotency-Key` é o `request_id` e `If-Match` o `etag`.
- **Os erros REST são documentos RFC 9457** (`application/problem+json`),
  construídos pelo dicionário de códigos do motor, e o OpenAPI publicado declara-o.
- **As rotas REST são GERADAS** das anotações `google.api.http` no `build.rs`: uma
  rota do contrato ainda não servida responde 501 (`DX-6001`), um caminho fora do
  contrato 404.
- `GET /openapi.json`, e as páginas `GET /docs` (Swagger UI) e `GET /redoc`,
  servidas de ficheiros embebidos no binário — byte-idênticos aos pacotes npm,
  conferidos contra o seu `SHA256SUMS` no build, sob uma CSP que só permite o
  socket (medido num browser: o logo que o ReDoc busca a um CDN é bloqueado antes
  de qualquer pedido sair).

**Providers de rede por papel** (ADR-0059 F2–F5, `#614`–`#636`, `#654`, `#656`)

O contrato de rede passou a ser por PAPEL, com um contexto próprio
(`delonix-networking`): `GatewayProvider` e `SegmentProvider`, escolhidos por
nome no `providers.yaml`. Em cima disso:

- **Uma IR de política tipada** com avaliador de referência: o registo de firewall
  ou parseia nela ou é recusado inteiro, e a chain do holder, a firewall de uma VM
  e o filtro de perímetro de um gateway são três **descidas da mesma IR**.
- **Um plano de rede observa o aparelho e nomeia a deriva**, tem **digest**
  (`--plan-digest` recusa um plano velho com `DX-5390`) e um **livro de passos**
  que retoma um apply morto a meio.
- **Papéis novos no `NetworkGateway` e no segmento**: `nat:` (SNAT e 1:1),
  `ipam:` (subnets, DHCP, reservas) e `dns:` (registos escritos pelo nó).
- **Uma falha diz de que provider, papel e passo veio** — e os clientes remotos
  deixaram de ecoar a credencial para dentro dela (fuga real, fechada com um teste
  que faz grep nas duas).

**Pools de armazenamento** (ADR-0067 P0, `#689`)

`kind: StoragePool` e o contexto `delonix-storage`, com o driver `dir`. O
administrador declara os pools numa allowlist (`/etc/delonix/storage-pools.yaml`);
um manifesto só NOMEIA um pool, e `driver`/`path`/`device` num manifesto são
recusados pelo nome. A posse é por marca, nunca pelo nome: um directório com dados
e sem marca nunca é adoptado. Acima de 95 % de ocupação não se aloca (`DX-5202`).
O motor **nunca cria nem destrói** um pool.

**`delonix init`** (ADR-0061, `#625`, `#631`, `#640`–`#645`, `#649`, `#653`)

- Os sete templates de aplicação geram **um serviço pequeno e completo**: uma
  capacidade do transporte ao porto, configuração validada no arranque, prontidão
  `starting → ready → draining`, encerramento limitado, webhooks nos dois
  sentidos, OpenTelemetry, contrato OpenAPI com teste de deriva, e teste de
  direcção das dependências. O gerador **verifica o que substitui**.
- **Os três templates de edge nascem com HTTPS** (certificado gerado pelo `init`,
  `mkcert` ou auto-assinado), dimensionados para carga, com log em JSON e
  `request_id`. O `scripts/tls.sh` renova, instala um certificado próprio ou corre
  o Let's Encrypt por **HTTP-01 ou DNS-01** — os dois provados contra a produção
  real do Let's Encrypt.
- **Cada template traz um túnel opt-in para a internet** (`delonix-tunnel.yaml`),
  para quem não tem IP público.
- `--port`, `--tls-port` e `--hostname` respondem na linha de comandos ao que o
  `init` pergunta num terminal; um segundo `--up` sobre um projecto que já está de
  pé converge-o e implanta o que foi reconstruído.
- O template `odoo` volta a arrancar (Odoo 20 por omissão) e corre como `odoo`; o
  `python` chama-se agora `fastapi`, com o nome antigo como alias silencioso.

**Segredos versionados** (ADR-0069 item 6, `#696`, `#700`)

Um `Secret` carrega uma **versão** que o store atribui (1 na criação, +1 quando os
valores mudam), e `secret apply` reporta `created`/`unchanged`/`rotated to version
N (changed: CHAVES)` — **pelo nome das chaves, nunca pelo valor**. O `stack plan`
converge um Secret que traz os seus valores, comparando nomes de chave e uma
impressão digital **com chave** (nunca um valor; a chave-mestra do nó está nela, e
um nó sem chave planeia um `Create` e não cria nada). Um container grava a versão
de cada `--secret` com que arrancou, e o `describe` diz quando ela foi rodada
desde então.

**Appliance FreePBX** (`#657`, `#693`): FreePBX 17 + Asterisk 22 em Debian 12,
com segredos por clone no primeiro arranque, provada de ponta a ponta (27/27 nos
dois modos).

---

### Estrutura (ADR-0044 P4b)

O conhecimento dos backends saiu da orquestração e as VMs deixaram de viver num
adaptador:

- **Três portas no contexto de computação** (`LocalDiskImages`, `SeedBuilder`,
  `VmNetwork`) e os casos de uso sobre um store injectado (`#596`, `#597`).
- **O registo de backends** passou ao contexto, semeado pela raiz de composição
  (`#599`).
- **Dois crates de provider novos**: `delonix-provider-libvirt` (`#602`) e
  `delonix-provider-cloud-hypervisor` (`#608`).
- **`delonix-vm` ficou reduzido à raiz de composição** (`#610`), e cada provider
  carrega as suas próprias guardas de locale.

---

### Desempenho

Medido, por item, com a razão de cada decisão (incluindo as recusadas) no
`AGENTS.md`:

| Caminho | Antes | Depois |
|---|---|---|
| Memória no `pull` do `node:22` | 214 MB | **27 MB** |
| Memória no `pull` de uma imagem de VM | 278 MB | **13 MB** |
| Memória no `push` de uma imagem de VM | 837 MB | **10–13 MB** |
| `push` de 4 layers a 20 MB/s | 65 s | **11 s** (em paralelo) |
| `push` de uma tag que o registo já tem noutro repositório | 356 s | **7,0 s** (monta em vez de enviar) |
| Build a frio de um template | 6,8 s | **2,2 s** |
| Build com só o `COPY` mudado | 7,2 s | **1,1 s** |
| Extracção do `node:22` no primeiro `run` | 4,3 s | **2,1 s** (layers em paralelo) |
| `run --rm … true` com 1 GB sujo no disco | 9,5–14 s | **0,14 s** (overlay `volatile`, ADR-0056) |
| `cluster load` em 3 nós | 33,2 s | **17,6 s** |

Um `pull` cortado a meio **retoma** a partir do que já está em disco, entre
processos (medido com um `kill -9`: o pull seguinte continuou e o digest bateu), e
um `push` cortado continua do último bloco que o registo confirmou. Um token de
registo emitido **anonimamente** fica em cache em disco (ADR-0060) — nunca um
obtido com `image login`, nunca num push.

---

### Build e desenvolvimento

- **A métrica de maturidade do catálogo passou a ser CALCULADA, com ratchet**
  (plano 65 F0.1, `#715`). Era contada à mão sobre o markdown publicado, e as
  duas contagens em circulação estavam **ambas erradas**: a do plano 66 vinha de
  um `grep -c` que contou os sumários por provider, a prosa da introdução e a
  palavra `supported` dentro de `unsupported-by-provider`; a do plano 65 dizia 99
  de 265. A contagem a sério é **100 de 252 (39,7 %)**, e o `provider ls` concorda.
  Um número medido por grep a um documento não é uma métrica — é o documento a
  falar de si próprio.
- **As credenciais de um laboratório deixaram de estar a um `git add -A` de serem
  publicadas** (`#716`). Medido na raiz deste repo, que é PÚBLICO:
  `opnsense-lab.env`, com chave e segredo em claro, não estava ignorado por nada —
  a única coisa entre ele e um push era a regra da casa de nunca usar `git add -A`.
  Uma regra é uma intenção; isto é um ferrolho. Nada foi apagado.
- **Um `Makefile` para o ciclo do desenvolvedor**: `bootstrap`, `doctor`, `build`,
  `install`, `ci`.
- **O binário de release deixou de depender do caminho do checkout** — duas
  árvores diferentes produzem o mesmo binário.
- Perfil `dev` mais leve, perfil `release-ci`, e `nextest` no `make test`.
- **Laboratório nocturno** (`lab.yml`): workflow num runner self-hosted com
  preflight que falha se o host não tiver KVM, userns, cgroup delegado e
  `br_netfilter`, e um portão que chumba um cenário que salte sem razão aceite.
  **O runner não está registado, e o agendamento está a disparar sobre o vazio**:
  medido a 2026-10-06, as corridas de 2026-10-04 e 2026-10-05 ficaram em fila à
  espera de um runner que não existe e foram canceladas às 24 horas, e a de hoje
  está em fila desde as 09:04. Registar o runner é um passo do dono.
- Manual do contribuidor em dia com a `main`, nas quatro línguas (`#601`).
- ADRs novos: **0060** (cache de token anónimo), **0061** (contrato dos templates),
  **0065** (IPv6 no dataplane), **0066** (balanceador L4), **0067** (pools de
  armazenamento), **0068** (hotplug de VM), **0069** (reavaliação do catálogo de
  Kinds e manifestos estritos), **0070** e **0071** (o bloco `provider`). Os
  0065, 0066 e 0068 estão aceites como decisão e **sem nada implementado** — são
  trabalho das próximas releases, não desta.

---

### Consolidação e o que a medição encontrou (2026-10-06)

A última passagem antes desta release consolidou o trabalho de cinco sessões
paralelas num plano único (`docs/discovery/66_CONTINUITY_PLAN.md`) e correu as
medições que ele exige. O que isso produziu, além dos números da secção seguinte:

**Uma store aberta numa raiz deixou de escrever noutra** (`#719`), e a hipótese
óbvia estava errada. O `tmp_roots_gate.py` chumbava na `main` ao acaso — `cargo
test -p delonix-sdn` deixava 3, 0 e 1 restos em três corridas iguais. A leitura
natural era «o `Drop` do `TempDir` engole o erro de remoção», e um
`TempDir::close()` com o erro à vista teria **passado sem apanhar nada**: seguido
com `strace` sobre os seis binários de teste em paralelo, a remoção funcionava e
o directório **voltava** depois dela, posto lá por OUTRO teste. Não era uma
limpeza que falhava; era uma store a escrever numa raiz que não era a sua.

- **Um convidado governado por política nasce fechado, ou não nasce** (ADR-0069
  D6, `#705`). O D6 fechava containers e Pods; uma VM e um system container
  ficavam de fora, e a razão era real — a firewall deles é a do provider, não a
  do holder. Agora é criado com a sua própria firewall em vigor, default-deny nos
  dois sentidos, **antes de o convidado ter CPU**, e um backend que não o consiga
  (`VmBackend::holds_at_boot`) **RECUSA pelo nome** (DX-1501) em vez de criar o
  convidado aberto. Só o Proxmox a tem hoje; os dois backends locais recusam, e a
  recusa diz onde o `scope: vm` serve.
- **O mesmo nome de VM noutra namespace é um conflito, não «ensured»** (`#706`).
  O registo é `vms/<nome>.json`, qualquer que seja a namespace, por isso o nome é
  único no nó — e um manifesto que pedisse o mesmo nome noutra namespace recebia
  `stack plan` = «1 unchanged» e `stack apply` = «ensured» com rc 0, **com a VM a
  ficar na primeira namespace**. Numa VM a namespace É a fronteira de isolamento,
  logo o inquilino que pedia a segunda ficava com uma VM que a primeira alcança e
  a dele não. Passa a recusar (classe 5) antes de resolver ou criar o que seja.
- **Duas pré-condições da bateria que faziam uma falha do AMBIENTE ler-se como
  treze falhas do motor** (`#703`): um segundo pull de imagem com a saída
  descartada, e chaves de idempotência fixas no node API.
- **Um teste que perguntava ao hexadecimal** (`#702`): a verificação de que o
  fingerprint de um segredo não deixa escapar o valor usava `"bc"`, que É hex —
  21 falhas em 300 corridas, medido.
- **Nove ADRs aceites** (0020, 0040, 0041, 0049, 0055, 0061, 0063, 0064, 0067) e
  **dois novos** (0072, 0073) a fechar as duas últimas linhas da tabela do
  ADR-0070 por medição em vez de movimento. Aceitar o 0040 desbloqueou o
  `capability discovery` do M01.
- **Dois gates novos sobre o próprio registo** (`#707`, `#708`): um ADR não pode
  contradizer-se sobre o seu estado, e o índice não pode discordar do documento —
  sete linhas discordavam, duas delas há doze dias.
- **Uma política que se escreve pelo nome do pod lê-se pelo mesmo nome** (ADR-0069
  pendente 1, `#713`). As mutações já aceitavam o nome do pod desde o `#705`; as
  LEITURAS não. `net ingress ls <pod>` e `describe networkpolicies <pod>/<dir>`
  respondiam `no such container` sobre uma política que o motor acabara de
  aceitar com aquele nome, e o `get networkpolicies` nomeava o MEMBRO onde o
  operador escreveu o POD — num pod de dois membros, quatro linhas, e as duas do
  membro sem firewall a dizer «allow (default), 0 rules» por cima de uma política
  que existe. Medido ao vivo contra `main`: a escrita passa, as três leituras
  falham. **E o DELETE genérico não apagava**: com um `deny` em vigor,
  `delete networkpolicies <c>/ingress` respondia rc 0 e «removed 0 inbound
  rule(s)», as leituras continuavam a ler `deny`, e o tráfego continuava
  bloqueado. Com isto fecha o último «STILL TO DO» do pendente 1 do ADR-0069.
- **Um campo que o workspace já declara não se escreve à mão** (`#711`, `#712`).
  O `inline_versions` só olhava para as versões de DEPENDÊNCIA, por isso os
  metadados `[package]` do próprio crate não tinham guarda — e foi assim que o
  manifesto do `delonix-mgmt` disse `version = "0.1.0"` durante dois meses e meio
  enquanto o workspace caminhava para a 4.5.0. Os quatro campos copiados passaram
  ao workspace e um gate novo chumba o quinto.

### A evidência desta release, medida

Contra o código de `0640a9c7` mais o bump desta nota, a 2026-10-06, neste host,
com raiz de estado isolada. **A ponta da release mudou depois disso** — ver o
adendo no fim desta secção, que diz o que foi re-medido e o que não foi:

| Medição | Resultado |
|---|---|
| Bateria da CLI (`scripts/e2e.sh`) | **1097 PASS / 0 FAIL / 13 SKIP**, rc=0 |
| Arnês de caos (`scripts/chaos.sh`) | **54 PASS / 0 FAIL / 1 SKIP**, rc=0 |
| Portão de performance | `delonix` **81 ms** contra os 87 ms da linha de base desta máquina (×0.93, dispersão 1,3×); docker 291 ms (×1.30), podman 264 ms (×0.95) |
| Métrica de célula do catálogo | **100 / 252 aplicáveis = 39,7 %** |

O portão de performance **passou**, e o docker é a âncora que o diz: degradou
×1.30 na mesma corrida em que o motor melhorou ×0.93, o que é ruído da bancada e
não regressão — se o motor tivesse degradado tanto como ele, o portão recusava-se
a julgar em vez de nos acusar.

Os 13 SKIP têm razão escrita, um a um: **7** são integrações remotas sem
credenciais neste host (4 Proxmox, 3 OPNsense), **2** são caminhos de FALHA que
este host não consegue exercitar precisamente porque tem a ferramenta
(`virt-customize`, `wg`), e os outros **4** dizem o que faltou — sem imagens de
VM no store, montar NFS exige `CAP_SYS_ADMIN`, o `system boot` escreve units
fora do `DELONIX_ROOT`, e o `init --up` de um template não completou em 180 s
nesta ligação.

**A primeira corrida desta bateria deu 13 falhas e nenhuma era do motor**: o
setup da secção de backup puxava uma SEGUNDA imagem com a saída descartada, e
num host de ligação lenta isso fazia treze checks chumbarem a dizer «no such
container». Está corrigido na mesma série.

#### Adendo (2026-10-08): a ponta moveu-se, e o que isso vale

A `main` foi fundida nesta branch **três** vezes a 2026-10-08, trazendo
`#714`–`#722`. A ponta passou de `0640a9c7` para `831b9601` e desta para
`7f5b5125`, logo os números da tabela acima descrevem **a árvore de 2026-10-06 e não
esta**. Dito aqui porque o `#714` existe exactamente por isto ter acontecido antes,
e porque o `version_gate` não o apanha: ele confere que as notas EXISTEM, não que
descrevem o conteúdo.

O segundo merge trouxe o `#717` — **quanto da CLI a bateria EXECUTA passa a ser
medido, não escrito à mão** (plano 65 F0.2). O `Cargo.toml` conflitava por
construção (a `main` em `4.5.0`, esta branch em `5.0.0`) e ficou em **5.0.0**,
com o `Cargo.lock` a concordar; o `AGENTS.md` e o `scripts/e2e.sh` ficaram com os
DOIS lados, que é a regra da casa — escolher um apagaria o achado de outra pessoa.

**E a ponta moveu-se outra vez antes da tag**, com o `#721` e o `#722`, que são as
duas metades do mesmo item: o `#717` entregou o INSTRUMENTO e o número que ele
media; estes dois entregaram a COBERTURA que o número conta. Por isso a tabela
abaixo substitui a anterior em vez de a acompanhar.

Re-medido na ponta final, nesta máquina:

| Medição | Resultado |
|---|---|
| Bateria de testes (`cargo test --workspace`) | **2836 passados / 0 falhas** |
| `cargo fmt --check` e `clippy -D warnings` | limpos |
| `version_gate` | ok — `release commit: 5.0.0 (notes present), previous tag v4.5.0` |
| `lang_ratchet`, `arch_fitness`, `adr_status_gate`, `contract_gate`, `capability_ratchet`, `dev_docs --check` | ok |
| `tmp_roots_gate` | ok — 0 restos |
| `cli_exec_ratchet` | ok — **160 de 272 folhas da CLI invocadas sob asserção = 58,8 %** |
| Bateria da CLI (`scripts/e2e.sh`), corrida completa | **PASS=1148 FAIL=0 SKIP=14 XFAIL=1 XPASS=0** |

**A cobertura passou de 125 para 160**, e as componentes separam-se de propósito,
porque só uma delas é trabalho novo: **33 folhas** que ninguém exercitava ganharam
`check`; **1** (`build`) a bateria já exercitava e o parser não conseguia ver; e
**0** foram recuperadas retroactivamente pela correcção do parser. Os 272 do
denominador não mexeram — nenhum destes PRs tocou o `scripts/cli_baseline.tsv`.

**O parágrafo anterior desta secção dizia que os 46,0 % SUBESTIMAVAM, e deixou de
valer pelas duas razões que nomeava**: o `#721` regravou o trace contra a ponta
fundida (125 → 126), e o `#722` correu a bateria inteira na ponta final. O que
sobra desse parágrafo é um defeito do instrumento que eu próprio tinha publicado
no `#717` e que o `#722` corrigiu: **um `--help` mais à frente no MESMO corpo de
shell apagava uma invocação real anterior**, e o teste do fim-de-comando era código
morto. Medido nos dois sentidos — a correcção recupera **zero** folhas
retroactivamente (o parser antigo e o novo dão ambos 126 sobre a corrida anterior),
logo o ganho é de hoje para a frente, não uma reescrita do passado.

**Os 14 SKIP, classificados até ao último** — o cabeçalho do trace commitado diz
«5 + 7», que não soma 14, e a conta certa é esta: **7** pedem um appliance OPNsense
(3) ou um cluster Proxmox (4); **3** este host não os pode medir (o `hostPort` em
root/CNI, que exige root e uma conflist com `portmap`; o NFS, que exige
CAP_SYS_ADMIN; o `system boot enable`, que escreve units fora do root isolado);
**2** são caminhos de FALHA cujo disparo é a ferramenta estar AUSENTE, e este host
tem-na (`virt-customize`, `wg`) — saltam por ele ser capaz, não por não ser; **1**
porque não há imagens VM neste host; e **1** — o `stack init --template httpd --up`
— porque o build não completou em 180 s. Só este último é uma lacuna de medição em
vez de uma propriedade do host. **O 1 XFAIL é o ACH-034**, um
defeito do motor com achado escrito (um `secret rotate-key` põe a versão a 1 em vez
de a preservar) — não chumba o portão, e chumba por XPASS no dia em que for
corrigido, que é o que força o marcador a sair.

**O arnês de caos e o portão de performance CORRERAM na ponta, e a versão anterior
desta secção dizia o contrário.** Dizia «NÃO re-medido», e para o portão arriscava
uma previsão — «recusaria julgá-lo» — que a medição contradiz. Correram noutra
sessão, e aqui ficam com a proveniência e com a parte que eu próprio verifiquei,
porque um número de outra pessoa não é uma medição minha:

| Arnês de caos | **54 PASS · 0 FAIL · 1 SKIP** — duas corridas independentes, o mesmo resultado |
|---|---|
| Onde | VM descartável, Ubuntu 24.04.4, 4 vCPU, 5 GiB, kernel 6.8.0-136 |
| Como | `systemd-run --user --scope -p Delegate=yes -- bash scripts/chaos.sh --bin /usr/local/bin/delonix` |
| Binários | os cinco irmãos `delonix 5.0.0`, construídos em `831b9601` |
| 1ª corrida | outra sessão, `load(1m) 0.11`; log em `/tmp/delonix-chaos-v5.0.0/` com sidecar de proveniência |
| 2ª corrida | **conduzida nesta sessão**, 14:51Z, `load 0.27 → 0.22`, `rc=0` |
| O único SKIP | `truenas-destroy`, sem `DELONIX_CHAOS_TRUENAS_URL/USER/PASS` |

Os quatro cenários de limites (`oom`, `scale`, `aggregate-ceiling`,
`delegated-scope`) **passaram** — é o scope delegado que os torna exercitáveis, e
numa sessão SSH saltariam com `DX-6000`, que é o motor a recusar com razão.

**Porque é que um binário de `831b9601` vale para esta ponta, verificado por mim e
não aceite por palavra**: o `#717` não toca uma linha de Rust (`git diff --stat
e1aacedf^1 e1aacedf -- '*.rs' Cargo.toml Cargo.lock` vem vazio); o `831b9601` já
CONTÉM as únicas mudanças de motor em jogo (`#718` e `#720` — confirmado com
`merge-base --is-ancestor` nos três commits); de `831b9601` até à ponta do `#722` o
diff de `*.rs`/`Cargo.toml`/`Cargo.lock` é **vazio** (só `AGENTS.md`,
`docs/discovery/`, `scripts/`); e o `scripts/chaos.sh` é byte-a-byte o mesmo nas
duas pontas (sha256 `1a405cf940819eaf`, igual em `main` e em `release/v5.0.0`).

**E a corrida foi repetida nesta sessão, para a nota não depender de uma medição
alheia**: mesmo comando, mesma VM, **54 PASS · 0 FAIL · 1 SKIP** e `rc=0`, com os
cenários contados do MEU log e não do sumário dele — **zero** linhas de FAIL, e os
quatro de limites (`oom`, `scale`, `aggregate-ceiling`, `delegated-scope`) a passar
nas duas. O log da primeira foi verificado contra o hash publicado
(`f85d2c4f…ba4`, confirmado por mim) e tem 54 linhas `^  PASS  `; a única menção a
FAIL é o próprio sumário a dizer `0 FAIL`.

**O arnês NÃO escreve `results.jsonl`** — ao contrário do `scripts/e2e.sh`, não tem
`OUT=` nenhum (`grep -nE 'results\.jsonl|OUT=|jsonl' scripts/chaos.sh` vem vazio),
por isso o log é a única saída e esta nota não promete um ficheiro estruturado que
não existe.

| Portão de performance | **julgou, e passou: `delonix` ×1.08** |
|---|---|
| `delonix` | 94 ms · baseline 87 ms · ×1.08 · dispersão 1,2× |
| `docker` | 299 ms · baseline 224 ms · ×1.33 · dispersão 1,3× |
| `podman` | 336 ms · baseline 279 ms · ×1.20 · dispersão 1,3× |
| Bancada | `load(1m) 6.41` de um limiar de 16 · `publishable: true` · 10 amostras por motor |
| Baseline | 2026-09-23, `ceec7f8c`, delonix 4.3.0, a MESMA máquina |

**Reproduzi o veredicto a partir do JSON cru** (`python3 scripts/bench_gate.py --run
/tmp/bench.json` → `rc=0`, `ok: delonix ×1.08, dentro da tolerância de 25%`), em vez
de confiar no número relatado. **E a minha previsão estava errada pela razão que
torna o portão útil**: as âncoras degradaram MAIS do que o motor na mesma corrida
(×1.33 e ×1.20 contra ×1.08), logo o que mexeu foi a bancada e não o motor — se o
motor tivesse degradado tanto como elas, ele teria recusado julgar. A terceira saída
existe exactamente para separar esses dois casos, e aqui separou.

**Duas pré-condições que o arnês não tem sonda para detectar**, e que deram corridas
INVÁLIDAS sem se anunciarem como tal: correr o binário de `target/release/` em vez do
caminho instalado dá `DX-9301 making / private … Permission denied`, porque o perfil
AppArmor `userns` está pinado ao CAMINHO — o sintoma foram 25 SKIP e 2 FAIL a apontar
para o motor quando a causa era o caminho do ficheiro; e uma sessão SSH não tem
delegação de cgroup, logo os quatro cenários de limites saltam. Nenhuma das duas é
defeito do motor, e nenhuma das duas se vê no relatório.

**Re-medido DEPOIS de a release estar publicada, contra o commit publicado**
(2026-10-08, 17:49:14Z). O parágrafo acima descreve a corrida de ANTES do corte, a
load 6.41 e com as três dispersões em 1,2–1,3×; esta foi conduzida numa bancada
genuinamente quieta e o que ela acrescenta é que **as âncoras ficaram paradas**:

| Portão de performance, 2.ª medição | **rc=0 — `delonix` ×0.94, sem regressão** |
|---|---|
| `delonix` | 82 ms · baseline 87 ms · ×0.94 · dispersão **1,16×** |
| `docker` | 231 ms · baseline 224 ms · ×1.03 · dispersão **1,17×** |
| `podman` | 259 ms · baseline 279 ms · ×0.93 · dispersão **1,14×** |
| Bancada | `load(1m) 3.13` de um limiar de 16 — a baseline foi colhida a 3.23, logo é a MESMA bancada |
| Binário | construído da ponta da `main`, `commit: 2837f5863` — o commit DA TAG, confirmado com `git rev-parse 'v5.0.0^{commit}'` |
| Baseline | 2026-09-23, `ceec7f8c`, delonix 4.3.0, a mesma máquina · **não foi regravada** |

A diferença que importa face à primeira corrida não é o motor ter melhorado 12 ms
— é as **três** dispersões estarem em 1,1–1,2×, abaixo do tecto de **1,5×** que o
portão exige para *gravar* uma linha de base e não só do tecto de 3× para a
julgar. Na primeira, o `docker` a ×1.33 e o `podman` a ×1.20 diziam «a bancada
mexeu-se»; aqui ficaram em ×1.03 e ×0.93, e é isso que torna o ×0.94 do motor uma
afirmação sobre o motor. **A linha de base NÃO foi regravada** (`--record` nunca
foi passado): a de 2026-09-23 é melhor bancada e continua a valer.

**Como é que a janela apareceu, dito porque é parte da medição**: não apareceu
sozinha. O host estava a 95 % de disco e `load(1m) 47`, e o `scripts/bancada.sh`
recusava. Libertaram-se **93,0 G** de directórios de `CARGO_TARGET_DIR` de sessões
já terminadas em `~/.cache` — disco **95 % → 84 %** — e o `load5` caiu quando dois
builds alheios acabaram. Os 93 G saíram com prova específica de Cargo em cada
directório (`.rustc_info.json` ou `.cargo-lock`; o `CACHEDIR.TAG` **não** serve,
porque o `uv` também o escreve) e com três provas de vida recolhidas **com root**
— sem isso, um `cargo` sob `sudo` passaria por «sem processo vivo». Nada do estado
do motor foi tocado.

**Por fim, porque duas corridas da bateria dão totais diferentes e as duas estão
certas**: `1110 + 7 + 1 = 1118` e `14 − 1 = 13` — os sete são checks `--up:` atrás do
SKIP de 180 s, e o `+1` é o check `provider matrix é a matriz publicada`, que FALHA se
a bateria correr na árvore `main` com binários 5.0.0 (a matriz embebe a versão do
motor, e a única linha diferente é `engine 4.5.0` contra `engine 5.0.0`). Contra a
matriz da ponta é idêntica byte a byte: é montagem, não defeito.

### O que NÃO foi validado

Dito aqui porque o implícito lê-se como provado:

- **O laboratório nocturno nunca EXECUTOU**: o agendamento dispara, mas cada
  corrida fica em fila à espera do runner `delonix-lab`, que não está registado,
  e é cancelada às 24 horas. Toda a evidência desta release vem de corridas à
  mão, num host de desenvolvimento — e o custo disso mediu-se nesta passagem: o
  portão de performance precisou de **três tentativas** para dar um veredicto,
  porque as duas primeiras pegaram a máquina com I/O a dispersar a medição 27×.
- **A métrica de célula do catálogo de capacidades é 100 de 252 aplicáveis
  (39,7 %)** — ou seja, 88 células estão implementadas e **não provadas**, e 64
  não existem. A matriz (`docs/providers/capability-matrix.md`) diz, célula a
  célula, qual é qual, e desde esta release imprime o número no topo: ele é
  CALCULADO a partir do catálogo em código e segurado por um ratchet nos dois
  sentidos (`scripts/capability_ratchet.py`). Até aqui era contado à mão sobre o
  markdown, e as duas contagens que circulavam — «111 de 281» e «99 de 265» —
  estavam ambas erradas: a primeira vinha de um `grep -c` que contava os
  sumários do próprio ficheiro como se fossem células.
- **O `hostPort` em root/CNI não foi validado com um kubelet** (`#720`): tudo foi
  conduzido pelo `crictl` — o cliente oficial do projecto Kubernetes, pelo mesmo
  transporte que um kubelet usa — e por testes, não por um kubelet a agendar os
  pods com as suas retentativas e a sua noção de prontidão. E foi **um nó, uma
  cadeia** `bridge`+`portmap`: uma CNI real de cluster (Calico, Cilium) a declarar
  a capacidade de outra forma, e mais de um nó, ficaram por medir.
- **O SCTP está provado no caminho do `portmap`, não nas nftables próprias do
  motor** (ADR-0074 D3.1): a rota em que o motor escreveria o seu próprio
  `sctp … dnat` em modo root só foi carregada num kernel como ruleset — nenhum
  pacote a atravessou. Quem a construir começa por fechar essa lacuna.
- **O endereço de bind registado não foi medido através de um reboot** (`#718`): o
  ciclo provado é `publish` → `stop` → `start`, que é o que um unit do `net boot
  enable` corre, mas o arranque da máquina em si não foi exercitado.
- **Nenhuma conformidade é reclamada**: as suites OCI, CRI, CNI e CSI não
  correram. O número do CRI publicado (79/103) é do `critest` v1.36 contra o
  motor **v0.63.1** e não foi remedido.
- **O IPv6, o balanceador L4 e o hotplug de VM não existem** — têm ADR aceite e
  zero código.
- **As pools de armazenamento estão na fase P0**: só o driver `dir`. O helper
  privilegiado, btrfs, ZFS, LVM-thin e os discos de VM em pools precisam de root e
  de discos numa VM de laboratório.
- **O OpenStack não é um provider do motor**: existe o ADR-0039 em *Proposed*, sem
  backend, e `provider.type: openstack` é recusado no carregamento.
- O ordenamento do trabalho que fica está em
  `docs/discovery/66_CONTINUITY_PLAN.md`.

---

## v4.5.0 — rede que falha fechada, Proxmox medido rota a rota, providers num ficheiro por nó

Cento e três commits desde a `v4.4.0` (110 contando os merges), de `#474` a `#576`.
Numerada como MINOR: há verbos novos (`vm resize`, `vm cloud-init`,
`vm move`, `provider ls|describe|matrix|config`, `net netns gc`,
`serve node-api`), Kinds novos (`NetworkGateway`, `NetworkZone`), um ficheiro de
configuração novo (`providers.yaml`) e uma ferramenta MCP nova
(`workload.usage`). Nenhuma superfície foi removida. **Várias mudanças alteram,
de propósito, comportamento que já existia**: um limite, uma regra ou uma
remoção que antes passava com um aviso ou em silêncio passa a ser recusada.
Estão todas juntas na primeira secção, antes das novidades.

O fio condutor é o de sempre, levado mais longe: **um resultado que o motor
não consegue provar não se reporta como sucesso**. Isto vale para uma regra de
firewall inválida, para um `rm -f` cujo processo ainda não morreu, para uma
tarefa do Proxmox que «acabou OK» sem ter aplicado nada, e para um `push` que
morria sem dizer porquê.

### Mudanças de comportamento

Quem actualiza deve ler esta secção primeiro.

**Rede de containers (auditoria de rede, sessões S1–S4, #554, #560, #553, #555, #552)**

- **A firewall por container falha fechada.** Uma spec com uma regra inválida é
  recusada inteira (antes, a regra era saltada e o resto aplicado). Também se
  recusa um `dir`, uma `action` ou uma política fora dos valores documentados:
  antes, um `dir` desconhecido era lido como `in` e uma política desconhecida
  como `allow`.
- **Uma falha do isolamento de namespace é um erro, não um aviso**, em
  `container run`, `start` e `pod create`. O attach é desfeito. Antes, o `run`
  saía com rc=0 e o container ficava alcançável a partir de outra namespace.
- **O egress da origem é avaliado sempre.** Antes, um `accept` na chain do
  destino terminava a avaliação e a política `egress deny` da origem nunca
  corria (medido: A→B passou 3/3 com o egress de A em `deny`). Agora são duas
  base chains, `fwout` e `fwcont`.
- **Regras de entrada explícitas deixam de desligar o isolamento** (#560). Um
  container com, por exemplo, `ingress deny tcp/22` passava a aceitar ligações
  novas de qualquer outra namespace em todas as outras portas. Agora há um
  guardrail que nenhuma regra de entrada retira; um `allow` explícito continua
  a abrir o par que nomeia. **Um container com regras de entrada explícitas
  deixa de aceitar ligações novas de outras namespaces nas portas que nenhuma
  regra abre.**
- **Remover a última regra (`ingress rm`/`clear`) fora da namespace `default`
  mantém a chain**, que é também o isolamento. Antes, o container ficava aberto
  a outras namespaces.
- **Redes com CIDR fora de 10.200–254 passam a ter firewall e isolamento.**
  Antes ficavam sem chain e fora do conjunto de workloads: medido em
  `172.30.5.0/24`, duas namespaces falavam 2/2 nos dois sentidos.
- **`network connect`/`disconnect` falham quando a firewall falha**, em vez de
  avisar e sair com 0.
- **`network rm` recusa uma rede com alguma coisa ligada** (**DX-5307**, exit 5)
  e nomeia os containers, os membros de pod e as VMs. Antes saía com rc=0 e
  deixava o container «Up» numa bridge apagada. O `stack --replace/destroy`
  mantém o seu caminho explícito.
- **IPAM transaccional** (#553): uma reserva de um IP que já é de outro dono é
  recusada (**DX-5308**, antes gravava-se com um aviso); o `stop` mantém o
  lease e o `start` volta ao mesmo endereço; o `rm` liberta também os leases
  das redes extra. `--subnet 10.200.0.0/16` (o espaço do ingress) e
  `10.0.2.0/24` passam a ser recusados.
- **O pool de DHCP das VMs vive dentro da rede.** Numa rede que não é `/16` o
  DHCP servia a rede errada (numa `/24` oferecia um endereço `/16` com outro
  router, e a VM ficava sem IP). O pool fica agora no topo da rede, com
  `min(240, tamanho/8)` endereços; num `/16` continua `.254.10–.249` byte a
  byte, para nenhuma VM existente mudar de IP. Consequência: um `/28` passa a
  ter 11 endereços para containers e 2 para o pool das VMs.
- **O holder de rede valida cada token de controlo antes de correr um comando**
  (S4, #552): um token que vá para um argv não pode começar por `-`, IPs e
  gateways têm de ser IPv4 estritos, `rate`/`burst` só dígitos, e o `type` de
  um plugin CNI tem de ser um nome simples (antes, `"type": "/tmp/x"` executava
  esse binário no holder). As linhas que um cliente legítimo envia não mudam.
- **`vm bridge` valida as subnets**: nem `default`, nem `/0`, nem uma subnet
  sobreposta à SDN, e o root é verificado antes de qualquer comando.

**API de gestão (`delonix-mgmt`)**

- **As sete mutações de rede que escreviam o dataplane sem passar pelo registo
  respondem `501` com `DX-6302`** (#555): firewall, egress, `attach` e
  `attach-extra`. As leituras e o `publish`, que grava o registo, continuam.
  Foi uma decisão explícita: recusar todas, em vez de as alargar.

**Containers e cgroups**

- **`--cpuset`, `--io-weight` e `--device-read/write-bps/iops` são recusados
  (exit 69) quando o cgroup do container não tem o controlador** (#545). Antes,
  `--device-write-bps 5mb` escrevia a 1,6 GB/s e saía com 0, só com um aviso. A
  válvula de escape `DELONIX_ALLOW_UNENFORCED_LIMITS` continua a valer. Chega a
  todos os pontos de entrada (CLI, manifesto, compose, Docker API, CRI); o CRI
  passa a receber exit 69 para um `cpuset_cpus` sem `cpuset`, como já recebia
  para CPU e memória. Como root, o `delonix.slice` passa a delegar também o
  `cpuset`, que antes era ignorado em silêncio.
- **`sudo delonix system setup --delegate` escreve sempre o drop-in** (#550).
  Antes, a escrita só era alcançável num ramo que um processo root nunca toma,
  por isso quase nunca escrevia. Sob `sudo`, o relatório passa a ser sobre o
  utilizador que escreveu o `sudo`, e o passo seguinte nomeia
  `systemctl restart user@<uid>.service`: um `daemon-reload` sozinho é um falso
  verde.
- **`container rm -f` só volta quando o processo morreu** (#562). Antes voltava
  no sinal: com o disco carregado, o `delete pod --force` voltava em 0,76 s
  com os membros ainda vivos em estado D. Agora espera pela saída real, com o
  mesmo limite de 30 s do `stop`. Passado esse limite, **mantém o registo** e
  devolve **DX-8101** `container.still_exiting` (exit 124); um `rm -f` repetido
  remove-o quando o processo morrer.
- **Um container `--rm` é montado com o overlay `volatile`** (ADR-0056, #537).
  A saída deixa de esperar pelo writeback do sistema de ficheiros do host.
  Medido com 1 GB sujo noutro ficheiro do disco: `run --rm … alpine true` em
  0,16 s, contra 9,5–14 s num container mantido. Containers mantidos, de pod e
  de CRI não mudam.

**Providers e VMs**

- **Um `defaultProvider` que o processo não consegue servir falha com 69**, a
  nomear o provider (ADR-0054, #514). Antes era descartado, e o `vm create`
  seguinte podia criar a VM num hipervisor local. Um `providers.yaml`
  ilegível faz falhar os pedidos de VM sem `--backend`; os comandos que não
  escolhem provider continuam a funcionar.
- **Um `providers.yaml` com `type: opnsense` passa a registar o appliance**
  (#556). Registar não contacta nada.
- **Proxmox: `network` com o nome de uma rede do motor é recusado** (#478). Antes
  era aceite e deitado fora: o registo dizia `Network: lab-net` e a VM ficava
  no `vmbr0`.
- **Proxmox: uma tarefa que acaba em `WARNINGS: <n>` é um sucesso** (#546), e as
  linhas `WARN:` ficam no livro de tarefas. Antes era lida como falha, e a acção
  podia ser reenviada por cima de um recurso vivo.
- **Proxmox: um apply da SDN espera pelo reload de rede de cada nó** (#571). O
  `reloadnetworkall` do PVE acaba `OK` logo que *arranca* os reloads; um reload
  que falhe, ou que não apareça dentro do timeout, recusa agora o apply com
  **DX-6512**.
- **`vm create --require` com uma cloud image filtra de facto** (#539). Antes o
  libvirt era pedido por nome e o filtro nunca corria.

**Outros**

- **O binário já não morre por SIGPIPE numa escrita para um socket** (#573). Um
  EPIPE numa ligação de rede passa a ser um erro devolvido (com retry, no push).
  Um stdout ou stderr fechado (`image ls | head`) continua a terminar o processo
  por SIGPIPE, como antes.
- **`net netns down` recusa uma raiz que não existe**, em vez de a recriar.
- **`kind: App` deixa de apagar a cache de layers do CNB depois de cada build**
  (#529), como o ADR-0035 já dizia.

### Rede: o que mais entrou nas sessões S1–S6

A auditoria de rede (`docs/discovery/62_NAAS_FASE0_AUDITORIA.md`, #544)
inventariou a rede do motor, encontrou cinco recursos com dois escritores e
sete achados de fail-open, e repartiu a correcção em sessões. As mudanças de
comportamento estão acima; o resto é isto:

- **Migração do despacho sem passo manual** (#560). Um holder criado por um
  binário anterior tem o despacho antigo, de uma chain. O host migra-o agora no
  `ensure_up` e no `apply_firewall_all` (`nsenter` para o userns e o netns do
  pin, `nft -f`, uma transacção atómica e idempotente), uma vez por pin. Se a
  migração falhar, é erro com o remédio.
- **`network rm` não deixa nada no holder** (#555). Saem as chaves de
  `@netpair`, o membro de `@dlxbr`, as regras de egress e o conjunto FQDN, e a
  thread de DHCP pára. Antes, uma rede recriada com o mesmo nome nascia sem
  DHCP e com o `deny` da anterior. Um `publish` que falha a meio desfaz só as
  portas que publicou; uma rede criada por uma VM passa a aparecer no
  `network ls`; o `httproute` escreve sob lock e de forma atómica.
- **Um só alocador de `/16`** (#553), sob um lock, que vê os registos e as
  NetDefs em qualquer forma. Antes havia dois, que se ignoravam. As escritas de
  NetDef trancam e são atómicas: 60 escritas de egress concorrentes com
  mudanças de gateway deixavam **0** sobreviventes.
- **Posse pela marca, não pelo nome, nos providers remotos** (S6, #558). Um alias
  ou regra do OPNsense e uma vnet da SDN do Proxmox eram do motor se tivessem o
  nome certo, por isso uma regra feita à mão com a mesma descrição era adoptada
  e depois apagada. Agora:
  - no OPNsense, a marca é uma categoria de firewall `delonix-owner:<token>`.
    Um objecto encontrado sem ela é recusado (**DX-5340**); um nosso editado no
    appliance, **DX-5341**; um commit com alterações preparadas por outra pessoa,
    **DX-5342**;
  - no Proxmox, a marca vai no `alias` da vnet. Uma zona não tem campo de texto,
    por isso só é do motor quando o registo diz que foi o motor a criá-la. O
    commit passa por uma transacção sob o lock global da SDN, e alterações
    pendentes alheias recusam-no (**DX-5516**);
  - um registo anterior à marca não prova posse: o teardown não toca no remoto e
    nomeia o que deixou.
- **`vm unbridge` desfaz o que o `vm bridge` aplicou** (#559). O `bridge --apply`
  grava as subnets em `<state>/ingress/vmbridge-<bridge>.subnets`, antes de correr
  o plano, e o `unbridge` apaga as regras e a rota dessas subnets. Antes, uma
  ponte feita com `--vm-subnet` deixava as duas regras `FORWARD … ACCEPT` e a
  rota de retorno depois do teardown. O `unbridge` ganha `--vm-subnet`
  (repetível).
- **ADR-0059 (aceite, #547)**: providers de rede por papel (segment, gateway, e
  mais tarde NAT, IPAM, DNS), escolhidos por nome e negociados pelo catálogo de
  capacidades, com o ciclo validate → plan → apply → observe → verify. Desta
  release só entra a fase F1 (#556):
  - **catálogo 1.0.0 → 1.1.0**, com 27 entradas novas de rede e firewall (127 no
    total);
  - **`ProviderKind::Gateway`** e o **OPNsense em `provider ls`**, com um
    relatório declarado e nunca sondado: 3 supported, 1 partial;
  - **`type: opnsense` no `providers.yaml`**, com `keyFile`/`secretFile` (um
    segredo escrito no ficheiro é recusado pelo nome) e `networkDefaults`.
    Nenhum Kind resolve ainda por `networkDefaults`; isso é a F2.
- **ADR-0051: `kind: NetworkGateway`** (#498), com o `GatewayProvider` como
  porta e o OPNsense como primeira implementação (aliases e regras de filtro
  pela REST API do appliance). A memória por omissão do appliance registado
  passou de 2G para 3G, o mínimo que o fabricante documenta (#494).

### Proxmox VE: a cobertura da API é uma matriz medida

O ADR-0049 (#474) fixou o denominador: as **675 rotas** (método, caminho) que o
próprio PVE 9.2.2 publica. `scripts/proxmox_api_inventory.py` lê esse schema e
o que o `delonix-proxmox` chama a partir do código fonte, e
`docs/proxmox/matrix-9.2.2.md` é gerada e guardada por um teste. A coluna
«tested» vem de um trace de rotas gravado em corridas ao vivo, com a
proveniência no cabeçalho.

**No início deste ciclo: 16 rotas chamadas (2,4 %). Na `main` desta release:
161 chamadas (23,9 %), 160 vistas num trace ao vivo.** A única por ver é o
`GET /nodes/{node}/tasks` da reconciliação de resposta perdida, que só a
injecção de falhas alcança. Por área: qemu 59/109, sdn 85/90, storage 5/25;
as 62 rotas de LXC estão `unsupported-by-design`.

- **Transporte** (fatia 1, #475): erros tipados por status HTTP (401, 403, 404
  com exit 4, 409 com exit 5, 502–504 com exit 69), um **livro de tarefas por VM**
  (`<vmdir>/proxmox-tasks.json`, UPID gravado antes da espera) e a regra de que
  **uma resposta perdida não é um pedido perdido**: depois de uma falha de
  transporte o cliente procura a tarefa ou o efeito, e nunca reenvia uma escrita
  não idempotente. Toda a escrita passa pelo caminho de tarefas, e um teste lê o
  código fonte do crate e falha com uma escrita fora dele (#478).
- **Verbos novos do motor**, com o Proxmox testado ao vivo:
  - `vm resize <nome> [--vcpus N] [--memory M]` (#503): resize a frio de uma
    VM parada, também no libvirt e no Cloud Hypervisor. Recusado com a VM a
    correr (**DX-5505**).
  - `vm cloud-init <nome> [--hostname] [--user] [--ssh-key]` (#509): muda o
    cloud-init de uma VM parada. No Proxmox, a prova é a renderização do próprio
    nó; no libvirt e no Cloud Hypervisor é recusado pelo nome, porque a seed é
    um ISO feito na criação.
  - `vm move <nome> --node <alvo> [--live] [--with-local-disks]
    [--target-storage <id>]` (ADR-0053, #522, #532, #536): move uma VM entre os
    nós do seu cluster, a frio ou a quente, com o mesmo `vmid` e o mesmo
    registo. Por omissão, uma VM com discos locais é recusada; com
    `--with-local-disks` o nó copia-os (espelho NBD com `--live`).
  - `provider describe proxmox --probe` (#505): cinco `GET` ao cluster do alvo
    (quórum, nós, storages, HA, zonas SDN), só leitura.
- **Cada VM é endereçada no seu próprio nó** (ADR-0053, #515). Antes, todos os
  caminhos por VM usavam o nó configurado, por isso uma VM migrada pela UI do
  Proxmox era procurada no nó errado. Uma VM movida fora do motor é encontrada
  com uma leitura de `/cluster/resources`.
- **Energia** (#502): `shutdown`, `reboot`, `reset`, `suspend` e `resume`. O
  `vm pause` deixa de ser recusado no Proxmox.
- **Criação**: `diskSize` num clone de template cresce o disco por
  `PUT …/resize`, e um shrink é recusado pelo nome (#485); `extraDisks` e
  `extraNics` no mesmo `POST` da VM, e o `destroy` leva todos os discos (#504).
- **Arranque de uma imagem do store do motor** (ADR-0057, #541): o
  `vm create --backend proxmox --disk <imagem do store>` envia a imagem para o
  storage de importação do nó (com o sha256, verificado pelo nó, uma vez por
  imagem e por nó) e cria a VM com `import-from`. Deixa de ser preciso preparar
  um template à mão. Storages por `DELONIX_PROXMOX_IMPORT_STORAGE` e
  `DELONIX_PROXMOX_DISK_STORAGE`, ou `storage.import`/`storage.disk` no
  `providers.yaml`. O motor nunca liga o tipo de conteúdo `import` no nó
  (**DX-6510**); espaço insuficiente é **DX-6511**.
- **Backup e restore** por `vzdump` (#487, #489): o restore nunca escreve por
  cima de um `vmid` ocupado. Com o guest agent (#528), um backup só é
  reportado como quiesced quando o log do nó mostra o `fs-freeze` e o
  `fs-thaw` (senão **DX-6509**), e o `vm describe` ganha um bloco *Guest*
  (SO, kernel, hostname, filesystems).
- **Firewall do próprio nó por VM** (ADR-0052, #492, #496, #512): o
  `kind: NetworkPolicy` ganha `scope: vm`, e a política de uma direcção de uma
  VM vai para a firewall do nó onde ela corre. O motor só substitui as regras
  marcadas `delonix-managed:<n>`, e as regras feitas à mão ficam. Com a firewall
  do datacenter desligada, o apply recusa com **DX-6508** antes de qualquer
  escrita.
- **SDN do próprio Proxmox** (#493, #497, #500, #542): zonas, vnets, subnets,
  controladores IPAM/DNS/BGP/EVPN, fabrics, DHCP, reservas de IP, prefix lists,
  route maps e a firewall de vnet. O lock global é usado como transacção
  (`sdn_transaction`), com rollback se um passo falhar. **`kind: NetworkZone`**
  (#501) liga a SDN do Proxmox a um manifesto, sem campo `provider`.
- **Três factos medidos que o schema não diz**, e que mudaram código:
  - `…/status/suspend` cria uma tarefa `qmpause`, não `qmsuspend`;
  - a firewall de vnet só é aplicada pela `proxmox-firewall` nftables, e filtra
    o tráfego em bridge dentro da vnet, **não o tráfego encaminhado entre vnets**
    (#549, #551, só documentação);
  - a imagem de appliance do repositório reescrevia `/etc/network/interfaces`
    sem o `source /etc/network/interfaces.d/*`, e os applies de SDN anteriores
    ao #500 foram aceites **sem nada realizado** no nó. O script foi corrigido.
    As imagens Proxmox passam também a reescrever o `/etc/hosts` com o endereço
    real do `vmbr0` em cada arranque (#548).
- **ADR-0058 (Aceite a 2026-09-28; proposto no #543)**: um container LXC do Proxmox **não** serve o
  `kind: Container`. Não há `exec`, nem logs, nem exit status na API, e nada da
  SDN do motor se aplica. Se o LXC entrar, será como recurso próprio. As 62
  rotas ficam `unsupported-by-design`, e desta release só entra a fatia 0
  (os três desfechos de uma tarefa, acima) e a fatia 1 (abaixo). Aceite com o
  recurso a chamar-se `kind: SystemContainer`, por paridade com o Proxmox
  (#574); o Kind ainda não existe.
- **Arquivo OCI que um nó Proxmox aceita** (#574, plano 63 fatia 1):
  `delonix_oci::write_oci_media_archive` escreve um arquivo OCI layout com
  media types OCI (os blobs do store byte a byte, só o manifesto reescrito) e
  recusa pelo nome uma layer zstd ou estrangeira (DX-1409). Medido no nó de
  laboratório: o arquivo do `alpine:3.20` foi aceite por `pvesh create
  /nodes/pve/lxc`; o Docker v2 do `image save` foi recusado. **Ainda nenhum
  comando o chama** — o upload é a fatia 2 — e o `image save` continua Docker v2.
- **O arquivo sobe para o nó como template de container** (#576, fatia 2):
  `Client::stage_template` envia-o como `vztmpl`, com o nome tirado do digest
  do manifesto (`dlx-<hex>.tar`) e o sha256 para o nó verificar; um arquivo
  que o nó já tem não é reenviado, e uma resposta perdida resolve-se listando
  o storage. Recusa antes de enviar um byte um storage sem `vztmpl`
  (**DX-6513**) ou sem espaço (**DX-6514**). Medido no nó de laboratório: um
  checksum errado termina a tarefa em `checksum mismatch` sem deixar ficheiro.
  **Também ainda sem comando que o chame**: ligar o pull do motor a isto é a
  fatia 3.

### Providers: um catálogo de capacidades e um ficheiro por nó

- **Catálogo de capacidades versionado** (ADR-0050, #476). `delonix provider
  ls|describe|matrix` responde, por provider, a cada entrada do catálogo com um
  de seis estados. **`supported` não se auto-certifica**: tem de citar um check,
  um e2e, um caso de caos ou um teste ao vivo, e um teste confirma que cada
  citação existe. `provider ls` não faz nenhum pedido; o Proxmox é declarado e
  nunca sondado. `docs/providers/capability-matrix.md` é gerada e guardada por
  um teste. Com checks novos na bateria, 17 linhas do libvirt e do Cloud
  Hypervisor passaram de `partial` a `supported` (#499); escrever esses checks
  encontrou três defeitos, também corrigidos (uma VM CH na namespace `default`
  sem regra anti-spoof, o backup ao vivo de uma VM libvirt com dois discos, e
  `vm vnc` a imprimir `127.0.0.1:0` para uma resposta `host:N`).
- **`vm create --require <capacidade>`** e `spec.requiredCapabilities` (#479): um
  nome fora do catálogo é **DX-1527** (exit 1), um backend que não serve a
  entrada é **DX-6507** (exit 69), e as duas recusas acontecem antes de o disco
  ser tocado. Os dois predicados internos que comparavam o nome do backend lêem
  agora o relatório de capacidades (#482).
- **`providers.yaml`** (ADR-0054, #514, #519, #521): um ficheiro por nó
  (`config.delonix.io/v1`) diz que providers o nó tem, como os alcançar e qual
  serve um pedido sem `--backend`. Procura-se por esta ordem:
  `DELONIX_PROVIDERS_CONFIG`, `$XDG_CONFIG_HOME/delonix`, `/etc/delonix`; o
  primeiro ficheiro ganha e os ficheiros nunca se fundem. Campos desconhecidos
  e segredos inline são recusados pelo nome. Verbos:
  - `provider config show [-o json]`: o ficheiro lido, os ficheiros que a ordem
    salta, o default e de onde vem. Uma credencial aparece só pela origem;
  - `provider config validate [-f]`: o parser e aquilo para que o ficheiro aponta
    (token legível só pelo dono, CA, segredo), sem contactar nada;
  - `provider config schema`: o JSON Schema, publicado em
    `docs/schema/v1/providers.json`;
  - `vm default-backend --set/--clear` passa a editar o ficheiro;
  - `install.sh --vm-provider <libvirt|cloud-hypervisor>` escreve-o **só se não
    existir** (noclobber, symlinks incluídos).
- **`vm create --allow-mac-spoofing`** / `allowMacSpoofing` (ADR-0055, #531):
  opt-out explícito, por VM, do filtro anti-spoofing do libvirt, para um
  hipervisor aninhado. Visível no `describe vm` (`Antispoof: OFF`), com um aviso
  no `create`, e recusado onde não há filtro (**DX-1505**).
- **Porta de VM no contexto de computação** (ADR-0044, aceite, #486, #517): o
  `VmBackend` e os seus tipos passam para o `delonix-compute`, e o
  `delonix-proxmox` deixa de depender do `delonix-vm`. É uma mudança interna;
  nenhum chamador mudou.

### Imagens OCI: pull e push sem a imagem inteira em memória

Um lote de performance sobre pull, push e build. Cada ganho foi medido; as
medições de tempo foram feitas num host partilhado e carregado, e os PRs dizem
isso mesmo.

- **Pull em streaming directo para o CAS** (#535): cada layer é escrita à medida
  que chega, com o hash calculado no fluxo e a escrita numa thread própria.
  Pico de RSS de um pull do `node:22`: **214 MB → 27 MB**. O config desce em
  paralelo com as layers (#518).
- **Pull de imagens VM directo para o disco, com retoma entre processos**
  (#523): um parcial `<dest>.<digest12>.download` é retomado por `Range` pelo
  processo seguinte, e só é renomeado depois de o conteúdo bater com o
  manifesto. Pico de RSS para 276 MiB: **278 MB → 13 MB**.
- **Primeira execução com extracção paralela das layers** (#533): até 4, em
  streaming a partir do CAS. RSS do primeiro `run` do `node:22`: **~220 MB →
  ~21 MB**.
- **Um pull quente já não reescreve o registo da imagem, e o CAS sobrevive a um
  crash** (#530): `fsync` do blob e do directório depois do rename. Custo medido:
  18 `fsync` por pull.
- **Push sem o tecto de 5 minutos** (#518). Antes, qualquer layer que não subisse
  em 300 s falhava sempre (a ~1,3 MB/s, acima de ~390 MB).
- **Push a partir do ficheiro** (#524), em vez de três cópias em memória. `vm
  push` de 276 MiB: pico de RSS **837 MB → 10–13 MB**.
- **Retry do upload** (#526): até 5 tentativas, com backoff de 1, 2, 4 e 8 s,
  para falhas de transporte e respostas 5xx, 408 ou 429. Cada tentativa começa
  com um `HEAD`, por isso um blob que o registo já aceitou não é enviado duas
  vezes. Um 400 ou um 403 falham de imediato.
- **Layers em paralelo no push, com barra de progresso** (#527): até 4 de cada
  vez. `node:22` (389,5 MiB) com um tecto de 20 MB/s por ligação: **22–65 s →
  11 s**. O `image sign` publica com o mesmo cliente que leu o manifesto.
- **Mount entre repositórios do mesmo registo** (#538): um blob que o registo já
  tem noutro repositório é ligado sem enviar bytes. Medido contra o ghcr.io:
  promover uma imagem para um repositório novo levou **7,0 s**, contra 356 s no
  primeiro push. Se o servidor de tokens recusar o âmbito mais largo, o push
  continua sem o mount.
- **Um push que perde a ligação diz porquê** (#573). Um `image vm push` para o
  ghcr.io saía com código ≠ 0 ao fim de ~4 min, e a única saída era a linha
  INFO do início. A causa, reproduzida contra um registo local que fecha a
  ligação a meio do PUT: o processo morria por **SIGPIPE (rc 141), sem uma
  palavra e sem retry**. Agora:
  - um EPIPE num socket de rede é um erro, e o retry corre;
  - a mensagem traz a cadeia de causas completa e quanto subiu
    (`connection lost with N of M bytes sent`);
  - uma resposta recusada traz o status e o início do corpo (o código OCI, por
    exemplo `DIGEST_INVALID`). A pista `delonix login` só aparece em 401/403.

  **O push tem retry mas não retoma**: cada tentativa reenvia o blob desde o
  byte 0 (PUT monolítico). O pull retoma por `Range`; o push não. Ver as
  limitações.

### Containers: a morte de um processo é um facto a medir

Três PRs sobre a mesma classe de defeito: um SIGKILL entregue não é um processo
morto.

- **`rm -f` espera pela saída real** (#562), descrito acima. O arnês de caos
  passa a medir processos, e não registos (#561, #562): um veredicto
  `sandbox-teardown` falha se ficar algum processo com a raiz do sandbox.
- **Um arranque que falha depois do `clone` recolhe o filho e depois remove o
  cgroup** (#564). Antes, com userns (o caminho rootless normal), ficava um
  `dlx-<id>` vazio no host em 4 de 4 corridas, e um zombie por cada arranque
  falhado.
- **Uma nova incarnação é publicada sob o lock do registo** (#570). O `spawn`
  gravava o pid novo com um `save` sem lock, e o `update` do supervisor antigo
  podia apagá-lo depois. O registo ficava sem processo, e o `rm -f` seguinte
  não sinalizava ninguém. Uma publicação por cima de outra incarnação viva é
  recusada com **DX-5101** e o filho é recolhido.

### Recursos, build e disco

- **`system df` conta o store inteiro** (#480): entram as áreas `VM disks`,
  `build cache` e `images-build`, uma linha `other` que fecha a tabela, e um
  `TOTAL` que bate com o `du`. Antes, num nó real, um estado de 190 GiB
  reportava 105.
- **`net netns gc [--force]`** (#481): encontra pelo `/proc` a infra de rede cuja
  raiz foi apagada (pins, controls, slirps) e, sem `--force`, só relata.
- **Tecto de arenas do malloc** nos processos que ficam de pé (pin, control, CRI,
  mgmt, Docker API): o `delonix-mgmt` em repouso passou de **2,3 GB para 111 MB**
  de VmSize. Um `MALLOC_ARENA_MAX` do operador continua a mandar.
- **Travessia paralela do disco** no `system df`, no uso de volumes e na quota
  rootless: **2,0–2,5 s → 0,35 s** sobre 32 GiB, com a saída idêntica.
  `DELONIX_WALK_THREADS=1` repõe a sequencial.
- **A cache de build expira** 7 dias depois do último uso, e o `system prune`
  varre-a. O `vm prune` nomeia os directórios de `vms/` para onde nenhum registo
  aponta.
- **`delonix build`** (#529): sem espera de graça por cada container de trabalho,
  e um passo em cache fecha com ✓ (antes fechava com ✗ e o build acabava bem).

### Contrato de nó e MCP

- **`delonix-node-api`: o contrato de nó passa a ter servidor** (ADR-0050 D5,
  #525). É um novo executável, `delonix-node-api` (crate `delonix-node-api-bin`),
  que o `delonix serve node-api` corre, instalado pelo `install.sh` ao lado do
  `delonix` e construído pelo `release.yml`. Serve gRPC e HTTP/JSON
  dos mesmos `.proto` num socket unix `0600` com `SO_PEERCRED`. Hoje só
  responde `NodeService.ListProviders` (também como `GET /v1/providers?kind=`);
  o resto responde `UNIMPLEMENTED` e nomeia o passo que o traz. O contrato ganha
  dois campos aditivos (`Capability.state`, `ProviderInfo.catalog_version`), e
  o `buf breaking` contra a v4.4.0 passa.
- **`workload.usage`** (MCP, #568): contadores cumulativos por workload (CPU,
  memória e pico, I/O de bloco, pids, os tectos em vigor), lidos do cgroup de
  cada container e do processo do VMM de cada VM local. São contadores, não
  taxas: quem amostra duas vezes obtém a taxa. **Um número que falta é nomeado
  com a razão, nunca zero.**

### Documentação

- O guia de utilizador foi posto em dia com a v4.4 (#508) e corrigido onde
  ensinava o contrário do que o motor faz (#506): por exemplo, as contas das
  imagens VM golden estão trancadas desde 2026-08-18, e o completion é
  `completion shell bash`.
- O manual do contribuidor passa a existir em chinês (24 páginas), e o pt-AO e
  o fr-FR acompanham o inglês (#513).
- A checklist dos PRs nomeia os gates que a CI corre de facto (#540).
- A imagem golden de VM passa a arrancar com SeaBIOS e com OVMF na CI antes de
  ser publicada (#516). Uma tag publicada a 2026-08-24 não arrancava com
  SeaBIOS.
- **Os testes deixam de sujar o `/tmp`** (#572): um `cargo test --workspace`
  num `TMPDIR` vazio deixava 29 entradas (30 no runner); agora deixa 0, e a
  linha de base do `tmp_roots_gate` está vazia — qualquer entrada é uma fuga
  nova.

### Limitações conhecidas e o que NÃO foi validado

**Push de imagens**

- **O #573 não foi reproduzido contra o ghcr.io real** (HTTPS/rustls). Foi
  reproduzido contra um registo local que fecha a ligação a meio do PUT. A
  causa no ghcr.io é inferida (mesmo sintoma, mesma escrita), não medida.
- **O push não retoma.** Tem retry (5 tentativas), mas cada tentativa reenvia o
  blob desde o byte 0. Numa ligação lenta, um blob de vários GiB pode esgotar as
  tentativas. A retoma precisa do protocolo em chunks (`PATCH` +
  `Content-Range`) e não foi feita.
- A mensagem de corte diz até onde o upload chegou, não *porque* o registo
  fechou: a resposta do registo é descartada pelo cliente HTTP.
- O handler de SIGPIPE vale para o binário inteiro: um EPIPE num pipe interno
  (que não seja o stdout ou o stderr) passa a ser um erro devolvido. A bateria
  do workspace passou no CI do #573 (x86 e arm64), mas nenhum caminho com um
  pipe interno foi exercitado de propósito contra esta mudança.
- O fallback do mount entre repositórios (um servidor de tokens que recusa o
  âmbito mais largo) só foi verificado por leitura, e o Docker Hub não foi
  medido. A poupança da assinatura com um só cliente só aparece contra um
  registo autenticado, e não foi medida.

**Rede**

- **Correcções dentro do holder só valem depois de o holder ser recriado**
  (`net netns down` + `up`): a limpeza do `network rm` e do DHCP (#555), a
  validação dos tokens (#552) e as firewalls das redes CIDR (#554). O despacho
  da firewall migra sozinho (#560); o resto não.
- A validação do S4 (#552) foi provada ao vivo só pelos caminhos legítimos
  (#557). Os vectores de ataque foram reproduzidos em testes, não contra um
  binário antigo. O `cni-add` com um plugin CNI real não foi exercitado.
- As baterias de caos da firewall correram com `--force` (load 51) no #554, e
  por isso os veredictos não são publicáveis pela regra da bancada. O #560
  repetiu-as abaixo do limiar.
- O rollback do `vm_attach` quando a firewall falha foi provado por leitura, e
  o lock do `httproute` por um teste em processo, não com dois `stack apply`
  reais em paralelo.
- **O OPNsense não foi exercitado ao vivo** (#556, #558): o relatório do
  catálogo é declarado, e `net.ownership-marker` fica `partial`. A comparação
  entre a configuração e o que está a correr não vê uma edição dos campos de
  match de uma regra que mantém o uuid, nem aliases com nomes de host.
- Uma queda entre o apply de uma zona Proxmox e a gravação do registo deixa uma
  zona sem marca de posse. O apply seguinte recusa-a, e o operador tem de a
  apagar à mão.
- No `vm unbridge`, a conectividade VM→container a partir de um guest real atrás
  do `virbr0` não foi medida, e um `network rm` com a ponte de pé deixa o
  ficheiro de registo até ao `unbridge`.
- Continua em aberto o `PUT /v1/containers/:id/rate` da API de gestão, que
  também escreve `tc` sem registo e ficou fora do #555.

**Proxmox**

- **O ADR-0049 continua Proposto.** Estão Propostos também o **ADR-0055**
  (`--allow-mac-spoofing`, já implementado), o **ADR-0057** (arranque de uma
  imagem do store, já implementado). O **ADR-0058** está Aceite, mas dele só
  entraram as fatias 0 e 1: o `kind: SystemContainer` não existe nesta
  release. As funções do #574 e do #576 não têm chamador nesta release: no
  #574 o arquivo foi produzido por um exemplo descartável e copiado para o nó
  por `scp`; no #576 o arquivo do caso ao vivo é um substituto, e o caso ao
  vivo do `import` não voltou a correr.
- Todas as corridas ao vivo foram feitas contra um cluster de laboratório de
  dois nós (PVE 9.2.2), nunca contra um cluster de produção. A excepção é a
  sonda de leitura do `provider describe --probe`.
- A firewall de vnet do Proxmox está guardada mas **não é aplicada** num nó com a
  `pve-firewall` iptables, e mesmo com nftables não filtra o tráfego entre vnets.
  Na firewall por VM (`scope: vm`), nenhum pacote atravessou a VM num teste, e
  as quatro entradas `firewall.*` ficam `partial`.
- Uma falha real do reload de rede de um nó (#571) só foi injectada. O caso ao
  vivo de controladores e firewall de vnet falha com 403 quando corrido com um
  token: essas rotas exigem a password de `root@pam`.
- A cópia a quente de um disco local (#536) foi medida numa VM sem SO, sem
  escritas no disco durante o espelho.
- O `vm.snapshot.persistent` e o `vm.snapshot.memory` do Proxmox continuam
  `partial`.

**Containers, cgroups e recursos**

- O caminho `DX-8101` (processo ainda a sair ao fim de 30 s) só foi coberto por
  teste unitário. Continuam em aberto duas corridas: um `rm -f` concorrente
  com um `start` em curso, e o `stop` que desiste de uma saída presa em D e
  esquece o processo.
- O intercalar exacto que o #570 corrige não foi apanhado de novo ao vivo. A
  prova é o teste determinístico com `Store` e `flock` reais, mais a
  pré-condição medida.
- O overlay `volatile` não foi validado no caminho rootful, nem o fallback em
  kernels anteriores ao 5.10. Um container `--rm --restart always` parado com
  `stop` não é removido.
- O caminho sem userns do #564 (root) não foi medido.
- O caminho `sudo` do `system setup --delegate` (#550) **não foi corrido como
  root**, e o efeito do drop-in no `user@.service` não foi revalidado neste
  ciclo. Foi medido a 2026-08-19 numa VM.
- `net netns gc --force` a terminar processos a sério, e a expiração real da
  cache de build ao fim de 7 dias, não foram exercitados: estão cobertos pelos
  classificadores puros.
- O ganho de tempo do build (#529) não foi provado, porque o host estava
  carregado. O que foi provado é a contagem de ✓/✗.
- O custo em tempo de relógio do `fsync` do CAS (#530) numa máquina calma não
  foi medido.

**Contrato de nó e MCP**

- O `delonix-node-api` serve só `ListProviders`. Não há socket activation, nem
  rotas `/openapi.json`/`/docs`, e nenhum cliente de outra linguagem foi testado
  contra o OpenAPI.
- O `workload.usage` não foi medido contra uma VM Cloud Hypervisor viva, nem com
  `include_network: true` numa rede própria.

**Geral**

- Vários PRs correram só a secção da bateria `scripts/e2e.sh` que tocavam, e
  não a bateria inteira, porque a máquina de desenvolvimento tem cargas de
  produção. As medições de tempo (pull, build, extracção) vêm de um host
  partilhado com load entre 2 e 52, e são indicações, não baselines.
- Continua por validar o que a `v4.4.0` já listava.

---

## v4.4.0 — `drift`, a matriz do Compose com denominador, e um portão de performance que se recusa a julgar

Dezassete commits desde a `v4.3.0`. Numerada como MINOR: há três verbos novos
(`delonix drift`, `delonix migrate assess`, `compatibility compose`) e um Kind
novo (`RuntimePolicy`), tudo aditivo — nenhuma superfície foi removida nem
mudou de significado. O tema comum das três melhorias de cabeça é o mesmo:
**deixar de responder a uma pergunta com outra**.

Uma mudança de PROCESSO entra também, e afecta quem publica: **empurrar uma tag
já não publica nada** — ver a última secção.

### `delonix drift` — o que a MÁQUINA mudou, sem o manifesto na comparação (M05, #470)

O `stack plan` responde «o que faria um apply», e essa resposta mistura duas
coisas de origens diferentes: as edições que alguém fez ao FICHEIRO e as que
alguém fez ao NÓ. O próprio `--help` do `plan` já admitia o limite — «with the
manifest unchanged, whatever it prints IS drift» — e é essa condição que o torna
inutilizável como portão: num PR o ficheiro mudou sempre.

- **Compara o carimbo do último apply (`delonix.io/last-applied`) contra o
  observado.** O que diferir aconteceu fora do caminho declarativo: um
  `container update`, um `nft` à mão, alguém a resolver um incidente de
  madrugada.
- **O manifesto é OPCIONAL**, e é isso que lhe dá o caso de uso principal: num nó
  sem o repositório o comando responde na mesma. Os seis Kinds que só se
  enumeram a partir do documento (`RuntimePolicy`, `Image`, `App`,
  `NetworkPolicy`, `HTTPRoute`, `Gateway`) são **nomeados na saída** em vez de
  omitidos — uma tabela mais curta lê-se como «nó limpo».
- **Um recurso sem carimbo é um TERCEIRO ESTADO, não deriva** (`unstamped`):
  criado à mão, ou adoptado por uma versão anterior ao carimbo. Não há intenção
  registada com que comparar, e chamar-lhe deriva era inventar uma linha de base.
- **Só se comparam as chaves que o carimbo traz** — um campo presente no
  observado e ausente do carimbo é um default do motor que o apply nunca
  definiu, e reportá-lo enterrava a linha que interessa debaixo de ruído.
- `--stack <nome>`, `-o json`, e `--detailed-exitcode` (2 = há deriva), o mesmo
  contrato do `plan` e do `diff`.
- **O gate apanhou um erro real na primeira corrida.** A lista de Kinds
  dependentes-de-documento seria a sétima lista deste repositório que tem de
  concordar com outra e deixa de concordar em silêncio, por isso
  `doc_scoped_matches_the_stack_wiring` lê o `stack.rs` em vez de confiar num
  comentário — e chumbou: o `network_access_rule::actual` recebe `docs` e
  **ignora-os**, logo não pertencia à lista.

Grupo novo, **não estável** (`docs/cli-stability.md`), com página própria no site.

### `compatibility compose` + `migrate assess` — o terceiro estado precisa de um denominador (M02, #471)

O `compatibility docker` publica três estados desde que existe: servido,
recusado com razão, em falta. O `compose` não tinha o terceiro, e o próprio
módulo dizia porquê — «tem uma allowlist mas nenhuma superfície de três estados
própria para reutilizar». O estado «em falta» precisa de um DENOMINADOR.

- **A lista da especificação foi MEDIDA, não escrita de memória**: um ficheiro
  compose mínimo por chave candidata, `docker compose config` em cada um, e
  ficou só o que o cliente aceitou — **89 chaves de serviço e 8 de topo**, contra
  o docker **v29.8.1**, a 2026-09-23.
- **Hoje: 28 servidas, 12 recusadas com razão, 49 em falta, de 89.** No topo, 7
  servidas e 1 recusada, de 8.
- **A tabela DESCREVE o comportamento, e há um check que o exige.** Não confere a
  tabela contra si própria: pega em cada uma das 88 chaves que classifica,
  escreve um ficheiro que a usa, e pergunta ao `compose config` o que acontece.
  As 88 concordam.
- **`delonix migrate assess -f docker-compose.yml`** é a outra metade. O
  `compose up` pára na primeira chave que não entende — correcto para CORRER,
  inútil para DECIDIR. Lê o YAML **cru** (o parser tipado recusaria no primeiro
  desconhecido, que é precisamente o conjunto a enumerar), não cria nem puxa
  nada, e conta USOS de chave e não chaves distintas: três serviços com
  `devices:` são três decisões. `-o json` e `--detailed-exitcode`.
- **Dois achados enquanto se construía**: a tabela que eu ia escrever JÁ EXISTIA
  (`ENGINE_HAS_IT_SERVICE`, a que já produz a recusa que nomeia a flag) — a minha
  cópia classificava como `missing` o que o motor recusa com mensagem própria; e
  estava INCOMPLETA: `sysctls`, `ulimits`, `gpus` e `userns_mode` têm flag no
  `container run` e caíam no erro genérico «not understood», que manda o leitor
  embora em vez de lhe dizer para onde ir. O teste que verificava duas linhas à
  mão passou a percorrer as doze.
- **`cri` e `oci` continuam de fora, por razões diferentes**: o número do CRI vive
  num documento mantido à mão e nada no binário o consegue derivar (seria uma
  segunda cópia de um número sem forma de dar pelo seu envelhecimento); o OCI não
  tem trabalho de conformidade neste repositório, logo a contagem honesta é zero.

### O portão de regressão de performance, e a terceira saída (M11, #469)

O `scripts/bench.sh` já media os três motores na mesma máquina e já se recusava a
correr numa bancada carregada. Faltava a outra metade: nada comparava o número de
hoje com um gravado.

- **`scripts/bench_gate.py` LÊ uma medição e nunca a faz** — duas opiniões sobre o
  mesmo número é como elas começam a divergir.
- **Três saídas, não duas**: `0` dentro da tolerância (25%), `1` regressão, e
  **`3` recusa-se a julgar**. A terceira é a que o torna confiável, e vem do
  incidente de 2026-08-10: docker 1406, podman 1351, delonix 640, e a corrida
  seguinte 208/268/89. Um portão ingénuo teria lido isso como «o delonix regrediu
  7×» por um problema da máquina. O docker e o podman são **âncoras**: se
  degradaram tanto como nós, o que mudou foi a bancada — e sem âncora nenhuma
  também recusa, em vez de adivinhar.
- **Baseline por MÁQUINA.** Uma mediana medida num desktop de 32 threads não diz
  nada sobre uma VM de 2 vCPU, e comparar as duas fabrica exactamente o ruído que
  o gate existe para filtrar.
- **Dispersão, porque o limiar de load não chega**: medido a load 6.44 (abaixo do
  limite), uma linha de dez amostras do docker foi de 434 ms a 5 407 ms e a
  mediana absorveu-o. Acima de **3×** o gate não julga; **gravar é mais estrito,
  1,5×** — uma corrida de juízo deita-se fora, uma baseline é reusada por todas
  as que vierem a seguir.
- **Três defeitos do harness corrigidos de caminho**: `--json` era aceite e
  IGNORADO (a variável era atribuída e nunca lida); `time_n` não olhava para o
  exit status, por isso um `docker` com o daemon parado falhava em milissegundos
  e entrava na tabela como o mais rápido dos três; e não havia medida de
  dispersão nenhuma.
- **Um gate morto**: nenhum `scripts/test_*.py` corria em CI — o
  `test_release_verify.py` nasceu no dia anterior com dez testes que nada
  executava. Job `script-tests` novo, que falha se não encontrar nenhum.

Baseline gravada na máquina de desenvolvimento (2026-09-23, `ceec7f8c`): docker
**224 ms**, podman **279 ms**, delonix **87 ms**.

### `kind: RuntimePolicy` — o tecto de admissão em forma declarativa (M04, #462)

- O `policy.json` (o tecto que o nó impõe no ponto de admissão) ganha Kind
  próprio. `stack apply` **sobe ou aperta**; nunca o baixa — `teardown: false`, e
  `stack destroy`/`--prune` recusam-no explicitamente.
- **`delonix policy unset` é o único verbo que o remove**: pede confirmação (ou
  `--force`) e imprime sempre o que estava lá antes de apagar.
- Kinds no manifesto: 19 → 20.
- Validado ao vivo com `DELONIX_ROOT` isolado: um `container run --privileged`
  REALMENTE recusado por uma política aplicada por manifesto; reaplicar com um
  campo mudado converge in-place (`stack plan` mostra `~`, não `-/+`); dois
  documentos do Kind no mesmo manifesto são recusados antes de qualquer escrita.

### Build: cache mounts e SBOM por imagem (M03, #468 e #466)

- **`RUN --mount=type=cache,target=<path>[,id=<name>]`** — directório persistente
  e de leitura-escrita que sobrevive entre builds, generalizando o mecanismo que
  o `type=secret` já usava (`mount_live`, invisível à imagem final). O directório
  é `<root>/build-cache/mounts/<sha256(id)>` — **em hash, nunca a string crua**:
  um `id` omitido é por omissão o `target`, um caminho arbitrário controlado pelo
  Dockerfile. `sharing=` e `ro` são **recusados** em vez de aceites-e-ignorados.
  Validado ao vivo: dois `delonix build` separados escreveram no MESMO cache mount
  e o segundo viu o que o primeiro escreveu; o conteúdo nunca apareceu na imagem
  final (confirmado por `container run` dentro dela).
- **SBOM SPDX 2.3 automático por imagem construída** — `delonix build` escreve-o
  best-effort logo a seguir a um commit com sucesso, e `delonix image sbom <ref>`
  serve-o. Reutiliza o `extract_sbom` já existente (apk/dpkg, sem montar nem
  correr a imagem). Validado ao vivo: um build de `alpine:3.20` + `apk add curl`
  produziu um SBOM com 25 pacotes, e o `image sbom` devolveu o ficheiro
  byte-a-byte idêntico — prova do cache hit, não de recomputação.
- **Dívida LANG-01 corrigida de caminho**: as mensagens de erro do parser de
  `--mount` estavam em português desde sempre e nunca tinham sido revistas.

### `POST /images/create` na Docker API (M02, #460)

- O `docker pull` através do socket passa a funcionar: streaming chunked-JSON,
  delegando no pull que já existe. Movido de `API_UNIMPLEMENTED` para a matriz.
- Validado com um `docker` CLI real (29.8.1) — duas imagens, incluindo uma de 8
  layers / 26 MB.

### Fuzz em CI, e o que encontrou na primeira corrida (M04, #461 e #463)

- **Job de fuzz novo** (60 s por alvo), sobre os parsers escritos à mão que comem
  input não confiado.
- **Na primeira corrida real encontrou um out-of-memory** no `parse_dockerfile`:
  uma cadeia de `ARG B=${A}${A}` dobra o valor guardado a cada linha — uma bomba
  de expansão exponencial, atingível por um `Dockerfile` de um repositório
  clonado, **antes de qualquer container existir para o isolar**. Corrigido com
  um tecto de 1 MiB em `substitute_vars`, partilhado pelos dois chamadores
  (`ARG` no parse, `ENV` no `cmd::build`). Confirmado com o próprio fuzzer: 2,8 M
  execuções em 45 s sem crash, depois da correcção.

### `scripts/release_verify.py` (M13, #464)

- A tabela de `system features` deixa de ser prosa não verificada. O script valida
  os **dois lados** — aceita quando a evidência é real e recusa quando não é,
  sabotado de propósito com uma referência inexistente e revertido a seguir,
  contra o binário real. `--report` gera o conformance report. Job `release-verify`
  novo no CI.

### BREAKING para quem publica: empurrar uma tag já não publica nada (#472)

O `release.yml` disparava em `push: tags: ["v*"]`, ou seja a release construía-se
no instante em que a tag chegava ao remoto — incluindo uma tag empurrada por
acidente (`git push --tags`, `push.followTags=true`), e sem um passo entre «a tag
existe» e «o mundo tem um binário». **A tag e a release passam a ser duas
decisões:**

```bash
git tag -a v4.5.0 -m "v4.5.0" && git push origin v4.5.0   # corta a versão
gh workflow run release.yml -f tag=v4.5.0                 # publica-a
```

- **A tag é DADO, não a ref**: todos os jobs lêem a tag do input e fazem checkout
  dela explicitamente, em vez do `GITHUB_REF_NAME` — que num dispatch é a ref que
  o operador calhou ter seleccionado, e uma release que constrói um commit e lhe
  chama outro é o pior tipo de errado.
- **Job `guard` novo**, primeiro e a custar segundos, porque o input passou a ser
  confiado por tudo a jusante (escolhe o commit a construir, dá o nome à release,
  e é comparado com a versão do próprio binário). Recusa uma tag sem a forma
  `vX.Y.Z`, uma que não exista no remoto, e uma que **já tenha release**
  (republicar por cima é como os assets mudam debaixo de quem já os descarregou).
  Sem a primeira guarda, um `tag: main` teria feito checkout de um ramo e
  publicado uma release chamada «main».
- **O custo, dito porque é o custo**: uma tag sem release passa a ser um estado
  intermédio normal. O `scripts/version_gate.py` não é afectado — sempre olhou
  para a TAG, nunca para a release.

### Correcções de baseline da matriz das 13 melhorias

Três entradas da matriz diziam que algo faltava quando já existia — a mesma classe
de deriva que uma tabela de achados não actualizada produz, e que este repositório
já pagou antes:

- **M12** (#467): o `system doctor [--strict]` (8 pontos de verificação) e o
  dicionário de error IDs `DX-CDNN` (ADR-0043, 198 códigos, `delonix explain`) já
  existiam, e a matriz dizia «0 ocorrências»/«sem error IDs».
- **M01** (#465): o gate de dependências proibidas existe desde 2026-09-16.
- **M04/M05**: o `SECURITY.md` existe desde 2026-07-27 (a linha estava errada
  desde o dia em que foi escrita, não é regressão), e o `delonix diff <kind>
  <name>` já era verbo de topo próprio com as três faces.

### O que NÃO foi validado

- **`bench_gate.py` em CI**: o job `perf` fica `skipped` num runner alojado — o
  GitHub bloqueia user namespaces não privilegiados, logo o motor rootless não
  corre lá (o mesmo que já mantém o arnês de caos de fora). Os seis desfechos do
  gate foram exercitados ao vivo na máquina de desenvolvimento, quatro deles
  contra a medição REAL e não contra números inventados.
- **`migrate assess`**: exercitado contra ficheiros compose escritos para o
  efeito e contra o `compose config` real; **não** contra uma migração verdadeira
  de um projecto de terceiros de ponta a ponta.
- **`drift`**: a deriva foi provocada por `container update` num root isolado. Os
  outros vectores (um `nft` à mão, um `vm` alterado pelo backend) não foram
  exercitados.
- **`RuntimePolicy`, cache mounts, SBOM e `/images/create`** foram validados ao
  vivo contra o binário de release, cada um com o detalhe registado no
  `AGENTS.md`; o que continua por medir está lá nomeado e não é repetido aqui.
- **O que a `v4.3.0` já listava** continua por validar: a escrita no `/etc/hosts`
  real, uma instalação completa em aarch64, e o que o ADR-0046 marca como não
  medido.

---

## v4.3.0 — `delonix hosts sync`, e o instalador passa a aceitar aarch64

Quatro commits desde a `v4.2.0`. Numerada como MINOR: há um grupo de comandos
novo (`delonix hosts`, marcado **não estável**). Uma release pequena, de duas
mudanças que o utilizador vê e duas de documentação; cada secção diz o que foi
provado e o que não foi, e o que não foi validado está listado no fim.

### `delonix hosts sync` — os nomes de serviço no `/etc/hosts` (#449, ADR-0048, fase 2)

- **`delonix hosts sync`** publica o nome padrão de cada container exposto
  (`<nome>.<ns>.svc.delonix.internal`) num bloco delimitado do `/etc/hosts` —
  o mesmo bloco, por raiz de estado, do ADR-0046, por isso um nome pedido por
  uma rota com `hosts: [host]` e um publicado aqui partilham **um** bloco e uma
  regra de recusa. Todos os nomes apontam para `127.0.0.1`: é a porta publicada
  do proxy L7 que o host usa para chegar a uma carga na SDN.
- **`--print`** mostra o bloco e não toca em nada; **`--off`** remove-o e deixa
  de o manter.
- **Depois de ligado**, `container run --expose` e `rm` mantêm o bloco atual, em
  melhor esforço: um nome que uma rota *pediu* falha o `apply`, mas um nome
  publicado por este comando é uma conveniência que o operador ligou, e um
  `run --expose` sem root **não** falha por o ficheiro precisar de root — só
  avisa.
- **Sem root recusa e imprime o bloco** a acrescentar à mão. Um nome que já tem
  entrada fora do bloco é recusado (nunca é sobreposto), e uma recusa nunca deixa
  o estado a meio: o ficheiro fica intacto e o interruptor desligado.
- **Sob `sudo` lê o registo do utilizador que o invocou**, e não o do root. Sem
  isto, `sudo delonix hosts sync` (o passo natural quando o comando recusa sem
  root) leria `/var/lib/delonix`, que está vazio, e publicaria «0 nomes». É a
  mesma correção que o `vm bridge` já tinha (`adopt_invoking_user_root`); um
  `DELONIX_ROOT` explícito continua a ganhar.
- **Grupo novo, não estável** (`docs/cli-stability.md`): documentado na tabela do
  `README.rst` e no site, com página própria (`docs/comandos/hosts.html`).

### Instalador: aceita aarch64 (#447)

- O `install.sh` aceita `aarch64` (e `arm64`, normalizado) em vez de recusar tudo
  o que não é x86_64. O nome do asset compõe-se de `uname -m`:
  `delonix[-cri|-mcp|-mgmt]-aarch64-linux` — os que o `release.yml` publica
  desde a `v4.2.0`. Outras arquiteturas continuam a recusar, dizendo quais são
  suportadas.
- **Em aarch64 não existe a variante `-v3`** (é um nível de microarquitetura do
  x86-64), e a sonda de QEMU pede `qemu-system-aarch64` — antes pedia o
  emulador x86 e instalava-o num host ARM.
- **Fora de x86_64 não se descarregam** o Cloud Hypervisor estático, o EDK2
  `CLOUDHV.fd` nem o `hypervisor-fw` (são todos builds x86-64, e os checksums
  fixados também); o instalador di-lo, e o backend de VM é o libvirt. Não foram
  inventados pins para arm64.
- **Nota importante:** o `install.sh` que a `v4.2.0` publicou é o antigo, que
  ainda recusa aarch64. **Este é o primeiro release cujo `install.sh` o aceita.**

### Documentação

- **Manual do contribuidor** (`docs/dev/`) atualizado para a `v4.2.0` (#453):
  `vm build`/`vm.yaml`, o `verify-images.sh`, os binários aarch64, o
  `--performance` e o nome padrão de serviço.
- **Notas da `v4.2.0` corrigidas** (#454): afirmavam que uma VM libvirt era
  recusada e que `kind: IPPool` era uma fase posterior. Ambas estão construídas
  (fases 2 e 3 do ADR-0046); só o `announce: l2` e o `hosts: guest` (fase 4)
  faltam. O erro foi copiar o corpo de um PR desatualizado.

### O que NÃO foi validado

- **`delonix hosts sync`:** a escrita no `/etc/hosts` **real** (exige root; foi
  provada só num ficheiro de teste, verificando o conteúdo por `md5`: escrita,
  idempotência, recusa por conflito, recusa sem permissão e `--off` a restaurar o
  ficheiro byte a byte) e um cliente a resolver um nome do bloco e a alcançar o
  backend. A **atualização automática** só funciona quando o ficheiro é gravável
  pelo processo: para um utilizador rootless típico, um `container run --expose`
  só avisa, e o `hosts sync` é, na prática, um comando manual. A fase 3 do
  ADR-0048 (credenciais) não está nesta release.
- **Instalador em aarch64:** uma instalação completa num host aarch64 real nunca
  correu (não se instalou um binário ARM em cima de uma máquina x86), nem os
  nomes dos pacotes de QEMU por distro em arm64, nem os caminhos `dnf`/`apk`.
  O que foi provado: a composição dos nomes de asset — a função `fetch_asset`
  real do script, contra os assets publicados da `v4.2.0`, com controlos
  negativos.
- **O que a `v4.2.0` já listava** continua por validar: a instalação de pacotes
  e os perfis dourados do `vm build`, o arranque das imagens construídas, os
  construtores de appliance reais, e o que o ADR-0046 marca como não medido.

---

## v4.2.0 — `vm build` com `vm.yaml`, receitas por distro e por appliance, serviços de VM por nome

Dezassete commits desde a `v4.1.1`. Numerada como MINOR: há superfície nova de
CLI (`vm build`, o ficheiro `vm.yaml`), um Kind novo (`IPPool`) e novos campos
de manifesto. A série vem
de várias frentes independentes, e cada secção diz o que foi provado e o que
não foi — o que não foi validado está listado no fim, junto.

### `vm build` e o `vm.yaml` — a experiência do `docker build` para VMs (#451)

- **`delonix vm build` existe agora.** Até aqui só havia `delonix image vm
  build`; os dois partilham o mesmo conjunto de argumentos e fazem o mesmo.
- **`vm build .` escolhe a receita como o `docker build`:** um `-f` explícito
  decide (um `.yaml`/`.yml` lê-se como `vm.yaml`, o resto como `VMfile`); sem
  `-f`, um `vm.yaml` na pasta vence um `VMfile`, que vence a receita dourada.
  `-t` dá o nome do resultado **e** o `${TAG}` dentro do ficheiro; `--target`
  escolhe uma imagem quando o ficheiro declara várias. Com um `vm.yaml`, o `-t`
  passa a ser opcional (usa o `tag:` da imagem).
- **`vm.yaml` é o «compose» do `VMfile`** (o «Dockerfile»). Esquema estrito — uma
  chave desconhecida é erro, e um campo que a rota escolhida não cumpre é
  recusado pelo nome, nunca ignorado. Descreve o qcow2 completo: base, tamanho,
  hostname, vcpus/memória, pacotes, utilizadores (sudo, grupos, shell, chaves),
  serviços, ficheiros (com o caminho de destino completo e o modo), `env`,
  cloud-init, `run:`, **o que remover** (`remove.packages/paths/users/services`,
  aplicado depois de tudo o resto, para também podar o que um pacote trouxe) e a
  limpeza (`cache de pacotes, logs, histórico, /tmp, machine-id`). `${TAG}` e
  `${VAR:-valor}` expandem-se nos valores, não nos comentários.
- **Sem segundo motor de build.** Cada imagem compila para um construtor que já
  existia: um `VMfile` sintetizado, a receita dourada (`profile: rootless|k8s`),
  um `VMfile` existente (`build.file`) ou um construtor de appliance.
- **Rota `appliance:`** para o que não se descreve como edições a uma cloud image
  (Proxmox a partir da ISO, OpenStack a puxar ~20 GiB de contentores). O
  `vm.yaml` **nomeia um construtor** (`builder: proxmox`), nunca um caminho: o
  motor resolve-o para `scripts/appliances/build-<nome>.sh` dentro do
  repositório, valida os argumentos e as variáveis de ambiente (sem `PATH`,
  `LD_*`, `BASH_ENV`…), corre-o com um `OUT_DIR` isolado ao lado do store,
  importa o único qcow2 que ele deixa e apaga a pasta de trabalho, mesmo em
  falha. Um `vm.yaml` alheio não consegue fazer o host correr um ficheiro à sua
  escolha.
- **`images/`**: uma pasta por imagem, cada uma com `vm.yaml` e README de ponta a
  ponta. Quatro distros que constroem **offline** (`ubuntu`, `debian`, `rocky`,
  `fedora`) e oito appliances (`opnsense`, `proxmox`, `truenas`, `openstack`,
  `monitoring`, `carbonio`, `glpi`, `wazuh`). Os scripts continuam em
  `scripts/appliances/`, de onde o workflow que publica as imagens os lê. As
  receitas mantêm `/usr/share/doc` de propósito: tem as licenças que uma imagem
  publicada tem de redistribuir.
- **`scripts/verify-images.sh`** constrói as receitas num `DELONIX_ROOT` isolado e
  **lê o conteúdo** do qcow2 (ou da VM) contra o que a receita declarou. Tem uma
  receita-sonda com valores que a base não pode ter (a base já traz uma conta
  `delonix` com sudo, por isso verificar isso na receita entregue não provava
  nada) e um `--self-test` que exige que as verificações **falhem** numa imagem
  que ninguém construiu.
- **Um teste fecha o ciclo**: `every_shipped_recipe_is_valid_and_complete` falha
  se uma receita entregue deixar de ser válida ou apontar para um ficheiro ou
  construtor que não existe.
- Mensagens de erro do `vm.yaml` e dos appliances passam pelo catálogo (`pt.po`).

### Serviços de VM por nome (#444, #448, ADR-0046/0047/0048)

- **`VirtualMachine.spec.expose`** publica um serviço HTTP/S de uma VM por nome:
  baixa no `load` para um `HTTPRoute/<vm>-expose`. O resolvedor de rotas conhece
  VMs **Cloud Hypervisor** (na SDN) e VMs **libvirt** em `nat`/`bridge` (ADR-0046,
  fase 2): estas chegam-se por uma segunda instância do proxy no netns do host,
  sem root, que escuta só em `127.0.0.1`. Uma VM libvirt em modo *user-mode* não
  tem endereço alcançável e é recusada com a correção (`netMode: nat` em
  `qemu:///system`); um documento que mistura backends das duas metades é
  recusado, com a indicação de qual cai em qual.
- **`kind: IPPool`** (grupo `networking`; ADR-0046, fase 3): um livro de reservas
  de endereços **do host** (um endereço, um intervalo ou um CIDR; só IPv4) que as
  rotas reclamam com `spec.pool` (e `expose[].pool`). Uma rota reclama um só
  endereço enquanto estiver declarada, fica alcançável nele em vez de em
  `127.0.0.1`, e o `hosts: [host]` aponta o nome para ele. É um Kind completo
  (`apply`, `plan`, deriva, `--prune`, `get`/`describe`/`delete ippools`) e aplica-se
  antes do `HTTPRoute`. **Só `announce: local`**: o endereço já tem de estar numa
  interface do host, e o `apply` verifica-o ligando-o; `announce: l2`,
  `interface`, IPv6 e BGP são recusados (fase 4, por fazer). Um pool com leases
  não se apaga nem encolhe por baixo de um lease, e a libertação é por
  declaração — não há um ceifador a adivinhar quem está vivo.
- **`hosts: [nome]`** num `HTTPRoute` (e em `expose[].hosts`) escreve cada host
  como `127.0.0.1 <host>` num bloco delimitado do `/etc/hosts` do operador
  (`cmd/hosts_file.rs`), lendo os nomes das duas instâncias do proxy. Só o bloco
  é tocado; um nome já presente fora dele é recusado, e um bloco sem linha `END`
  é recusado em vez de apagar o resto do ficheiro. Sem root, recusa e mostra o
  bloco a acrescentar.
- **Deriva corrigida**: uma rota para uma VM lia-se como deriva permanente,
  porque o `actual()` só mapeava endereços para nomes de containers.
- **ADR-0048, fase 1 (#448)**: o nome padrão de serviço `<nome>.<ns>.svc.delonix.
  internal` (o FQDN antigo continua como alias) e a coluna `SVC` em
  `container/vm/stack ls`. O ADR-0047 (o proxy L7 autorizar origens de
  containers, para `hosts: containers`) fica **só como proposta**, e o
  `hosts: guest` e o `announce: l2` do ADR-0046 (fase 4) estão por fazer.

### Appliances de monitorização e correio (#450)

Só scripts em `scripts/appliances/` (sem código do motor): imagens de
monitorização (`monitoring-7.0-r5`: Zabbix 7.0 LTS + Grafana + Prometheus + Loki
+ NetFlow, já ligados, com um hub WireGuard `monitoring-vpn`), Carbonio CE
(`carbonio-26.6.0-r3`), GLPI 11 (`glpi-11.0.9-r1`) e Wazuh 4.14.7. Cada uma com o
seu `verify-*.sh`.

### Instalador: `--performance` (#440)

`install.sh --performance` / `--no-performance`; sem elas pergunta cada ponto
(Enter = sim; sem terminal, não). CPU em modo performance (perfil de energia,
governor, EPP) por um serviço que guarda o estado do arranque e o repõe no
`stop`; THP em `madvise` com `irqbalance`; e um timer de utilizador
`system prune --auto --threshold 75` (nunca toca em volumes). Avisa se houver um
`delonix-cri`/`-mcp`/`-mgmt` diferente noutro ponto do `PATH`.

### `--version` nos servidores irmãos (#442, #443, #445)

`delonix-cri`, `delonix-mcp` e `delonix-mgmt` respondem a `--version`/`-V` e
saem, em vez de arrancarem o servidor.

### Binários aarch64 (#439, #446)

- Um job de CI **nativo** em aarch64 (`test (arm64)`) faz build e corre a suite
  completa; antes, os ramos `#[cfg(target_arch)]` nunca eram executados.
- O workflow de release passa a construir e publicar `delonix`, `delonix-cri`,
  `delonix-mcp` e `delonix-mgmt` para aarch64, com o mesmo `SHA256SUMS`,
  assinatura e proveniência. **Este é o primeiro release que os produz** — ver
  a lista abaixo. O `install.sh` **continua a recusar aarch64** (o passo
  seguinte não está nesta release).

### O que NÃO foi validado

- **`vm build`:** a instalação de pacotes (`network: true`), os perfis
  `rootless`/`k8s` de ponta a ponta e o arranque de uma VM a partir das imagens
  construídas nunca correram; nenhum construtor de appliance real foi executado
  (a canalização foi provada com um construtor falso). As fases `--packages`,
  `--profile`, `--boot` e `--appliance` do `verify-images.sh` estão escritas mas
  nunca executaram. O que foi provado com imagens reais: as quatro distros
  offline, os dois ramos (`dpkg` e `rpm`) e o auto-teste.
- **Serviços de VM** (o que o próprio ADR-0046 marca como não medido):
  - um cliente a resolver um nome do bloco do `/etc/hosts` real e a receber
    resposta de um backend (escrever o ficheiro real exige root); o açúcar
    `expose:` foi coberto por `validate`, `--dry-run` e testes unitários, não
    por tráfego;
  - as duas instâncias do proxy a correr ao mesmo tempo (uma rota de container e
    uma de VM libvirt no mesmo `apply`), uma VM libvirt em modo `bridge`, e o
    reinício do proxy depois de a VM ganhar outro endereço (a rota guarda o
    endereço no `apply`);
  - `IPPool`: um endereço realmente encaminhável (tudo foi medido com aliases de
    loopback, `127.0.0.10-11`), dois reclamantes a esgotar um pool por duas
    rotas vivas, a instância do host com um endereço do pool, `expose[].pool`
    por tráfego, e o `validate` a recusar `announce: l2` (só o `apply` recusa).
- **Appliances #450:** o cliente web do Carbonio num browser e a entrega de
  correio de/para o exterior; uma falha de arranque do `carbonio-videoserver`
  na primeira passagem, não investigada.
- **Instalador `--performance`:** o instalador de ponta a ponta, o serviço systemd
  real, o timer de GC e o ganho medido.
- **Binários aarch64 da release:** o job `build-arm64` do `release.yml` só corre
  com uma tag `v*`, por isso nunca tinha sido executado antes desta. O
  `strip` em aarch64, a assinatura e a proveniência dos novos assets são a
  primeira execução.

---

## v4.1.1 — endurecimento de segurança, `vm destroy`, e o CRI de um cluster deixa de vir do cwd

Cinquenta e dois commits desde a `v4.1.0`, numerados como PATCH por decisão do
dono. Nota de honestidade: a série inclui superfície nova (`vm destroy`,
`--rm`/`--rm-after`, `--cri-bin` no `cluster`), que pela convenção do repo seria
MINOR — fica dito aqui para ninguém ler «patch» como «só correcções».

### Segurança

- **`vm migrate`**: injecção de shell e colisão do isolamento por namespace
  corrigidas (CRÍTICOS, `6bedce83`).
- **`kubeadm join`**: os valores lidos do output remoto são validados antes de
  chegarem a um shell, e os manifestos OCI passam a ter tecto de tamanho.
- **`metadata.name` de um cluster** e o nome de um cluster kind são validados
  antes de compor caminhos (um nome com `..` escrevia a CA do etcd fora da raiz de
  estado); os registos `Service`/`NetworkRoute` passam a ser injectivos.
- **`ExecSync` do CRI** e o `fetch-media.sh` ganham tecto de tamanho; cinco
  endurecimentos BAIXOS (cache VM, `COPY --from`, socket de controlo, retoma de
  download, Proxmox).

### Cluster: a origem do runtime é a que o operador escolheu (#242, #243)

- **#242** — o `prepare_host` comparava só «o serviço está vivo?», por isso o
  `delonix-cri` da golden ganhava sempre e uma correcção nunca chegava a um nó
  existente. Compara agora o `sha256`: igual e vivo → nada; igual e parado →
  arranca; diferente → substitui, faz `restart` e diz-o.
- **#243** — `resolve_cri_bin` já não compila o código-fonte que estiver perto do
  cwd. Só com `DELONIX_CRI_FROM_SOURCE=1`; por omissão usa o `delonix-cri` ao lado
  do `delonix` e depois o asset verificado da release. A proveniência (origem,
  caminho, `sha256`) é sempre impressa. `cluster apply` e `cluster kubeadm`
  ganham `--cri-bin`.

### VMs

- **`vm destroy`** — teardown completo entre providers; sai com a classe da falha e
  remove o ficheiro de lock do store; `vm prune` deixa de engolir falhas.
- **`vm create --rm` / `--rm-after`** — VMs descartáveis; o temporizador leva o
  state root, é desarmado pelo `vm destroy` e não é rearmado pelo `vm ls`. Ctrl+D
  na consola.
- O seed de cloud-init do libvirt vai num disco virtio e não num cdrom SATA.

### Stack e Kinds

- Os grupos de um `kind: Stack` saem da tabela de Kinds (ADR-0045); os grupos de
  Kinds removidos (`storage`/`shareVolumes`/`egress`) saem.
- `metadata.labels` chega ao container, e por isso um `Service` passa a casar.
- Fechar uma rota cujo elemento nft já não existe é sucesso, não erro.
- Exemplos completos por Kind (`full-<kind>.yaml`) e o site regenerado.

### Não validado

- A substituição do CRI em `prepare_host` **não foi corrida contra um nó real por
  SSH** — só o comando remoto de comparação em local e os testes de decisão.
- O `vm destroy`/`--rm-after` foi validado por testes e por corridas isoladas; não
  numa bateria completa contra Proxmox/OpenStack reais.
- Ver o relatório desta release para o resultado do `scripts/e2e.sh`.

---

## v4.1.0 — o ADR-0043 fecha nos últimos cinco crates, e um nó root novo volta a aceitar `-m`/`--cpus`

Onze commits desde a `v4.0.0`. O gatilho foi uma correcção de produção (#415, PR
externo, revisto e fundido nesta sessão): um nó root FRESCO recusava `-m`/`--cpus`/
`--cpu-weight` mesmo quando o kernel delegava os três controladores — e, no caso em
que a delegação era mesmo real mas parcial, o operador via "Permission denied" onde
a resposta certa era "o `cpu` não está delegado, corre isto". Ao lado, os cinco
crates que a `v4.0.0` tinha deixado por fazer ("Conhecido, não corrigido nesta
série") ganharam o dicionário de códigos `DX-CDNN` do ADR-0043 — que passa a
**Accepted**. Nenhuma mudança de superfície de CLI (comandos, flags, Kinds):
esta série é sobre a QUALIDADE do que o motor já fazia.

### `-m`/`--cpus`/`--cpu-weight` num nó root: a recusa perguntava a coisa errada (#415)

`cgroup_limits_apply()`, em modo root, só perguntava "consigo criar um cgroup sob
`delonix.slice`?" — necessário, mas não suficiente: a resposta certa é "os
controladores foram mesmo ENTREGUES aos filhos da slice?". `ensure_delonix_slice`
descartava cada escrita de `cgroup.subtree_control` com `let _ =`, por isso uma
slice que não recebesse `cpu` do `/sys/fs/cgroup` ficava sem `cpu.max` — e a
mensagem de erro dizia **"Permission denied"**, porque `std::fs::write` abre com
`O_CREAT` e o cgroupfs recusa criar um ficheiro regular com `EACCES`: a mesma
directoria, o mesmo uid, `memory.max` aceite uma linha antes.

Duas correcções, as duas medidas e não só lidas:

- `cgroup_limits_apply()` em root pergunta as DUAS coisas agora, como a perna
  rootless já fazia — criar o cgroup, **e** confirmar que `memory`/`cpu`/`pids`
  estão em `cgroup.subtree_control`.
- Um `cpu.max` (ou `memory.max`/`pids.max`) AUSENTE deixa de reportar o errno do
  kernel e passa a nomear o controlador em falta e o comando que o repara. Um
  ficheiro que EXISTE mas recusa a escrita mantém o errno — aí é a resposta certa.
  A reparação em si é bounded: só activa um controlador que o nível acima já
  oferece, nunca move um processo nem aperta um limite alheio.

Quatro testes cobrem o par (a leitura da delegação, a mensagem, e as duas
distinções — ficheiro ausente vs. ficheiro que recusa). Não validado ao vivo nesta
sessão contra um nó root real (o achado original foi reproduzido numa VM aarch64
pelo autor do PR, 2026-09-18); validado nesta sessão com o build/clippy/testes
completos do crate e com o merge contra a `main` actual (que entretanto já tinha
renomeado `Error::Runtime` para `Error::Syscall`, ADR-0043 — o merge de 3 vias
resolveu sozinho, sem conflito).

### ADR-0043 fecha — os últimos cinco crates ganham códigos numerados

A `v4.0.0` tinha deixado escrito: "`delonix-truenas`, `delonix-proxmox` e o
`delonix-vm` continuam com o `Error` genérico e partilhado (...) não entram nesta
release, entram na próxima assim que fundidos." Entraram, e o `delonix-linux` e o
`delonix-sdn` (que nem estavam nessa lista, por serem os dois maiores) foram atrás:

- **`delonix-vm::cloudinit`** — o gerador de seed NoCloud, 5 falhas (nome de VM
  inválido, cópia de `--user-data`, `cloud-localds` em falta ou que falha).
- **`delonix-truenas`** — as 20 recusas do provider de armazenamento (ADR-0009): 15
  variantes, incluindo job assíncrono falhado/desaparecido/esgotado e quota abaixo
  do mínimo.
- **`delonix-proxmox`** — as 19 recusas do provider de VM (ADR-0008), o primeiro a
  povoar o domínio `Vm` do dicionário.
- **`delonix-linux`** — ~65 sítios (`cdi.rs`, `run_host.rs`, `supervise.rs`,
  `workload.rs`, `lib.rs`: `spawn`/`exec`/`mount_live`/cgroups). Os ~30 que
  construíam `Error::Runtime{context,message}` (clone, mount, setns, escritas de
  `/proc`/cgroup, `waitpid`, `busctl`) colapsam num único `Error::Syscall` — zero
  mudança de texto ou de classe, só o número por trás. `Domain::Host` (vazio até
  aqui) ganha as falhas que são mesmo sobre o HOST: o próprio system call, o
  AppArmor, a ausência de um spec CDI.
- **`delonix-sdn`** — ~180 construções (a SDN rootless, o firewall de ingress, o
  CNI, o IPAM, o overlay WireGuard), o maior dos cinco: 50 variantes próprias + um
  bucket `Command{context,message}` para "uma ferramenta do host falhou", o
  primeiro crate a povoar o domínio de rede do dicionário.

O dicionário publicado em `docs/codigos.html` passa de **61 para 198** códigos
`DX-CDNN` catalogados. `delonix explain <código>` continua a ler a mesma fonte que
a linha de erro e o `-o json` — nenhum destes cinco crates inventa um dicionário
à parte. O ADR-0043 já estava `Accepted` desde 2026-09-17; esta série fecha o
rollout "landed crate by crate" que o próprio ADR previa — os últimos cinco dos
crates listados no plano ganham o dicionário nesta release.

### ADR-0044 (P4) — os cinco spikes exigidos estão feitos; a arquitectura continua `Proposed`

Sem mudança de comportamento observável nesta série. Três achados de investigação
que valem registar, e um pedaço de código que FICA (mas ainda não tem consumidor):

- **`StateRepository<T>`** — um port novo em `delonix-model` (a fundação; o trait
  não nomeia nada de `delonix-compute`), implementado para `Store`/`JsonStore<T>`
  em `delonix-state`. `delonix-linux`'s `wait_and_record`/`stop`/`persist_stop`/
  `remove` passam a receber `&impl StateRepository<Container>` em vez do `Store`
  concreto — validado ao vivo que o registo em disco fica byte-a-byte igual (um
  ciclo `run`/`stop`/`rm` isolado, antes e depois). `spawn`/`create_with` continuam
  a abrir o `SecretStore` directamente — ficam por fazer, para uma porta própria.
- **Proxmox: `--namespace` já era recusado** (o guarda vive uma camada acima do
  provider, desde antes do próprio ADR-0044 ter sido escrito — o achado estático
  original tinha procurado no crate errado); **`--network` é aceite e IGNORADO**,
  confirmado pelo formulário HTTP real enviado ao nó Proxmox (nunca a rede pedida).
  Fica registado como decisão pequena (recusar, não construir uma ponte remota),
  não como trabalho novo.
- **`VmSpec`/`Extensions`/`VmProvider`** (`delonix-vm::provider_spike`) — um spike
  que convirgiu Cloud Hypervisor e libvirt sob o MESMO contrato, provado ao vivo
  nos dois backends. Fica no repositório (zero risco à superfície pública — não é
  chamado por nenhum caminho da CLI), como base para a fase seguinte do ADR; não é
  uma capacidade nova que um utilizador possa invocar hoje.
- O par de binários `delonix-launcher`/`delonix-netns-holder` foi validado num
  spike isolado (hardlinks, perfil AppArmor por caminho) — nenhum dos dois existe
  como binário publicado nesta release.

A decisão de aceitar o ADR-0044 continua do dono do repositório; esta série só
fecha os cinco spikes que o próprio ADR exigia antes disso.

### Conhecido, não corrigido nesta série

- O achado do #415 foi medido numa VM aarch64 por quem abriu o PR; esta sessão
  validou a lógica (build/clippy/testes/gates) e o merge contra a `main` actual,
  mas não repetiu a medição num nó root x86_64 real.
- `image vm ls-remote` sem argumento continua sem `--timeout` numa ligação lenta
  (herdado, não tocado nesta série).

### Não validado nesta release

O caminho `--network`/`--namespace` do backend Proxmox contra uma bridge SDN real
(a topologia do host de teste já respondia à pergunta sem precisar de um convidado
a sério — ver `docs/discovery/58_P4_D1_D8_PROXMOX_NETWORK_NAMESPACE_LIVE.md`); e o
par `delonix-launcher`/`delonix-netns-holder` contra uma carga de produção (só o
spike isolado foi corrido).

---

## v4.0.0 — o motor ganha camadas e ports (ADR-0040), os erros ganham número (ADR-0043), e três crates mudam de nome

Cento e quinze commits desde a `v3.1.0`, a maior série desde a extracção do repo: a
reestruturação em camadas do ADR-0040 (fases P0–P3x), o dicionário de códigos numerados
`DX-CDNN` do ADR-0043, três renomeações de crate, a eliminação do `delonix-runtime-core`,
e uma dezena de correcções reais de produção (CRI/cgroups, VM, rede, containers). É a
razão do salto de major: quem consome este repo por `git`+path/tag sente as renomeações
de crate como quebra de compilação, e a CLI ganhou várias mudanças incompatíveis.

### BREAKING — três crates mudaram de nome, e um deixou de existir

Fase P3 do ADR-0040 (#393–#396, #406): o directório de cada crate já dizia a camada
(`crates/adapters/`, `crates/contexts/`, ...) desde o P0; esta fase alinhou o NOME ao
papel real de cada um.

| antes | agora |
|---|---|
| `delonix-scan` | `delonix-scanner` |
| `delonix-image` | `delonix-oci` |
| `delonix-net` | `delonix-sdn` |
| `delonix-runtime` | `delonix-linux` |
| `delonix-runtime-core` | **eliminado** — os tipos partilhados (`Error`, `records`, `secret`, `typestate`, `exitcode`, os códigos `DX-CDNN`) foram para `delonix-model`; o estado persistido (`JsonStore` e o que ele guarda) foi para o novo `delonix-state` |

Quem depende deste repo via `path`/`git`+tag num `Cargo.toml` externo (o caso do
`delonix-paas`) tem de actualizar os nomes de crate no próximo bump do pin — o binário
`delonix` e o seu comportamento não mudam por causa disto, só os nomes internos dos
crates Rust que o compõem.

### ADR-0040 — o motor por camadas: contextos, ports, e um binário por interface

Cinco fases (P0–P3x, #319 e a série de commits `arch(Pxx)` que se segue) fecham a
reestruturação que a `AGENTS.md` já impunha desde o P0 (a tabela `LAYERS`/`ALLOWED` do
`scripts/arch_fitness.py`), agora completa até ao código:

- **Novos crates de contexto** (`crates/contexts/`): `delonix-compute` (as portas de
  execução — `RunHost`, `ImageStore`, `StorageProvider`, `DeviceResolver`, `VmNetwork`,
  `NetworkProvider`, `WorkloadRuntime` — e o `RunSpec` único que `container run` e
  `container start` agora partilham em vez de reconstruírem cada um o seu), `delonix-node`
  (o contrato de nó, `proto/delonix/node/v1`), `delonix-stack` (o reconciliador de 3 vias e
  a tabela de Kinds).
- **Cada porta ganhou o SEU adaptador**, um caso de uso de cada vez (P2c–P3m):
  `DeviceResolver` no adaptador do kernel (CDI), `NetworkProvider`/`VmNetwork` no adaptador
  de rede, `StorageProvider`/`ImageStore` nos adaptadores de armazenamento e imagem,
  `RunHost`/`WorkloadRuntime` no adaptador do motor. O `container run`, o `container start`
  e o arranque de uma VM passam a resolver a mesma sequência de fases (rede → segredos →
  `--security-opt` → caminho de log → arranque) através dessas portas, em vez de três
  cópias divergentes.
- **Um binário compõe UMA interface** (P3k–P3m): `delonix serve cri` passa a EXECUTAR o
  binário `delonix-cri` em vez de o embutir; `delonix serve api` executa o `delonix-mgmt`;
  `delonix mcp` executa o `delonix-mcp`. O `delonix` deixa de ligar três servidores dentro
  de si — cada um ganhou o seu próprio `[[bin]]`.
- **A telemetria saiu da fundação** para `delonix-telemetry` (P3j) — a fundação
  (`delonix-model`) fica só com o que qualquer camada nomeia sem depender de mecanismo.
- **O `build` arranca o container de trabalho pelo `HostWorkload`** (#391), fechando a
  última chamada directa ao motor que o `build` ainda fazia por fora das portas.

Ver `docs/adr/0040-*.md` para o detalhe de cada fase e a evidência medida.

### ADR-0043 — o dicionário de códigos `DX-CDNN`

Cada falha ganha um número de quatro dígitos — classe (o que fazer a seguir) + domínio
(onde aconteceu) + sequência — que nunca muda de significado e nunca é reaproveitado.
`delonix explain <código>` lê o mesmo dicionário que a linha de erro, o JSON e a página
gerada em `docs/codigos.html`. Cinco passos fecham a fundação (#399–#403):

1. O dicionário em si + `delonix explain codes`/`delonix explain <código>`.
2. O número aparece na linha de erro, no `-o json` e na página gerada.
3. O exit code pergunta-se pela CLASSE do erro, não por um `match` de variante.
4. O número específico viaja DENTRO do erro (o `delonix-scanner` foi o primeiro crate a
   ganhar o seu próprio `Error` tipado com números).
5. `delonix-volume` ganhou o seu.

Mais tarde, já sem número de passo formal: `delonix-scanner` (#398) e `delonix-oci`
(#407) também ganharam o seu `Error` tipado. **`delonix-truenas`, `delonix-proxmox` e o
`delonix-vm` (incluindo o gerador de seed cloud-init) têm PRs próprios em curso** — não
entram nesta release, entram na próxima assim que fundidos.

### CRI/cgroups — três correcções de produção medidas contra um kubelet real

- **Um nó root novo não arrancava nenhum pod** (#310): a sonda de delegação de cgroup
  media um caminho que só existe em modo delegado de utilizador; em root, tudo o que
  pedisse `-m`/`--cpus`/`--cpu-weight` era recusado — incluindo os pods estáticos do
  próprio control-plane. Corrigido a criar o slice à mão quando ainda não existe.
- **O control-plane de nó único entrava em crash-loop** (#315): o kubelet cria os cgroups
  de pod como slices `systemd`, e o `systemd` retira o controlador `cpuset` de um slice
  sem unidades por baixo — exactamente o padrão deste motor, cujos containers vivem FORA
  da hierarquia do kubelet. O CRI passa a dizer `cgroupDriver: cgroupfs`, que é a verdade.
- **Em root, o nó ficava `NotReady` por `BridgeMissing`** (#322): o `RunPodSandbox` em
  root nunca teve rede de pod nenhuma — chamava um comando que o `clap` recusava. Cinco
  defeitos em cadeia fechados: rede CNI real no host para sandboxes root, `USER`
  não-root com `--cap-drop ALL` a arrancar, `/proc/sys` gravável sob `--privileged`,
  variáveis de ambiente do `ContainerConfig` finalmente entregues ao container, e
  `NET_ADMIN`+portas não-privilegiadas por netns de pod.
- Os containers do CRI entram na hierarquia de cgroup do kubelet (ADR-0038 fase 1, #325):
  «sem limite» só é mesmo sem limite debaixo do kubelet — o tecto da casa continua a
  proteger fora dele. O eviction manager voltou a ter estatísticas e o `kubeadm reset`
  deixou de largar órfãos (#316).

### `runtime`/`container` — supervisor, restart, e um `run -d` mais honesto

Uma dezena de correcções encadeadas em torno do supervisor de `--restart` e do registo
de containers (#357, #362, #365, #367, #368, #372, #374, #377–#380): um perfil seccomp
allow-all deixou de abortar o container; os reinícios da política de restart passam a
contar em `RESTARTS`; um `run -d` cujo comando não arranca é recusado com a razão real do
`exec`; um `stop` que interrompe a espera entre reinícios é respeitado; um supervisor
atrasado, um stop que retoma, e um restart concorrente deixaram de se pisar mutuamente no
PID gravado — cada um destes era uma corrida real, reproduzida antes da correcção.
`container run --net-bps` num `-d` passa a ser aplicado (P2e); um `run` recusado já não
deixa o directório do container para trás (#359); um container privilegiado volta a ter
`/proc/sys` gravável (#318, o mesmo defeito de raiz do achado de CRI acima).

### `vm` — cloud-hypervisor, libvirt, pausa e recriação

- **O `boot` do cloud-hypervisor só devolve quando o VMM confirma a VM a correr** (#350) —
  antes devolvia ao lançar o processo, sem confirmar que arrancou.
- **`vm stop` tenta desligar graciosamente e detecta disco corrompido de imediato** (#304).
- **Uma VM libvirt pausada deixava de aparecer no `vm ls` sem `-A`** (#312, #363).
- **Uma VM recriada com o mesmo nome deixava de anunciar o IP da anterior** (#320).
- O motor de VM liga-se à SDN pela porta `VmNetwork` e deixa de depender directamente do
  `delonix-net`/`delonix-sdn` (P3i, #383).

### `network`/`proxmox`/`pod` — ceifador de IPAM, portas de pod, e o lock do nó Proxmox

- **`network ipam ls`/`prune`** — o ceifador do registo de leases, respeitando a
  vivacidade de pods (#308), fechando a fuga documentada na `AGENTS.md`.
- **As portas de um membro de pod são libertadas e republicadas correctamente**, e
  publicar a quente num membro de pod passa a atravessar o ingress (#375, #376).
- **`delonix-proxmox`**: o `create` deixou de mandar `ipconfig0` duas vezes, e operações
  seguidas deixaram de morrer no lock do nó (#314).

### `delonix init` — CI/CD, adopção de projecto existente, e o template Odoo

- `stack init --template --up` passa a honrar o manifesto declarativo (`stack apply`) em
  vez de um `container run` cru escrito à parte (#291).
- `delonix init` sobre um directório já existente **adopta** em vez de despejar código de
  exemplo por cima (#292).
- Os 7 templates com código real ganharam CI/CD (GitHub Actions + GitLab CI),
  `sonar-project.properties`, `CONTRIBUTING.md` (git-flow + Conventional Commits + SemVer)
  e `commitlint` nos três templates JS (#293).
- `-v/--template-version` chegou aos 11 templates (#297); o template `odoo` segue as
  recomendações da OCA, com live reload (#296).
- **Validado ao vivo, os 11 Delonixfiles**: build e serviço real confirmados para cada um
  (#300).

### `docker-api`/`compatibility`/outras correcções

- `POST /wait` devolve o código de saída real; `HostConfig.RestartPolicy` passa a ser
  servido (#370, #371).
- `delonix compatibility docker` — a matriz de compatibilidade Docker de topo, com
  `-o json` (#306).
- Uma imagem que o registo não autoriza ler responde "não existe" em vez de vazar a
  existência (#381); um layer em cache substitui correctamente o obsoleto em vez de o
  duplicar (#299).

### Documentação

- `AGENTS.md` ganhou a secção que faltava sobre `Gateway`/`VirtualMachine`/
  `KubernetesCluster` (#290) e a narrativa completa do manual do contribuidor em
  `docs/dev/` (#392, gerado a partir de factos verificados, não escrito à mão).
- ADRs novos: 0038 (CRI/kubelet dono da política de recursos), 0039 (spike de um
  `VmBackend` OpenStack, aceite só depois de validar contra uma cloud real), 0040 (esta
  reestruturação), 0041 (o contrato local de nó), 0042 (uma só API do motor, maturidade
  de Richardson), 0043 (o dicionário de códigos).
- O site (`docs/gen.py`) reorganizou-se pelas 9 categorias do `--help` (#281, #294).

### Conhecido, não corrigido nesta série

- `delonix-truenas`, `delonix-proxmox` e o `delonix-vm` continuam com o `Error` genérico e
  partilhado — as três conversões para o dicionário `DX-CDNN` estão em PR aberto, não
  fundidas a tempo desta release.
- `image vm ls-remote` sem argumento pode passar de 90s numa ligação lenta e não tem
  `--timeout` (herdado da `v3.1.0`, não tocado nesta série).

### Não validado nesta release

O ciclo completo `kubeadm apply`/`kubeadm` multi-VM com SSH real entre nós, e o caminho
CNI em root além do que o `crictl` já exercitou directamente — precisam de um segundo nó
físico ou de um kubelet real, nenhum dos dois disponível neste sandbox.

---

## v3.1.0 — a auditoria grupo a grupo: 33 achados, zero folha nova, e o README já não ensina uma CLI de há duas versões

Vinte e nove commits desde a `v3.0.0`, quase todos de uma varredura sistemática da
CLI **grupo a grupo** (`container`, `image`, `secret`, `stack`, `manifest`, `net`)
mais o resíduo de correcções que a série ACH já vinha registando desde a `v3.0.0`.
Sem quebras de superfície — o `cli-tree.sh --gate` confirma as mesmas 245 folhas da
linha de base; esta release é comportamento, não forma.

Cada achado abaixo foi medido contra o binário ANTES de haver código, e tem gate
próprio (`scripts/e2e.sh` ou teste unitário) que chumba se a correcção for revertida.

### `container` — a sonda de saúde enchia o container de zombies, e o `stop` não parava

- **A sonda de `--health-cmd` deixava DOIS zombies por execução.** O watchdog que a
  mata ao fim do timeout nunca era esperado (`wait`), nem o `sleep` que ele próprio
  usava. Medido com `--health-interval 2`: +8 processos zombie a cada 10s, sem
  tecto — com o `pids.max=512` por omissão, um container com healthcheck deixa de
  conseguir forkar em ~10 minutos a esse intervalo, e em menos de três horas com o
  intervalo por omissão de 30s. O watchdog passa a matar E reapar o seu próprio
  filho, e o chamador passa a esperar por ele.
- **Um `stop` pedido podia ficar `Crashed`.** `runtime::stop` já grava `Stopped`
  "mesmo que precise de SIGKILL" — e precisa, sempre que o PID 1 não tem handler de
  SIGTERM (o caso mais comum: `alpine sleep 600`). O supervisor, à espera no mesmo
  processo, via `Signaled` e escrevia `Crashed` por cima — `ps -a` dizia `Dead`,
  `wait` respondia 137, para um container que o operador tinha acabado de mandar
  parar. Corrigido honrando `stopped_by_user` no ponto onde o supervisor regista o
  estado final.
- **`start`/`restart` perdiam o supervisor.** `run -d` forka sempre um (é o que
  torna o exit code de um detached conhecível); `start` só o fazia com uma política
  de restart definida. `run -d … 'exit 7'` dava `wait` = 7; o MESMO container, após
  um `container start`, respondia "exit code … was not captured". Agora `start`
  faz a mesma pergunta que `run`.
- **`container healthcheck <id>` ignorava a sonda do próprio container.** Lia só o
  `HEALTHCHECK` da imagem — um `run --health-cmd …` mostrava `Up (healthy)` no
  `ps` e o verbo respondia "image defines no HEALTHCHECK", vermelho permanente no
  script de CI que é a razão de existir deste comando.
- **`container port` mostrava `0.0.0.0` sobre um bind de loopback.**

### `image`/`secret` — um `load` que apagava nomes, um `create` que apagava chaves

- **`image load` apagava os OUTROS nomes da mesma imagem.** `pull` já funde tags
  (`merged_tags`, "identical content ⇒ same id, can have several tags — like
  Docker"); `load` guardava o `repo_tags` do arquivo tal e qual, e `save` escreve o
  registo inteiro. Medido: um store com `alpine:3.20` e um segundo nome para o
  MESMO id, mais um `load` do `save` do segundo, ficava só com o segundo — num nó
  offline, um manifesto ou compose que use o primeiro nome deixa de resolver.
- **`secret create` sobre um segredo existente apagava as chaves não repetidas e
  respondia `created`.** `a=1,b=2` mais `secret create --from-literal c=3` deixava
  só `c`, lido como "criei um segredo novo". Passa a dizer `replaced (1 key(s)) —
  2 key(s) dropped: a, b` e a apontar para `secret set`, o verbo não-destrutivo.
- Duas mensagens partidas: `image vm rm` de um inexistente respondia com uma 2.ª
  linha sem sentido (`no such one or more VM images were not removed` — o molde
  `no such {0}` recebia uma frase inteira), e `image vm describe`/o aviso de pull
  do `resolve_or_pull` saíam meio-traduzidos numa CLI em inglês.

### `stack`/`manifest`/verbos genéricos — uma adopção que mudava o dono em silêncio

- **`stack apply` adoptava um recurso alheio e dizia "nothing to do".** Adoptar é o
  instante em que um recurso sem dono passa a ser destruível pela stack — é a
  posse que autoriza o `destroy`/`--prune`. Medido de ponta a ponta: container
  criado à mão → `apply` responde "already exists, nothing to do" e carimba
  `delonix.io/stack` → `destroy` remove-o. O `apply` continua a adoptar (recusar
  partiria o caso normal de trazer para um manifesto o que já existe) — agora
  di-lo: `Container/x: adopted — it belonged to no stack, and a destroy or apply
  --prune of 'st' now removes it`.
- Um `kind: Stack` com um grupo mal escrito (`contaienrs:`) expandia para nada, e o
  erro dizia "`<ficheiro>` is empty (no YAML documents)" sobre um ficheiro com um
  documento — o aviso que nomeava a causa real ficava para trás no scroll.
- Mais duas mensagens que o molde `no such {0}` estragava: um `kind: Workload`
  inválido saía com classe **4** ("não existe") para um manifesto que nem
  parseia — passa a `Invalid` (1), como já era o irmão vinte linhas acima — e
  `volume snapshot restore` respondia meia frase em português dentro do molde
  inglês.

### Herdados da série ACH desde a `v3.0.0`

- **ACH-011 — `pod exec` sem `--container` era um sorteio.** O desempate entre
  membros nascidos no mesmo segundo caía na ordem do `read_dir`, que é aleatória a
  cada `pod create`. Passa a usar o índice declarado no manifesto.
- **ACH-014 — o `stop` do backend cloud-hypervisor devolvia com o VMM ainda a
  segurar o disco.** Um `qemu-img` a seguir apanhava o lock do qcow2 ainda tomado.
- **ACH-015/016 — um `kill -9` no *pin* de rede fechava o nó a trabalho novo para
  sempre, e o pidfile provava o argv em vez do dono real** — um `netns down` de um
  root podia matar o *pin* de outro.
- **ACH-017 — o `rm` de um HTTPRoute de um root matava o proxy L7 de OUTRO root**,
  e o `apply` dizia "serving" sem estar a servir.
- **ACH-018 — a prova de posse de um túnel era "o cmdline contém `ssh`"**, o que
  qualquer `ssh-agent`/mux Ansible do mesmo utilizador passava — e o `stop`
  mandava SIGTERM a esse processo alheio.
- **Runtime — o tecto de recursos cobria o PID 1 e mais ninguém.** `-m 64M`
  matava uma alocação feita pelo próprio PID 1 (correcto) e deixava passar a
  MESMA alocação feita por um filho forkado (3 em 3 corridas). A bateria tinha 700
  verificações e nenhuma testava um workload a tentar exceder o limite.
- **`vm` — a série do console foi para um socket unix e o ficheiro que o DKS lê
  ficou por escrever.** Das seis referências a `.serial` nos dois repositórios,
  cinco liam e nenhuma escrevia.
- **`cluster` — `get clusters` dizia "no clusters" com quatro clusters de VM na
  máquina.** A pertença era derivada de uma etiqueta que só os nós em modo *kind*
  carregam.
- **`volume` — o `rm` do pai não via os shares de um inquilino, e destruía-os**
  (ACH-001), e um `pod` sem membros nomeados escolhia a âncora de boot pelo menor
  nome em vez do primeiro do manifesto.

### README e `--help` alinhados com a restruturação (ACH-032, ACH-033)

A tabela "Command groups" do README ainda descrevia a CLI de antes do B2/B5/B6/B7:
citava cinco grupos removidos (`volumes`, `storage`, `sharevolume`, `schema`,
`dash`), prometia seis subcomandos cortados (`pod ls/describe/rm`, `vm status`,
`image --vm`, `net boot`) e omitia catorze grupos reais — os verbos genéricos entre
eles. Reescrita com os 33 grupos actuais e a contagem de Kinds corrigida (16 → 19).

A RAIZ da CLI (`delonix --help`) era o único sítio sem COMMAND MAP — listava os 33
grupos numa coluna plana, sem distinguir o imperativo do genérico do declarativo.
Causa: um comentário que descrevia um AND com o código a testar só metade. Agora
publica o mapa, categorizado (`Workloads`, `Artifacts`, `Storage`, `Networking`,
`Clusters`, `Declarative`, `Resources`, `Serve`, `Engine`).

Dois gates novos garantem que isto não volta a acontecer: `check_readme_groups`
(`docs_cli_gate.py`) compara a tabela do README com os grupos reais do binário nos
dois sentidos, e `the_root_help_maps_every_top_level_group` deriva o conjunto
esperado da árvore `clap` viva.

### Harness e infra-estrutura de CI

- Um portão que saía **0 com 36 falhas** — o `scripts/e2e.sh` não tinha exit code
  próprio, e três verificações do tecto de recursos nunca corriam (`_cg_of` não
  atravessava o `bash -c` do `check`).
- Um job de CI com todos os passos saltados reportava `SUCCESS` — a sonda agora
  sobe até ao veredicto real da job.
- Os cenários de caos partilhavam um sandbox, e três culpavam-se do que outro
  deixara para trás.
- As recusas de verbos removidos pelo B4 apontavam para comandos que já não
  existem.

### Validado, sem achado

O ciclo de registo completo contra um `registry:2` real subido pelo próprio motor
(`push`/`pull`/`login`/`sign`/`verify` com ECDSA-P256, incluindo a recusa da chave
errada); `serve docker-api` conduzido pelo cliente `docker` real (v29.7.2:
`create`→`start`→`inspect`→`stop`→`rm`); **`serve cri` conduzido pelo `crictl`
oficial do Kubernetes** — `version`/`info`/`images` e o ciclo `runp`→`create`→
`start`→`exec`, com os labels `io.kubernetes.pod.*` a sobreviverem ao
`ListContainers`; `backup`/`compose`/`build`/`pod`/`network`/`net` (ingress,
egress, l4guard, flow, netns, httproute, tunnel) e os verbos genéricos, todos pelo
efeito — dados de backup destruídos e repostos, `depends_on` do compose respeitado,
`RUN`+`COPY` do build verificados dentro do container final, netns partilhada de um
pod confirmada pelo mesmo IP nos dois membros.

### Conhecido, não corrigido nesta série

- `spec.resources.limits.{memory,cpu}` é honrado na forma Pod (`spec.containers[]`)
  e avisado-e-ignorado na forma plana do mesmo `kind: Container`.
- `container stats` não aceita `--no-stream`, embora o `system monitor` use esse
  nome para o mesmo conceito.
- `image vm ls-remote` sem argumento pode passar de 90s numa ligação lenta (vai a
  vários repositórios oficiais) e não tem `--timeout`.

### Não validado nesta release

Um `cluster kubeadm`/`apply` real (multi-VM com SSH entre si), `image vm build` de
ponta a ponta, e um kubelet de verdade a falar com `serve cri` (o `crictl` prova o
protocolo; um kubelet é outra máquina).

---

## v3.0.0 — o `--vm` e o `sharevolume` desaparecem, e o compose fica completo

Trinta e nove commits desde a `v2.0.0`. É `3.0.0` e não `2.1.0` porque a superfície da
CLI **perdeu 14 folhas sem alias** — e uma delas está num grupo que o
`docs/cli-stability.md` declara *Estável*, que é a mesma razão pela qual a `v2.0.0`
não coube num `1.x`.

### Quebras — o que deixa de existir, e o que usar

Cada substituto da tabela foi **corrido**, nao so verificado com `--help`. No `get` o
recurso e um ARGUMENTO e nao um subcomando, por isso `get vms --help` imprime a ajuda
do `get` e devolve 0 seja qual for o nome — foi assim que a primeira versao desta nota
mandou toda a gente para um `get vms` que responde `no such resource kind`. O nome e
`virtualmachines`, que abrevia a `vm`.

Medido contra `v2.0.0:scripts/cli_baseline.tsv`: **249 → 244 folhas**, 14 fora e 9
dentro. Nada disto tem alias: um script que ainda invoque a forma antiga falha com
`unrecognized subcommand`, nunca em silêncio.

| Deixou de existir | Usa |
|---|---|
| `image build`, `image convert`, `image import`, `image init`, `image ls-remote` (as formas com `--vm`) | `image vm build\|convert\|import\|init\|ls-remote` |
| `sharevolume apply\|describe\|ls\|rm` | `volume create --parent <volume>`, `volume ls -A`, `volume describe`, `volume rm` |
| `sharevolume migrate` | — removido; ver o aviso abaixo |
| `storage create`, `storage ls` | `volume create --type nfs\|cifs\|smb\|webdav …`, `volume ls` |
| `pod ls` | `get pods` |
| `vm status` | `vm ls`, `get virtualmachines` (abrevia a `get vm`), ou `vm describe <nome>` |

**Um `ShareRecord` nunca migrado (anterior à v0.53.x) deixa de ser lido.** O
`sharevolume migrate` que o convertia foi removido nesta série; quem estiver nesse
estado tem de o correr num binário anterior **antes** de actualizar.

### Dois defeitos que derrubavam um cluster inteiro

Nenhum falhava a compilar nem um teste unitário — só falhavam um nó a sério.

- **`PodSandboxStatus` devolvia `linux: null`, e nenhum `kubeadm` subia** (#233).
- **Um contentor privilegiado levava os `masked`/`readonly paths` na mesma** (#239).
  O kubelet manda `readonly_paths` com `/proc/sys` para TODOS os contentores; decidir
  quais honrar é do runtime, e para um privilegiado a resposta é nenhum — como no
  containerd e no CRI-O. Sem isto o `kube-proxy` não escrevia `nf_conntrack_max`,
  ficava em `CrashLoopBackOff`, e sem ele não há `ClusterIP` nem CoreDNS: o plano de
  serviço inteiro do cluster caía por dois argumentos.

E um terceiro que não tem nada de Kubernetes: **o `mount(2)` clássico trunca o
`lowerdir=` em SILÊNCIO acima de ~4 KB**, por isso uma imagem com muitas layers dava
`ENOENT` a preparar o rootfs. Medida a fronteira: 20 layers montam, 30 falham sempre,
e a `paketobuildpacks/builder-jammy-base` precisa de 9 107 bytes. O overlay passou para
a API nova (`fsopen`/`fsconfig`/`fsmount`), uma chamada `lowerdir+` por layer — sem
tecto a que uma chamada por layer possa esbarrar (ADR-0037).

### O `docker compose` deixou de ter buracos

Oito capacidades numa série: `profiles:` (com fecho transitivo por `depends_on`),
`extends:` (mesmo ficheiro, `depends_on` nunca herdado), `build.target`,
`deploy.replicas`, `networks.*.ipv4_address` fixo, `configs:`/`secrets:` de topo,
volumes anónimos, e **multi-ficheiro** (`-f a.yml -f b.yml`).

Fica de fora só a directiva `include:` do YAML — tem regras próprias de relatividade
de caminhos e de propagação do nome do projecto, e a recusa aponta para `-f a -f b`.

**Um volume anónimo é nomeado pelo CAMINHO e não pela posição no ficheiro.** Trocar
duas linhas `- /path` — uma edição banal — trocaria qual volume respalda qual caminho:
uma base de dados subiria sobre o volume que tinha os logs, em silêncio e com os dados
intactos no sítio errado.

### Dois Kinds novos

- **`kind: Service`** (ADR-0032) — selecciona containers por `matchLabels` e publica-os
  como vários registos DNS `A` sob `<nome>.<namespace>.delonix.internal`, com rotação
  na resposta. **Sem VIP, sem dataplane novo, sem daemon.** Um cliente que resolve uma
  vez e mantém a ligação não é rebalanceado — a limitação de qualquer round-robin por DNS.
- **`kind: App`** (ADR-0035) — Cloud Native Buildpacks ligadas a um caminho de build
  real. Os três módulos CNB existiam desde sempre no `delonix-image` **sem um único
  chamador**; o que os impedia era o `creator` exportar para um registo OCI que o
  container builder não alcançava. Resolvido com um registo descartável na SDN.

### Comandos novos

`image sign` (ECDSA-P256 compatível com cosign, chave gerada uma vez a `0600` na
criação), `cluster drain`/`uncordon`/`upgrade`, `vm migrate` (stop-copy-start, com
paragem real — a migração a quente é NO-GO medido, ADR-0031), `vm pause`/`unpause`,
`net capture`, `pod port-forward`.

### O que NÃO muda

O motor continua daemonless e rootless-first. Nenhuma dependência nova entrou no
supply chain por causa do `image sign` — o `ring` já estava no `delonix-image` para o
`verify`. A gestão de frota continua bloqueada pelo ADR-0010, e o `cluster
drain`/`upgrade` não a reabre: são operações sobre a API do Kubernetes e sobre SSH, o
caminho que o próprio ADR chama *built and shipped*.

### Higiene medida, não afirmada

O `Cargo.toml` esteve 38 commits a dizer `2.0.0` — o binário da série e o publicado
apresentavam-se com o mesmo número, e o que discriminava passava a ser o
comportamento. E o *ratchet* de língua estava a ser usado como um tecto que se
levanta: o #218 subiu `identifiers` de 1056 para 1061 e o #220 para 1062. Dezasseis
nomes de teste traduzidos; a base **desce** para **1045**, abaixo dos 1049 da própria
`v2.0.0`.

---

## v2.0.0 — `image list`/`backup list` voltam a `ls`

Reversão de uma quebra de contrato anterior, ela própria uma quebra de contrato — por
isso é `2.0.0` e não outro `1.x`. `docs/cli-stability.md` diz, desde a v1.0.0, que "um
breaking change deixa de caber num `1.x`": `image list`/`image remove` estavam na
tabela *Estável*, e reverter `list` desfaz uma promessa dessa tabela.

### O que muda

| antiga (v0.67.0+) | nova |
|---|---|
| `delonix image list` | `delonix image ls` |
| `delonix backup list` | `delonix backup ls` |

Corte limpo, sem alias — a grafia `list` falha com `unrecognized subcommand` (rc=2),
nunca em silêncio, a mesma regra de todas as reorganizações anteriores (v0.30.0,
v1.0.0). `image remove` **fica** — só o verbo de listagem volta atrás.

### Porquê

O B2 da reestruturação da CLI (v0.67.0) tinha renomeado `image ls`/`image rm` para
`image list`/`image remove`. Isso deixou `image list` como a **única** excepção de
nomenclatura numa CLI onde as outras 15 folhas do tipo "listar" usam `ls` — `network
ls`, `volume ls`, `vm ls`, `pod ls`, `secret ls`, `stack ls`, e mais — seguindo o
padrão Docker/Podman/kubectl que este mesmo projecto já cita para os verbos de
`container`. `backup list` nunca tinha sido `ls` (o grupo `backup` nasceu depois do
B2, já com essa grafia), mas ficou junto nesta reversão para a CLI contar uma história
consistente ("`ls` em todo o lado, sem excepção") numa única release em vez de duas.

O grupo `backup` está declarado **não estável** em `docs/cli-stability.md` — essa
metade sozinha não obrigaria a um major. Só `image list` obriga.

### O que NÃO muda

Nenhum outro comando, flag, ou semântica. `image remove`, `image pull`, `image push`,
`image build --type`, e o resto do grupo `image`/`backup` continuam exactamente como
estavam. Os códigos de saída não mudam.

### Bónus encontrado a medir, corrigido de caminho

`scripts/e2e.sh` tinha ~6 verificações que já invocavam `image ls` desde antes do B2
— mortas desde então (`unrecognized subcommand`, rc=2, nunca detectado por CI). Esta
reversão fá-las passar de novo, sem tocar nelas. As verificações de `backup list`
foram renomeadas para `backup ls`.

### A conta de folhas

`scripts/cli_baseline.tsv` continua em **236** — é uma troca de nome de duas linhas
(`image list`→`image ls`, `backup list`→`backup ls`), não uma adição nem remoção.

### `delonix system resources` — as flags que este anfitrião aceita e ignora

`system info` dizia `cgroup2 delegated: yes`. **Quais** controladores, não dizia
— e é aí que mora uma classe inteira de falhas silenciosas: um `--cpuset-cpus`
ou um `--io-max` num anfitrião sem esses controladores é lido, aceite e
ignorado, o container corre sem tecto nenhum e nada em lado nenhum o diz.

O comando novo responde a três perguntas de uma vez, porque separadas não valem:
quanto há, quanto se consegue mesmo impor, e o que está parado agora.

```
$ delonix system resources
control — what the engine can actually enforce here
  mode                rootless
  cgroup base         /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/dlx-containers
  cpu       enforced  --cpus, --cpu-weight
  memory    enforced  --memory, --memory-swap
  pids      enforced  --pids-limit
  cpuset    IGNORED   --cpuset-cpus, --cpuset-mems
  io        IGNORED   --io-weight, --io-max

pressure — share of time some task was stalled (avg10 / avg60 / avg300)
  cpu         0.00% /   0.01% /   0.03%
  memory      0.00% /   0.00% /   0.13%
  io         19.04% /   5.01% /   2.68%   ← the bottleneck right now

4 flag(s) are accepted and silently ignored on this host: --cpuset-cpus --cpuset-mems --io-weight --io-max
```

Três coisas que ele faz e que a leitura ingénua não faria:

- **Lê os controladores na fronteira de delegação, não no cgroup actual.** Numa
  sessão gráfica o cgroup actual é um `app-*.scope` que oferece `memory pids`,
  enquanto o `user@<uid>.service` duas camadas acima oferece `cpu memory pids`.
  O motor escapa para essa fronteira antes de criar seja o que for, por isso
  reportar a vista do scope inventaria um `cpu` em falta que nunca se atinge.
- **Só nomeia um gargalo acima de 5% de tempo parado.** Numa máquina em repouso
  os três números andam perto de zero e o que ganhar por 0,01 não significa
  nada — dar-lhe o nome de gargalo seria uma mentira com um número ao lado.
- **Não manda ninguém atrás de uma correcção que não existe.** O `io` não é uma
  delegação que este anfitrião se esqueceu de dar: o systemd não a dá a um
  utilizador sem privilégios, ponto — nenhum motor rootless, este, o podman ou o
  docker, escreve `io.max`. Para tectos de I/O o caminho é correr o nó como root.

`-o json` para um painel de capacidade ou um portão. É só leitura: não escreve
nada, em lado nenhum.

#### O conselho é código, com identificadores estáveis

O `system resources` não se limita a mostrar números: cada achado tem um id
`DLX-RES-nnn` que sobrevive à reescrita do texto, uma severidade e uma **classe**.

```
findings
  DLX-RES-001   2 flag(s) are accepted and silently ignored: --cpuset-cpus --cpuset-mems
                → sudo delonix system setup --delegate, then log out and back in
  DLX-RES-003   no aggregate ceiling: one workload with no --memory can take the whole host
                → run the node as root for delonix.slice, or set a per-container --memory
  DLX-RES-002   --io-weight and --io-max cannot apply: systemd never delegates the io
                controller to a rootless user
```

As três classes existem para o portão, não para decoração: `config` e `capacity`
descrevem algo estável sobre o qual se pode agir, e `--strict` sai com código
diferente de zero perante um deles; `load` descreve **este minuto** e nunca
reprova nada — um portão que fica vermelho porque alguém correu uma compilação
ensina toda a gente a ignorá-lo. É por isso que a pressão crónica (avg300) e um
pico (avg10 alto, avg300 baixo) são achados diferentes, com ids diferentes.

Repare no `DLX-RES-002`: é o único sem acção. O `io` não é uma delegação que
alguém se esqueceu de dar — o systemd não a dá a um utilizador sem privilégios,
e nenhum motor rootless escreve `io.max`. Mandar correr o `system setup` sobre
isso seria mandar o operador atrás de uma correcção que não existe.

#### `resources.get` no MCP, e o veredicto sobre correr o modelo aqui

A mesma função pura serve o MCP. Um agente e a CLI dão exactamente os mesmos ids
sobre a mesma máquina — se divergissem, quem lesse os dois não teria como saber
qual mentia.

E o servidor responde a uma pergunta que um agente sobre esta máquina devia
fazer primeiro: **esta máquina devia estar a correr o modelo que está a ler
isto?** O veredicto é aritmética medida, não opinião:

```json
"local_inference": {
  "verdict": "marginal",
  "largest_model_b_q4": 10,
  "reasons": [
    "this GPU also drives a display: a model that fills its VRAM makes the desktop stutter…",
    "10 GiB already swapped out — CPU offload would swap, and swapped inference is unusable"
  ]
}
```

O dimensionamento está ancorado num número que qualquer pessoa confirma — um 8B
a `Q4_K_M` ocupa ~4,7 GiB — e conta o que quase toda a gente esquece: **o
contexto não é grátis**. Nos 7581 MiB livres desta placa cabe um modelo de 10B
com 8k de contexto e só de 8B com 32k. Um modelo que "cabe" sem contar a cache
KV é um modelo que morre a meio da resposta.

Recusa com razão escrita quando não há GPU utilizável, quando não há spec CDI
(o container nunca chegaria à placa), ou quando nem um modelo de 7B cabe —
abaixo disso o modelo preenche um template mas não pesa um compromisso.

#### `delonix system regulate` — devolver o CPU a quem está à espera

A metade determinista. Lê a pressão, encontra a carga que a **causa**, corta-lhe
o `cpu.weight` a metade, e devolve-o quando a disputa passa.

**Causa, não vítima.** Pressão alta *dentro* de um container quer dizer que esse
container está a ser esfomeado — é a vítima. A causa é quem mais consome.
Estrangular «quem mais espera» castigaria exactamente a carga errada, por isso o
alvo escolhe-se por quota de consumo, medida em duas amostras (o `usage_usec` é
um contador desde que o container arrancou; uma leitura só diz quem gastou mais
CPU desde terça-feira passada).

**Só o `cpu.weight`, de propósito.** É uma quota-parte, não um tecto: sem nada
com que competir, uma carga estrangulada continua a ter a máquina inteira. Nada
é morto, suspenso nem limitado. O `memory.high` seria o segundo botão óbvio e
fica de fora por decisão: mal posto, prende uma base de dados em reclamação
permanente, e isso merece desenho próprio.

Duas marcas de água em duas janelas (25% em 10s para agir, 5% em 60s para
devolver) porque com uma só o regulador estrangula e repõe a mesma carga a cada
tique. E a recuperação **ganha** a um pico novo: sem isso, uma compilação custa a
uma carga metade da sua quota até o nó reiniciar.

Provado sob contenção a sério — 48 processos a queimar CPU num container, com o
anfitrião a 95% de tempo parado:

```
cpu stalled 95.0% over 10s, 46.4% over 60s · 3 workload(s)
  reg-burn               100.0%  cpu.weight 100
  oqsi-e2e-server          0.0%  cpu.weight 100
  kaeso-db                 0.0%  cpu.weight 100
  throttle reg-burn: cpu.weight 100 → 50  [applied]
    cpu stalled 95.0% over 10s and this workload is taking 100% of the engine's cpu time
```

Escolheu a causa, deixou as duas vítimas em paz, desceu 100 → 50 → 25 → 20 e
parou no piso. A reposição foi provada no sentido inverso, com o evento no log.

Foi essa corrida que encontrou um defeito que nenhum teste apanhava: o container
morreu estrangulado antes de o minuto acalmar, e o **memo sobreviveu-lhe**. Agora
os memos de cargas que já não existem são varridos a cada tique.

Não muda nada sem `--apply`.

#### `scripts/advisor_eval.py` — escolher o modelo com números

A escolha de um modelo para ler o estado de um anfitrião estava a ser feita por
opinião. Este é um dos raros casos em que a resposta certa é **calculável**: o
`bottleneck()` e os achados `DLX-RES-nnn` são deterministas, portanto há
gabarito, e um modelo que não o bate não serve — valham-lhe os benchmarks que
valerem.

Dezassete anfitriões sintéticos cobrem o espaço de decisão, e a verdade de cada
um é **computada pelas regras reais, nunca escrita à mão**: uma expectativa
escrita à mão é uma segunda implementação das regras, e no dia em que as regras
mudam passa a ser uma mentira que um teste verde defende. Mudar um limiar sem
regenerar os goldens falha o teste, no mesmo commit.

```
DELONIX_UPDATE_FIXTURES=1 cargo test -p delonix-runtime --test advisor_fixtures
scripts/advisor_eval.py --backend ollama --model qwen3:8b
scripts/advisor_eval.py --backend stub      # prova o arnês sem modelo nenhum
```

Mede, por ordem de importância: **JSON válido** (um conselheiro que devolve
prosa não se liga a nada — abaixo de 100% está fora), **gargalo certo**,
**achados certos** (Jaccard sobre o par `id:subject`), **achados inventados** e
latência p50/p95. Os inventados contam tanto como os certos: um modelo que
encontra problemas num anfitrião saudável ensina o operador a ignorá-lo.

Dois pares no corpus existem só para separar quem entendeu de quem decorou:

| par | o que separa |
|---|---|
| `io-bound-chronic` vs `io-spike-transient` | o mesmo instante mau, cinco minutos diferentes |
| `memory-thrashing` vs `swap-used-but-calm` | 10 GiB em swap num anfitrião calmo é **saúde**, não problema |

E o corpus tem um portão contra si próprio. A primeira versão dava **69%** a
quem respondesse sempre «nada» — nove de treze anfitriões estavam calmos, e um
ranking onde o silêncio tira 69% não separa um bom modelo de um mudo. Um teste
mede agora a resposta trivial e recusa um corpus onde ela passe dos 60%; hoje
tira 53%, e é essa a linha que o relatório imprime ao lado de qualquer
pontuação.

O `--backend openai` fala o dialecto `/v1/chat/completions`, que qualquer
fornecedor serve. O utilizador final escolhe o seu modelo e o seu fornecedor;
isto só diz quanto custa a escolha.

#### O tecto agregado passa a existir em rootless

O `delonix.slice` é `/sys/fs/cgroup/delonix.slice` — um caminho de **raiz**. E
rootless é o modo por omissão deste motor, portanto até aqui o orçamento
agregado simplesmente **não existia onde a maior parte das instalações corre**.
Duas consequências, ambas medidas: uma carga sem `--memory` podia levar o
anfitrião inteiro, e o governador térmico baixava o `cpu.max` de um cgroup que
não estava lá.

A resposta já existia e estava a ser ignorada: em rootless, a base delegada onde
o motor cria as folhas (`<user@uid.service>/dlx-containers`) **é** o pai comum de
todas as cargas. Pôr o tecto nela não precisa de privilégio novo nem de caminho
novo — é o mesmo directório onde as folhas já vivem.

Medido nesta máquina, depois da mudança:

```
memory.max   27878420480     (85% de 30,5 GiB)
cpu.max      2720000 100000  (27,2 dos 32 cores)
pids.max     131072
```

Três coisas destrancam com isto:

- o **`DLX-RES-003`** («sem tecto agregado») deixa de aparecer, porque deixou de
  ser verdade;
- a **admissão** (`admission_check`) deixa de desistir por ser rootless — passa a
  desistir só quando não há fronteira de delegação nenhuma, que é outra coisa;
- o **`system thermal`** deixa de exigir root. Provado ao vivo: a 85 °C baixou o
  tecto do motor de 27,2 para 21,8 cores, sem sudo.

Onde não há fronteira — uma sessão SSH, que é um *scope* **irmão** do
`user@<uid>.service` e não um descendente — a resposta é `None` e o comando
recusa dizendo isso. Escrever um tecto para onde o motor nunca conseguiria mover
um container seria pior do que não ter tecto.

#### O `doctor` deixa de ter uma segunda lista escondida

O `system doctor` dizia «every prerequisite holds» num anfitrião onde duas flags
estavam a ser aceites e ignoradas — porque essa lista vivia noutro comando. Duas
listas de problemas do mesmo anfitrião, em dois sítios, é como um operador lê uma
e perde a outra, e o `doctor` é o que se corre quando algo está mal.

Passa a mostrar os achados de recursos, das **mesmas** regras, e só as classes
estáveis: `doctor` responde «este anfitrião consegue fazer o que o motor
promete», e a pressão deste minuto não é resposta a essa pergunta. O `--strict`
passa a reprovar também por elas.

#### O regulador ganha a memória — e nunca o `memory.max`

A segunda metade do `system regulate`. Só o `memory.high`, e a distinção não é
de estilo: `high` põe uma carga acima da linha em reclamação agressiva e
abranda-a; `max` mata-a. Um regulador que pode matar uma base de dados porque
uma média de cinco minutos passou um limiar não é um regulador, é um incidente.

**Duas decisões, dois culpados.** O maior consumidor de CPU raramente é o maior
detentor de memória, por isso os planeadores são separados e cada observação
credita **uma** decisão por recurso. Provado sob pressão real nesta máquina —
39,2% de tempo parado em memória, com o CPU a 0,1%:

```
stalled: cpu 0.1%/0.3%, memory 39.2%/35.4% (10s/60s) · 2 workload(s)
  oqsi-e2e-server          0.2% cpu  648.7 MiB mem  cpu.weight 100
  kaeso-db                99.8% cpu    2.7 GiB mem  cpu.weight 100
  throttle kaeso-db: memory.high 2.7 GiB → 2.4 GiB  [would apply (use --apply)]
    memory stalled 39.2% over 10s and this workload holds 81% of the engine's memory
```

Repare que o `kaeso-db` tem 99,8% do CPU e **não** foi estrangulado no CPU: a
0,1% de tempo parado não há disputa nenhuma para resolver. É o mesmo princípio
que já estava lá — 90% do CPU numa máquina que ninguém está à espera não é
problema.

O aperto é para **90% do que a carga usa agora**: um empurrão para a reclamação,
não um precipício. A carga fica com o que tem e paga só por crescer, que é
exactamente para o que o `memory.high` serve. Nunca abaixo de 128 MiB, e nunca
duas vezes na mesma carga — quem já está apertado é excluído, ou um só evento
sustentado levá-la-ia ao piso em quatro tiques.

Um memo por **par (carga, botão)**, e não um por carga: uma carga pode estar
estrangulada no CPU e na memória ao mesmo tempo, e um só ficheiro faria a segunda
reivindicação apagar a primeira — uma delas nunca mais voltaria. Repor a memória
escreve o literal `max`, e não um número, porque um número continuaria a ser um
tecto. Um `memory.high` que um humano pôs à mão não tem memo e **nunca** é
tocado.

#### Primeira medição de um modelo: o `qwen3:8b` perde para o silêncio

O arnês existia e nunca tinha visto um modelo. Viu.

```
modelo qwen3:8b · ollama · 17 casos
  json válido       100%
  gargalo certo      47%     (a resposta trivial tira 53%)
  achados (jaccard) 0.32     (a resposta trivial tira 0.35)
  inventados           5
  latência p50/p95  7.0s / 9.7s
```

Passa o portão duro — **100% de JSON válido**, e um conselheiro que devolve prosa
não se liga a nada — e depois **perde para quem responde sempre «nada»** nas duas
métricas que interessam.

O porquê está nos casos, e é um modo de falha único:

| onde | resultado |
|---|---|
| anfitriões **em disputa** (cpu/io/memória crónicos, pico, sem PSI) | 8 em 9 certos |
| anfitriões **calmos** (saudável, swap-mas-calmo, as três variantes de GPU, disco cheio) | 0 em 6, e inventou achados em 5 |

**O modelo não consegue dizer «não se passa nada».** É a pior propriedade
possível num conselheiro, porque é exactamente o que ensina um operador a
ignorá-lo — e é a razão pela qual os anfitriões calmos estão no corpus. Sem eles,
este modelo teria tirado ~89% e parecido excelente.

Três coisas que este número **não** é: não é um veredicto sobre o Qwen3 (o prompt
não foi afinado, e o arnês ainda não distingue culpa do modelo de culpa do
prompt); não é uma medição de velocidade de GPU (correu em **CPU** — o servidor
Ollama desta máquina não tinha bibliotecas CUDA, e a placa esteve o tempo todo
livre); e não é uma comparação (um modelo, um prompt, temperatura 0, uma corrida
por caso).

O arnês ganha `--think`, desligado por omissão. Os modelos de raciocínio emitem
um bloco de pensamento antes da resposta e ele domina a latência — e aqui os
números vêm de uma tabela fixa, não de uma cadeia de raciocínio, portanto o bloco
gasta-se em nada. Com ele ligado a mesma pergunta demorava três vezes mais.

#### `system resources` passa a olhar para dentro de cada carga

Os achados do anfitrião respondem «este anfitrião consegue impor alguma coisa».
A pergunta a seguir é outra, e num anfitrião misto tem outra resposta: **«e o
tecto que EU pedi, ficou de pé?»** Uma carga arrancada antes de uma delegação ser
corrigida continua a correr sem o limite que pediu, e nada o dizia.

Provado ao vivo — uma carga que pede um `--cpuset` que esta máquina não consegue
impor:

```
workloads — what each asked for, and its own stall (10s cpu/mem/io)
  wl-probe                 0%    0%    0%
      --memory        256M      in force
      --cpus          0.5       in force
      --cpuset        0-1       IGNORED   controller not delegated — the flag was accepted and ignored
```

A comparação é contra **o que o motor teria escrito**, calculado pelos
conversores que o caminho de arranque usa — não por um segundo entendimento de
«o que quer dizer `--memory 512M` em termos de cgroup». Uma segunda
implementação derivaria, e o primeiro sinal da deriva seria este relatório a
chamar partido a um anfitrião correcto.

E cada carga traz a **sua própria** pressão. A do `io` aparece mesmo onde o
controlador `io` não está delegado: o kernel contabiliza a paragem quer alguém
a possa limitar quer não. Em rootless, é a única resposta honesta a «quem está à
espera do disco».

#### Quatro flags que o conselho nomeava não existem

Medido a 2026-08-31: `--memory-swap`, `--pids-limit`, `--cpuset-mems` e
`--io-max` são a grafia do Docker. Este motor tem `--cpuset` e a família
`--device-*-bps/iops`, e o `pids` **não tem flag nenhuma** — o tecto é
propriedade do grupo de cgroup, não do `container run`.

Portanto o `system resources` andou a dizer a operadores para deixarem de usar
flags que não conseguem escrever. Nada podia dar por isso: são literais de
string noutro crate, e o compilador não tem opinião sobre o interior de uma
string. Agora um teste percorre a árvore do `clap` e recusa qualquer nome que
não exista mesmo — mais um que falha se alguma das quatro grafias do Docker
voltar a aparecer sem ser acrescentada ao mapa no mesmo commit.

#### O regulador passa a ter quem o arranque

Não é um daemon — este motor é daemonless por desenho, e um processo residente
precisaria do seu próprio ADR. É a outra metade do problema: **um regulador que
ninguém arranca não regula nada.**

```
delonix system regulate --install-timer --interval 60
delonix system regulate --uninstall-timer
```

Um temporizador `systemd` a correr `--apply --once`. Em rootless instala uma
unidade de **utilizador** — nenhum privilégio — e como root vai para o sistema.
Provado ao vivo: disparou, correu o regulador, registou a decisão no journal, e
agendou a seguinte.

```
Aug 31 08:11:11 delonix[…]:   kaeso-db  52.0% cpu  1.7 GiB mem  cpu.weight 100
Aug 31 08:11:11 delonix[…]:   nothing to do
Aug 31 08:11:11 systemd[1775]: Finished delonix-regulate.service
```

`Persistent=false` de propósito: uma decisão perdida enquanto a máquina esteve
desligada não vale a pena recuperar — a pressão da semana passada não é pressão.

### Limitações conhecidas

- **O I/O continua sem regulação**, e não é uma escolha nossa: o `io` não é
  delegável a um utilizador sem privilégios, portanto não há botão para rodar. O
  que existe é a visibilidade, e agora está exposta — a pressão de `io` de cada
  carga aparece no `system resources`.

#### Os achados passam a falar português

Não davam, e a razão era estrutural: o catálogo da CLI indexa pela string
inglesa EXACTA, e um `format!("{n} flag(s)…")` produz uma chave diferente para
cada anfitrião. Traduzir texto já formatado é impossível por construção.

Agora o motor emite **template mais dados** em vez de texto acabado:

```rust
finding: Message::new(
    "{n} flag(s) are accepted and silently ignored: {flags}",
    &[("n", …), ("flags", …)],
)
```

O template é um literal `&'static str` — portanto é uma chave de catálogo — e a
CLI renderiza-o na língua do utilizador. **O motor diz o quê, a CLI decide em que
língua**, e o crate do motor continua sem saber que o português existe.

```
achados
  DLX-RES-001   cgroup   2 flag(s) são aceites e ignoradas em silêncio: --cpuset
                         → sudo delonix system setup --delegate, e depois sair e voltar a entrar
  DLX-RES-002   io       o --io-weight e as flags --device-*-bps não se aplicam: o
                         systemd nunca delega o controlador io a um utilizador sem privilégios
```

O JSON e o MCP continuam a receber **inglês**, sempre: um consumidor de máquina
que tenha de adivinhar a locale de um campo é um consumidor que se parte quando
alguém muda a shell. Para máquinas, a chave é o `id`.

E a `action` passou de `String` vazia a `Option<Message>` — «não há nada a
fazer» deixou de se escrever como «a acção é a string vazia», que é a mesma
coisa dita de uma forma em que ninguém repara.

### Limitações conhecidas
- **O `--apply` da memória não foi exercido contra um container vivo.** A decisão
  foi provada sob pressão real, e a mecânica de escrita e reposição por testes com
  ficheiros a sério; espremer a base de dados de alguém para fazer uma
  demonstração não era proporcionado.
- **Um modelo medido, e em CPU.** O `qwen3:8b` correu inteiro na CPU porque o
  servidor Ollama desta máquina não tem bibliotecas CUDA; isso afecta a latência,
  não a exactidão. Nenhum segundo modelo, e nenhuma corrida com GPU.
- **O prompt não foi afinado.** Uma pontuação baixa pode ser do modelo ou do
  prompt, e o arnês ainda não distingue as duas coisas — o passo seguinte é
  variar o prompt com o modelo fixo.
- **Os anfitriões do corpus são sintéticos.** Capturas reais de uma máquina só
  ocupam um canto do espaço (esta só produziria «rootless, cpu memory pids, em
  repouso»), mas o arnês lê qualquer ficheiro da mesma forma — a saída de
  `delonix system resources -o json` entra ao lado destes.

## Restruturação da CLI, blocos B1 e B3 (fatia 1) — identidade, diagnóstico, e uma preferência local

Continuação directa do plano em `docs/discovery/52_CLI_PLANO_MIGRACAO.md`. O B1 fecha
a identidade endereçável dos dois Kinds que nunca tinham um nome de documento
persistido; o B3 (fatia 1) acrescenta quatro peças baratas — mecanismo já existente,
só faltava a superfície.

### B1 — `get`/`describe`/`delete` alcançam `NetworkRoute` e `NetworkPolicy`

`NetworkRoute` (`<from>-><to>`) e `NetworkPolicy`/`FirewallPolicy` (`<target>/<direction>`)
não tinham identidade própria em lado nenhum — o registo, quando existe, só conhecia o
par ou o (alvo, direcção). Os verbos genéricos exigem um NOME, e agora respondem por
essa identidade. `get networkpolicies` ganhou, de caminho, a listagem combinada que
faltava: `net ingress ls`/`net egress ls` só respondiam à sua direcção, uma de cada
vez. `cluster describe` (modo kind) também passou a existir.

Sem verbo de CLI novo — os dois Kinds continuam sem `network route describe`/
`net ingress describe` nativos, só o verbo genérico foi ligado, o mesmo padrão que
`kind: Dependency`/`kind: NetworkAccessRule` já seguiam.

### `delonix diff <kind> <name>` — as três faces de um recurso, lado a lado

O motor de diff de 3 vias já existia dentro de `stack plan` (`desired`/`last-applied`/
`observed`) — faltava a superfície que mostra as três faces de UM recurso nomeado, em
vez da lista de mudanças do plano inteiro. `delonix diff <kind> <name> [-f <manifesto>]`
imprime uma tabela `FIELD | DESIRED | LAST-APPLIED | OBSERVED`; `--detailed-exitcode`
segue o mesmo idioma do `stack plan` (0 sem diferenças, 2 com elas). Zero mudança a
qualquer Kind — reaproveita os `desired()`/`actual()` que cada um já expõe.

### `delonix cluster health <name>` — o control-plane responde, os nós estão `Ready`

Só cobre clusters modo-kind (mesma fronteira que `cluster kubeconfig`/`get
kubernetesclusters` já têm). Corre `kubectl get --raw=/readyz?verbose` e `kubectl get
nodes` **dentro do container do control-plane** — sem exigir `kubectl` no host. Exit
não-zero se o control-plane não responder ou um nó não estiver `Ready`: um `health`
que devolvesse 0 sobre um nó `NotReady` seria o relato desonesto que este repo
persegue.

### `delonix system metrics [-o json]`

Mais um consumidor do `delonix-mgmt::dashstats::collect` que já alimenta `dash --json`
e o `/metrics` Prometheus — zero lógica nova. Devolve o `DashSummary` cru (os
contadores, para scripts), e não o `DashData` moldado para o TUI que `dash --json` já
dá. `system state` não se construiu de propósito: `system info` já responde à mesma
pergunta, e um segundo comando para a mesma resposta seria a duplicação que este
projecto já evita noutros sítios.

### `delonix config get|set|unset` — uma preferência, um ponto de leitura

Âmbito reduzido de propósito: só a preferência de `output` (`table`/`json`) fica
coberta — não `namespace`, que teria uma dúzia de pontos de aplicação em vez de um
só, e vira decoração se ninguém a lê de verdade em todos eles. Persistida em
`<DELONIX_ROOT>/config.json`, mesmo padrão do `delonix_vm::{get,set,clear}_default_
backend`. O único ponto de leitura é `output::OutputFormat`'s default — todos os
comandos com `-o/--output` ganham o novo comportamento de graça, sem tocar em mais
nenhum ficheiro. `config set output json` muda o default de QUALQUER comando sem
`-o` explícito; `config unset output` volta à tabela.

### `delonix secret rotate <nome> <chave> [--length N]`

Gera um valor novo para UMA chave já existente de um segredo, sem tocar nas outras
chaves nem na chave-mestra (isso continua a ser o `rotate-key`, comando distinto).
Reaproveita por inteiro o `SecretStore::update` já provado (flock, read-modify-write)
e a resolução por nome que já faz um valor rodado aplicar-se só no `start` seguinte.
Recusa uma chave que não existe — nunca cria uma nova a fingir que é rotação — e
nunca ecoa o valor gerado, mesma disciplina do `secret create`/`set`. Risco: nenhum
(zero privilégio/mecanismo novo).

### `volume ls` ganha SIZE/USED BY, `network ls` ganha USED BY

Pedido directo do utilizador, lido pelas lentes de DevOps/SRE/platform engineering: um
`volume ls` sem tamanho nem consumidor obriga a um `inspect` por volume só para
responder "que espaço ocupa isto, e quem o usa"; o mesmo para redes. Duas colunas
novas em cada:

- **`volume ls`**: `SIZE` (o mesmo `measured_usage` do `volumes inspect` — nunca um
  `0` mentiroso quando um directório de subuid mapeado não é legível daqui: mostra
  o valor com sufixo `+` quando a medição está incompleta) e `USED BY` (os nomes dos
  containers cujo `Mount.source` aponta para o `mountpoint` do volume).
- **`network ls`**: `USED BY` (os containers cujo `network`/`extra_networks` inclui
  a rede).

Nos dois casos, `None` (não foi possível saber) e `Some(vazio)` (ninguém usa mesmo)
são distintos — `?` contra `-` na tabela, e em `-o json` o campo só desaparece no
primeiro caso (`skip_serializing_if` só para `None`, nunca para uma lista vazia
genuína). Colunas sem informação nenhuma em toda a tabela escondem-se sozinhas
(`Table::drop_uninformative`).

### `pod`/`secret`/`image`/`container`/`cluster ls` ganham mais seis colunas

Continuação do mesmo pedido — a lente DevOps/SRE/platform engineering aplicada
ao resto dos `ls`:

- **`pod ls`**: `AGE`, do `created_unix` do membro mais antigo (um pod não tem
  criação própria — deriva-se dos containers que o compõem).
- **`secret ls`**: `AGE` (`updated_unix`) e `USED BY` — mesma distinção
  `None`/`Some(vazio)` do volume/network, contra `Container.secrets`.
- **`image ls`**: uma linha por TAG em vez de só a primeira (uma imagem
  multi-tag escondia as outras), e `ORPHAN` (zero containers referenciam-na).
- **`container ls`**: `RESTARTS` (do registo de eventos) e uma coluna `SIZE`
  opt-in (`-s/--size`) — o espaço da própria camada de escrita do container,
  nunca as layers de imagem partilhadas; fica fora por omissão porque é uma
  travessia de directório por container, o mesmo custo que o `volumes inspect`
  já assume em vez de pagar em toda a listagem.
- **`cluster ls`** (modo kind): `K8S VERSION`, lida da tag da imagem do nó
  control-plane (`kindest/node:v1.34.0…`) — a versão real que corre, não a que
  o `kubeadm init` foi mandado esperar. Filtro por namespace ficou de fora: o
  `cluster create` não tem `--namespace` nenhum hoje, e adicioná-lo é âmbito
  maior, separado.

**Um bug de segurança real, encontrado a validar o `ORPHAN`**: `image remove`
nunca correspondia um container criado com o nome NU da imagem
(`container run alpine ...` grava `image: "alpine"`, que nunca batia com uma
entrada de `repo_tags` como `"alpine:latest"`) — `image remove alpine:latest`
desmarcava em silêncio uma imagem de que um container vivo ainda dependia.
Corrigido normalizando pelo mesmo `normalise_tag` que o `ImageStore::resolve`
já usa, partilhado entre o `cmd_rm` e o novo `ORPHAN`. Confirmado ao vivo: antes
da correcção o `remove` tinha sucesso com o container ainda a apontar para a
imagem; depois, recusa correctamente.

---

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

A `v1.0.0` é a primeira release estável do Delonix Engine: a partir daqui o
`docs/cli-stability.md` é um contrato de semver, e uma quebra só sai numa major.
Junta tudo o que foi construído na fase de desenvolvimento (a série `v0.x`, de
16 de Julho a 29 de Agosto de 2026), que deixou de ser publicada como releases
avulsas. O resumo dessa fase está na secção «O que a 1.0 traz», mais abaixo; o
texto integral de cada versão `v0.x` está em
[`docs/releases/archive/v0.md`](https://github.com/angolardevops/delonix-runtime/blob/main/docs/releases/archive/v0.md).

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

### O que a 1.0 traz

Um motor de containers e microVMs **rootless-first e daemonless**, escrito em
Rust, que corre num nó sem processo residente e sem socket de root.

#### Containers e pods
- Ciclo de vida completo ao nível do Docker/Podman (`run`, `exec`, `logs`,
  `stop`, `kill`, `wait`, `restart`, `rename`, `port`, `attach`, `cp`, `commit`),
  com `container update` a mudar portas, volumes, redes, memória e CPU **a
  quente, sem mudar o PID**.
- Pods reais multi-container, com netns, IPC e UTS partilhados.
- Containers rootless partilham as layers da imagem por overlay, em vez de as
  copiarem.
- GPU por CDI (`--gpus`, `--device nvidia.com/gpu=…`).
- Backup e restauro por recurso, a quente, com os dados dos volumes.

#### Imagens e build
- Pull/push OCI com retoma e verificação de digest, CAS local, `image save`/`load`.
- `delonix build` com Dockerfile/Delonixfile, multi-stage, `--target`, cache por
  instrução, `RUN --mount=type=secret` e `--platform`.
- `kind: App` com Cloud Native Buildpacks; scan de CVE e SBOM.

#### Rede
- SDN rootless num holder de rede com plano de controlo reiniciável: reiniciar o
  controlo não mexe em nenhum workload.
- Firewall de entrada e saída por container, isolamento por namespace para
  containers, pods e VMs, alcançabilidade dirigida (`kind: Dependency`) e
  caminhos entre redes (`kind: NetworkRoute`).
- DNS interno `<nome>.<namespace>.delonix.internal`, proxy L7 com TLS
  (`kind: HTTPRoute`/`Ingress`) e auto-registo de containers.
- Overlay VXLAN entre nós, cifrado com WireGuard; túneis públicos
  (pinggy, ngrok, Cloudflare).

#### VMs
- Cloud Hypervisor e libvirt atrás de uma porta única, mais um backend remoto
  Proxmox VE.
- Imagens douradas (Ubuntu, Debian, Rocky, com ou sem Kubernetes), `VMfile` e
  `vm build`, cloud-init por instância, snapshots e consola série.

#### Kubernetes
- `delonix-cri`, o runtime que serve um kubelet, com tecto de capabilities
  definido no nó.
- `cluster kubeadm` de um comando, com HA (HAProxy automático) e etcd externo;
  clusters em modo kind sem Docker; `cluster load` sem registo.

#### Infraestrutura declarativa
- Manifestos multi-documento com Kinds agrupados por `apiVersion`, e o ciclo
  `stack plan` / `apply` / `destroy` convergente com diff de 3 vias e posse por
  label, sem ficheiro de estado.
- Schema JSON gerado do código (`delonix schema`, `delonix explain`) e
  `docker-compose.yml` nativo (`delonix compose`).

#### Armazenamento
- Volumes nomeados e bind mounts, shares com quota, volumes de rede
  NFS/CIFS/WebDAV e provisionamento numa NAS TrueNAS.

#### Operação
- Instalador multi-distro com releases assinadas (minisign) e verificadas por
  SHA256.
- Códigos de saída com classe, dashboard (`delonix dash`) e métricas Prometheus,
  units systemd para sobreviver a um reboot, interface em inglês e português.

---


# Delonix Runtime — features por release

> Gerado por `scripts/gen-releases.sh` a partir de `docs/releases/<tag>.md`
> (regenerado automaticamente pelo pipeline de release a cada tag publicada).
> Não editar à mão — edita a nota da release respectiva.

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

Ver `CLAUDE.md` no repositório para detalhes de arquitectura, limitações conhecidas desta v1 (só etcd `stacked`, execução sequencial em `cluster apply`) e as próximas fases já registadas.

---


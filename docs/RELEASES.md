# Delonix Runtime — features por release

> Gerado por `scripts/gen-releases.sh` a partir de `docs/releases/<tag>.md`
> (regenerado automaticamente pelo pipeline de release a cada tag publicada).
> Não editar à mão — edita a nota da release respectiva.

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
parte — ver `CLAUDE.md`, secção "Próximas fases".

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

Ver `CLAUDE.md` no repositório para detalhes de arquitectura, limitações conhecidas desta v1 (só etcd `stacked`, execução sequencial em `cluster apply`) e as próximas fases já registadas.

---


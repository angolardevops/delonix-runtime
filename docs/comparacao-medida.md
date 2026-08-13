# Docker × Podman × Delonix — comparação medida

> **O que este documento é.** Uma tabela em que **cada linha foi executada**, nas três
> ferramentas, na mesma máquina, no mesmo dia. Não decide quem é melhor: mostra o que cada
> um faz, incluindo aquilo em que o Delonix é **pior** — e há uma linha em que é o pior dos
> três por uma margem larga.
>
> **O que não é.** Uma análise de funcionalidades lida do `--help`. Isso já existe em
> [`paridade-docker-podman.md`](paridade-docker-podman.md), que aliás começa por dizer que
> não mediu nada. Aqui, uma célula sem medição diz `não verificado` — nunca uma afirmação.

| Campo | Valor |
|---|---|
| Data | **2026-08-13** |
| Onde | VM libvirt `pari053`, 4 vCPU / 4 GiB, Ubuntu 24.04.4, kernel 6.8.0-136-generic |
| Porquê numa VM | O host de desenvolvimento **não tem daemon do docker**, e instalá-lo ali seria mexer numa máquina com produção a correr. Sem as três no mesmo sítio, duas colunas seriam `não verificado` e a comparação não valia nada. A VM foi criada pelo próprio motor (`delonix vm create`). |
| Docker | 29.1.3 (`docker.io` do Ubuntu, daemon `systemd` activo, utilizador no grupo `docker`) |
| Podman | 4.9.3 (rootless) |
| Delonix | **0.53.0** (rootless), binário da release oficial da tag, em `/usr/local/bin/delonix` |

> **Nota de montagem, e ela importa para a justiça da comparação.** O binário do Delonix tem
> de estar em `/usr/local/bin/delonix`: a golden aplica-lhe um perfil AppArmor, e com
> `kernel.apparmor_restrict_unprivileged_userns=1` (activo, medido) o `unshare` é negado a
> qualquer outro caminho. Uma bateria anterior mediu, sem o saber, um `run` que rebentava.
> Cada ferramenta tem de ser medida configurada como é suposto, ou a tabela mente a favor de
> quem estiver bem instalado.
>
> As imagens (`alpine:latest`, `nginx:alpine`) estavam locais nos três antes do primeiro
> cronómetro, e cada motor corre com o **seu** default de rede.

---

## A tabela

| # | Capacidade | Docker 29.1.3 | Podman 4.9.3 | Delonix 0.53.0 | Como foi medido |
|---|---|---|---|---|---|
| 1 | Daemon residente | **Sim** — `systemctl is-active docker` → `active` | Não — 0 processos | Não — 0 processos | `systemctl is-active docker`; `pgrep -c podman`; `pgrep -cf 'delonix (netns\|serve)'` |
| 2 | Corre sem privilégio | Não sem o grupo `docker`/daemon: `permission denied … unix:///var/run/docker.sock` | Sim (userns, `id -u` → 0) | Sim (userns, `id -u` → 0) | `sudo -u nobody docker ps`; `<eng> run --rm alpine id -u` |
| 3 | `run` básico | `ok` | `ok` | `ok` | `<eng> run --rm alpine echo ok` |
| 4a | **Latência de `run --rm`, default de cada um** (mediana de **10**) | **208 ms** (bridge) | **268 ms** (slirp) | **89 ms** (host) | `date +%s%N` à volta de `<eng> run --rm alpine true` |
| 4b | **Latência com rede isolada por container** (mediana de 10) | **208 ms** (a mesma — bridge JÁ é isolada) | **268 ms** (a mesma) | **333 ms** (`--net <rede>`) | idem, com `delonix network create` antes |
| 5 | Código de saída de um container `-d` | `42` | `42` | `42` (`Exited (42)`) | `run -d … sh -c 'exit 42'`, depois `inspect -f '{{.State.ExitCode}}'` / `container ls -a` |
| 6 | **Mudar portas a quente, sem recriar** | **Não** — `unknown flag: --publish-add` | **Não** — `Error: unknown flag: --publish-add` | **Sim** — `port 19312->80/tcp hot-published`, **PID 4118 → 4118**, `container port` lista as duas | `<eng> update --publish-add 19312:80 <c>`; PID lido no `describe` antes e depois |
| 7 | **stdin de um pipe, SEM flags** | **Não chega** — saída vazia | **Não chega** — saída vazia | **Chega** — `oi` | `echo oi \| <eng> run --rm alpine cat` |
| 7-ctl | idem, **com `-i`** | `oi` | `oi` | (não tem a flag; já chega sem ela) | `echo oi \| <eng> run -i --rm alpine cat` |
| 7b | **TTY quando o CHAMADOR tem um, sem flags** | **Não** — `NOTTY-IN`/`NOTTY-OUT` | **Não** — `NOTTY-IN`/`NOTTY-OUT` | **Sim** — `TTY-IN`/`TTY-OUT` | os três sob `script -qec … /dev/null`, com `test -t 0`/`test -t 1` dentro do container |
| 7c | **Forçar um TTY quando o chamador NÃO tem** | **Sim** (`-t`) — `TTY-OUT` | **Sim** (`-t`) — `TTY-OUT` | **Não, e não há flag** — `NOTTY-OUT`; `-t` → `error: unexpected argument '-t' found` | `echo x \| <eng> run -t --rm alpine sh -c 'test -t 1 …'` |
| 8 | microVMs declarativas | Não (`docker vm` não existe) | `podman machine` — «Manage a virtual machine», uma VM **para** correr containers, não um workload | `delonix vm` — «Declarative microVMs: create/ls/stop/rm/status» | `<eng> vm --help` / `podman machine --help` |
| 9 | `docker-compose.yml` nativo | `não verificado` — `unknown command: docker compose` (o plugin não está instalado; existe como plugin oficial) | `não verificado` — `looking up compose provider failed` (o `podman-compose` não está instalado) | Sim, embutido — «Native `docker-compose.yml` support (up/down/ps/logs/config)» | `<eng> compose version` / `delonix compose --help` |

---

## Onde o Delonix ganha, e porquê

**Latência no default: 89 ms contra 208 e 268** (linha 4a) — 2,3× mais rápido que o docker,
3,0× que o podman. Não é afinação, é arquitectura: não há daemon a contactar nem serviço a
acordar. O `run` é um processo que faz `clone()` e sai. **Mas leia-se a linha 4b antes de
citar este número** — parte da margem é o default ser `host`, e não uma rede isolada.

**Reconfiguração a quente (linha 6) continua a ser a diferença de fundo.** No docker e no
podman mudar uma porta obriga a recriar o container; aqui o dataplane não pertence ao ciclo
de vida do processo, e o PID prova-o — **4118 antes e 4118 depois**, com as duas portas
activas. É a única linha em que os outros dois respondem `unknown flag`.

**stdin e terminal chegam por omissão** (linhas 7 e 7b). Docker e podman **destacam** o
stdin: sem `-i` o `cat` não recebe nada, e sem `-t` não há terminal mesmo quando o chamador
tem um. O Delonix herda os dois, e é por isso que não tem as flags — o caso que elas servem
já é o comportamento normal.

## Onde o Delonix perde

**Com rede isolada por container é o MAIS LENTO dos três, e por uma margem larga** (linha
4b): **333 ms** contra 208 do docker e 268 do podman — 60 % mais lento que o docker, no
mesmo teste em que o default ganhava por 2,3×. A comparação justa é esta, porque a `bridge`
do docker **já** dá ao container a sua própria netns e o seu próprio IP; o default `host` do
Delonix não dá. O custo é o re-exec `nsenter … ip netns exec` que a arquitectura rootless
exige para entrar na netns do holder — duas passagens do binário em vez de uma. É o preço de
não haver daemon privilegiado, e até aqui nunca tinha sido medido.

**Não consegue forçar um TTY** (linha 7c). Quando o chamador não tem terminal — CI, um pipe,
um cron — `docker run -t`/`podman run -t` dão na mesma um pty ao container, e o Delonix não
tem como (`-t` é recusado como argumento desconhecido). Conta para o que muda de
comportamento com `isatty`: cores, barras de progresso, um `read` que espera um terminal.
Estreito, mas real, e **fazível** — o motor já tem a maquinaria de pty do modo consola.

## Porque a bateria de 2026-08-10 (delonix 0.46.0) foi RETIRADA

A versão anterior desta tabela foi medida a 2026-08-10 contra o **0.46.0**, e os seus números
de latência eram: docker **1 406 ms**, podman **1 351 ms**, delonix **640 ms**. Nesta corrida,
com **os mesmos docker 29.1.3 e podman 4.9.3**, na mesma distro e no mesmo kernel, os três
deram **208 / 268 / 89 ms** — todos cerca de **6× mais rápidos**.

Três motores a acelerarem seis vezes ao mesmo tempo não é uma melhoria de nenhum deles: é a
bancada. Aqueles números mediam a contenção da VM naquele momento, não as ferramentas. O
rácio qualitativo sobreviveu (o Delonix ganha no default), os valores absolutos não valiam
nada, e a linha 4b — a comparação que interessa — nunca chegou a ser corrida.

Duas afirmações dessa bateria também não sobreviveram:

1. **«stdin interactivo NÃO existe»** era o **inverso** do comportamento real, e é hoje a
   linha 7. O defeito foi de método: aquela célula não foi executada como as outras, foi um
   `run --help | grep -E '^\s+-i,'`. Mediu a ausência de uma **flag** e concluiu a ausência
   da **capacidade** — no documento que promete que cada linha foi corrida.
2. **Os «dois achados» eram um só.** O primeiro dizia que `--net host` com `-p` falha por a
   combinação ser contraditória; não é — `--net host` é o modo **por omissão** e `-p` sobre
   ele é o caminho slirp-por-container normal e suportado
   ([`container.rs:3466`](../crates/delonix-runtime-bin/src/cmd/container.rs)). O que fez o
   `add_hostfwd` falhar foi o segundo achado: um `slirp4netns` órfão a segurar a porta. O
   sintoma foi arquivado como bug independente, com um raciocínio que o código contradiz.

## Um achado apanhado a montar esta bancada

**`delonix vm create` não tem forma de dimensionar o disco** — não há `--disk-size` nem
equivalente, e o overlay herda os 2,4 GiB de raiz da golden. Instalar `docker.io` e `podman`
dentro encheu-a a 100 % e o `apt` morreu a meio com `No space left on device`, deixando o
`dpkg` por configurar. A saída foi parar a VM, `qemu-img resize +14G` à mão e voltar a
arrancar (o `growpart` do cloud-init faz o resto no boot seguinte). É um gap real, sem
fronteira de privilégio nova pelo meio.

Verificado de passagem, e **não** é bug: o aviso de cgroup delegation vai para **stderr**, não
polui o stdout. Um `run … | cat` devolve exactamente a saída do container.

## O que ficou por medir

- `docker compose` e `podman-compose` (linha 9): os plugins não estão instalados nesta VM.
  Marcado `não verificado` em vez de convertido numa afirmação sobre as ferramentas.
- Desempenho de `pull` e de `build`, e qualquer coisa com volumes ou com tráfego entre
  containers: fora do âmbito desta passagem, que foi deliberadamente curta e executável de
  uma vez.
- A decomposição dos 333 ms da linha 4b (quanto é o re-exec, quanto é o holder, quanto é o
  IPAM): sabe-se o total, não a repartição.

# Docker × Podman × Delonix — comparação medida

> **O que este documento é.** Uma tabela em que **cada linha foi executada**, nas três
> ferramentas, na mesma máquina, no mesmo dia. Não decide quem é melhor: mostra o que cada
> um faz, incluindo aquilo em que o Delonix é **pior**.
>
> **O que não é.** Uma análise de funcionalidades lida do `--help`. Isso já existe em
> [`paridade-docker-podman.md`](paridade-docker-podman.md), que aliás começa por dizer que
> não mediu nada. Aqui, uma célula sem medição diz `não verificado` — nunca uma afirmação.

| Campo | Valor |
|---|---|
| Data | 2026-08-10 |
| Onde | VM libvirt, 4 vCPU / 4 GiB, Ubuntu 24.04, kernel 6.8.0-136-generic |
| Porquê numa VM | O host de desenvolvimento **não tem docker nem podman**, e instalá-los ali seria mexer numa máquina com produção a correr. Sem as três no mesmo sítio, duas colunas seriam `não verificado` e a comparação não valia nada. |
| Docker | 29.1.3 (`docker.io` do Ubuntu, daemon `systemd` activo) |
| Podman | 4.9.3 (rootless) |
| Delonix | 0.46.0 (rootless) |

> **Nota de montagem, e ela importa para a justiça da comparação.** A primeira bateria deu
> o Delonix a falhar tudo (`EPERM`, `Exited (126)`). Não era o motor: a golden aplica um
> perfil AppArmor a `/usr/local/bin/delonix`, com
> `kernel.apparmor_restrict_unprivileged_userns=1` a negar o `unshare` a qualquer outro
> caminho — e eu tinha corrido `/tmp/delonix`. Os números de latência dessa bateria mediam
> um `run` que rebentava. Foram deitados fora. Cada ferramenta tem de ser medida
> configurada como é suposto, ou a tabela mente a favor de quem estiver bem instalado.

---

## A tabela

| # | Capacidade | Docker 29.1.3 | Podman 4.9.3 | Delonix 0.46.0 | Como foi medido |
|---|---|---|---|---|---|
| 1 | Daemon residente | **Sim** — `systemctl is-active docker` → `active` | Não — 0 processos | Não — 0 processos | `systemctl is-active docker`; `pgrep -c podman`; `pgrep -cf 'delonix (netns\|serve)'` |
| 2 | Corre sem privilégio | Não sem o grupo `docker`/daemon: `permission denied … unix:///var/run/docker.sock` | Sim (userns) | Sim (userns) | `sudo -u nobody docker ps`; `podman run --rm alpine id -u`; `delonix container run --rm alpine id -u` |
| 3 | `run` básico | `ok` | `ok` | `ok` | `<eng> run --rm alpine echo ok` |
| 4 | **Latência de `run --rm` (mediana de 3)** | **1 406 ms** (1381/1493/1406) | **1 351 ms** (1421/1340/1351) | **640 ms** (684/626/640) | cronometrado com `date +%s%N` à volta de `<eng> run --rm alpine true`, mesma VM, imagem já local nos três |
| 5 | Código de saída de um container `-d` | `42` | `42` | `42` (`Exited (42)`) | `run -d … sh -c 'exit 42'`, depois `inspect -f '{{.State.ExitCode}}'` / `container ls -a` |
| 6 | **Mudar portas a quente, sem recriar** | **Não** — `unknown flag: --publish-add` | **Não** — `Error: unknown flag: --publish-add` | **Sim** — `port 19312->80/tcp hot-published`, **PID 5985 → 5985**, `container port` passa a listar as duas | `<eng> update --publish-add 19312:80 <c>`; PID lido no `describe` antes e depois |
| 7 | **stdin interactivo (`-i`)** | **Sim** — `echo oi \| docker run -i --rm alpine cat` → `oi` | **Sim** → `oi` | **NÃO — a flag não existe** (0 ocorrências de `-i`/`--interactive` no `run --help`) | `<eng> run --help \| grep -E '^\s+-i,\|--interactive'` |
| 8 | microVMs declarativas | Não (o Docker Desktop usa uma VM para correr containers, que é outra coisa) | `podman machine` — uma VM **para** correr containers, não um workload | `delonix vm` — «Declarative microVMs: create/ls/stop/rm/status» | `<eng> vm --help` / `podman machine --help` |
| 9 | `docker-compose.yml` nativo | `não verificado` — o plugin `docker compose` **não está instalado** nesta VM (`unknown command`); existe como plugin oficial | `não verificado` — `looking up compose provider failed` (o `podman-compose` não está instalado) | Sim, embutido — «Native `docker-compose.yml` support (up/down/ps/logs/config)» | `docker compose version`; `podman compose version`; `delonix compose --help` |

---

## Onde o Delonix ganha, e porquê

**Latência: menos de metade** (640 ms contra 1 351–1 406 ms). Não é afinação — é arquitectura:
não há daemon a contactar nem serviço a acordar. O `run` é um processo que faz `clone()` e
sai.

**Reconfiguração a quente (linha 6) é a diferença de fundo.** No docker e no podman mudar
uma porta obriga a recriar o container; aqui o dataplane não pertence ao ciclo de vida do
processo, e o PID prova-o — **5985 antes e 5985 depois**, com as duas portas activas. É a
única linha desta tabela em que os outros dois respondem `unknown flag`.

## Onde o Delonix perde

**stdin interactivo não existe** (linha 7). `docker run -i` e `podman run -i` passam o stdin
para o container; o `delonix container run` **não tem a flag**. Para um `cat`, um `psql`, um
`sh` lido de um pipe, os outros dois fazem e este não. É a lacuna mais visível desta tabela
e não tem atenuante.

## Dois achados apanhados a montar isto

1. **`--net host` com `-p` falha com o JSON cru do slirp.** `container run --net host -p
   18085:80` devolve
   `slirp hostfwd failed: {"error":{"desc":"bad request: add_hostfwd: slirp_add_hostfwd failed"}}`,
   enquanto `-p` **sem** `--net host` funciona. A combinação é contraditória (com `--net
   host` não há netns próprio onde publicar), e devia ser recusada com um erro que a explique
   — é a mesma classe do preflight de portas privilegiadas que a v0.36.1 acrescentou.
2. **Sete `slirp4netns` órfãos** ficaram das tentativas falhadas, e um deles segurava a porta
   18085 — o que fez a tentativa seguinte no mesmo porto falhar por uma razão diferente da
   original. É o caso conhecido do reap de slirps órfãos, aqui visto ao vivo.

## O que ficou por medir

- `docker compose` e `podman-compose` (linha 9): os plugins não estão instalados nesta VM.
  Marcado `não verificado` em vez de convertido numa afirmação sobre as ferramentas.
- Desempenho de `pull`, build, e qualquer coisa com volumes ou rede entre containers: fora
  do âmbito desta passagem, que foi deliberadamente curta e executável de uma vez.

# 03 — Containers e imagens (ciclo rootless)

SUT: `delonix 5.1.1` @ `bca34f6c3`. Modo: **rootless** (uid 1000). Imagem: `nginxinc/nginx-unprivileged:1.30`, `ubuntu:22.04`. Recursos: prefixo `relc-`.

## Ciclo completo verificado (verificação independente, não só a resposta do Delonix)

| # | Caso | Comando | Verificação independente | Observado | Estado |
|---|---|---|---|---|---|
| 1 | pull + run detached + porta + limites | `run -d --name relc-web -p 18080:8080 --memory 128m --cpus 0.5 nginx-unprivileged` | `ps`; kernel | pull live de registry-1.docker.io (platform amd64); `Up`, id `5ad98df8ac21` | **PASSOU** |
| 2 | exec | `exec relc-web sh -c 'id -u; hostname'` | saída do processo | `EXEC_OK=101:relc-web` (uid 101 nginx, UTS ns = nome) | **PASSOU** |
| 3 | porta publicada + HTTP externo | `curl 127.0.0.1:18080` | log do nginx | `HTTP 200`; log mostra `10.0.2.2 ... "GET / HTTP/1.1" 200` (pedido chegou via slirp gw) | **PASSOU** |
| 4 | **limites efectivos no kernel** | ler `memory.max`/`cpu.max` da hierarquia | `/sys/fs/cgroup/.../dlx-5ad98df8.../` | `memory.max=134217728` (**=128 MiB**), `cpu.max=50000 100000` (**=0.5 core**) — efectivo, não só config | **PASSOU** |
| 5 | logs | `logs relc-web` | — | linhas do nginx + o GET do curl | **PASSOU** |
| 6 | bind mount RW | `run --rm -v $H:/data ubuntu ...` | ler ficheiro no host | container lê `host.txt`; host vê `cont.txt` escrito pelo container | **PASSOU** |
| 7 | wait + exit code | `run -d ... sh -c 'sleep 1; exit 42'; wait` | — | `wait` devolve **`42`** (código real propagado) | **PASSOU** |
| 8 | stop gracioso | `stop relc-web` | `ps -a` | `Exited (0)` (SIGTERM limpo) | **PASSOU** |
| 9 | start + persistência | `start relc-web`; curl | HTTP | `HTTP 200` (porta re-publicada, mesmo container) | **PASSOU** |
| 10 | restart | `restart relc-web`; curl | HTTP | `HTTP 200` | **PASSOU** |
| 11 | `--rm` | mount case usou `--rm` | `ps -a` | container removido ao sair | **PASSOU** |
| 12 | **rm + ausência de órfãos** | `rm -f relc-web relc-exit` | kernel/rede/ps | ps=0; **cgroup dir desaparecido**; **porta 18080 libertada** (slirp listener foi-se); curl recusado | **PASSOU** |

## Erros de entrada (classe de recusa correcta, sem efeito)

- Porta inválida `99999:80` → `error[DX-1307]`, exit 1, **0 containers criados** (ver `02-cli.md`).
- Nomes inválidos (`../`, `/`, espaço, vazio) → `error[DX-1000]`, exit 1, **0 leaks**.
- Imagem inexistente / exec em container inexistente → `error[DX-4101] no such container`, exit 4.

## Limites: efectivo vs aceite-e-ignorado (honestidade do motor)

`delonix system doctor` + `system resources` reportam com exactidão o que é efectivo neste host:

- **Efectivos:** `--memory`, `--cpus` (cgroup v2 `cpu memory pids` delegados) — confirmado no kernel acima.
- **DLX-RES-001:** `--cpuset` é **aceite e silenciosamente ignorado** (controlador cpuset não delegado) — o motor di-lo.
- **DLX-RES-002:** `--io-weight`/`--device-*` **recusados (exit 69)** — io não delegado.

Isto satisfaz o requisito §7/§8 de "verificar os limites efectivos, não apenas a configuração guardada": o motor não só aplica como declara o que não consegue aplicar.

## Preservação

Containers pré-existentes `kaeso-odoo` e `kaeso-db` permaneceram `Up` durante e após toda a campanha de containers (verificado: `ps | grep -c kaeso = 2`).

## PIDs — nota de requisito

Não há flag `--pids-limit`/`--pids` no `run`/`update` (decisão deliberada: `main.rs:1689` testa a sua ausência — é a grafia do Docker). O controlador `pids` **é** delegado e usado (default `pids.max=512` observado; compose honra `deploy.resources.limits.pids`). Logo o requisito §7 "limites de PIDs" está coberto por **compose/kind**, não pela flag ad-hoc do `run`. → ver DEF-001 para a contradição na documentação/UX.

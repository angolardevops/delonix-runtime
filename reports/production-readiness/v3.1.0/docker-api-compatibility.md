# Compatibilidade Docker Engine API — medida sobre `fa8c6297`

## Medição

| Campo | Valor |
|---|---|
| Servidor | `delonix serve docker-api --addr unix:///tmp/dxd.sock` (binário de `fa8c6297`) |
| Cliente | Docker CLI **29.8.0** real, `DOCKER_HOST` para o socket, `DOCKER_CONFIG` isolado |
| Estado | `DELONIX_ROOT`/`DELONIX_NET_RUNTIME_DIR` isolados |
| API anunciada | 1.43 (mínimo 1.24) |
| Socket | `srw-------` (medido), SO_PEERCRED mesmo euid |
| Matriz publicada pelo binário | 14 servidas / 12 recusadas com razão (`raw/docker-api-matrix.txt`) |
| Logs | `raw/docker-cli-live.txt`, `raw/dockerapi-server.log` |

## Docker CLI real → resultado

| Comando | rc | Estado | Observação |
|---|---|---|---|
| `docker version` | 0 | PASS | API 1.43 |
| `docker info` | 0 | PARTIAL | `Name` vazio |
| `docker images` | 0 | PASS | |
| `docker pull busybox` | 1 | UNSUPPORTED | `POST /images/create` recusado com razão |
| `docker create` | 0 | PARTIAL | vários `HostConfig.*` ignorados **com aviso** |
| `docker start` / `ps` / `inspect` | 0 | PASS | |
| `docker logs` | 1 | UNSUPPORTED | recusado com razão |
| `docker exec` | 1 | UNSUPPORTED | recusado com razão |
| `docker stop` / `start` / `kill` | 0 | PASS | |
| `docker wait` (após kill) | 0 | PASS | devolve 137 |
| `docker restart` / `rename` / `rm -f` | 0 | PASS | |
| **`docker run -d`** | **124** | **FAIL** | **pendura** (timeout 60 s); o container é criado |
| **`docker run --rm alpine echo hi`** | **1** | **FAIL** | `unable to upgrade to tcp, received 404`; `AutoRemove` ignorado → **container órfão fica** |
| `docker run -d --restart always` | 125 | DELIBERATE DIVERGENCE | recusado com razão escrita |
| `docker network ls` / `volume ls` / `stats` | 1 | UNSUPPORTED | recusados com razão |
| `docker compose *` | — | NOT TESTED | depende de `/networks` e `/volumes`, recusados |

## Defeitos encontrados ao vivo

| ID | Sev | Defeito | Evidência |
|---|---|---|---|
| G-027 | P1 | `docker run -d` não devolve o controlo | rc=124 em 60 s, reproduzido duas vezes (`dr1`, `dr3`); documentado em AGENTS.md como «limitação», mas é o comando mais usado |
| G-028 | P2 | `docker run --rm` falha e deixa containers órfãos (`AutoRemove` ignorado) | 2 containers `echo hi` ficaram no store |
| G-029 | P1 | **Estado divergente**: `docker ps` reporta `Up About a minute` para um `echo hi` que já terminou; `delonix container ls` diz `Exited (unknown)` para o mesmo id `b8d87aee9b8e`, e o outro aparece `Dead` | `raw/docker-cli-live.txt` + saída de `ps -a` na sessão |

## Veredicto por caso de uso

| Consumidor | Veredicto |
|---|---|
| Docker CLI para ciclo de vida manual (create/start/stop/rm) | PARTIAL — funciona |
| `docker run` (qualquer forma) | **FAIL** |
| Docker Compose | **UNSUPPORTED** (sem `/networks`, `/volumes`, `/images/create`) |
| Testcontainers / Dev Containers / GitLab Runner | **UNSUPPORTED** (sem pull, exec, logs, attach) |
| kind com provider docker | NOT TESTED |

Documentação que diz o contrário e está desactualizada: ver `stale-evidence.md`.

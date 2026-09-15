# Inventário técnico — `fa8c6297`

Fonte: leitura do código desta árvore (grep/wc/awk). Números de padrão são aproximados
e o método está indicado. Nada aqui foi inferido de documentação.

## 1. Workspace

15 crates, todas presentes no `Cargo.toml` raiz. **Nenhuma declara `[features]`.**

| Métrica | Valor | Método |
|---|---|---|
| LOC `src/**/*.rs` | 156 185 | `wc -l` |
| LOC `tests/*.rs` | 1 926 (11 ficheiros) | `wc -l` |
| `#[test]` / `#[tokio::test]` | 1 598 / 24 | grep (não conta `proptest!`) |
| `unsafe` em código | 291 | grep sem linhas de comentário |
| `#![forbid(unsafe_code)]` | 0 crates | grep |
| `pub fn` / `pub struct` / `pub enum` | 1 349 / 242 / 93 | grep (inclui `pub(crate)`) |

| crate | LOC | unsafe | #[test] | deps internas | privilégio / comandos externos |
|---|---|---|---|---|---|
| runtime-core | 5 287 | 11 | 71 | — | flock, `kill(pid,0)`, SO_PEERCRED |
| **runtime** | 11 742 | **150** | 95 | core | clone, pivot_root, ~36 mount/umount, setns, unshare, seccomp, cgroup v2, `newuidmap/newgidmap`, `ldconfig`, `iptables-nft` |
| runtime-bin | 90 618 (58%) | 57 | 866 | 13 crates | ~22 programas externos (virsh, qemu-img, ssh, systemctl, tcpdump, …) |
| image | 6 487 | 0 | 81 | core | overlay mount; reqwest; assinatura |
| net | 15 434 | 44 | 191 | net-rules, core | `ip`, `nft`, `nsenter`, `unshare`, `slirp4netns`, `wg`, `tc`, `bpftool`, CNI |
| net-rules | 607 | 0 | 15 | — | puro, zero deps |
| vm | 7 307 | 5 | 96 | core, net | `virsh`, `qemu-img`, `cloud-localds` |
| volume | 1 894 | 0 | 25 | core | `mount`, `losetup`, `mkfs.ext4`, `resize2fs` |
| cri | 6 331 | 21 | 40+2 | core, runtime, image, net | setns(net) no port-forward; re-exec do `delonix` |
| mgmt | 3 094 | 1 | 11+18 | 6 crates | re-exec do `delonix` |
| scan | 939 | 0 | 12 | core, image | — |
| security-runtime | 2 369 | 0 | 55 | core | nenhuma I/O |
| truenas | 843 | 0 | 8 | core | HTTP |
| proxmox | 1 897 | 0 | 22 | core, vm | HTTP |
| mcp | 1 336 | 2 | 10+4 | 6 crates | re-exec do `delonix`, stdio |

Funções > 300 linhas no caminho privilegiado: `spawn` (607, `delonix-runtime/src/lib.rs:5197`),
`exec_with` (349, `:6155`), `container_init` (334, `:2850`); na CLI `cmd_run` (1 042,
`cmd/container.rs:2955`).

## 2. Superfícies públicas

| Superfície | Medido |
|---|---|
| CLI | 245 folhas (`scripts/cli_baseline.tsv`), 34 grupos de topo |
| Docker Engine API | anuncia 1.43 (min 1.24); **14 rotas servidas**, **12 recusadas com razão** (`exec`, `attach`, `logs`, `events`, `build`, `networks`×3, `images/create`, `images/{n}/json`, `stats`, `volumes`) |
| CRI RuntimeService | implementado salvo `UpdateContainerResources`, `CheckpointContainer`, `GetContainerEvents` (Unimplemented); `Attach` só saída; `UpdateRuntimeConfig` só regista |
| CRI ImageService | 5/5 implementados |
| Kinds | 19 (todos `*.delonix.io/v1alpha1`); 13 convergem |
| Compose | allowlist a 1 nível (7 topo, 28 serviço); **sub-chaves aninhadas não verificadas** (ver gaps) |
| Management API | unix socket, 32 caminhos / 40 pares método+caminho, SO_PEERCRED mesmo euid |
| MCP | stdio apenas; 12 READ, 1 SAFE_WRITE, 1 DISRUPTIVE |
| Env vars | ~60 `DELONIX_*` de runtime (lista em §8 do inventário bruto) |

## 3. Fronteiras de privilégio

| Processo | Namespaces / privilégio |
|---|---|
| CLI | namespaces do host, uid real |
| container init | mnt+pid+uts+ipc+net+user (rootless); 14 caps por omissão; `--privileged` = todas + sem seccomp; `no_new_privs` por omissão |
| netns pin | `unshare --user --map-root-user --net --mount`; caps plenas só no seu userns |
| netns control | `nsenter` para o pin; socket de controlo, DNS, DHCP, nft |
| slirp4netns | host, uid do motor |
| supervisor / `--rm` watcher / log shim | `fork`+`setsid` no host |
| re-execs mapeados (`__rmtree`, `__ovlhold`, …) | userns novo com o mapa subuid completo |
| CRI / mgmt / MCP | mesmo uid; mutações por re-exec da CLI |
| `vm bridge` | exige root real; **sem verificação de uid no código** (`cmd/vmbridge.rs:275`) |

| Socket | Modo | Verificação |
|---|---|---|
| CRI `/run/delonix-cri.sock` | 0600 **após** bind, erro do chmod ignorado | SO_PEERCRED euid |
| mgmt / docker-api | idem | SO_PEERCRED euid |
| holder `control.sock` | 0600, dir 0700 | SO_PEERCRED euid |
| slirp por container `/tmp/delonix-slirp-<pid>.sock` | **medido `srwxr-xr-x` com umask 022**, caminho previsível em `/tmp` partilhado | nenhuma |

## 4. Suporte OCI Runtime relevante

| Requisito | Estado lido no código |
|---|---|
| hooks OCI | **não executados** (hooks CDI são lidos e ignorados com aviso) |
| seccomp de ficheiro | suportado; falha de parse → exit 126 |
| AppArmor | suportado; perfil não carregado é recusado |
| masked/readonly paths | aplicados; **falha de mount só avisa** |
| rlimits | 8 nomes; **nome desconhecido e erro de `setrlimit` ignorados** |
| mount propagation | rslave/rshared/private por mount |
| read-only recursivo (rro) | **não suportado** no caminho de criação; o `recursive_read_only` do CRI não é lido |

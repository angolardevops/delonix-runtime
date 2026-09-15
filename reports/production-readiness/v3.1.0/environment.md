# Ambiente analisado — fase 1 (inventário + baseline)

Medido a 2026-09-15. Tudo o que está aqui foi lido de um comando executado nesta sessão.

## Código

| Campo | Valor |
|---|---|
| Commit | `fa8c6297c19b7429e60644c5b950724254e6e9e2` (= `origin/main`) |
| Descrição git | `v3.1.0-26-gfa8c6297` — **26 commits depois da tag v3.1.0** |
| Tag v3.1.0 | `6680745557eb84bd005c93c3d0bdabe0e5e90db2` (publicada 2026-09-11T12:45Z) |
| Versão no `Cargo.toml` | `3.1.0` (igual à da tag — duas builds diferentes com o mesmo número) |
| Worktree | `/tmp/wt-prodready`, branch `auditoria/prod-readiness-v3.1`, limpo |
| Binário usado nas medições ao vivo | construído desta árvore (`commit: fa8c6297c`) |
| Binário instalado no host | `delonix 3.1.0`, `commit: 668074555` (= tag) — **não usado** |

> Nome do directório: `v3.1.0` por pedido. O commit analisado **não é** a tag v3.1.0;
> cada resultado deste relatório refere-se a `fa8c6297`.

## Toolchain

| | |
|---|---|
| rustc / cargo | 1.96.0 (fixado em `rust-toolchain.toml`) |
| protoc | libprotoc 25.1 |
| cargo-deny | 0.20.2 |
| cargo-audit | **ausente** — `NOT RUN` |
| cargo-llvm-cov | **ausente** — `NOT RUN` |

## Host

| | |
|---|---|
| SO | Zorin OS 18.1 |
| Kernel | Linux 7.0.0-31-generic x86_64 |
| CPU / RAM / disco | 32 vCPU / 30 GiB (16 usados) / raiz 87% ocupada (117 GiB livres) |
| cgroups | v2 (`cgroup2fs`) |
| cgroup desta sessão | `user@1000.service/app.slice/app-com.anthropic.Claude-*.scope` |
| LSM | `lockdown,capability,landlock,yama,apparmor,ima,evm` — AppArmor; **sem SELinux** |
| userns | `unprivileged_userns_clone=1`, `apparmor_restrict_unprivileged_userns=0`, `max_user_namespaces=113359` |
| subuid/subgid | `walter:100000:65536` |
| KVM | `/dev/kvm` presente, utilizador no grupo `kvm` |
| nftables | v1.0.9 |
| eBPF | `bpftool` presente |
| slirp4netns / pasta / passt | 1.2.1 / presente / presente |
| runc / crun | 1.5.1 / **ausente** |
| Docker / Podman | 29.8.0 (**daemon activo**) / 4.9.3 |
| critest / crictl | v1.36.0 / v1.36.0 |
| kubectl / kind | v1.36.4 / ausente |
| CNI plugins | `/usr/lib/cni` (bridge, host-local, loopback, firewall, …); `/opt/cni/bin` ausente |
| cloud-hypervisor / virsh / qemu-img | v53.0 / libvirt 10.0.0 / presente |
| WireGuard | `wg` presente |

## Restrições que condicionam as fases seguintes

1. **Host partilhado com produção** (containers reais do utilizador, Docker e Podman
   activos). Todas as medições ao vivo usam `DELONIX_ROOT` **e** `DELONIX_NET_RUNTIME_DIR`
   isolados. Nada de `prune`, respawn do holder real ou `--force` sobre estado alheio.
2. **Sem SELinux** — toda a validação SELinux fica `ENVIRONMENT BLOCKED` neste host.
3. **Só x86_64** — nenhum artefacto aarch64 existe para validar.
4. **Rootful** exige `sudo` sobre um host de produção; não foi exercitado nesta fase.

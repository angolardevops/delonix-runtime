# Campanha de testes pré-release — SUT e ambiente

**ID da campanha:** `relcamp-bca34f6c` · prefixo de recursos criados: `relc-` · rede: `relc-net`
**Data de arranque:** 2026-10-23

## 1. Sistema sob teste (SUT)

| Campo | Valor |
|---|---|
| Repositório | `angolardevops/delonix-runtime` (público, Apache-2.0) |
| Directório | `/home/walter/workspaces/NgolaCloud/projects/delonix-runtime` |
| Branch | `develop` |
| Commit | `bca34f6c3cfc366e32e4fb5650fbaf541cb46eed` |
| Working tree | **limpo** no arranque (`git status --short` = 0 linhas) — o SHA representa o estado testado |
| Versão anunciada | `delonix 5.1.1` (`commit: bca34f6c3 (+243 commits since v5.1.1)`, built 2026-10-23) |
| **Binário testado** | `target/release/delonix` construído **neste commit** (`cargo build --release -p delonix-runtime-bin`, 3m17s) |
| **Digest (sha256)** | `725112b6454f6431bf6229137f01a10538e012cf64483a89fb9edd986213e328` |
| Método de instalação | build local a partir da árvore; **NÃO** o binário em `~/.local/bin/delonix` (esse é `f49d3570c`, de 2026-10-18 — stale) |

> **Regra anti-mistura.** Todos os testes correm o binário recém-construído em
> `target/release/delonix` (digest acima). O binário instalado em `~/.local/bin`
> (commit `f49d3570c`) só foi usado para o inventário inicial de recursos
> pré-existentes e **não** conta como SUT.

## 2. Ambiente

| Campo | Valor |
|---|---|
| Host | `delonix` |
| OS | Zorin OS 18.1 (base Ubuntu 24.04 "noble") |
| Kernel | `7.0.0-38-generic` x86_64 |
| CPU | 20 threads |
| RAM | 62 GiB total, ~30 GiB disponível |
| Disco `/` | 938 G, **95% usado, 45 G livres** — restringe pulls grandes, builds de imagem cloud e VMs |
| Privilégios | utilizador `walter` (uid 1000); grupos: `sudo`, `docker`, `kvm`, `libvirt`, `render`, `ollama` |
| cgroups | **v2 unificado**; controladores delegados ao utilizador: **`cpu memory pids`** (io/cpuset **não** delegados) |
| Rootless | `subuid/subgid` = 100000:65536; `unprivileged_userns_clone=1`; `apparmor_restrict_unprivileged_userns=0` → userns rootless OK |
| TMPDIR | `/tmp` |

### Ferramentas auxiliares

| Presente | Ausente |
|---|---|
| `runc`, `nft`, `iptables`, `slirp4netns`, `newuidmap` | `crun`, `skopeo` |
| `kind`, `kubectl`, `kubeadm` | — |
| `virsh`, `qemu-system-x86_64`, `cloud-hypervisor` | — |
| `mkcert`, `cloudflared`, `ngrok`, `pinggy` | — |
| **`docker` e `podman` AUSENTES** (esperado: delonix é o runtime) | — |

### Providers de VM

| Provider | Estado |
|---|---|
| libvirt/KVM | **disponível** (`virsh qemu:///system`): VM `opnsense` a correr, `pve92` desligada; muitas redes de lab |
| Cloud Hypervisor | binário presente |
| Proxmox | appliance `pve92` (desligada) em libvirt; alvo PVE documentado na memória do projecto |
| OpenStack | não disponível no host |

### Conectividade

- `registry-1.docker.io/v2/` → HTTP 401 (alcançável; 401 = sem auth, esperado)
- `github.com` resolve (140.82.121.3)
- Online — testes offline (secção 13) exigem bloqueio egress deliberado no ambiente de teste

## 3. Recursos PRÉ-EXISTENTES — a PRESERVAR (não tocar)

> A campanha **não** remove nem altera nada abaixo. Só mexe em recursos com prefixo `relc-`.

**Containers delonix a correr (~1h):**

- `kaeso-odoo` (`kaeso-odoo:16`, `127.0.0.1:8069->8069`) — stack Odoo viva
- `kaeso-db` (`pgvector/pgvector:pg15`) — na rede `kaeso-net`

**Redes delonix:** `demos-net`, `k8s-net`, `kaeso-net` (em uso), `kambacode-odoo20-net`, `kind`, `nsnet`, `pnet`

**libvirt:** VM `opnsense` (a correr), `pve92` (desligada); redes `br-lan/br-dev/br-backup/br-dmz/br-wan/br-pmx-*/cluster1`

**Serviço do sistema:** `delonix-engine prod --bind 0.0.0.0:9443` (pid 1682) — não é o SUT; não reiniciar

**Imagens já em cache (reutilizáveis sem pull):** `hello-world`, `nginxinc/nginx-unprivileged:1.30`, `redis:7.4-alpine`, `ubuntu:22.04`, `python:3.13-slim`, `rabbitmq:4.0-management-alpine`, `mariadb:11`, `wordpress:6.7`, `kindest/node:v1.34.0`, `postgres:17-alpine`

## 4. Limites e política da campanha

- Imagens pequenas preferidas (hello-world, nginx-unprivileged, redis-alpine) — disco a 95%.
- Sem pulls/builds grandes de imagem cloud sem necessidade; sem encher o disco.
- Sem publicar em registries públicos; push/pull só contra registry local.
- Recursos exclusivos com prefixo `relc-`; limpeza verificada ao fim de cada percurso.
- Nenhuma operação destrutiva sobre `kaeso-*`, libvirt lab, ou o serviço `delonix-engine prod`.

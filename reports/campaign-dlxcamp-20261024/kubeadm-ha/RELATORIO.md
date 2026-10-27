# Ronda kubeadm offline/HA — §13 da campanha dlxcamp-20261024

**Data:** 2026-10-24 · **Branch:** develop (HEAD 34f7afd5 no fim) · **Binário:** delonix 5.1.1 (+260 commits desde v5.1.1, cópia estável no scratchpad) · **Host:** rootless-first, libvirt + Cloud Hypervisor, 20 CPU / 62 GiB.

## Veredicto

O percurso `delonix cluster kubeadm` provisiona e faz bootstrap de um **plano de controlo HA completo** (3 control-planes com etcd stacked + 1 worker + HAProxy), mas **nenhum nó chega a Ready**: um **bug no caminho CRI** (`compose_command`) impede qualquer CNI que sobreponha o `command` de uma imagem com `ENTRYPOINT`. Além disso, `--cni-offline` **não** torna o bootstrap offline. Os critérios 1 e 2 ficam **bloqueados**; o critério 3 (falha/recuperação HA + destroy limpo) foi **cumprido**.

| Critério | Estado | Nota |
|---|---|---|
| 1. Nós Ready, offline/egress bloqueado | ❌ BLOQUEADO | nós NotReady (bug CNI); offline inviável (imagens flannel vêm de ghcr.io) |
| 2. Endpoint HA + CNI + Pod-to-Service/DNS | ⚠️ PARCIAL | HAProxy activo e a servir a API ✔; CNI/DNS/app ✗ (bug CNI) |
| 3. Falha de CP + recuperação + destroy limpo | ✅ CUMPRIDO | API 8/8 com 1 CP em baixo; etcd rejoin 3/3; virsh limpo, dono preservado |

## O que FUNCIONA (provado ao vivo)

- Provisionamento de 5 VMs a partir de `k8s-golden` (3×CP, 1×worker, 1×HAProxy) em libvirt NAT (rede `dlxha-cfdff4dd`, virbr4, 10.240.0.0/16 — sem colisão com pod/service).
- `kubeadm init` + **join HA de 3 control-planes** com **etcd stacked (quórum 3/3 Running)** + join do worker. Todos os nós registados `v1.34.12`, runtime `delonix://5.1.1`.
- Componentes do plano de controlo todos `Running`: etcd ×3, kube-apiserver ×3, kube-controller-manager ×3, kube-scheduler ×3, kube-proxy ×4.
- **Endpoint HA = HAProxy** (NÃO kube-vip — este motor não tem kube-vip; `ensure_haproxy` em cluster.rs:3973). kubeconfig server = `https://10.240.84.84:6443` (VM `dlxha-lb`). `kubectl`/`/readyz` servidos através dele.
- **Falha/recuperação HA** (ver `evidence/ha-failover.txt`): `delonix vm stop dlxha-cp1` → `/readyz` 8/8 OK via HAProxy (quórum 2/3); `delonix vm start dlxha-cp1` → volta com o mesmo IP, etcd reentra (3/3), API disponível na recuperação (7/8, 1 blip).
- **Destroy limpo**: `cluster destroy --force` removeu 5 VMs + rede + contexto kube; `virsh` sem VMs/redes/discos dlxha; VMs do dono (opnsense, pve92) e `k8s-golden` intactas.

## BUG 1 (BLOQUEADOR) — CRI: o `command` do pod é apensado ao ENTRYPOINT da imagem

**Sintoma:** com flannel (`--cni flannel --experimental-cni --cni-offline`), todos os nós ficam `NotReady`. Os pods `kube-flannel-ds-*` ficam eternamente em `Init:1/2`: o init container `install-cni` (spec `command:[cp] args:[-f, /etc/kube-flannel/cni-conf.json, /etc/cni/net.d/10-flannel.conflist]`) **corre `/opt/bin/flanneld`** (o ENTRYPOINT da imagem `flannel:v0.28.10`) em modo etcd, como daemon que nunca sai → init nunca completa → nó nunca Ready.

**Reprodução mínima (caminho exacto kubelet→delonix-cri):** Pod com `command:["echo"] args:["ENTRYPOINT_OVERRIDE_OK"]` sobre a imagem `flannel:v0.28.10` → correu `flanneld` (modo etcd), não o `echo`. O override foi **apensado ao**, não substituiu, o ENTRYPOINT.

**Raiz (vivo em develop HEAD):**
- `crates/contexts/delonix-compute/src/run.rs:294` `compose_command(entrypoint, cmd, user)`:
  ```rust
  let mut v = entrypoint.to_vec();          // começa SEMPRE pelo entrypoint da imagem
  if user.is_empty() { v.extend(cmd) }      // sem override → entrypoint + cmd da imagem
  else { v.extend(user) }                   // COM override → entrypoint + command do pod  (APENSA!)
  ```
- `run.rs:192`: `image_command = compose_command(&config.entrypoint, &config.cmd, &o.command)`.
- `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs:1861`: `o.command = rec.command.chain(rec.args)` e o CRI **nunca** define `o.entrypoint` (grep vazio em delonix-cri/src).
- Resultado: argv efectivo = `image.entrypoint ++ pod.command ++ pod.args`.

**Correcto (contrato CRI/K8s):** o `command` do pod **substitui** o `ENTRYPOINT`; os `args` substituem o `CMD`. O caminho docker-like (`build_record`/`--entrypoint`, run.rs:324-332) está correcto; o defeito é o CRI não mapear `pod.command → RunOpts.entrypoint` e `pod.args → RunOpts.command`.

**Impacto:** qualquer workload que sobreponha `command` numa imagem com `ENTRYPOINT` corre o programa errado. critest não apanha (usa imagens em que command==entrypoint ou sem entrypoint). Bloqueia flannel e, muito provavelmente, os passos de instalação de cilium/calico.

**Fix sugerido (fora do escopo desta sessão — não aplicado):** no CRI, `o.entrypoint = Some(rec.command)` quando `rec.command` não-vazio e `o.command = rec.args`; `build_record` já trata `Some(argv)` correctamente.

## BUG 2 — `--cni-offline` não cobre as imagens do CNI

`delonix cluster network fetch --cni flannel` só traz o **manifesto** (`net-profiles/kube-flannel-v0.28.10.yml`). O DaemonSet continua a referenciar `ghcr.io/flannel-io/flannel-cni-plugin:v1.9.1-flannel3` e `ghcr.io/flannel-io/flannel:v0.28.10`, **puxados ao vivo de ghcr.io** no bootstrap (eventos `Pulling`/`Successfully pulled`, não "already present"). A `k8s-golden` **não** pré-carrega estas imagens. Logo, com egress bloqueado o bootstrap do CNI **falha** — o critério "offline/egress bloqueado" é inatingível por este caminho. `cluster network fetch` precisaria de descarregar também as imagens OCI e o bootstrap de as importar no store CRI de cada nó (como `cluster load` faz no modo kind).

## Nota operacional — corrida de rebuild do dono

A 1ª tentativa abortou em `install_cli` com `DX-1000 resolving this CLI's own binary path: No such file or directory`: `std::env::current_exe().canonicalize()` falhou porque o dono recompilou `target/release/delonix` (mtime 15:24:07) **durante** o bootstrap, substituindo o inode em uso. Mitigação: correr sempre de uma **cópia estável** do binário (scratchpad), imune aos rebuilds. Fragilidade menor do produto: `cluster kubeadm` aborta duro se o seu binário for movido/substituído a meio (registado, não corrigido).

## Ponteiros de evidência

- `evidence/nodes.txt`, `evidence/etcd-pods.txt`, `evidence/kubeconfig-server.txt`, `evidence/haproxy-lb-ip.txt`, `evidence/ha-failover.txt`
- `bootstrap.log` (1ª tentativa, falha de binário), `bootstrap2.log` (2ª, chega a NotReady)

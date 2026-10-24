# 08 — Clusters (kind-mode) e kubeadm offline

SUT: `delonix 5.1.1` @ `bca34f6c3`. Imagem: `kindest/node:v1.34.0` (cache). Cluster: `relccluster`.

## Kind-mode (CLUSTER-1/2)

| # | Caso (§12) | Observado | Estado |
|---|---|---|---|
| criação | `cluster create --name relccluster` (sem Docker) | nó em ~1m07s control-plane + kindnet; cluster `up`, API :6443, kubeconfig escrito, contexto `delonix-relccluster` | **PASSOU** |
| nó Ready | `kubectl get nodes` | `relccluster-control-plane Ready v1.34.0` (containerd 2.1.3, no engine Delonix) | **PASSOU** |
| **app + tráfego** | deployment nginx + Service + **port-forward curl** | `rollout successfully`; pod `1/1 Running` (IP 10.244.0.5 kindnet); **Service HTTP 200 via port-forward** | **PASSOU** |
| DNS pod→service | `wget http://relcweb-svc.default.svc.cluster.local/` de um pod | página nginx obtida; CoreDNS 2/2 Running | **PASSOU** |
| remoção | `cluster destroy relccluster --force` | nós removidos, rede `dlx-relccluster` libertada, kubeconfig+contexto limpos, disco reclamado | **PASSOU** |

> **§12 cumprido onde importa:** não parei em "listar nós" — corri uma aplicação e provei
> tráfego **duas formas** (port-forward externo + DNS pod→service interno).

## Notas

- **Erro de método (meu):** numa primeira criação, uma limpeza minha com `grep relc` removeu
  acidentalmente o nó `relc-k8s-control-plane` a meio do `kubeadm init` → `exit 137` (SIGKILL).
  **Essa falha é minha, não do produto.** A 2ª corrida, limpa, chegou a Ready sem incidentes
  (control-plane 1m07s vs 4m20s abortados). Registado para não contaminar o veredicto.
- **UX (baixa):** `cluster destroy` exige `--force` fora de terminal (bom, fail-safe). Mas
  `cluster delete <name>` (palpite natural) devolve um erro confuso de kubeconfig em vez de
  "subcomando desconhecido; talvez `destroy`". Nota menor.
- **Carregar imagens locais:** `cluster load` existe (sem registry) — não exercitado.

## CLUSTER-2 (workers/mounts/portmap, config kind real) — PARCIAL

Testado single control-plane. `--workers`, mounts, port-mappings e a aceitação/recusa de um
`kind: Cluster` config real (`kindconfig.rs`) **não exercitados** nesta ronda → cobertura parcial.

## CLUSTER-3 (kubeadm offline HA, §13) — **BLOQUEADO**

`cluster kubeadm` existe (provisiona VMs da golden image + bootstrap kubeadm), mas exige a imagem
`k8s-golden` e espaço em disco para múltiplas VMs (host a 96%, 44 G livres). **Não executável** nesta
campanha sem a imagem golden e sem folga de disco. É uma lacuna de ambiente, não N/A — fica como
**risco aberto** para a avaliação de release (HA, kube-vip, perfis CNI, falha de CP, modo offline real).

## CLUSTER-4 (multi-cluster + kubeconfig preservado, §14) — NÃO EXECUTADO

Um único cluster criado (disco). Observação lateral: existem 3 contextos de kubeconfig antigos
(`delonix`, `lindo`, `mbandi-saurimo-32`) sem nós vivos — confirma a distinção escala-de-gestão vs
escala-de-execução que §14 pede, mas o teste de preservação ao adicionar/remover não foi corrido.

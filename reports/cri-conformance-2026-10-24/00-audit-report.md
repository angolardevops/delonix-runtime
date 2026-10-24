# Auditoria de conformidade CRI — delonix-runtime

> Medição independente contra `cri-tools critest` **v1.36.0** (upstream),
> motor `delonix-cri` **v5.1.1**, **rootless**, host Linux 7.0, cgroup v2.
> Data: **2026-10-24**. Reproduzir: `scripts/critest.sh`.

Este relatório **re-mede** a conformidade (o `docs/cri-conformance.md` publicado
dizia 79/103 e era de 2026-08-25 / motor v0.63.1 — vinte versões atrás) e
**corrige** duas falhas reais do `ImageService`.

## 1. Matriz de compatibilidade (configuração de referência)

| Eixo | Valor |
|---|---|
| delonix-runtime | v5.1.1 (`main` @ `ab4751ac`; fix em `fix/cri-image-repodigests-and-user`) |
| Contrato CRI | `runtime.v1` (`k8s.io/cri-api`), **34 RPCs** (RuntimeService + ImageService) |
| Suite | `cri-tools critest` **v1.36.0** (binário em `/usr/local/bin/critest`, `crictl` v1.36.0) |
| Kubernetes alvo | 1.36.x (cluster kubeadm real provado à parte — VM `delonix-vm-k8s:1.36`, k8s 1.36.4) |
| Host | Linux 7.0.0, x86_64 |
| cgroups | v2, driver `cgroupfs` (anunciado ao kubelet) |
| Modo | **rootless** (uid delegado; cgroup2 cpu/memory/pids delegados; io/cpuset não) |
| CNI | SDN nativa (slirp+holder por-UID); modo CNI disponível mas não exercitado aqui |
| Storage | overlay per-layer rootless + CAS em `$DELONIX_ROOT` |
| Cap ceiling | `DELONIX_CRI_CAP_CEILING=all` (a suite cria containers privilegiados) |

**Modo NÃO medido**: rootful. Várias falhas (AppArmor, mount propagation,
HostPID) podem mudar de sinal como root e numa VM com privilégio para o harness
montar no host — **não foi medido, portanto não se afirma**. É a decisão de
matriz a seguir a esta auditoria.

## 2. Resultado medido (baseline, antes da correcção)

```
Ran 104 of 122 Specs in 511.184 seconds
84 Passed | 20 Failed | 0 Pending | 18 Skipped
```

Progressão face ao documento publicado (mesma suite v1.36.0): **79 → 84**. Não
regrediu; melhorou cinco, graças a correcções que entraram depois de 2026-08-25
(eviction/`writable_layer`, `kubeadm reset`, `ReopenContainerLog` a seguir o
inode). O documento antigo estava desactualizado — esta é a razão pela qual
re-medir foi o primeiro passo.

> `critest` **não** é certificação CNCF e **não** exercita um kubelet real. É
> uma medição reproduzível de specs nomeados, nada mais. As cinco dimensões
> (compatibilidade CRI · cobertura critest · integração Kubernetes · robustez ·
> segurança) reportam-se em separado; esta auditoria cobre as duas primeiras.

## 3. As 20 falhas, classificadas

| # | Spec | Área | Classe | Nota |
|---|---|---|---|---|
| 1-3 | mount propagation `rprivate`/`rshared`/`rslave` | Mount | **Ambiente** | o `critest` monta no HOST (`operation not permitted`): precisa de root, não do motor |
| 4-9 | AppArmor (deprecated/novo/prefer, 6 specs) | Segurança | **Ambiente** | carregar perfis exige privilégio que rootless não tem |
| 10 | Image Identifier Consistency (registries diferentes) | Imagem | **Gap real** | container init sai 126: `could not mask '/proc/sysrq-trigger': ENOENT` — mascarar falha fail-closed quando o caminho não existe (runc tolera). Candidato à próxima sessão |
| 11 | Multiple Containers — should support network | Rede | **Arquitectura** | timeout; alcance do IP do pod a partir do host em rootless |
| 12 | Streaming — portforward in host network | Streaming | **Gap real** | portforward |
| 13 | Networking — port mapping só com container port | Rede | **Arquitectura** | IP do pod inalcançável do host em rootless |
| 14 | Security Context — HostPID | Segurança | **A investigar** | pode mudar como root |
| 15 | Security Context — PodPID (`shareProcessNamespace`) | Segurança | **Decisão** | toca o `spawn()`; Fase 3 do trabalho de pods |
| 16 | SeccompProfilePath — nil profile unconfined | Segurança | **Divergência deliberada** | motor aplica allowlist (`Seccomp:2`) em vez de `0`; mais restrito de propósito |
| 17 | Streaming — portforward [Conformance] | Streaming | **Gap real** | portforward |
| 18 | Image Manager — digest pulled and removed | Imagem | **Gap real → CORRIGIDO** | ver §4 |
| 19 | Image Manager — Uid/Username não vazios | Imagem | **Gap real → CORRIGIDO** | ver §4 |
| 20 | Container Mount Readonly — não-recursivo | Mount | **Ambiente** | o harness monta no host |

Resumo: **10 ambiente** (3 mount + 6 AppArmor + 1 readonly), **2 arquitectura
rootless** (rede), **1 divergência deliberada**, **1 decisão** (PodPID),
**1 a investigar** (HostPID), **5 gaps reais** → dos quais **2 corrigidos** aqui
(#18, #19) e **3 na fila** (portforward ×2, masking ENOENT #10).

Nenhuma falha de ambiente é reclassificada como "não aplicável": ou se corrige o
ambiente (medir rootful numa VM) ou o resultado desse eixo fica declarado
**incompleto**.

## 4. Correcção aplicada (#18, #19)

Causa-raiz confirmada por reprodução ao vivo (`crictl` contra `delonix-cri`
isolado — ver `02-evidence.md`), não por leitura:

- **#18** — uma referência por digest (`name@sha256:…`) era devolvida em
  `repo_tags`, e `repo_digests` vinha SEMPRE vazio. O `ImageStore` do motor
  guarda todas as referências numa só lista; a separação é trabalho da fronteira
  CRI, porque o proto tem campos distintos que o kubelet lê em separado. Fix:
  `partition_refs` parte a lista por `@` (uma referência com `@` é digest).
- **#19** — para um `USER` da forma `nome:grupo`, o `username` vinha com o grupo
  colado (`www-data:1004`); o spec compara com o nome só (`www-data`). Fix: tirar
  o grupo com o mesmo `split(':').next()` que o caminho numérico já usava.

Ambos em `crates/interfaces/delonix-cri/src/lib.rs`, com testes de regressão
(`image_user_*`, `partition_refs_*`) e a mapear-`Image` partilhada entre
`list_images`/`image_status` para não voltarem a divergir.

**Prova**: focused `critest -focus "Image Manager"` → **10 Passed | 0 Failed**
(os dois specs antes vermelhos agora verdes). Regressão: suite completa re-corrida
— ver `01-postfix-result.md`.

## 5. Follow-ups registados (cada um a sua sessão)

1. **Decisão de matriz**: re-medir rootful numa VM descartável (muda o sinal de
   ~10 falhas de ambiente), ou declarar o eixo rootful incompleto.
2. **portforward** (#12, #17) — gap real de streaming.
3. **masking ENOENT** (#10) — mascarar deve ignorar caminhos inexistentes como o
   runc, em vez de abortar o arranque do container.
4. **`shareProcessNamespace`** (#15) — toca o `spawn()`.
5. **Modelar digests no `ImageStore`** — persistir o manifest-digest por imagem
   (a correcção de #18 é correcta na fronteira, mas o store continua a tratar
   toda a referência como "tag"); e **`blob_path` usar o `base` do serviço** em
   vez de `default_root()` (coincidem só porque `DELONIX_ROOT` está no ambiente).

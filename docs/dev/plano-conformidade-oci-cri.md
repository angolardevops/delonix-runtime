# Plano de conformidade OCI + CRI — alinhar o Delonix com Docker/Podman

> **Objetivo.** Fazer o Delonix passar os **mesmos contratos de interoperabilidade** que o
> containerd/CRI-O (CRI) e qualquer runtime OCI (image/distribution/runtime) passam, para ser
> drop-in para o kubelet e para registries — e, num track separado e com ressalva, cobrir a
> Docker Engine API que ferramentas de terceiros chamam via `DOCKER_HOST`.
>
> **Princípio de medição.** Cada fase fecha contra uma **suite upstream com um número**, não
> "parece feito". Nunca divergência silenciosa (recusa explícita + razão). Teste que falha sem a
> correção e passa com ela, como o resto do projeto. Onde um teste não se aplica ao modelo
> (daemonless, rootless), nomeia-se **N/A** com a razão — não se inflaciona a percentagem.
>
> **Âmbito e a ressalva do `paridade-docker-podman.md §5`.** OCI (image/distribution/runtime) e
> CRI são contratos de interop onde conformidade total vale a pena — fazem do Delonix um
> substituto real. A Docker Engine API (Track D) é conveniência de ecossistema; o próprio projeto
> desaconselha perseguir paridade total com a Docker CLI, por isso o Track D cobre só o que
> ferramentas automáticas chamam, não `docker run` interactivo.

## Baseline medido (binário `ee615f3b`, 2026-10-24)

| Contrato | Estado | Fonte |
|---|---|---|
| CRI — critest v1.36.0 | **79 / 103** (24 fail, 19 skip) | `docs/cri-conformance.md`, `scripts/critest.sh` |
| OCI Image | sem suite upstream corrida; testes dirigidos | `reports/.../oci-conformance.md` |
| OCI Distribution | push/pull por digest OK; sem suite corrida | campanha 2026-10 |
| OCI Runtime | init próprio + `image export` p/ runc/crun; sem `runtime-tools` | idem |
| Docker Engine API | **15 / 26** rotas servidas | `delonix serve docker-api --matrix` |

**As 24 falhas do CRI, por área:** AppArmor 9 · Security Context 4 · Mount Propagation 3 ·
Image Manager 2 · Streaming 2 · Multiple Containers 1 · Networking 1 · RO-mount 1 · OOM 1.

## Decisões do dono (2026-10-24)

1. **Prioridade: CRI primeiro** (kubelet drop-in — o diferenciador container+VM+k8s).
2. **`NoNewPrivs=false`:** honrar o pedido do CRI (passa a conformidade) **mantendo NNP sempre-ligado
   por omissão no caminho CLI/normal**. A garantia por omissão não muda; só o CRI a pode relaxar.
3. **OOM post-mortem:** **investir no supervisor** — o supervisor por-container (que já existe para
   `--restart`/`wait`) lê o cgroup ANTES do teardown e regista `OOMKilled`/137. Não é N/A.

---

## Track C — CRI (PRIORITÁRIO) · gate: 79 → ~100/103 em rootful

Cada fase = uma área de falha do critest, com a spec que falha nomeada.

| Fase | Área (nº specs) | O que fazer | Gate |
|---|---|---|---|
| **C1** | Security Context (4) | `PodPID` namespace; `RunAsUserName`; `SeccompProfilePath` por caminho (liga ao seccomp-custom que já erra em vez de ignorar); **`NoNewPrivs=false`** (decisão 2 — relaxável só pelo CRI) | +4 |
| **C2** | Image Manager (2) | pull por digest via CRI; `Uid`/`Username` no `ImageStatus` | +2 |
| **C3** | Streaming (2) | servidor de streaming CRI: `PortForward` + base de `Exec`/`Attach` com hijack (**partilhado com D3**) | +2 |
| **C4** | Networking (1) + Mount Propagation (3) | port mapping só com porta de container; `rshared`/`rslave` (**vem do Track B2**) | +4 |
| **C5** | RO-mount (1) + Multiple Containers (1) | mounts read-only não-recursivos; caso multi-container | +2 |
| **C6** | OOM (1) | supervisor capta OOM antes do teardown do cgroup (decisão 3) | +1 |
| **C7** | AppArmor (9) | traduzir o perfil por-container do CRI; correr a suite **rootful** (carregar perfis exige privilégio — N/A em rootless, nomeado); provar os que não são de ambiente | +0 a +9 conforme rootful/rootless |

> **N/A honestos a nomear no relatório final:** as specs de AppArmor que exigem carregar um perfil
> (privilégio que o rootless não tem) ficam N/A em rootless e provam-se em rootful. Nenhuma outra.

**Pré-requisito do Track B:** C4 depende de B2 (mount propagation). Fazer B2 no arranque do Track C.

---

## Track B — OCI Runtime Spec (habilita o CRI)

| Fase | O que fazer | Gate |
|---|---|---|
| **B1** | Correr `opencontainers/runtime-tools` validation contra (a) o bundle de `image export` + runc/crun e (b) o init próprio do Delonix (config.json → namespaces/caps/seccomp/mounts/hooks/rlimits). Script reproduzível. | baseline publicado |
| **B2** | Mount propagation `rshared`/`rslave` (**fecha 3 do CRI — C4**) | runtime-tools + critest |
| **B3** | Readonly recursivo + masked/readonly paths (decisão G-009) | runtime-tools |

## Track A — OCI Image + Distribution (fundação de interop de imagens)

| Fase | O que fazer | Gate |
|---|---|---|
| **A1** | Instalar e correr as suites `opencontainers/image-spec` e `distribution-spec` (como o `critest.sh` fez para CRI); publicar baseline + script | baseline publicado |
| **A2** | Fechar **G-033** (tag+digest em conflito grava conteúdo errado sem aviso → recusar/avisar), **G-035** (`.tmp` órfãos; `prune` limpa), **G-039** (load de archive formato antigo `layer.tar`) | testes dirigidos verdes |
| **A3** | Manifest-list / multi-arch no `push` (hoje 1 arch por invocação) | distribution-spec |
| **A4** | Regressão de whiteouts (já corrigido em `507c7cad`, char-dev de overlay por-camada) contra `wordpress:6.7` e `rabbitmq` que o bug quebrava | E2E verde |

## Track D — Docker Engine API (ecossistema; ressalva §5)

| Fase | Rota(s) | Porquê |
|---|---|---|
| **D1** | `GET /containers/{id}/logs` (framing stdout/stderr) | desbloqueia `wait_for_logs` do testcontainers — maior valor isolado |
| **D2** | `/events`, `/build`, `/networks`, `/volumes`, `GET …/stats` | superfície que compose/kind/ferramentas tocam |
| **D3** | `POST …/exec` + `…/attach` (hijack HTTP) | **partilha o servidor de streaming do C3** |

> Fora de âmbito deliberado: `docker run` interactivo contra o servidor (comportamento interno do
> CLI Go), e paridade de `--format`/CLI que o `§5` desaconselha.

---

## Sequência e dependências

```
B1 ─► B2 ──────────────► C4
          \
A1 ─► A2 ─► A3 ─► A4      C1 ─► C2 ─► C3 ─┬─► C5 ─► C6 ─► C7
                                          └─(C3 streaming)─► D3 ─► D1 ─► D2
```

1. **Arrancar com B1+B2** (runtime-tools + mount propagation) — pequeno e desbloqueia C4.
2. **Track C** em paralelo (C1→C2→C3→C5→C6→C7) — o retorno estratégico.
3. **Track A** quando houver folga (fundação; não bloqueia o CRI no dia-a-dia mas certifica as imagens).
4. **Track D** por último, com a ressalva; D3 cai de borla com o C3 (mesmo servidor de streaming).

## Como medir (reproduzível, por fase)

- CRI: `scripts/critest.sh` → `Ran N of 122 ... X Passed | Y Failed`. Publicar o diff de X a cada fase.
- OCI runtime: `runtime-tools` validation (instalar em A1/B1); publicar pass/fail por secção do spec.
- OCI image/distribution: suites upstream `image-spec`/`distribution-spec`; publicar baseline e deltas.
- Docker API: `delonix serve docker-api --matrix` (rotas servidas/recusadas) + `tests/compat/docker_api_smoke.py` e um run de testcontainers.
- Cada fase traz um teste que falha sem a correção (o método do projeto).

## Riscos / tensões conhecidas

- **NNP vs CRI** (decisão 2): o caminho que relaxa NNP só pode ser alcançável pelo CRI, nunca pela CLI.
- **AppArmor rootless** (C7): parte das 9 specs é limite de privilégio, não lacuna — medir rootful para separar.
- **OOM daemonless** (C6): o supervisor tem de ler o cgroup na janela entre a morte e o teardown; corrida real.
- **Disco do host a 96%**: as suites OCI/CRI puxam imagens e escrevem artefactos — libertar espaço antes de A1/B1.

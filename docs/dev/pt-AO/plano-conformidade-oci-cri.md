<!-- translated-from: plano-conformidade-oci-cri.md sha256:fd53478c1e7c11c6637436232549328ed2872216e71bd1550ffd75b59d30c549 -->
# Plano de conformidade OCI + CRI — alinhar o Delonix com Docker/Podman

> **Objectivo.** Fazer o Delonix passar os **mesmos contratos de interoperabilidade** que o
> containerd/CRI-O (CRI) e qualquer runtime OCI (image/distribution/runtime) passam, para ser
> drop-in para o kubelet e para registries — e, num track separado e com ressalva, cobrir a
> Docker Engine API que ferramentas de terceiros chamam via `DOCKER_HOST`.
>
> **Princípio de medição.** Cada fase fecha contra uma **suite upstream com um número**, não
> "parece feito". Nunca divergência silenciosa (recusa explícita + razão). Teste que falha sem a
> correcção e passa com ela, como o resto do projecto. Onde um teste não se aplica ao modelo
> (daemonless, rootless), nomeia-se **N/A** com a razão — não se inflaciona a percentagem.
>
> **Âmbito e a ressalva do `paridade-docker-podman.md §5`.** OCI (image/distribution/runtime) e
> CRI são contratos de interop onde conformidade total vale a pena — fazem do Delonix um
> substituto real. A Docker Engine API (Track D) é conveniência de ecossistema; o próprio projecto
> desaconselha perseguir paridade total com a Docker CLI, por isso o Track D cobre só o que
> ferramentas automáticas chamam, não `docker run` interactivo.

## Baseline medido (binário `ee615f3b`, 2026-10-24)

| Contrato | Estado | Fonte |
|---|---|---|
| CRI — critest v1.36.0 | **79/103 é de v0.63 (2026-08) — A RE-MEDIR na v5.1.1** (o dono está a correr critest agora em C2/C3) | `scripts/critest.sh` |
| OCI Image | **interop provada na v5.1.1**: delonix→docker **e** delonix→podman + docker→delonix, por digest (IMG-1 corrigido 2026-10-24) | `reports/conformance/A1-oci-image-distribution.md` |
| OCI Distribution | push/pull cross-engine OK na v5.1.1; suite `distribution-spec` é server-side → **N/A** (Delonix é cliente) | A1 |
| OCI Runtime — bundle | **v5.1.1**: ociVersion 1.3.0 ✅, seccomp ✅, `runc run` rootless rc=0 ✅ (RT-1/2/3) — `oci-runtime-tool validate` OK | B1/B2 reports |
| OCI Runtime — CLI runc | **não existe** (o único gap de runtime real) — RT-4 / ADR-0080 | B1 |
| Docker Engine API | **15/26** rotas (medido ao vivo na v5.1.1) | `delonix serve docker-api --matrix` |

> **Aviso de proveniência (importante).** A primeira versão deste plano herdou a matriz de gaps do
> relatório **v3.1.0** (`fa8c6297`). Estamos na **v5.1.1**: re-medir contra o código actual mostrou
> que vários "gaps" estão **stale** — a mount-propagation já funciona (B2), o `.tmp` de layer já é
> limpo em-processo (G-035), e o `tag@digest` é formato válido, não bug (G-033). **Este plano é
> conduzido por medições na v5.1.1, não pela matriz v3.1.0.** Um "gap" só entra se REPRODUZIR na v5.1.1.

**As 24 falhas do CRI, por área:** AppArmor 9 · Security Context 4 · Mount Propagation 3 ·
Image Manager 2 · Streaming 2 · Multiple Containers 1 · Networking 1 · RO-mount 1 · OOM 1.

## Decisões do dono (2026-10-24)

1. **Prioridade: CRI primeiro** (kubelet drop-in — o diferenciador container+VM+k8s).
2. **`NoNewPrivs=false`:** honrar o pedido do CRI (passa a conformidade) **mantendo NNP sempre-ligado
   por omissão no caminho CLI/normal**. A garantia por omissão não muda; só o CRI a pode relaxar.
3. **OOM post-mortem:** **investir no supervisor** — o supervisor por-container (que já existe para
   `--restart`/`wait`) lê o cgroup ANTES do teardown e regista `OOMKilled`/137. Não é N/A.

---

## Track C — CRI (PRIORITÁRIO) · gate: re-medir na v5.1.1, depois → ~100/103 em rootful

**Passo zero: re-medir o critest na v5.1.1** (`scripts/critest.sh`). O 79/103 é de v0.63 (2026-08) e
a repartição de falhas abaixo é dessa corrida — pode já não bater. O dono está a correr isto agora
(worktrees `cri-image-repodigests-and-user` = C2, `cri-portforward-conformance` = C3). As fases
abaixo mantêm-se como mapa das ÁREAS, mas os números (`+4`, etc.) só valem depois da re-medição.

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

**Re-baseline na v5.1.1 (2026-10-24): esta track está em grande parte FEITA — o resto é opcional.**
A interop OCI de imagem foi **provada ao vivo** nos três motores (IMG-1 corrigido: `push` publica
manifesto OCI, puxável por docker E podman). Os "gaps" G-033/035/039 do relatório v3.1.0 **saíram**
após re-medição:

- **G-033** (tag+digest) — **NÃO é bug**: `repo:tag@digest` é formato OCI/Docker válido e comum (o
  kind usa-o); recusar partiria o kind. Se um dia se quiser detectar um *conflito* real (a tag
  resolve para outro digest), é uma decisão de representação local — fora do âmbito até ser pedido.
- **G-035** (`.tmp` de layer) — **já limpo em-processo** (`overlay.rs:492-508`). Resíduo mínimo: só
  um `.tmp` órfão de um processo MORTO a meio, que o `prune` não varre — melhoria pequena, agrupável
  numa futura passagem do `prune`, não um gap de conformidade.
- **G-039** (archive docker-save LEGADO `<hash>/layer.tar`) — real mas **só afecta archives
  pré-OCI**; o `docker save` moderno produz o layout `blobs/sha256/` que o `load` já aceita. Baixa
  prioridade; a correcção (mapa path→digest) tem subtileza nos layers.

| Opcional | O que fica | Gate |
|---|---|---|
| A-sbom/scan/sign | `image sbom`/`scan`/`verify`/`sign` — medir a cobertura real na v5.1.1 (não medido nesta campanha) | baseline publicado |
| A-manifest-list | multi-arch no `push` (hoje 1 arch por invocação) | round-trip multi-arch |

> `oci-image-tool` (image-spec) **não compila** (breakage `Sirupsen/logrus`) e a `distribution-spec`
> é server-side — por isso a prova de imagem na v5.1.1 é a **interop entre motores**, não uma suite.

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

**Estado na v5.1.1:** Track A (imagem) **provada** (IMG-1); Track B RT-1/2/3 + B2 **feitos** — só falta
**RT-4** (CLI runc, ADR-0080, GO-gated). O trabalho real restante é RT-4, o Track C (a re-medir) e o
Track D.

```
[v5.1.1 feito]  A (interop) ✅   B2 mount-prop ✅   RT-1/2/3 bundle ✅
[real restante]
   RT-4 (ADR-0080, GO) ─► runtime-tools validation suite contra o Delonix
   C0 re-medir critest ─► C1 ─► C2 ─► C3 ─┬─► C4 ─► C5 ─► C6 ─► C7
                                          └─(C3 streaming)─► D3 ─► D1 ─► D2
```

1. **Track C**: re-medir o critest na v5.1.1 (C0), depois fechar por área — o retorno estratégico
   (em curso pelo dono: C2/C3).
2. **RT-4** (quando o dono der GO): desbloqueia medir o runtime pela suite upstream.
3. **Track D** por último, com a ressalva; D3 cai de borla com o C3 (mesmo servidor de streaming).
4. **Track A**: só o opcional (sbom/scan/sign, manifest-list) — a interop já está provada.

## Como medir (reproduzível, por fase)

- CRI: `scripts/critest.sh` → `Ran N of 122 ... X Passed | Y Failed`. Publicar o diff de X a cada fase.
- OCI runtime: `runtime-tools` validation (instalar em A1/B1); publicar pass/fail por secção do spec.
- OCI image/distribution: suites upstream `image-spec`/`distribution-spec`; publicar baseline e deltas.
- Docker API: `delonix serve docker-api --matrix` (rotas servidas/recusadas) + `tests/compat/docker_api_smoke.py` e um run de testcontainers.
- Cada fase traz um teste que falha sem a correcção (o método do projecto).

## Riscos / tensões conhecidas

- **NNP vs CRI** (decisão 2): o caminho que relaxa NNP só pode ser alcançável pelo CRI, nunca pela CLI.
- **AppArmor rootless** (C7): parte das 9 specs é limite de privilégio, não lacuna — medir rootful para separar.
- **OOM daemonless** (C6): o supervisor tem de ler o cgroup na janela entre a morte e o teardown; corrida real.
- **Disco do host a 96%**: as suites OCI/CRI puxam imagens e escrevem artefactos — libertar espaço antes de A1/B1.

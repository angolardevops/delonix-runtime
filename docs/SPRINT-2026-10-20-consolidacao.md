# Sprint de consolidação — delonix-runtime — 2026-10-20

Junta numa só frente as cinco sessões do grupo `delonix-runtime` que estavam
abertas em paralelo. Escrito para ser retomado **noutra máquina**: tudo o que
interessa está versionado, porque `.claude/` é `gitignored` e por-worktree.

- **HEAD medido**: `main` em `84c88601`
- **Tag de segurança do estado anterior**: `consolidacao-pre-20261020-1857` → `8565b5f5`
- **Branch deste plano**: `docs/sprint-consolidacao-2026-10-20`

---

## 0. Como isto foi medido

Nada aqui é presumido. As fontes, por ordem de autoridade:

| facto | comando que o deu |
|---|---|
| sessões vivas e as suas PRs | `list_sessions` do gestor de sessões |
| estado das PRs e dos gates | `gh pr list --json statusCheckRollup` |
| trabalho só-local | `git rev-list --count <upstream>..<branch>` por branch |
| `develop` vs `main` | `git rev-list --count origin/main..origin/develop` → **0** |
| issues | `gh issue list --state open` → **15** |
| frentes por abrir | os 5 rascunhos em `.claude/sessions/pendentes/` |

> **`main` faz squash-merge.** Os commits de uma branch desaparecem da história
> da `main` e o conteúdo fica. Para saber se algo já lá está usa
> `git cherry main <branch>`, **nunca** `git log`.

---

## 1. Retrato — as cinco sessões do grupo

| sessão | branch | estado real |
|---|---|---|
| `unruffled-proskuriakova-b581a6` | `claude/unruffled-proskuriakova-b581a6` | PR **#891** aberta, **1 gate vermelho**, e **1 commit por empurrar** |
| `nervous-morse-38a32c` | `net/unconditional-guards-are-reconciled` | PR **#893** aberta e **verde**. A PR #892 dessa sessão já foi merged |
| coding session (primária) | `docs/ingress-phase4-describes-what-exists` | PR **#890** aberta e **verde** |
| coding session (primária) | `net/ra-only-when-ipv6-opted-in` | contrato **1/3**, falta a medição de operador. Sem PR |
| «Fluxograma de comunicação» | — | só leitura, sem artefacto por aterrar |

Fora do grupo mas no mesmo repo: a worktree `delonix-folhas` em
`test/folhas-reais` → PR **#889**, verde.

---

## 2. Em execução — as 4 PRs abertas

| PR | branch | gates | o que falta |
|---|---|---|---|
| **#893** | `net/unconditional-guards-are-reconciled` | ✅ 19 pass, 2 skip declarado | só o merge |
| **#890** | `docs/ingress-phase4-describes-what-exists` | ✅ verde | só o merge |
| **#889** | `test/folhas-reais` | ✅ verde (inclui `leaf inventory ratchet`) | só o merge |
| **#891** | `claude/unruffled-proskuriakova-b581a6` | ❌ `generated docs and valid examples` | regenerar `docs/` **e** empurrar o commit local |

O erro exacto da #891, do log do run `39399365138`:

```
::error::docs/ está desactualizado — corre 'python3 docs/gen.py' e commita.
```

O gate é `git diff --quiet -- docs/` **depois** de correr o gerador. Duas armadilhas
conhecidas antes de lhe tocar:

1. `docs/gen.py` embebe o `--help` **real** do binário. Corre
   `cargo build --release -p delonix-runtime-bin` primeiro, ou um binário velho
   inventa um diff que não é teu.
2. Mexer nos docs EN re-renderiza os **quatro** handbooks (`en`, `pt-AO`,
   `fr-FR`, `zh-CN`) e o HTML é conferido byte-a-byte contra `markdown==3.10.3`.
   Esta máquina tem a `3.10.2` — instala a pinada para um alvo à parte e aponta
   o `PYTHONPATH`, senão geras um diff que a CI recusa.

> ⚠️ **PR #841** (`consolida/develop-2026-10-14`, 166 ficheiros, +13 888/−688)
> tem base `develop`. Ressuscitar `develop` a partir da `main` **move-lhe a base
> 55 commits para a frente** e o `mergeable` pode virar `CONFLICTING`. Nenhum dos
> seus 166 ficheiros está na `main` — **não a feches**. O resgate são rebases por
> linha de trabalho, não um merge.

---

## 3. Por fazer — o inventário

### 3.1 Issues abertas (15)

Agrupadas por causa, não por número — três delas partilham a mesma raiz já provada.

| grupo | issues | nota |
|---|---|---|
| **k8s não arranca** | #736, #799, #825 | causa **provada**: ver 3.3 |
| **paridade Docker API** | #826, #827, #881, #776 | rotas por servir e o `HostConfig` vazio |
| **CLI mente com rc 0** | #804, #809 | mesma forma dos já corrigidos #805-#808; esbarram no ratchet `library_prints` |
| **defeitos de estado** | #812, #815, #823, #824 | cgroup órfão, build-cache parcial, segredo ausente, nome duplicado |
| **plataforma** | #438 | arm64 — a CI já corre `test (arm64)` |

### 3.2 Critérios por medir (contrato `net/ra-only-when-ipv6-opted-in`)

O critério de código está **cumprido e observado** em `8565b5f5`: o teste
`the_ra_emitter_only_runs_when_ipv6_is_opted_in` falha sem a mudança (rc=101) e
passa com ela; `clippy`, `arch_fitness` (`library_prints` estável em 82),
`cargo test` 3424/0 e os restantes gates do `make ci` deram rc=0.

Faltam **duas medições de operador**, que não são minhas — tocam no engine:

```bash
cargo build --release -p delonix-runtime-bin
bash .claude/sessions/medir-ra-net-ra-only-when-ipv6-opted-in.sh
```

Arm A: sem `DELONIX_ENABLE_IPV6`, **0** RAs em `delonix0`. Arm B: com a variável,
**≥3**. O script isola `DELONIX_ROOT`/`DELONIX_NET_RUNTIME_DIR`, mantém o caminho
curto (o `SUN_LEN` do socket são 108 bytes) e entra na netns de infra por
`nsenter`. Esta branch **ainda não tem PR**.

### 3.3 Rascunhos prontos a virar sessão (`.claude/sessions/pendentes/`)

| rascunho | estado | valor |
|---|---|---|
| `bind-source-is-created-on-demand` | causa provada, **desenho por decidir** | 🔥 o mais alto |
| `golden-preseeds-cni-agent-images` | **hipótese REFUTADA**, reduzido | não executar como está |
| `dedup-measured-reports-credential-value` | 2 duplicações confirmadas | dívida barata |
| `decisoes-duplicacao-menor` | **decisão**, não execução | não abrir sem decidir |
| `handbook-zh-cn-inalcancavel` | 25 páginas órfãs | dívida barata |

**O achado que liga tudo.** O `calico-node` não falha por falta de imagem — o
`describe` do cluster `probe` vivo não tem `Pulling`, `Pulled` nem
`ImagePullBackOff`. Tem isto:

```
Error: failed to start container upgrade-ipam:
  delonix: no such bind path /var/lib/cni/networks
```

`VolumeStore::resolve_spec` (`crates/adapters/delonix-volume/src/lib.rs`, ramo
`src.starts_with('/')`) faz `canonicalize` à origem de um **bind** e devolve
`DX-4000` se ela não existir. O ramo logo abaixo, para **volume nomeado**, cria-a
on demand — e tem o comentário «creates on demand, like Docker». O motor é
incoerente consigo próprio; o Docker cria nos dois casos. Um `HostPath` do
Kubernetes com `HostPathType` vazio conta com que o runtime crie o directório.

Reproduzido isolado, com `rc` real:

| caso | resultado |
|---|---|
| bind, origem ausente | `rc=4`, `DX-4000`, directório **não** criado |
| bind, origem presente | `rc=0` |
| volume nomeado, igualmente ausente | `rc=0`, **criado** |

Por isso a pré-semeagem de imagens na golden **não** resolve o `NotReady`.

---

## 4. O plano em sprint

Seis blocos, por ordem de dependência. Os blocos 0 a 3 esvaziam o que está em
voo; o 4 é o único que ataca produto, e é o que paga.

### Bloco 0 — pré-voo: salvar o só-local  ⏱ ~10 min

Nada do que se segue pode correr antes disto, porque apaga branches.

1. Empurrar o commit à frente em `claude/unruffled-proskuriakova-b581a6`.
2. Empurrar `cons/integration-test-2026-10-20` (sem upstream) ou declará-la
   descartável — ver §5.
3. `git push origin main:develop` — fast-forward de 55 commits.

**Feito quando**: `git rev-list --count <upstream>..<branch>` dá 0 para toda a
branch local, e `git ls-remote origin develop` iguala `origin/main`.

### Bloco 1 — aterrar o que já está verde  ⏱ ~20 min

Merge de **#893**, **#890** e **#889**, por esta ordem. Entre cada uma, confirmar
que a seguinte continua `MERGEABLE` — a base mexe-se.

**Feito quando**: as três `MERGED` e `git cherry main <branch>` sem `+` para cada.

### Bloco 2 — desbloquear a #891  ⏱ ~30 min

Construir o release, correr `python3 docs/gen.py`, commitar o `docs/` e empurrar
junto com o commit local que ainda lá está. Respeitar as duas armadilhas de §2.

**Feito quando**: `generated docs and valid examples` passa a verde e a #891 fica
`MERGEABLE` sem falhas.

### Bloco 3 — fechar o `net/ra`  ⏱ ~15 min, **do operador**

Correr as duas arms de §3.2. Se derem o esperado, ticar os dois critérios, abrir
PR para a branch `net/ra-only-when-ipv6-opted-in` (que já está no remoto) e
fechar o contrato.

**Feito quando**: arm A lê 0, arm B lê ≥3, e existe PR.

### Bloco 4 — o bind cria a origem  ⏱ 1 sessão inteira  🔥

A fatia de maior retorno de todo este inventário: uma incoerência de ~10 linhas
que mantém três issues abertas e o Kubernetes `NotReady`.

A causa está provada; **o desenho não está decidido**. Decidir antes de escrever:

- criar sempre, como o Docker — simples, mas um `src` com erro de escrita passa a
  criar lixo em vez de recusar;
- criar só quando o chamador o pede (uma flag no spec) — seguro, mas o
  Kubernetes não a passa, logo não resolve o `NotReady`;
- criar quando a origem está sob um prefixo conhecido do runtime.

**Feito quando**: um `HostPath` com `HostPathType` vazio arranca; existe teste que
falha sem a mudança; e o `calico-node` do cluster `probe` sai de `NotReady` —
medido no cluster, não no teste.

### Bloco 5 — dívida barata, se sobrar tempo  ⏱ ~40 min

`measured_reports()` duplicada byte-a-byte entre
`bins/delonix-runtime-bin/src/cmd/provider.rs:119` e
`crates/interfaces/delonix-node-api/src/providers.rs:18` — 34 linhas sem o
compilador a proteger da deriva. E a landing do handbook, que tem as línguas
*hardcoded* a três e deixa 25 páginas `zh-CN` inalcançáveis
(`scripts/dev_docs_site.py:812-819`); gerá-la a partir de `LANGS`.

**Não** abrir `decisoes-duplicacao-menor` como execução — é uma decisão.

---

## 5. O que só existe nesta máquina

Por ordem de risco. Tudo o resto já está no remoto.

| artefacto | risco | destino |
|---|---|---|
| commit órfão `829f17fe` | **alto** — em branch nenhuma, o GC leva-o | ver abaixo |
| commit à frente em `claude/unruffled-proskuriakova-b581a6` | médio | empurrar (bloco 0) |
| `cons/integration-test-2026-10-20` | baixo | ver abaixo |
| contratos em `.claude/sessions/` | médio | `gitignored` — por isso este plano existe |

**`829f17fe`** — «cli: pin the two message decisions #806 and #807 turn on, in
tests». São os 8 testes da sessão `fix/cli-commands-say-what-they-did`, cujo push
foi recusado pelo classificador. **As quatro issues #805-#808 já estão CLOSED** e
o fix está na `main` pela #885. Logo o commit é *só* a rede de testes, que não
aterrou. Vale como fatia própria — não o deixes morrer sem decidir.

**`cons/integration-test-2026-10-20`** — merge local das quatro branches em voo,
para as testar juntas. Os 15 commits que `git cherry main` marca com `+` são os
das PRs #889/#890/#891/#893; quando elas aterrarem, esta branch não acrescenta
nada. É descartável **depois** do bloco 1, não antes.

---

## 6. Ordem de arranque noutra máquina

```bash
git clone git@github.com:angolardevops/delonix-runtime.git
cd delonix-runtime
git checkout develop          # ou main — são o mesmo conteúdo depois do bloco 0
cat docs/SPRINT-2026-10-20-consolidacao.md
```

Depois, do `AGENTS.md`, ler só a secção da frente que se vai atacar. O guia tem
120 secções e não é leitura de arranque.

Antes do primeiro commit, confirmar o ambiente:

```bash
make ci                       # o que a CI corre
df -h /                       # o disco desta máquina anda nos 96%
git worktree list             # o tecto são 3 worktrees
```

> `cargo test` quer um `TMPDIR` **vazio e só seu** — o `tmp_roots_gate.py` julga
> o que lá ficar. Não o partilhes com os `scripts/test_*.py`: eles deixam ~7
> directórios e a medição passa a acusar o `cargo test` de um leak que não é dele.

# Backlog de refactorização — Delonix Runtime (`cebf895`)

Cada item cumpre §98: melhora correcção, segurança, manutenção ou compreensão
por agentes. Nenhum é estético.

---


## ~~P0-1~~ — `ContainerInitSpec`: tipar os 37 parâmetros de `container_init` — **FEITO**

**ID:** ARCH-0002 · **Severidade:** HIGH · **Quebra:** NÃO (fn privada)
**Estado:** **FUNDIDO** na `main` — PR [#170](https://github.com/angolardevops/delonix-runtime/pull/170), merge `5e40074`.

Grupos criados: `ProcessSpec`, `FilesystemSpec`, `SecuritySpec`,
`NamespaceSpec`, `IoSpec`. Spec inteiro `Copy` (todos os campos são empréstimos
ou escalares), portanto viaja para dentro do `clone` sem tocar no alocador do
filho — restrição que o `main.rs` documenta e que uma struct com `String` teria
violado.

**Como se provou que preserva o comportamento:** o corpo não foi tocado — o spec
é destruturado no topo para os mesmos 37 nomes locais (verificado que nenhum
parâmetro era sombreado ou reatribuído no corpo). A correspondência
campo-a-campo entre a chamada posicional antiga e o literal novo foi verificada
por script: **37/37 iguais, zero em falta, zero a mais**. `cargo test -p
delonix-runtime`: 52 verdes.

**Descoberta durante o trabalho (ver P1-5).**

---

**Problema (era).** `crates/delonix-runtime/src/lib.rs:2546` recebia 37 parâmetros
posicionais, 9 deles `bool` adjacentes e intermutáveis à vista do compilador
(`read_only`, `seccomp_unconfined`, `seccomp_detect`, `no_new_privs`,
`has_own_netns`, `host_pid`, `inherit_userns`, `privileged`, `node_cgroup`).
Trocar dois numa chamada **compila** e desliga uma barreira de segurança em
silêncio.

**Desenho recomendado.** O que o FIXME no próprio ficheiro já propõe: um
`ContainerInitSpec` com sub-estruturas por área — `rootfs`, `process`,
`limits`, `security`, `namespaces`, `io`. Os 9 `bool` de segurança passam a
viver num `SecurityFlags` onde a troca deixa de ser possível.

**Crates afectados:** `delonix-runtime` (e chamadores em `delonix-runtime-bin`).
**Migração:** mecânica; a fn é privada, não há contrato externo.
**Testes exigidos:** um teste por flag de segurança que prove que ligá-la muda
o comportamento observável — hoje a troca silenciosa não é detectável por
nenhum teste.
**Aceitação:** `#[allow(clippy::too_many_arguments)]` sai do ficheiro; nenhum
`bool` nu na assinatura.

**Nota que este passe descobriu:** o FIXME diz «30 positional arguments». São
**37**. A assinatura cresceu 7 parâmetros *depois* de alguém ter escrito que era
um smell. Um FIXME sem portão não trava crescimento — por isso este item deve
sair acompanhado de um teste ou lint que fixe o tecto.

---

## P1-5 — Três `#[allow]` que silenciavam a função errada — **FEITO**

**ID:** QUALITY-0001 · **Severidade:** MEDIUM · **Quebra:** NÃO
**Estado:** **FUNDIDO** no mesmo commit que o P0-1 (PR #170).

**Problema.** O `delonix-runtime/src/lib.rs` tinha quatro
`#[allow(clippy::too_many_arguments)]`. Ao tirar o do `container_init`,
verificou-se que **três dos restantes não descreviam nada**:

| Linha | Estava em | Parâmetros reais |
|---|---|---|
| 2177 | `apply_env(hostname, env)` | **2** |
| 2203 | `apply_tmpfs(specs)` | **1** — e com o comentário «container init: many namespace/security parameters», que é de outra função |
| 2397 | `mask_slow_node_units()` | **ZERO** |

Tinham-se desligado das funções que era suposto silenciarem. É o mecanismo que
explica o número errado no FIXME do P0-1: com o lint desligado nos sítios
errados, `too_many_arguments` deixou de avisar quando a assinatura do
`container_init` passou de 30 para 37 argumentos.

**Prova de que eram mortos:** removidos os três, `cargo clippy --workspace
--all-targets` continua com **zero avisos**. O único que sobra
(`setup_rootfs`, 10 parâmetros) é legítimo e leva agora a razão escrita.

**Lição para §88:** um `#[allow]` colocado ACIMA do bloco `///` — como estes
estavam — separa-se da função ao primeiro reordenamento e ninguém dá por isso.
Regra a adoptar: o `#[allow]` fica sempre ENTRE a doc e a assinatura.

---

## P1-1 — `tls.hosts` do `kind: Ingress`: avisar em vez de engolir

**ID:** ARCH-0003 · **Severidade:** MEDIUM · **Quebra:** NÃO

**Problema.** `httproute.rs:591` — `IngressTls.hosts` tem `#[allow(dead_code)]`
sem uma linha de razão. O utilizador que escreve dois hosts SNI recebe **um só**
certificado (o primeiro elemento de `tls`) e não é avisado. Contraria a regra da
casa: um campo que o cliente escreve e o sistema ignora é pior do que um campo
que não existe. É o único dos quatro campos ignorados deste ficheiro sem razão
escrita — os outros três (`ingressClassName`, `pathType`, `port.name`) cumprem.

**Desenho.** (a) doc a dizer porquê; (b) aviso no `apply` quando
`tls[].hosts.len() > 1` ou quando `tls.len() > 1`.

**Aceitação:** aplicar um Ingress com dois hosts TLS imprime um aviso nomeando
o host que fica sem certificado.

---

## P1-2 — `pathType: Exact` aceite e servido como prefixo

**ID:** ARCH-0003 · **Severidade:** MEDIUM · **Quebra:** NÃO

**Problema.** `httproute.rs:504`. Documentado no código como limitação
conhecida, mas o utilizador que escreve `Exact` recebe correspondência por
**prefixo** — encaminha tráfego que não pediu. Documentar não é avisar.

**Desenho.** Aviso no `apply` quando `pathType == "Exact"`, nomeando a rota.
Alternativa mais cara e mais correcta: recusar até o proxy suportar `Exact`.

---

## P1-3 — SAFETY nos `unsafe` que dependem de invariantes

**ID:** DOC-0003 · **Severidade:** MEDIUM · **Quebra:** NÃO

**Problema.** 58 blocos `unsafe` sem comentário SAFETY nas 6 linhas anteriores.
**Não** os tratar todos por igual: `libc::close(fd)` local é trivial e não
merece justificação; `libc::kill(pid, SIGKILL)` **depende** do invariante de que
o PID ainda pertence ao processo que se julga estar a matar — PID reciclado
mata-se o processo errado — e esse invariante não está escrito.

**Alvo:** os `kill`, `read` e `write` sobre PIDs e fds capturados em
`delonix-cri/src/spdy.rs`, `delonix-cri/src/streaming.rs`,
`delonix-runtime-bin/src/cmd/container.rs`.
**Aceitação:** cada `unsafe` sobre PID/fd capturado nomeia o invariante e quem
o garante.

---

## P1-4 — Erros estruturados no caminho de segurança

**ID:** ERROR-0001 · **Severidade:** MEDIUM · **Quebra:** sim, em 3 assinaturas

**Problema.** 12 funções devolvem `Result<_, String>` (§51). Três estão no
caminho de imposição de segurança, onde a mensagem é a única coisa que sobra
para diagnosticar uma recusa:

```
crates/delonix-runtime/src/lib.rs:325       fn install_filter_privileged(…) -> Result<(), String>
crates/delonix-runtime/src/lib.rs:2105      fn apply_apparmor(…)            -> Result<(), String>
crates/delonix-runtime/src/seccomp_profile.rs:578  pub fn compile(…)        -> Result<_, String>
```

**Desenho.** Um `SeccompError`/`LsmError` estruturado com a syscall, a acção e a
causa — não uma frase. `compile` é `pub`: mudança quebradiça, verificar
consumidores antes (§25).
**Restantes 9:** parsers de flags da CLI (`parse_add_host`, `parse_io_rate`,
`cap_ceiling::parse`, …) — aí `String` é defensável, a mensagem vai direita ao
utilizador. **Não mexer.**

---

## P2-1 — 45 `.map_err(|_| …)` a deitar fora a causa

**ID:** ERROR-0002 · **Severidade:** MEDIUM · **Quebra:** NÃO

Contra §24. Nem todos são iguais: em `parse::<u16>()` a causa não acrescenta
nada; em `sign.rs:107` (`c.get_manifest(&sig_tag).map_err(|_| …)`) perde-se a
distinção entre «assinatura não existe» e «o registo respondeu 500», que é
exactamente o que se quer saber ao diagnosticar uma verificação falhada.

**Regra a aplicar:** manter `|_|` quando o erro descartado é de conversão pura;
encadear a causa em tudo o que atravesse rede, disco ou processo.

---

## P2-2 — Documentar os 28 enums públicos sem doc

**ID:** DOC-0001 · **Severidade:** MEDIUM · **Quebra:** NÃO

Cobertura de enums a 58,8 %, a pior das quatro categorias. Prioridade: os que
aparecem em erro ou estado de recurso primeiro. **Não** documentar os 110
`pub mod` — é percentagem sem valor (§81).

---

## P2-3 — Deixar de ramificar por nome de provider

**ID:** NAMING-0007 · **Severidade:** MEDIUM · **Quebra:** NÃO

```
crates/delonix-runtime-bin/src/cmd/conditions.rs:319  if backend == "libvirt" {
crates/delonix-vm/src/lib.rs:3981                     (vm.backend == "libvirt").then(…)
```

O porto `VmBackend` existe; estes dois sítios saltam-no e perguntam pelo nome.
**Desenho:** um método de capacidade no trait (ex.: `fn needs_tap_name()`), e a
decisão passa a ser do adaptador. Fecha, no motor, a fuga que a `ngolacloud-arch`
regista como ARCH-002.

---

## P3-1 — Traduzir os 3 itens nomeados privados em PT

**ID:** NAMING-0002 · **Severidade:** LOW · **Quebra:** NÃO

`netdef_path_legado` → `legacy_netdef_path`; `static AVISO` → `WARNED_ONCE`;
`const CONSUMIDOS_TOPO` → `CONSUMED_TOP_LEVEL`. Auto-corrigível (§97).
**Não tocar** nos falsos positivos do léxico: `fn todo<T>`, `fn topo_sort`,
`enum StringOrNum`, `pub fn cap_num`.

---

## P3-2 — Traduzir as ~44 ligações locais em PT

**ID:** NAMING-0003 · **Severidade:** LOW · **Quebra:** NÃO

Concentradas em `delonix-net/src/infra.rs` (12) e `cmd/kindmode.rs` (8). Fazer
por ficheiro, junto com outra mudança nesse ficheiro; baixar a linha de base do
ratchet no mesmo commit (o gate exige-o).

---

## P4-1 — Traduzir os 985 nomes de teste

**ID:** NAMING-0004 · **Severidade:** LOW · **Quebra:** NÃO

95 % de toda a dívida de identificadores. Os nomes **são bons** — descrevem
comportamento como §59 pede — só estão na língua errada. Fazer por crate, um PR
por crate, baixando a linha de base a cada um. Não bloqueia nada.

---

## P4-2 — Uma linha no `AGENTS.md` sobre a língua dos comentários

**ID:** DOC-0005 · **Severidade:** LOW · **Quebra:** NÃO

Dizer que os comentários históricos estão em português, que os novos se escrevem
em inglês, e que o `scripts/lang_ratchet.py` é o portão. Custo: uma linha.
Ganho: um agente deixa de tratar 3 453 comentários como anomalia.

---

## Explicitamente NÃO recomendado

- **Partir o `Container` de 71 campos.** É estado persistido; parti-lo é
  migração (§80), e está documentado campo a campo. Só com ADR.
- **Criar `NetworkProvider`/`StorageProvider` agora.** Há um caminho de rede e um
  de armazenamento. Um porto com um adaptador é YAGNI (§36). Regista-se o limite
  por escrito; cria-se o porto quando o segundo aparecer.
- **Documentar os 110 `pub mod`.** Sobe a percentagem, não acrescenta informação.
- **Uma campanha de newtypes.** Fazer só onde paga: dentro do P0-1.

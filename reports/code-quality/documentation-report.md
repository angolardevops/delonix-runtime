# Relatório de documentação — Delonix Runtime (`cebf895`)

## Cobertura medida (§81)

Só itens `pub` fora de módulos de teste. «Documentado» = `///` imediatamente
acima (saltando atributos e linhas vazias).

| Tipo | Total | Documentados | Cobertura |
|---|---|---|---|
| `trait` | 2 | 2 | **100,0 %** |
| `struct` | 161 | 141 | **87,6 %** |
| `fn` | 848 | 722 | **85,1 %** |
| `enum` | 68 | 40 | **58,8 %** |
| `mod` | 111 | 1 | 0,9 % |

Crate-level `//!` (§68): **13 de 13** crates com `lib.rs` têm documentação de
crate nas primeiras 3 linhas.

## DOC-0001 — ~~Enums a 58,8 % é o buraco real~~ — **RETIRADO** (era MEDIUM)

**Corrigido a 2026-08-29, ao executar o item.** O número estava certo e a
leitura estava errada.

Dos 28 enums públicos sem doc, **27 são enums `*Cmd` do clap** — `NamespaceCmd`,
`VmCmd`, `StackCmd`, `SchemaCmd`, … — e **um** é de domínio (`Decision`, em
`crates/delonix-runtime-bin/src/cmd/backup.rs`). Os `*Cmd` têm **todas as
variantes documentadas**, porque cada `///` é o texto que sai no `--help`:

```rust
#[derive(Subcommand)]
pub enum NamespaceCmd {
    /// List the isolation namespaces IN USE, with what is in each.
    Ls { … },
    /// What is inside ONE namespace, by Kind, `kubectl describe` style.
    Describe { … },
}
```

Um `///` no enum por cima disto não acrescenta nada a ninguém: é exactamente a
caça à percentagem que o DOC-0002, duas secções abaixo, diz para não fazer.
As duas secções foram escritas no mesmo passe e contradiziam-se.

A frase «um enum público é o sítio onde se declara o significado semântico de um
estado do domínio» é verdadeira em geral e **falsa para esta amostra**: a
cobertura de 58,8 % é dominada por encanamento de CLI.

**O que fica:** documentar o `Decision` — o único enum de domínio da lista.
Fica também a lição de método: uma cobertura por categoria não diz o que a
categoria CONTÉM, e neste repo `enum` quer dizer «subcomando» 27 vezes em 28.

## DOC-0002 — 110 `pub mod` sem doc — INFO, não corrigir por corrigir

67 dos 110 estão em `crates/delonix-runtime-bin/src/cmd/mod.rs` e são linhas
`pub mod x;` de declaração. Pôr `/// Módulo de X.` em cima de cada uma sobe a
percentagem e não acrescenta nada — é exactamente o que §81 avisa («do not
optimize purely for percentage»). Recomendação: **ignorar esta linha do
placar**; documentar um módulo só quando o nome não chegar.

Se se quiser um número honesto, a cobertura de itens que interessam
(struct+enum+trait+fn) é **1 079 de 1 079 analisados → 905 documentados = 83,9 %**.

## DOC-0003 — 58 blocos `unsafe` sem comentário SAFETY — MEDIUM (§17)

Método: `unsafe {` sem a palavra `SAFETY` nas 6 linhas anteriores.

Concentrados em `delonix-cri/src/spdy.rs`, `delonix-cri/src/streaming.rs` e
`delonix-runtime-bin/src/cmd/container.rs`, e quase todos são FFI de uma linha:

```rust
unsafe { libc::close(slave) };
unsafe { libc::kill(pid as i32, libc::SIGKILL) };
```

Julgamento: §17 diz «todo o bloco `unsafe` **não trivial**». `libc::close(fd)`
é trivial e não merece seis linhas de justificação. Mas `libc::kill(pid, …)`
**não é trivial** — depende do invariante de que o PID ainda é do processo que
se julga estar a matar (senão mata-se um PID reciclado), e esse invariante não
está escrito em lado nenhum.

Recomendação afinada: exigir SAFETY nos `kill`/`read`/`write` sobre fds e PIDs
capturados; deixar os `close` de fd local em paz. Nota positiva: onde o repo
escreve SAFETY, escreve-o bem — `delonix-runtime/src/lib.rs:2590` documenta o
handshake do pipe de userns em três linhas claras.

## DOC-0004 — Qualidade dos comentários — **PASSA, e acima da média** (§12)

Amostragem em `delonix-vm/src/lib.rs`, `delonix-net/src/infra.rs` e
`delonix-runtime/src/lib.rs`: os comentários explicam **porquê**, com o custo
medido ao lado, que é o que §12 pede e quase nunca se vê.

```rust
// Measured, and it cost hours: every Proxmox appliance image (the vendor's
```

```rust
/// It was private, so `delonix-proxmox` grew its own copy — and the copy did
/// 2 GiB on libvirt and Cloud Hypervisor and 1 GiB on Proxmox, silently. Same
```

Zero comentários a repetir sintaxe encontrados na amostra.

## DOC-0005 — 3 453 comentários em português — LOW (§13)

Contra §13 («all technical comments must be written in English»), é a maior
dívida por volume do repositório inteiro. Contra o valor prático: são
comentários **bons** — explicam porquê, com evidência — apenas na língua errada.

O risco real não é estilístico: é que um agente de código que trabalhe em inglês
lê 3 453 explicações de invariantes numa língua que pode não pesar bem, e o
`AGENTS.md` não o avisa disso. Recomendação de custo baixo: uma linha no
`AGENTS.md` a dizer que comentários históricos estão em PT e que **os novos se
escrevem em inglês** — que é o que o ratchet já impõe na prática.

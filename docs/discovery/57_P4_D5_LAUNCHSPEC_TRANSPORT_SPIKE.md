# 57 — P4/D5: spike do transporte do `LaunchSpec` (ADR-0044)

**Data:** 2026-09-18 · **Base:** `origin/main` (`d16b2f69`, v4.0.0 +7 commits) ·
**Veredicto: o fd ganha — fecha uma janela real que o ficheiro não consegue fechar por
desenho, não por descuido.** Contrário ao que o ADR-0044 tinha por hipótese mais provável
("an fd needs to earn its added complexity against that baseline").

## A pergunta

D5 deixa em aberto se um fd herdado (`memfd_create`/pipe, passado através de um `fork`)
fecha uma janela real que o precedente já existente — ficheiro `0600` +
`O_CREAT|O_EXCL` + nome único + `unlink` dos dois lados, o que `__apirun` já usa
(`delonix-cri::write_run_spec`, `cmd/dockerapi.rs`) — não fecha. Se não fechar, o D5 fica
com a forma do ficheiro, generalizada.

## Como se mediu

Harness autónomo (`harness.rs`, sem depender de nenhum crate do `delonix-runtime` — só
`std` + `libc`, que já é dependência do workspace via `delonix-linux`/`delonix-vm`/
`delonix-sdn`/`delonix-state`, logo zero superfície de supply-chain nova), com quatro
verbos: `write-file`/`read-file` (replica byte a byte o padrão de `write_run_spec`:
directório `0700`, ficheiro `0600` com `create_new`, nome único, `unlink` dos dois
lados) e `write-fd`/`read-fd` (`memfd_create` **sem** `MFD_CLOEXEC`, offset reposto a
zero antes do `fork`, número do fd passado como um argumento de string — nunca o
conteúdo, nunca um nome a procurar no `PATH`).

1. **Correcção**: um par de cada, confirmando que o fd sobrevive a um `execve` REAL para
   um binário diferente (não só um `fork` dentro do mesmo processo) — o ponto que D5
   exige e que P1b nunca tinha medido para este mecanismo.
2. **Concorrência**: 50 pares em paralelo de cada transporte, cada um com um payload
   distinto e uma verificação própria (`checksum` FNV-1a comparado pelo ESCRITOR contra o
   que o leitor devolveu) — sem depender de uma leitura humana do log.
3. **Janela de crash**: o escritor morre (via `--die-after-write`) logo a seguir a
   escrever, antes de gerar o leitor e antes do seu próprio `unlink` defensivo — a mesma
   classe de falha que uma OOM-kill ou um `SIGKILL` externo provoca a qualquer processo.
4. **Prova estrutural**: `strace -f -e trace=openat,open,unlink,unlinkat` sobre os dois
   caminhos — não "por construção não deveria tocar em disco", mas as chamadas de sistema
   reais.

## O que se mediu

| | FICHEIRO (precedente) | FD (`memfd_create`) |
|---|---|---|
| Correcção, 1 par | OK, 21 bytes | OK, 19 bytes, sobrevive a `execve` real |
| Concorrência, 50 pares | **50/50 ok, 0 cross-talk, 0 leftovers** (`concurrency-50.txt`) | **50/50 ok, 0 cross-talk** |
| Escritor morre logo após escrever | **Segredo de 32 bytes fica em disco, legível pelo dono, indefinidamente** (`crash-window.txt`) | **Zero traço em disco** — confirmado por varredura E por `strace` |
| `openat`/`unlink` reais (`*.strace`) | `openat(..., O_WRONLY\|O_CREAT\|O_EXCL\|O_CLOEXEC, 0600)` + `unlink(...)` × 2 (o leitor e, depois, o escritor com `ENOENT` — a mesma corrida dupla que o `let _ =` do código real já tolera) | **Zero `openat`/`unlink` sobre qualquer caminho do payload** — só `/proc/self/maps` e `/dev/null` (bookkeeping do runtime Rust) |

## Interpretação

**O precedente de ficheiro já fecha a classe de risco que o próprio ADR-0044 tinha
nomeado** ("a spec readable by another user" — TOCTOU de `chmod`, symlink attack): o
`create_new` recusa um caminho pré-existente, o `0600` é atómico na criação, o
directório `0700` impede sequer listar. Isso está correcto e não muda.

**Mas há uma SEGUNDA classe de risco que nenhuma forma de "escrever bem" um ficheiro
consegue fechar**: entre a escrita e o `unlink`, o segredo existe em disco. Se QUALQUER
um dos dois lados morrer antes de correr o seu `unlink` — um OOM-kill, um `SIGKILL`
externo, um crash do próprio motor — o ficheiro fica órfão, com o segredo dentro, e
**nada no motor hoje varre isto** (não há ceifador para `cri/run/*.json` do jeito que há
para leases de IPAM ou marcadores de referência — ver `AGENTS.md`, "O IPAM vaza"). Não é
um bug do `write_run_spec`; é uma propriedade de qualquer transporte que escreve o
segredo num caminho antes de o entregar. Medido, não hipotético: a simulação acima
reproduz exactamente essa morte, e o segredo fica lá.

Um `memfd` não tem essa classe: não existe um `unlink` a esquecer, porque não existe um
`open()` a fazer, porque não existe um caminho. Quando o último fd que aponta para ele
fecha — por qualquer razão, incluindo a morte do processo — o kernel liberta a memória.
Não há ceifador a escrever porque não há nada para ceifar.

**Custo, medido e não hipotético**: uma chamada a `libc::memfd_create` (já dependência do
workspace), sem dependência nova; o número do fd continua a viajar por um argumento de
string — não é zero superfície de argv, é uma redução dela (de "o payload inteiro, ou um
caminho para o descobrir" para "um inteiro pequeno, que não é segredo"). SCM_RIGHTS
**não foi medido nem é necessário para este caso**: `delonix-launcher` é sempre
`fork`+`exec`ado directamente pelo processo que o invoca (CLI/CRI), nunca recebe um fd de
um processo que não o gerou — a herança simples por `fork` chega. SCM_RIGHTS só
interessaria a um consumidor que recebesse fds de processos não-filhos sobre um socket
já existente, o que não é este caso (é o caso do `delonix-netns-holder`, mas esse não
transporta `LaunchSpec`).

## Decisão para o D5

**O código do D5 fica como está** — o sketch do ADR já propõe a fd, não o ficheiro; este
spike só o confirma em vez de o rebaixar. O que muda é a certeza: o texto original
tratava isto como "intenção declarada, não medida" e como possivelmente devendo regredir
para a forma de ficheiro; passa a **medido, e o ficheiro é que fica documentado como a
alternativa com uma janela residual que o D5 existe para fechar**.

## Proven vs not validated

**Proven**: as quatro linhas da tabela acima, medidas neste host, com o binário deste
harness (não código de produção — ver a nota abaixo) e `strace` real.

**Not validated**: o mesmo harness dentro de um `unshare(2)`/userns real (o `fork`+`exec`
de `delonix-launcher` acontece a partir de dentro de um processo que já criou os seus
próprios namespaces — este spike correu no processo do utilizador, sem namespaces);
`SCM_RIGHTS` (deliberadamente fora de âmbito, ver acima); o comportamento sob AppArmor
(fica para o spike nº2, `53_P1B_LAUNCHER_SPIKE` repetido a três binários — item 2 da lista
de spikes do ADR-0044).

## O que aconteceu ao código do spike

`harness.rs` fica neste directório como evidência (não compila como parte do workspace —
é um crate `std`+`libc` isolado, correu de `/tmp` nesta sessão). Não há branch a fundir
nem a apagar: este spike nunca tocou numa árvore do `delonix-runtime`.

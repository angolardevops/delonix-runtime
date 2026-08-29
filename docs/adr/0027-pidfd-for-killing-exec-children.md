# ADR-0027 — pidfd para matar os filhos de `exec`/`attach`

**Estado:** aceite · **Data:** 2026-08-29 · **Âmbito:** `delonix-cri`

## Contexto

O servidor CRI lança um filho `delonix` por sessão de `exec`/`attach` e mata-o
quando o cliente desaparece — senão uma shell interactiva abandonada corre para
sempre, e com ela ficam presos o pty, os fds de netns e as referências aos
namespaces do container. Essa correcção já existia; o que ela usava é que estava
errado.

O killer guardava o **número do PID**. Um número não é um nome durável de um
processo. Enquanto o filho é zombie o slot está preso e o número não pode ser
reatribuído — mas quem espera por ele (`child.wait()`, numa thread destacada)
**ceifa-o**, e ceifar é exactamente o que liberta o número para reutilização.
A partir daí, `kill(pid)` nomeia o processo a quem o kernel entretanto deu esse
número.

Três sítios tinham a janela:

| Sítio | Janela |
|---|---|
| `spdy.rs` `Input::close` | o `Input` guardava `pid: u32` cru; o `Child` já tinha sido consumido pela thread que ceifa |
| `streaming.rs` `exec_tty` | a thread ceifa e só depois envia o código por um `oneshot`; o teste `exit_code.is_none()` ainda diz «não saiu» com o PID já livre |
| `streaming.rs` `exec_pipe` | idem |

O comentário que lá estava afirmava o contrário — que por a thread já ter
ceifado o sinal seria «um ESRCH inofensivo, não um sinal mal dirigido a um pid
reutilizado». O raciocínio está invertido, e é por isso que ninguém investigou
antes: quem lê encontra a garantia e segue em frente.

Probabilidade baixa — `pid_max` é 4 194 304 num kernel moderno, portanto é
preciso o contador dar a volta. Mas o sinal é `SIGKILL`, o nó corre cargas de
clientes, e a vítima seria trabalho de outra pessoa, sem nada em log nenhum a
ligar o facto a nós.

## Decisão

Um `ChildHandle` (`crates/delonix-cri/src/child_handle.rs`) envolve
`pidfd_open(2)` e `pidfd_send_signal(2)`. Um pidfd refere-se ao **processo**, não
ao número: o sinal chega àquele processo ou falha com `ESRCH`, nunca a outro.

O handle é aberto **imediatamente a seguir ao `spawn`**, antes de qualquer coisa
poder ceifar o filho. Abri-lo mais tarde não é erro mas também não é garantia.

## Alternativas recusadas

**Guardar o `Child` e usar `Child::kill`.** O `wait()` bloqueante tem de viver
algures; partilhar o `Child` entre quem espera e quem mata dá impasse — o
esperador segura o lock precisamente enquanto está bloqueado.

**Trocar `wait()` por `try_wait()` em ciclo.** Fecha a janela à custa de
transformar uma espera bloqueante barata em polling, num caminho que existe uma
vez por sessão de exec.

**Matar o grupo de processos.** O identificador de grupo recicla-se pela mesma
razão que o PID.

**Só documentar.** Foi considerado e recusado: um SAFETY honesto aqui teria de
dizer «isto pode sinalizar um PID reciclado», o que é melhor do que a garantia
falsa mas deixa o defeito de pé.

## Consequências

Piso de **Linux 5.3** para a garantia (`pidfd_send_signal` entrou em 5.1,
`pidfd_open` em 5.3). Não é piso para funcionar: se o `pidfd_open` for recusado,
o `ChildHandle` cai no `kill(pid)` de sempre e herda a janela — igual a hoje,
melhor em todo o lado onde o kernel deixa. O `is_process_stable()` diz qual dos
dois está em uso, e o teste que prova a propriedade salta-se sozinho no
fallback, em vez de afirmar o que ali não é verdade.

As três syscalls já estavam na lista de permitidas do seccomp
(`delonix-runtime/src/seccomp_profile.rs`), portanto não há perfil a alargar.

O `HeldChild` do `delonix-runtime` **não** muda: mata e só depois faz `waitpid`,
e como nunca ceifa antes disso o zombie segura o número. É correcto pela ORDEM
das duas chamadas, e passou a ter isso escrito — com o aviso de que trocar as
linhas o transforma neste mesmo defeito.

# Conformidade CRI — resultado medido

> Suite: **cri-tools `critest` v1.36.0** (a de upstream, não uma nossa).
> Motor: `delonix-cri` v0.42.2, **rootless**, host Linux 7.0, cgroup v2.
> Reproduzir: `tests/compat/cri-conformance.sh`.

## Resultado

```
Ran 103 of 122 Specs in 570 seconds
77 Passed | 26 Failed | 19 Skipped
```

> Progressão medida: **65 → 69 → 71 → 72 → 77**. O que fechou os quatro está na secção «Segunda ronda» abaixo.
> **Correr sempre num root LIMPO**: com estado acumulado de várias execuções,
> três specs de «preserving attributes»/«Image Identifier Consistency» falham por
> poluição e não por conformidade — confirmado a passar num root virgem.

**Não é conformidade completa, e não vale a pena escrevê-lo de outra maneira.**
O número publicado é o que dá valor à afirmação: «serve um kubelet» é uma
alegação, «77 de 103 specs nomeados» é um facto que outra pessoa verifica.

## O bug que a primeira corrida encontrou

A primeira execução deu **0 de 122**. Não por conformidade: o servidor arrancou
com o `root` por omissão (`/var/lib/delonix`), que um utilizador normal não
escreve, e cada pull de imagem morria com `Permission denied (os error 13)`.
Uma causa, 103 sintomas.

Corrigido o caminho, a corrida seguinte deu **19 de 109** — e a causa dominante
era um bug a sério:

```
failed to create the ingress sandbox cri-cef8d63cf86e9f4d
```

O `delonix-cri` invocava **`delonix netns attach`**. A reorganização da CLI da
v0.30.0 moveu esse comando para **`delonix net netns attach`**, com corte limpo
e sem aliases — decisão deliberada e registada — e este chamador nunca foi
actualizado. Ou seja: **a criação de pod em rootless estava partida desde a
v0.30.0**, e o CRI é a aposta cloud-native mais forte deste motor.

Duas coisas o esconderam durante meses, e a segunda é a mais grave:

* `delonix_detached` mandava o **stderr para `/dev/null`** e devolvia um `bool`.
  A mensagem que chegava ao kubelet nomeava a vítima e escondia o assassino.
  O erro verdadeiro — `unrecognized subcommand 'netns'` — teria dito tudo no
  primeiro pod.
* Não havia nenhuma corrida de conformidade. Um teste unitário não apanha isto:
  o comando é um `Command::new` para um binário externo, e compila na mesma.

Corrigido nos dois eixos: o argv certo, e `delonix_detached_why` a propagar a
primeira linha do stderr para dentro do `Status::internal`. **19 → 65 passes com
uma linha de correcção.**

## Segunda ronda — o que fechou, e o bug do log que estava por baixo

Cinco specs fechados: `ReadOnlyRootfs`, `MaskedPaths`, `ReadonlyPaths`,
`SupplementalGroups` e `HostPID`.

**O achado de fundo foi um bug do log, não de segurança.** `ReadOnlyRootfs` e
`NoNewPrivs` falhavam ambos em «verify log contents»: o ficheiro existia,
parseava, e a linha esperada não estava lá. A causa é que o `log_shim`
escrevia `format!("{ts} stdout {stream} …")` — com **`stdout` literal**. Os dois
fluxos do contentor iam por UM pipe e saíam todos etiquetados `stdout`, e a
variável chamada `stream` era afinal a etiqueta F/P (linha completa/parcial),
confusão que ajudou o bug a sobreviver. Uma mensagem como
`touch: /tmp/test: Read-only file system` é stderr por definição, e a suite
compara a stream.

Corrigido com um **segundo pipe, só em modo CRI** (o formato não-CRI não tem
etiqueta e separar lá só interlaçaria pior) e um `poll()` sobre os dois no shim.
O caminho antigo fica byte-a-byte igual.

**Funcionalidades novas no motor**: `--group-add` (grupos suplementares,
aplicados mesmo com o contentor a correr como root), `--masked-path` (ficheiro
tapado com `/dev/null`, directório com um tmpfs vazio read-only — a técnica do
runc), `--readonly-path` (bind sobre si próprio + remount RDONLY; passar
`MS_RDONLY` no primeiro bind é ignorado em silêncio pelo kernel, a armadilha
clássica) e `--security-opt no-new-privileges=false`.

**Três regressões que eu próprio abri e fechei, todas por medição:**

1. **O verificador fail-closed exigia `NO_NEW_PRIVS` incondicionalmente.** Estava
   certo enquanto o motor o punha sempre; no momento em que um chamador o pôde
   desligar, todos esses contentores abortavam com 126 — e o sintoma eram quatro
   specs de *seccomp* a falhar, nada com o nome NNP. Uma verificação fail-closed
   tem de verificar a política PEDIDA, ou passa a ser uma segunda política que
   ninguém declarou.
2. **O `seccompiler` liga o NNP incondicionalmente** (`lib.rs:347`), porque o
   kernel só deixa um processo SEM privilégio instalar um filtro com NNP posto.
   Instalar o mesmo programa pela via crua com CAP_SYS_ADMIN resolve — é o que o
   `SCMP_FLTATR_CTL_NNP=0` do libseccomp faz — mas há **dois** filtros
   (o pré-filtro do `clone3` e o principal), e encaminhar só um não muda nada.
3. **Um aviso meu foi parar ao stderr do `exec`.** O recuo «não consegui manter o
   NNP desligado» era informação correcta escrita no sítio errado: o `ExecSync`
   do CRI devolve o stderr tal e qual e a suite compara-o com uma string exacta.
   Uma linha de diagnóstico bem-intencionada chumbou uma dúzia de specs sem
   relação nenhuma.

**O que ficou por fechar de propósito**: `no_new_privs: false` é honrado (medido:
`NoNewPrivs: 0` com `Seccomp: 2`), mas o spec continua a falhar por outra razão —
corre como uid 1000 e espera que um binário setuid escale para 0, e o
`chown_tree_once` do motor passa o rootfs inteiro para o uid pedido, o dono do
binário setuid incluído. É um compromisso de desenho antigo (imagens como o
Elasticsearch precisam da árvore escrevível), não um esquecimento, e mudá-lo
merece decisão própria.

## Terceira ronda — os mounts do CRI não existiam

**`ContainerConfig.mounts` era lido por ninguém.** Nem uma linha. Um kubelet a
montar configMaps, secrets, emptyDirs ou hostPaths não punha nada dentro do
contentor — em silêncio, porque nada dava erro. Cinco specs falhavam por isto e
lêem-se como cinco lacunas separadas em vez de uma funcionalidade em falta.

O mesmo padrão apareceu logo a seguir em **`dns_config`** e **`port_mappings`**
do sandbox: aceites pela API, deitados fora. É a assinatura desta camada — vale
grepar por cada campo do `ContainerConfig`/`PodSandboxConfig` antes de assumir
que está ligado.

Fechado com: `Mount.propagation` no motor (`:rprivate`/`:rslave`/`:rshared` no
`-v`, com a raiz do namespace a passar a `MS_SLAVE` **só** quando alguma
montagem o pede — um `MS_PRIVATE` corta toda a propagação e tornaria a flag um
no-op), `--dns`/`--dns-search`/`--dns-option`, e a tradução dos três no
`delonix-cri`.

**Armadilha do kernel, a mesma família do `MS_RDONLY`**: a flag de propagação não
pode ser combinada com `MS_BIND` na mesma chamada — passada junto é ignorada em
silêncio. Tem de ser um `mount()` à parte, depois do bind.

**Descoberta que muda a leitura da tabela**: os 3 specs de propagação e o de
readonly não-recursivo falham **no próprio harness**, a montar no host com
`operation not permitted`. Precisam de root para o `critest`, não do motor.
Somados aos 9 do AppArmor, são **13 das 31 falhas que não são lacunas nossas**.

## Quarta ronda — três bugs que se escondiam uns atrás dos outros

**`exec` não entrava no IPC namespace do contentor.** A lista era
`["user","uts","net","pid","mnt"]`. Um `exec` deve parecer o contentor visto de
dentro, e sem o IPC vê os objectos System V do HOST e os `kernel.shm*`/
`fs.mqueue.*` do host — precisamente os knobs que o `--sysctl` mexe. Medido: um
contentor criado com `kernel.shm_rmid_forced=1` reportava `0` através do `exec`,
porque o valor é resolvido na ipc-ns de QUEM LÊ. Lia-se como «o sysctl não foi
aplicado» e mandou-me investigar o caminho dos sysctls, que estava correcto desde
o início. **Quatro specs de uma linha** (2 sysctls + 2 HostIpc).

**`apply_sysctls` deitava fora o erro da escrita** (`let _ = fs::write`). Um
sysctl pedido e silenciosamente não aplicado é a pior falha que este repo nomeia:
o contentor corre, o registo diz que o knob está posto, e o valor lá dentro é o
que era. Agora reporta.

**Um membro de pod levava DOIS caminhos de publicação.** A netns é do pod, não
dele — mas o código só excluía o caso `--net <custom>`, por isso um membro
ganhava um slirp por-contentor a reclamar a mesma porta que o ingress já tinha
publicado. Medido antes de corrigir: o DNAT `10.0.2.100:12345 → 10.200.0.2:80`
estava certo, o nginx respondia 200 de dentro do holder, e `curl 127.0.0.1:12345`
do host ficava pendurado. Pelo caminho apareceu a **quinta** ocorrência da mesma
armadilha antiga: `c.ip` nunca era atribuído a um membro de pod, por isso o
registo descrevia um contentor sem endereço.

**`NetworkReady` era um impasse.** A condição era `infra.up`, e este motor é
daemonless: a netns de infra arranca A PEDIDO. Num nó acabado de arrancar isso
dava `NetworkReady: false` → o kubelet marcava-o NotReady → não agendava pod
nenhum → nada trazia a infra acima → NotReady para sempre. A verificação existia
para apanhar uma falha real de SDN e descrevia o estado normal de repouso do
motor. Passou a distinguir pelo ref-count: infra em baixo COM workloads é falha,
em baixo sem nenhum é ócio.

## As 26 falhas restantes, por área

| Área | # | Natureza |
|---|---|---|
| AppArmor | 9 | **Ambiente** — carregar perfis exige privilégio que rootless não tem |
| Mount propagation | 3 | **Ambiente** — o `critest` não consegue montar no host sem root |
| Mount readonly não-recursivo | 1 | **Ambiente** — mesma razão |
| Streaming (portforward ×2) | 2 | **Lacuna real** |
| Seccomp profile path | 2 | Um é perfil custom (recusa fail-closed); o outro é **divergência deliberada** — ver abaixo |
| Image Manager | 2 | Um é `DeadlineExceeded` num pull de rede — potencialmente ambiente |
| `PodPID` | 1 | `shareProcessNamespace` — toca o `spawn()`, decisão própria |
| Port mapping só com container port | 1 | **Arquitectura**: o IP do pod é inalcançável do host em rootless |
| NoNewPrivs | 1 | Honrado, mas o `chown_tree_once` tira o dono ao binário setuid |
| Restantes (log reopen, stats por label, OOM, multi-container) | 4 | **Lacuna real** |

**13 das 26 são de ambiente** (AppArmor + os 4 de mount). Das 13 restantes, duas
são decisões de desenho por tomar e uma é um limite arquitectural do rootless.

### Divergência deliberada: sem perfil ≠ sem confinamento

O spec «nil profile, which is unconfined» exige que um contentor SEM perfil
declarado corra com `Seccomp: 0`. É o que o containerd faz. **Aqui não**: sem
perfil aplica-se o allowlist embutido do motor, e o valor é `Seccomp: 2`.

Somos mais restritos do que a especificação pede, e isso não se vai mudar para
ganhar um spec. Fica registado como divergência conhecida, não como lacuna.

Nenhuma destas é surpresa de arquitectura; são superfície por escrever. Tirando
o AppArmor, que é ambiente, os maiores blocos restantes são **propagação de
mounts** (3) e **rede** (3), e o próximo com valor real para um kubelet é
`shareProcessNamespace`, que já estava identificado como Fase 3 do trabalho de
pods e toca o `spawn()` de ~405 linhas que este repo assinala como função de
risco.

## O que este número NÃO diz

* Não foi corrido como **root**. Várias falhas (AppArmor, HostPID) podem mudar
  de sinal aí; não foi medido, portanto não se afirma.
* `critest` não exercita um kubelet real. Um cluster kubeadm a arrancar sobre
  este motor está provado à parte (ver o `CLAUDE.md`, secção «CLUSTER KUBERNETES
  REAL A CORRER»), e é uma prova diferente — mais estreita e mais funda.
* 19 specs foram **skipped** pela própria suite (features que ela deteta como
  não anunciadas). Não contam como passe nem como falha.

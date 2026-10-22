# Plano de casos por folha

> **O que este documento é.** Que casos cada folha deve ter, e o que conta como
> prova de cada um. É o contrato que o `scripts/folhas_gate.py` impõe sobre o
> `folhas.tsv`.
>
> **Porque não são 538 blocos escritos à mão.** Porque isso seria uma mentira
> em duas direcções: o bloco do `container ls` e o do `vm image push` seriam
> idênticos e ninguém os leria, e as 538 cópias envelheceriam sem ninguém dar
> por isso — exactamente o que aconteceu ao «EXECUTA 91 — 37 %» que viveu um mês
> num comentário do `e2e.sh`. Logo: **regras de aplicabilidade por superfície**,
> que o gate pode verificar, e **blocos próprios só onde o caso é próprio**.

## Os doze casos, e o que conta como prova

| # | caso | o que conta como prova — e o que NÃO conta |
|---|---|---|
| 1 | **Caminho feliz** | o efeito no estado real: kernel, cgroup, netns, nftables, ficheiros do state root, a resposta do provider. **Não** o exit code sozinho |
| 2 | **Entrada inválida** | `../`, `/`, unicode, vazio, limites; CIDR e referências malformadas → código `DX-` estável, exit code documentado, **e nenhum efeito** |
| 3 | **Não encontrado / conflito** | inexistente, duplicado, apagar em uso → a CLASSE da recusa (3 parado / 4 inexistente / 5 conflito), não só «falhou» |
| 4 | **Idempotência** | 2.º `apply` sem alterações; `rm` do já apagado com resultado documentado; `Idempotency-Key` repetida → a MESMA resposta |
| 5 | **Concorrência sem daemon** | dois processos no mesmo recurso: um ganha, o outro falha limpo; ledger e locks coerentes (ADR-0076) |
| 6 | **Interrupção** | SIGINT e SIGKILL a meio de cada mutação; a corrida seguinte recupera ou limpa; **zero** veth, bridges, regras, montagens, cgroups, processos ou discos órfãos |
| 7 | **Rootless vs root** | a mesma folha nos dois modos, com as diferenças esperadas escritas |
| 8 | **Saída** | `-o json`/`yaml`/tabela com esquema estável contra `docs/schema/v1`; `--help` coerente com `docs/comandos` |
| 9 | **Isolamento** | uma namespace do motor não vê nem mexe noutra |
| 10 | **Segurança** | seccomp/AppArmor/userns como declarado; scan e SBOM; assinatura verificada quando a política a exige |
| 11 | **Provider em falha** | timeout, TLS inválido, 4xx/5xx, **redirect nunca seguido**, resposta truncada → erro claro e **nada meio-criado** |
| 12 | **Upgrade** | estado escrito pela versão anterior lido pela actual |

## Aplicabilidade por superfície

`●` obrigatório · `○` obrigatório só se a folha mutar estado · `—` não aplicável

| superfície | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| CLI | ● | ● | ● | ○ | ○ | ○ | ● | ● | ○ | ○ | ○ | ○ |
| Node API (gRPC e REST) | ● | ● | ● | ● | ○ | ○ | — | ● | ● | ○ | ○ | ○ |
| Mgmt API | ● | ● | ● | ○ | ○ | ○ | — | ● | ○ | ● | — | ○ |
| MCP | ● | ● | ● | ○ | — | — | — | ● | ○ | ● | — | — |
| CRI | ● | ● | ● | ○ | ○ | ○ | ● | — | ● | ● | — | ○ |
| Kinds (× verbo) | ● | ● | ● | ● | ○ | ○ | ○ | ● | ● | ○ | ○ | ● |
| Providers (op) | ● | ● | ● | ● | ○ | ○ | — | — | — | ○ | ● | — |

Três decisões dentro desta tabela, e a razão de cada uma:

- **Caso 7 não se aplica às APIs.** O servidor herda o modo de quem o arrancou;
  testá-lo por rota seria medir o mesmo processo duas vezes. Mede-se uma vez
  por servidor, não por folha.
- **Caso 11 é obrigatório para os providers e não aplicável ao mgmt/MCP**, que
  não falam com nada remoto — fazem shell-out para a própria CLI.
- **Caso 4 é obrigatório no Node API** porque é a única superfície que honra
  `Idempotency-Key` (`service.rs:595`). No mgmt é `○` e hoje seria sempre
  falso: a string não existe no crate.

## Os três trabalhos estruturais — antes de qualquer folha

Estes não são folhas; são a razão pela qual superfícies inteiras estão em `C`.
Cada um converte dezenas de células de uma vez, e fazê-los primeiro é o que
torna o resto barato.

### E1 — a perna gRPC não existe em corrida real

A bateria é toda `curl --unix-socket`; não há `grpcurl` nem cliente gRPC em
`scripts/` ou `tests/`. **57 folhas** (cada RPC mapeada) não têm a codificação
gRPC medida. Um bloco de bateria que fale gRPC contra o mesmo socket converte-as
em conjunto.

### E2 — o `test_state()` do mgmt aponta para `/bin/false`

`delonix-mgmt/src/lib.rs:1470`. Logo **nenhuma** rota de shell-out tem caminho
de sucesso testado em lado algum: 7 rotas (`run`, `rm`, `action`, `pull`,
`build`, `networks`, `rate`) só têm o ramo da recusa. Trocar por um binário real
num root isolado converte as sete. Decisão do dono (2026-10-18): **fazer**.

### E3 — não existe `tools/call` no repositório

As 15 ferramentas MCP nunca correram pelo protocolo. Um único cliente de teste
que faça `initialize` + `tools/call` converte **15 folhas** e fecha a lacuna de
não haver ratchet sobre a lista (que o `folhas_gate.py` agora já guarda).

## As folhas da CLI — blocos próprios

**Actualizado a 2026-10-20.** Das 19 originais, **nove** são agora classe `A`:
as quatro que eram `B` sem bloqueio de host (`net l4guard set/clear`, `system
boot enable/disable`) e as cinco que o `socat` destrancou (`net netns
exec/detach/firewall/publish/unpublish`). **Já não há nenhuma `B` na CLI.** As
**10** que restam são todas `E` — esperam infra-estrutura, por ordem do
`lab-requisitos.md`.

> **2026-10-22, depois do merge de `origin/main`.** O trace commitado passou a
> ser o de `6dc31582` (2026-10-21), anterior aos checks deste ramo, por isso
> `net l4guard set/clear` e `system boot enable/disable` estão em **B** no
> `folhas.tsv` até o trace ser regravado de uma corrida da bateria — os checks
> continuam no `scripts/e2e.sh`, e a classe do gate é a do trace, nunca a da
> tabela. `vm pause`/`unpause` estão em **A** por esse mesmo trace.
> No mesmo dia o trace foi regravado da bateria corrida sobre o merge
> (`fa34383b`, `PASS=1577 FAIL=0 SKIP=14`, 272 de 280), e as quatro voltaram a **A**.

Atenção ao que `A` significa aqui, porque é fácil ler a promoção como trabalho
fechado: a corrida de 2026-10-20 (`2663c846`, `PASS=1340 FAIL=0`) provou o
**caso 1** de cada uma — o efeito — e não a matriz completa. Os casos
2/3/4/6/8/9 abaixo continuam em dívida, e é por isso que estes blocos ficam.

### `net netns exec` · `net netns detach` — **A** (caso 1 provado a 2026-10-20)

O `net netns attach` **já é executado**, e `up`/`down`/`gc`/`status` também.
Casos: 1 (o comando corre dentro da netns certa — compara `ip netns identify` ou
o inode de `/proc/self/ns/net` com o do container; o `detach` devolve veth e
endereço ao pool), 2 (nome com `../`, netns inexistente), 3 (`detach` duas
vezes → classe 4), 6 (SIGKILL a meio do `attach` não deixa veth órfão —
`net-lifecycle-leaks.sh` já tem o instrumento), 8 (`-o json`).

### `net netns firewall` · `net netns publish` · `net netns unpublish` — **A** (caso 1 provado a 2026-10-20)

Casos: 1 (a regra **aparece** no `nft list ruleset` e o DNAT encaminha de
facto; `unpublish` leva a regra **e** o hostfwd do slirp), 2 (spec inválida:
`abc:80`, porta 0, porta 70000, protocolo desconhecido → `DX-` estável), 3
(`unpublish` de uma porta não publicada), 4 (publicar a mesma porta duas vezes),
6 (SIGKILL entre o `add_hostfwd` e o DNAT não deixa meio-publicado), 9 (a regra
de uma namespace não afecta outra).

### `net l4guard set` · `net l4guard clear` — **A** (caso 1 provado a 2026-10-20)

O `net l4guard status` já corre. Casos: 1 (o guard **existe** no ruleset com as
duas contagens, e o `status` lê-as de volta), 2 (`set 0 0`, negativos, não
numéricos), 4 (`set` duas vezes = actualiza, não duplica; `clear` duas vezes),
6 (SIGKILL no `set` não deixa meio-guard).

### `system boot enable` · `system boot disable` — **A** (caso 1 provado a 2026-10-20), com precaução

**A precaução é o caso:** escrevem units de utilizador e activam linger, fora do
`DELONIX_ROOT`. A corrida tem de redirigir `XDG_CONFIG_HOME` para o root
isolado e provar que **nada** apareceu em `~/.config/systemd/user` — senão
viola «cada execução deixa o host como o encontrou».
Casos: 1 (a unit existe, com o `Restart=` da política do container), 2
(`--restart` inválido), 3 (`disable` sem nada instalado), 4 (`enable` duas
vezes), 1-bis (units obsoletas de containers que já não existem são removidas).

### `vm pause` · `vm unpause` — **E** (imagem); **A** no trace de `6dc31582` (2026-10-21)

Reais nos três backends; o default do trait falha fechado de propósito.
Casos: 1 (a VM fica `Paused` **e** o VMM continua vivo com a memória — distingue
de `stop`; o `unpause` devolve-a a `Running`), 3 (`pause` de uma VM parada →
recusa nomeada; `unpause` de uma que não está em pausa), 7 (os dois modos), 12.

### `image push` · `image sign` — **E** (registry)

Casos: 1 (o manifesto **está** no registry e o digest bate; a assinatura fica ao
lado e o `image verify` aceita-a), 2 (referência malformada, tag `latest` sob
política que a proíbe), 3 (push de imagem inexistente), 4 (push duas vezes =
mesmo digest; `sign` sem `--force` não re-assina), 10 (a chave privada **nunca**
é impressa — só o caminho da pública), 11 (registry a dar 500, TLS inválido,
**redirect não seguido**, resposta truncada).

### `vm image pull` · `vm image push` · `vm image ls-remote` — **E** (registry)

Casos: 1, 2 (source malformada), 3 (tag inexistente), 4, 11 (os mesmos cinco
modos de falha do registry).

### `vm image build` — **E** (rede)

Casos: 1 (a imagem resultante arranca), 2 (`VMfile` inválido), 6 (SIGKILL a meio
não deixa disco meio-construído no store).

### `cluster create` — **E** (imagem `kindest/node`)

Casos: 1 (o apiserver responde e `kubectl get nodes` dá `Ready`), 2
(`--workers` negativo, `--control-planes 2` sem LB → a recusa que o `--help`
promete), 3 (dois `create` com o mesmo `--name`), 6 (SIGKILL a meio não deixa
nós nem kubeconfig órfãos).

### `cluster kubeadm` — **E** (golden + VMs)

Os mesmos, sobre VMs em vez de containers, mais: a rede própria do cluster é
criada **e removida** pelo `cluster destroy`, e uma rede nomeada por quem chama
**nunca** é removida.

## As restantes superfícies — o que o gate exige, por classe

- **`D` (29 no Node API, 6 no mgmt, 2 no CRI, 23 em Kinds):** enquanto a decisão
  do dono não aterrar em código, o caso obrigatório é o **3** sobre a recusa —
  que ela é a recusa certa (`501 DX-6001` / `grpc-status 12` / `DX-6302`) e não
  um 404 nem um 500. Decisão de 2026-10-18: implementar as 29 do contrato de
  nó; até lá, a recusa é o que se prova.
- **`C` (15 + 19 + 10 + 5 + 8):** o caso em falta é o **1** na camada que falta
  — a maioria resolve-se com E1, E2 e E3 acima, não folha a folha.
- **`E` (10 na CLI, 8 em Kinds):** nada a fazer até a infra-estrutura existir. O
  gate aceita-as **com a razão escrita**, e o `lab-requisitos.md` nomeia a peça.

## Ordem de trabalho e estimativa

| # | trabalho | folhas convertidas | estimativa |
|---|---|---:|---|
| 1 | E3 — cliente `tools/call` para o MCP | 15 | 1 sessão |
| 2 | ~~As folhas `B` da CLI (`net netns`, `l4guard`, `system boot`)~~ — **FEITO 2026-10-20** (`87992e4f`): 9 folhas, não 7, porque as cinco do `net netns` estavam em `E` e não em `B` | ~~9~~ | — |
| 3 | E2 — `test_state()` com binário real | 7 | 1 sessão |
| 4 | E1 — a perna gRPC na bateria | 57 | 1–2 sessões |
| 5 | Registry local → as 5 folhas de push/pull | 5 | 1 sessão |
| 6 | Imagem de VM → `vm pause`/`unpause` | 2 | 1 sessão |
| 7 | Os 7 defeitos achados (ver o inventário) | — | 1–2 sessões |
| 8 | **As 29 RPCs do contrato de nó** (decisão do dono) | 29 | **semanas** |
| 9 | As 40 operações do mgmt a sério (decisão do dono) | 32 | 2–3 sessões |
| 10 | Kinds: os 9 sem `apply` em corrida real | ~45 células | 2–3 sessões |

O passo 2 já está dentro do total: **341/538 (63,4 %)** a 2026-10-20
(**349/550** a 2026-10-22, depois do merge de `origin/main` — ver a nota acima
sobre as quatro folhas que voltaram a `B`). Os passos
que faltam de 1 a 7 levam-no a cerca de **427/538 (79 %)** sem uma linha de
funcionalidade nova. Os passos 8 a 10 são projecto, não sessão, e o 8 é o maior
de todos: quatro serviços inteiros.

**Nota de honestidade sobre o 8 e o 9.** Foram decididos a 2026-10-18 na forma
maximal («implementar as 29», «as 40 a sério»). Nenhum dos dois cabe no
time-box de 45 min por folha que o plano usa para o resto, e nenhum cabe nesta
sessão. Entram como **contrato próprio**, não como alargamento silencioso
deste.

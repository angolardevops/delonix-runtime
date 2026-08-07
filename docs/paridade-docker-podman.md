# Paridade com Docker e Podman — levantamento e plano

> **Estado:** levantamento feito a 2026-08-06 contra `delonix 0.42.2`, a partir
> de uma sessão real de uso (montar e operar a stack do Delonix Meet com Odoo)
> e da leitura do `--help` de cada grupo de comandos.
>
> **Não é uma auditoria.** Não li os 76 mil linhas dos 10 crates, não medi
> desempenho, não exercitei `vm` nem `serve cri`. Onde não verifiquei, digo-o.

---

## 1. Porque é que este documento começa por uma correcção

A primeira versão desta análise afirmava que faltavam ao Delonix as *restart
policies*, o subcomando `restart`, os health checks, os limites de recursos e
o registo em registries privados.

**Estava errada em quase tudo.** Todos existem. O que aconteceu foi o erro
mais comum numa comparação de ferramentas: inferir ausência a partir de um
sintoma, sem ir ver.

O mesmo se passou com a resolução de nomes. Concluí que "não funcionava"
porque tive de fixar IPs. Existe um servidor DNS embutido
(`delonix-net/src/infra.rs`), a escutar UDP `:53` em todas as bridges,
funcionalmente equivalente ao dnsmasq — que não corre rootless — e já com uma
correcção de um bug em que uma query lenta bloqueava o node inteiro.

O que me falhou foi outra coisa, e é a lição central deste documento:
**o engine caiu para host-net e eu não soube.** Não é um bug de rede. É
ausência de sinal sobre um modo degradado — categoria diferente, e mais
importante do que qualquer funcionalidade em falta.

---

## 2. O que JÁ existe (verificado)

| Capacidade | Como | Nota |
|---|---|---|
| Restart policies | `--restart no\|on-failure[:max]\|always\|unless-stopped` | Supervisor destacado por contentor, sem daemon — captura o exit code real |
| `restart` | `delonix container restart <ids>` | Reutiliza rootfs e configuração original |
| `wait` | `delonix container wait` | — |
| Health check | `delonix container healthcheck <id>` | Corre o `HEALTHCHECK` da imagem, **sob demanda** |
| Limites de recursos | `--memory`, `--cpus`, `--cpuset` | Exigem cgroup delegation |
| Eventos | `delonix system events`, `system monitor` | — |
| Registry privado | `delonix image login` / `logout` | — |
| DNS entre contentores | servidor embutido no holder de ingress | Equivalente ao dnsmasq, rootless |
| `depends_on` com condições | `service_started` / `service_healthy` / `service_completed_successfully` | Resolvido por ordenação topológica |
| Pods, microVMs, CRI, Docker API | `pod`, `vm`, `serve cri`, `serve docker-api` | Fora do alcance do Docker/Podman |

Conclusão: **a lacuna não é de funcionalidades.** É de *operabilidade* — saber
o que se passa — e de *ergonomia* — o custo de chegar lá.

---

## 3. Lacunas reais

### L1 · Degradação silenciosa (a mais grave)

Quando a netns do holder falha, o stack cai para host-net. O contentor
arranca, parece saudável, e a resolução de nomes deixou de existir. Nada no
`ps`, no `inspect` ou nos logs o diz.

Custou-me, numa sessão: IPs fixados à mão em vez de nomes, um upstream de
nginx a apontar para um IP obsoleto, e um **502 em produção-de-demo detectado
pelo utilizador, não por mim**.

*Lente SRE.* Um modo degradado invisível é pior do que uma falha: a falha
diagnostica-se, a degradação envenena tudo o que vem a seguir.

### L2 · Health check é sob demanda, não contínuo

`container healthcheck` corre o `HEALTHCHECK` e sai com 1 se não estiver
saudável — "usable in a script/CI", diz a própria ajuda. Não há
`--health-interval` que faça o engine monitorizar, nem o `STATUS` do `ps`
distingue `Up` de `Up (healthy)`.

Consequência directa: o `depends_on: service_healthy` do compose, que **está
implementado**, tem de resolver a saúde de alguma forma — mas quem opera não
tem como ver esse estado nem construir alertas sobre ele.

### L3 · `--add-host` — FEITO, e o que a revisão apanhou

O `/etc/hosts` do contentor é recriado no arranque, portanto entradas
injectadas à mão desapareciam em cada reinício. Implementado com persistência
no registo, validado ao vivo (sobrevive a `restart`, `getent` resolve).

**A revisão de segurança encontrou um achado ALTO que esta funcionalidade
armou**, e vale registar porque a lição é maior do que o bug:

`write_etc_files` escrevia com `format!("{rootfs}/etc/...")` + `fs::write`, e
tanto `metadata()` como `write()` SEGUEM symlinks. O rootfs é conteúdo
controlado pela imagem e, em rootless, a árvore pertence ao uid mapeado — o
próprio contentor podia plantar `etc/hosts -> ~/.ssh/authorized_keys` e o
arranque seguinte escrevia-o **como o motor**, fora do rootfs.

Isto já existia, mas era um primitivo de *truncar com conteúdo fixo do motor*.
O `--add-host` tornou o conteúdo **escolhido pelo atacante** — o que o
converte em execução de código. Provado ao vivo antes da correcção.

Fechado com `safe_bind_target` (que já existia no MESMO ficheiro, e que este
repo já tinha aplicado duas vezes — a bind mounts e ao `COPY` do build; este
caminho ficara de fora) + validação na fronteira: endereço parseado como
`IpAddr`, nome por whitelist LDH, `\n`/`\t`/espaço recusados, e erro em vez de
descarte silencioso.

**Lição**: acrescentar conteúdo controlado pelo utilizador a um caminho de
escrita já existente muda a classe de risco desse caminho. Não basta rever o
código novo — é preciso reavaliar o que ele passa a alimentar.

### L4-bis · Uma lacuna que eu inventei

Ao escrever a promessa de estabilidade afirmei que faltava `--format json` nos
comandos de listagem. **Não falta**: `-o json` existe desde a ADR-0005 e
funciona nos nove comandos de listagem, verificado um a um.

Fica registado porque é a SEGUNDA vez neste documento que o mesmo erro aparece —
e a primeira está logo na secção 1. Inferir ausência a partir de um sintoma, sem
ir ver, é o modo de falha característico de comparar ferramentas.

### L4 · Ergonomia divergente

`delonix container ps`, não `delonix ps`. Isoladamente é um detalhe;
multiplicado pelos verbos usados dezenas de vezes por dia, é atrito constante
para quem vem do Docker ou do Podman.

### L5 · cgroup delegation é trabalho do utilizador

O engine avisa que sem delegação não há limites de memória/CPU — honesto —
mas resolver implica saber montar um `systemd --user` com `Delegate=yes`.
Devia ser um comando.

### L6 · `serve docker-api` é uma fatia não documentada

Enquanto for parcial e sem matriz publicada, ferramentas de terceiros
(Testcontainers, plugins de CI, IDEs) partem de forma imprevisível. Falhar com
"não implementado" explícito vale mais do que aproximar-se do completo.

---

## 4. Plano

### Fase 1 — operável

1. **Estado de rede explícito.** `bridge` vs `host (degradado: <razão>)` no
   `ps`, no `inspect` e um aviso, uma vez, no log. Fecha L1.
2. **Health check contínuo.** FEITO. `--health-cmd`/`--health-interval`/
   `--health-timeout`/`--health-retries`/`--health-start-period`; o `STATUS` do
   `ps` passa a `Up 21 seconds (healthy)`, o `describe` mostra sonda, política e
   último veredicto, e uma mudança de estado emite um evento. Fecha L2.

   **Quem monitoriza, num motor sem daemon** — era a decisão de desenho. É o
   **supervisor do container destacado**: já existe, um por container, já
   sobrevive à CLI, e morre com aquilo que vigia. Sem processo de frota, sem
   ciclo de vida novo. O custo é um container em primeiro plano não ser
   monitorizado, e isso está certo — estás a olhar para ele.

   **O probe limita-se a si próprio**, dentro do container (`sh` com watchdog).
   Não foi escolha estética: o `runtime::exec` bloqueia num intermediário, e
   matar esse intermediário deixa o probe vivo no pid-namespace do container —
   uma fuga que se repetia a cada intervalo, para sempre. Um probe que se mata
   não tem esse buraco e não precisa de plumbing novo do lado do host.

   Validado ao vivo: `starting` durante os 6s em que o ficheiro não existia,
   `healthy` no segundo exacto em que apareceu; e com `retries 3`, a sequência
   1 → 2 → 3 antes de virar `unhealthy`, com um sucesso a zerar a contagem.
3. **`--add-host` persistido** na spec e reaplicado no arranque. Fecha L3.
4. **`run --wait`** — FEITO. Bloqueia até o `HEALTHCHECK` da imagem passar.
   Medido: 64 ms sem a flag (com o serviço ainda a arrancar) contra 6086 ms
   com ela, e o serviço pronto à saída. Sem `HEALTHCHECK` na imagem é ERRO,
   nunca um retorno instantâneo. Reutiliza a resolução do comando que o
   `depends_on: service_healthy` do compose já usava, para os dois não
   poderem divergir.

   **Dois bugs reais encontrados ao ligar isto**, ambos silenciosos:

   * **O supervisor tomou todo o caminho `-d`**, não só o `--restart` — e
     retorna cedo. A primeira versão da flag era aceite e ignorada em
     silêncio na esmagadora maioria dos `run -d`. Só apareceu porque o
     `eprintln!` de depuração nunca chegou a imprimir.
   * **`commit_flat_rootfs_from_tar` gravava `healthcheck: None` fixo.** Um
     `HEALTHCHECK` no Dockerfile era parseado, aceite e DESCARTADO — no
     caminho rootless, que é o normal. O caminho overlay (root) sempre o
     honrou, portanto o mesmo ficheiro produzia imagens diferentes conforme o
     modo do motor, e o `depends_on: service_healthy` do compose falhava com
     "declares no healthcheck" contra uma imagem cuja fonte declara uma.

### Fase 2 — adoptável

5. **Aliases de topo** — FEITO. `ps`, `run`, `exec`, `logs`, `rm` e `images`.
   Implementados por **reescrita de argv**, não por variantes clap duplicadas:
   uma segunda declaração das ~70 flags do `run` é uma segunda declaração para
   manter sincronizada, e no dia em que divergissem o atalho deixava de aceitar
   uma flag em silêncio. Assim são o MESMO comando por construção, `--help`
   incluído. `stop`/`start` ficam deliberadamente de fora: este motor também
   pára VMs e pods, e um `delonix stop <nome>` a significar «container» em
   silêncio seria adivinhar — `workload stop` já cobre o caso e recusa a
   ambiguidade em voz alta. Fecha L4.
6. **`system setup [--delegate]`** — FEITO. Diagnostica e corrige a delegação de
   cgroup. **Dois remédios, porque há dois problemas** — a metade que toda a
   resposta de StackOverflow perde: o drop-in em `user@.service` resolve serviços
   de utilizador e logins futuros, mas NÃO a shell onde estás a escrever, porque
   um `session-N.scope` é IRMÃO do `user@.service` e não herda nada dele
   (medido neste host: `subtree_control` é `root:root` num, do utilizador no
   outro). Fecha L5.
7. **Matriz do `serve docker-api`** — FEITO. `delonix serve docker-api --matrix`
   imprime as 14 rotas implementadas e as 7 deliberadamente ausentes com a razão
   de cada uma; o 404 aponta para lá em vez de só recusar. **A matriz não pode
   divergir**: um teste lê o código-fonte do próprio dispatch e falha se existir
   um braço sem linha na tabela. Esse teste já apanhou uma ambiguidade real
   (uma limitação de UM CAMPO do `create` estava arquivada como se a rota
   inteira faltasse). Fecha L6.

### Fase 3 — defensável

8. **CRI validation** — FEITO, e é o item que mais valeu a pena. `critest`
   v1.36.0 de upstream, corrido a sério: **65 de 103 specs**, publicado em
   [cri-conformance.md](cri-conformance.md) com o script para reproduzir.

   **Encontrou um bug grave e antigo.** O `delonix-cri` invocava `delonix netns
   attach`; a reorganização da v0.30.0 moveu-o para `net netns attach` com corte
   limpo, e este chamador nunca foi actualizado — **a criação de pod rootless
   estava partida desde então**. Escondido por o `delonix_detached` mandar o
   stderr para `/dev/null` e devolver um `bool`: a mensagem que chegava ao
   kubelet nomeava a vítima e escondia o assassino. Uma linha de correcção
   levou a corrida de **19 para 65 passes**.

   Das 38 falhas restantes, 9 são de ambiente (AppArmor rootless) e as outras
   são superfície por escrever — 8 delas em Security Context, que é por onde
   começar.

9. **Promessa de estabilidade da CLI** — FEITA, em
   [cli-stability.md](cli-stability.md). Diz o que é estável (verbos de ciclo de
   vida + JSON do `inspect`), o que é estável em CONTEÚDO mas não em formato (as
   tabelas — para automação usa-se `inspect`), e o que não promete nada
   (`serve *`, `cluster`, `vm`, schemas de manifesto).
10. **Testes de fumo** — FEITO. `tests/compat/docker_api_smoke.py` corre a
   sequência que ferramentas de terceiros usam de facto (`create` → `start` →
   `inspect` → `rename` → `stop` → `remove`, uma chamada de cada vez), com o SDK
   oficial de Python — o mesmo protocolo dos clientes Java/Go/Node que o
   Testcontainers usa. **14/14 verdes** contra o nosso socket. Não é `docker run`
   de propósito: nenhum harness de testes usa `docker run`.

---

## 5. O que NÃO fazer

**Não perseguir paridade com o Docker.** É uma corrida perdida e sem prémio:
quem quer Docker usa Docker. Paridade só interessa onde a falta dela *impede*
de usar o que é diferenciador — microVMs e CRI.

**Não construir um Docker Desktop.** Custo enorme, retorno para quem não é o
público-alvo.

**Não prometer produção a terceiros antes da Fase 1.** Um engine cujo modo
degradado é invisível não é operável por quem não o escreveu.

---

## 6. As quatro lentes, em resumo

| Lente | O que vê | Prioridade |
|---|---|---|
| **SRE** | Não é possível saber em que estado está. Degradação silenciosa, saúde não observável | Fase 1 (1, 2) |
| **DevOps** | Atrito diário: caminho longo para verbos quentes, esperas escritas à mão | Fase 1 (4), Fase 2 (5) |
| **Platform Eng** | Já à frente em capacidades; falta **contrato** — o que é estável, o que está coberto | Fase 2 (7), Fase 3 (9) |
| **Cloud Native** | `serve cri` é a aposta estruturalmente mais forte; exige conformidade demonstrada | Fase 3 (8) |

---

## 7. Método

Cada item fecha com: implementação, teste que falha sem ela, e validação pelo
revisor da área (`delonix-code`, `delonix-devops`, `delonix-security-compliance`).
Nada é dado por feito só porque compila — foi assim que a primeira versão
deste documento nasceu errada.

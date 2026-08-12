---
name: delonix-carga
description: Teste de carga, desempenho e caça a fugas de recursos no delonix-runtime — latência de arranque, comportamento sob N containers/VMs em paralelo, contenção de locks, e fugas de memória, fds, PIDs, disco, portas, entradas nft e cgroups órfãos. Mede primeiro, com linha de base, e só depois propõe. Usa quando o utilizador pedir teste de carga/stress/performance/benchmark, reportar lentidão, ou perguntar se há vazamento/fuga de recursos.
---

# Carga, desempenho e fugas — mede-se, não se estima

Este motor **já teve fugas reais em produção**, e todas passavam nos testes
unitários. Nada nesta skill é teoria: cada item veio de um incidente medido.

## A regra que separa isto de um benchmark de brochura

**Um número sem linha de base não diz nada.** Antes de tocar em código, mede o
estado actual, guarda o número, e mede outra vez depois. Um «ficou mais rápido»
sem os dois números é uma impressão.

E **mede a coisa, não um proxy**: `$?` depois de um pipe é do último comando do
pipe; um comando cancelado não é um comando que passou; um `du` do filesystem
inteiro mede a produção ao lado, não o teu cenário (foi assim que a primeira
versão do `scen_scale` reportou 1168 MiB de «fuga» que eram outra coisa a
escrever no mesmo disco).

## As fugas que este motor já teve — procura primeiro estas

| Fuga | Como apareceu | O que a causou |
|---|---|---|
| **Disco** | kubelet aplicou `disk-pressure`; 49 rootfs órfãos, ~45 GiB | directórios de container sobrevivem a mortes abruptas e ninguém os reapa |
| **Zombies** | `ps` com `<defunct>`, `inspect` a dizer `Running` para sempre | `spawn()` sem `waitpid` num servidor que NUNCA sai (`serve docker-api`) |
| **File descriptors** | sockets HTTP de outras ligações segurados para sempre | o `log_shim` (fork sem execve, vive para sempre) só fechava fds 0/1/2 |
| **Portas** | `slirp4netns` órfão a segurar a porta depois de um `run` falhado | `--rm` num caminho de erro sem teardown |
| **Refcount** | ingress com 16 referências e 3 containers vivos | contabilidade incrementada sem par no caminho de falha |
| **Threads** | colheita de disco presa >1 min bloqueava o TUI e estourava o scrape | trabalho caro em linha; hoje `collect_with_timeout` e refresh em background |
| **Regras nft** | ~100 regras percorridas por pacote com 49 containers | 2 jumps por IP por container; hoje um verdict map e 2 regras fixas |

**A pergunta que apanha uma fuga nova:** *este recurso é criado num caminho e
libertado em quantos?* Se a criação tem um sítio e a libertação tem dois (sucesso
e falha), verifica o de falha — é sempre esse que falta.

## Como montar uma corrida de carga aqui

**Não instales ferramentas de carga neste host.** Não há `wrk`/`hey`/`ab`/`fio`/
`stress-ng`/`valgrind`/`heaptrack`, e o host tem produção a correr. O que há e
chega: `perf`, `bpftrace`, `pidstat`, `/proc`, `nft`, e o próprio `scripts/
chaos.sh` (que já tem `scen_scale`, `scen_oom`, `scen_disk_full`,
`scen_aggregate_ceiling`).

**Isolamento, sempre** — o mesmo do caos:

```bash
export DELONIX_ROOT=<scratchpad>/root
export DELONIX_NET_RUNTIME_DIR=<scratchpad>/run
# prefixo próprio em TODOS os nomes criados, e limpa no fim
```

**Escala com o que já existe** antes de escrever harness novo:

```bash
DELONIX_CHAOS_SCALE=60 scripts/chaos.sh    # o scen_scale já valida IPs únicos + disco devolvido
```

O `scen_scale` cobre concorrência de attach e fuga de disco. O que ele **não**
cobre, e é o teu trabalho acrescentar: latência por percentil, fds/PIDs/RSS dos
processos de longa vida, entradas nft, e o comportamento **sustentado** (o mesmo
ciclo N vezes — uma fuga de 2 MiB por ciclo só se vê ao 50.º).

## O que medir, e onde se lê

| Dimensão | Onde se lê (medição, não estimativa) |
|---|---|
| Latência de arranque | `/usr/bin/time -f %e` por invocação, N vezes → p50/p95, nunca a média sozinha |
| Memória de um processo | `VmRSS` em `/proc/<pid>/status` (o pin, o controlo, o slirp, o shim de logs, o `serve *`) |
| File descriptors | `ls /proc/<pid>/fd | wc -l` ao longo de N ciclos — a inclinação é a fuga |
| Threads/PIDs | `Threads:` em `/proc/<pid>/status`; `pids.current` no cgroup |
| CPU sustentado | `pidstat -p <pid> 1` — um processo «adormecido» a consumir CPU é um poll |
| Disco por área | `du -sk` do `DELONIX_ROOT` antes/depois, com a limpeza feita |
| Regras por pacote | `nft list ruleset | wc -l` e as chains de dispatch — cresce com N containers? |
| Contenção de lock | tempo de N operações concorrentes vs sequenciais; o `handle_control` é ponto de serialização |
| Zombies | `ps -o stat= -p <pid>` = `Z`, ou `ps --ppid <servidor>` |

**Percentis, não médias.** Uma média esconde exactamente o caso que dói: a v0.47
mediu 15 de 30 attaches concorrentes a FALHAR — a média do tempo dos que
passaram estava óptima.

## Os caminhos quentes conhecidos deste motor

Onde procurar primeiro, com o número medido ao lado:

- **Extracção de rootfs** — a 2.ª passagem do re-exec de `--net <rede-custom>`
  reextraía a imagem inteira: 1526 ms (`--net none`) contra 3143 ms, delta de
  exactamente uma extracção. Reextrair por cima de árvore preenchida custa preço
  inteiro; não há poupança acidental.
- **Socket de controlo do holder** — é O ponto de serialização de toda a SDN. Um
  chamador em fila espera por todos os attaches à frente dele. Escala com a
  concorrência, e o tecto de leitura decide onde parte (5 s → 15/30 falhas;
  30 s → 30/30 em 21 s).
- **Colheita de disco do dash/métricas** — 68 GiB / >1 min neste host. Nunca em
  linha num tick de UI nem num scrape Prometheus (10 s por omissão).
- **Pull de imagem** — `Cas::has` antes de cada GET de blob; sem ele cada pull
  redescarrega tudo e um `kubeadm init` estoura o deadline interno do rate-limiter.
- **Compressão da golden** — zstd contra zlib: 10 s vs 53 s a comprimir, menor, e
  sobretudo muito mais rápido a DESCOMPRIMIR, que é o que conta (a golden é o
  backing file read-only de cada VM).

## Micro-benchmark: quando vale, e onde

`criterion` já é dev-dependency (`delonix-image`), `make bench` corre-o. Regras:

- **Benchar só função PURA e quente.** Benchar setup impuro (rede/disco) mede o
  SO, não o código.
- **Fuzz e bench o MESMO alvo** (o parser), para partilharem ground truth —
  `proptest` prova que não entra em pânico, `criterion` diz quanto custa.
- **Dev-only, sempre.** Nada disto pode aparecer em `cargo tree -e normal` de um
  crate de motor. É a mesma fronteira que confina `ratatui`/`hyper` ao `-bin`.
- Um micro-benchmark **não substitui** a medida ponta-a-ponta: 70 ns num parser
  não explica 3 segundos num `run`.

## Antes de propor uma optimização

1. **O número, antes e depois**, na mesma máquina, no mesmo dia.
2. **O custo do lado do desenho.** Uma cache é um invalidador novo; um índice é
   um estado novo a manter coerente; um background refresh é dados stale que
   alguém vai ler como frescos (é por isso que os campos caros do `dash` são
   `Option<u64>` — `None` explícito, nunca um `0` enganoso).
3. **Nunca troques correcção por velocidade em silêncio.** Se a versão rápida
   mede menos, o campo diz que mediu menos (`Usage { bytes, unreadable }`,
   `QuotaState.measured`). Medição incompleta é *desconhecida*, nunca zero.

## Antes de dar por feito

O que foi medido, com que carga, em que máquina, e **o que ficou por medir e
porquê** (sem 2.º nó, sem GPU, holder não respawnável com produção viva). Se a
corrida encontrou uma fuga, o fecho é um cenário em `scripts/chaos.sh` que
**falha com a correcção revertida** — a regra do repo. Ver `delonix-testing` para
a disciplina e o agente `performance-engineer` para o método de bench.

# Lab 2 — Limites de recursos que pegam mesmo

> **Identificador:** `lab-2` · **Nível:** iniciante · **Jornada:** primeiro contacto

## Objectivo concreto

Pôr um tecto de memória num container (`-m 128M`) e **provar que pegou lendo o
`memory.max` REAL do cgroup v2 do kernel** — não a saída do comando, que mente
por omissão quando a delegação de cgroup não está feita. É o erro nº1 de quem
começa em rootless: o container parece limitado e corre sem limite nenhum.

## Conhecimentos prévios

Terminal Linux e noção de cgroup v2. O Lab 1 ajuda mas não é obrigatório.

## Versão e ambiente testados

> **Baseline histórica (linha 5.1.x):** os valores abaixo foram medidos na linha antiga e **ainda não foram re-medidos na linha 0.1.0 actual**.

`delonix 5.1.1`, Linux com cgroup v2 e delegação rootless de `cpu memory pids`
(`cpuset`/`io` ausentes, e está certo). Execução rootless. Validado a 2026-10-24.

## Recursos mínimos

~8 MiB de disco para a imagem `alpine`; rede para o primeiro `pull`.

## Privilégios e acessos necessários

Nenhum `sudo` no caminho feliz. Precisa de delegação do controlador `memory`
para o teu `user@uid.service` — o que num Ubuntu de fábrica já vem ligado. Se
não vier, corre o lab dentro de um scope delegado (ver «Como correr»).

## Duração estimada

~1 minuto (dominado pelo primeiro `pull` da imagem).

## Ficheiros de preparação

Nenhum ficheiro externo. O arnês está nesta pasta:

| Ficheiro | Fase |
|---|---|
| `prepare.sh` | garante pré-requisitos e a imagem local |
| `run.sh`     | executa a jornada (system setup → run -m → localizar cgroup) |
| `verify.sh`  | asserção do `memory.max` REAL do cgroup |
| `cleanup.sh` | remove só o que o lab criou |
| `lab.sh`     | orquestrador `prepare → run → verify → cleanup` |

## Como correr

```bash
# com o delonix no PATH (instalação normal):
bash docs/labs/lab-2/lab.sh

# apontando a um binário específico (ex.: build local):
DELONIX=/caminho/para/delonix bash docs/labs/lab-2/lab.sh

# se o teu host não delegar `memory`, sem root, dentro de um scope delegado:
systemd-run --user --scope -p Delegate=yes -- bash docs/labs/lab-2/lab.sh
```

## Passos (o que `run.sh` faz) e porquê

1. `system setup` — pergunta ANTES de assumir se o host aplica limites. Se
   disser INERT/PARTIAL para `memory`, a correcção não precisa de root (scope
   delegado acima).
2. `container run -d --name limitado -m 128M alpine sleep 300` — tecto de
   memória declarado.
3. Localiza o container no cgroup v2 do teu utilizador: lê o PID de
   `container inspect` (que devolve um **array** — ADR-0005) e a linha `0::` de
   `/proc/<pid>/cgroup`.
4. `cat /sys/fs/cgroup<rel>/memory.max` — a verdade do kernel, não a saída do
   `run`.

## Checkpoints / asserções de sucesso (o que `verify.sh` exige)

- **A** — o container está `Up` (lido de `container ps -o json`).
- **B** — o `memory.max` REAL do cgroup é **`134217728`** (128 MiB). O arnês
  distingue três desfechos e **falha em voz alta** em qualquer um que não seja
  este:
  - `memory.max=max` → delegação em falta, o container corre **sem** tecto, em
    silêncio (o que o lab existe para apanhar);
  - ficheiro ausente → o controlador não desceu até ti (caso `cpuset`/`io`);
  - outro valor → o limite não pegou.

## Limpeza

`cleanup.sh` remove o container `lab2-limited` e o estado do lab. **Não** remove
a imagem `alpine` (outros labs reutilizam-na).

## Resultado aprendido

A saída de um comando pode dizer `128M` e o container correr sem limite: a única
prova é o `memory.max` do cgroup do kernel. Em rootless, o limite só pega se o
controlador `memory` estiver delegado — e sabe-se a ler, não a confiar.

## Próximo lab

**Lab 3 — Duas aplicações que se falam por nome** (rede de utilizador e DNS
interno).

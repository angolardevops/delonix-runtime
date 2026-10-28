# Lab 4 — Do Dockerfile à imagem, sem daemon

> **Identificador:** `lab-4` · **Nível:** intermédio · **Jornada:** primeiro contacto

## Objectivo concreto

Construir uma imagem a partir de um **`Delonixfile`** (sem `-f`, sem daemon) e
correr o que construíste com `--wait`, provando de forma **funcional** que o
`HEALTHCHECK` passou: a coluna STATUS de `container ps` mostra `(healthy)`, não
apenas `Up`. O motor sonda o container **sozinho** — sem systemd, ao contrário
do Podman rootless.

## Conhecimentos prévios

Terminal Linux e noção de container/imagem. O Lab 1 ajuda mas não é obrigatório.

## Versão e ambiente testados

> **Baseline histórica (linha 5.1.x):** os valores abaixo foram medidos na linha antiga e **ainda não foram re-medidos na linha 0.1.0 actual**.

`delonix 5.1.1`, Linux com cgroup v2, execução rootless. Validado a 2026-10-24
(build a partir do `Delonixfile` com precedência sobre o `Dockerfile`, e
`(healthy)` lido de `container ps -o json` — positivo `EXIT=0`; os dois modos de
falha — «build falhou» e «healthcheck nunca passou» — comprovados a dar
`EXIT=1` com mensagens distintas).

## Recursos mínimos

~15 MiB de disco para `alpine:latest` + o `curl` instalado na camada; rede para
o primeiro `pull` da base e para o `apk add`.

## Privilégios e acessos necessários

Nenhum `sudo`. Build e execução correm rootless.

## Duração estimada

~10 segundos depois da base em cache (dominado pelo `apk add --no-cache curl` e
por uma ou duas sondas do healthcheck).

## Ficheiros de preparação

Nenhum ficheiro externo. O `Delonixfile`, o `ola.txt` que ele faz `COPY` e um
`Dockerfile`-**isco** (base inexistente, para provar a precedência) são **gerados
pelo próprio lab** num diretório de build **no estado do lab** (nunca no repo) e
removidos na limpeza. O arnês está nesta pasta:

| Ficheiro | Fase |
|---|---|
| `prepare.sh` | garante pré-requisitos e a imagem base; limpa restos |
| `run.sh`     | escreve o `Delonixfile` (+ `Dockerfile`-isco), `build` sem `-f`, `run --wait` |
| `verify.sh`  | asserção do build, da precedência e do `(healthy)` |
| `cleanup.sh` | remove só o que o lab criou (container, imagem, diretório de build) |
| `lab.sh`     | orquestrador `prepare → run → verify → cleanup` |

## Como correr

```bash
# com o delonix no PATH (instalação normal):
bash docs/labs/lab-4/lab.sh

# apontando a um binário específico (ex.: build local):
DELONIX=/caminho/para/delonix bash docs/labs/lab-4/lab.sh

# deixar o container e a imagem de pé para explorar:
bash docs/labs/lab-4/lab.sh --no-clean
```

## Passos (o que `run.sh` faz) e porquê

1. Escreve um `Delonixfile` (`FROM alpine` → `RUN apk add curl` → `COPY ola.txt`
   → `HEALTHCHECK test -f /ola.txt` → `CMD`) e um `Dockerfile`-**isco** cuja base
   não existe, ambos no diretório de build.
2. `build -t minha-app:1.0 <dir>` **sem `-f`**: o `Delonixfile` é procurado
   **antes** de qualquer `Dockerfile`. Como o isco falharia o build, o build a
   passar é a prova da precedência.
3. `container run -d --name a1 --wait --health-interval 2`: o `--wait` bloqueia
   até o `HEALTHCHECK` passar (ou o `--wait-timeout` esgotar), poupando o
   `until ...; do sleep 1; done` que todo o script acaba por escrever — e mal.
4. `container ps` para veres o estado pelo CLI.

## Checkpoints / asserções de sucesso (o que `verify.sh` exige)

- **A** — o **build produziu a imagem** (`minha-app:1.0` existe no store). É a
  primeira bifurcação: sem imagem, dizemos **«BUILD FALHOU»** — não é o
  healthcheck.
- **B** — **precedência**: o `Dockerfile`-isco esteve presente e, ainda assim, o
  build passou → `delonix build` sem `-f` escolheu o `Delonixfile` primeiro.
- **C** — o container está **Up** (não saiu). Um processo que morre é dito como
  tal, não confundido com um healthcheck que não passa.
- **D** — a coluna STATUS de `container ps -o json` mostra **`(healthy)`**, com
  **retry** (o `(healthy)` só surge depois de uma ou duas sondas, mesmo após o
  `--wait`). O arnês **falha em voz alta** se nunca ficar healthy — e, como **A**
  já provou o build, essa falha é **inequivocamente** o healthcheck, dita
  **«HEALTHCHECK NUNCA PASSOU»**, nunca um build partido em silêncio.

## Limpeza

`cleanup.sh` remove o container `a1`, a imagem `minha-app:1.0` e o diretório de
build. **Não** remove a base `alpine` (outros labs reutilizam-na).

## Resultado aprendido

Uma imagem nasce de um `Delonixfile` sem daemon e sem `-f`, e o motor passa a
sondar a saúde do container **ele próprio**. A prova de que a imagem está boa
nunca é «o build não deu erro»: é o container a atingir `(healthy)` por si — e o
arnês separa, com palavras diferentes, uma imagem que não construiu de uma sonda
de saúde que nunca ficou verde.

## Próximo lab

**Lab 5 — Uma microVM a sério** (uma VM completa com o mesmo CLI dos
containers).

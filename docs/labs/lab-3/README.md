# Lab 3 — Duas aplicações que se falam por nome

> **Identificador:** `lab-3` · **Nível:** intermédio · **Jornada:** primeiro contacto

## Objectivo concreto

Pôr duas aplicações numa **rede de utilizador** própria e provar duas coisas
que uma rede sozinha não dá: a app resolve a base de dados **pelo nome** (DNS
interno, sem `/etc/hosts` nem variáveis), e um `kind: Dependency` fecha a `db`
a **toda a gente menos à app** — alcançabilidade *dirigida*. A prova é
funcional: a mesma rede, **dois desfechos** (a app alcança, um terceiro não).

## Conhecimentos prévios

Terminal Linux e noção de container/rede. O Lab 1 ajuda mas não é obrigatório.

## Versão e ambiente testados

`delonix 5.1.1`, Linux com cgroup v2, execução rootless. Validado a 2026-10-24
(DNS interno, `kind: Dependency` → `NetworkPolicy` com *default deny*, e a
distinção política-vs-serviço comprovada ao vivo — positivo `EXIT=0`, negativo
`EXIT=1`).

## Recursos mínimos

~140 MiB de disco para `postgres:16-alpine`, `nginx:alpine` e `alpine`; rede
para o primeiro `pull`.

## Privilégios e acessos necessários

Nenhum `sudo`. Tudo corre na rede de utilizador rootless.

## Duração estimada

~15 segundos depois das imagens em cache (dominado pelo arranque do postgres).

## Ficheiros de preparação

Nenhum ficheiro externo. O `dep.yaml` do `kind: Dependency` é **gerado pelo
próprio lab** (no diretório de estado) e removido na limpeza. O arnês está
nesta pasta:

| Ficheiro | Fase |
|---|---|
| `prepare.sh` | garante pré-requisitos e as imagens locais; limpa restos |
| `run.sh`     | executa a jornada (rede → db+app → DNS → baseline → Dependency → bloqueio) |
| `verify.sh`  | asserção de DNS interno e isolamento dirigido |
| `cleanup.sh` | remove só o que o lab criou (containers, rede, `dep.yaml`) |
| `lab.sh`     | orquestrador `prepare → run → verify → cleanup` |

## Como correr

```bash
# com o delonix no PATH (instalação normal):
bash docs/labs/lab-3/lab.sh

# apontando a um binário específico (ex.: build local):
DELONIX=/caminho/para/delonix bash docs/labs/lab-3/lab.sh

# deixar a rede e os containers de pé para explorar:
bash docs/labs/lab-3/lab.sh --no-clean
```

## Passos (o que `run.sh` faz) e porquê

1. `network create loja` — uma rede própria; o DNS interno **vem de graça**.
2. `container run` da `db` (`postgres:16-alpine`) e da `app` (`nginx:alpine`),
   ambas em `--net loja`.
3. A app resolve a `db` **pelo nome** (`getent hosts db` → IP) — sem config.
4. **Baseline** (antes da política): um terceiro container na mesma rede
   **alcança** a `db`. Primeiro esperamos a `db` abrir o `5432` (o postgres
   aquece devagar), senão o baseline mentia por *serviço ainda não pronto*.
5. `stack apply -f dep.yaml` do `kind: Dependency` (`app` → `db`, porta
   `5432`). Baixa para um `NetworkPolicy` *default deny* aplicado à `db`.
6. **Depois da política**: o mesmo terceiro **já não alcança** a `db`, mas a
   app continua a alcançar. Mesma rede, dois desfechos.

## Checkpoints / asserções de sucesso (o que `verify.sh` exige)

- **A** — `db` e `app` estão `Up`.
- **B** — a app resolve `db` para um **IPv4 válido** (DNS interno).
- **C** — o **par autorizado** (a app) alcança `db:5432`, com repetição. É a
  âncora que prova que o serviço está **em pé** — e distingue um bloqueio por
  política de uma `db` em baixo.
- **D** — um **terceiro** container na mesma rede está **BLOQUEADO**. O arnês
  **falha em voz alta** se o terceiro ainda alcançar (`a db ficou ABERTA A
  TODOS` — a falha que o lab existe para apanhar) e também se faltar o `nc` no
  provador (asserção inconclusiva, não um falso bloqueio).
- **E** — a app é um serviço real: **HTTP 200** na porta publicada do host.

## Limpeza

`cleanup.sh` remove os containers `db`/`app`, a rede `loja` e o `dep.yaml`. A
`NetworkPolicy` do `Dependency` vive no firewall da `db` e **vai-se com ela**
(comprovado ao vivo) — não há política órfã a limpar à parte. **Não** remove as
imagens (outros labs reutilizam-nas).

## Resultado aprendido

Uma rede de utilizador dá DNS interno de graça, mas deixa tudo aberto a tudo
dentro dela. O `kind: Dependency` transforma «quem partilha a rede» em «quem
tem permissão explícita»: a `db` passa a *default deny* e só admite a `app`. E a
prova nunca é o exit-code de um `nc` isolado — é a mesma rede a dar **dois**
desfechos, com o par autorizado como âncora de que o serviço está vivo.

## Próximo lab

**Lab 4 — Do Dockerfile à imagem, sem daemon** (construir e correr o que
construíste).

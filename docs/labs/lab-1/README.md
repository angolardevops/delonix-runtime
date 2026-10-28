# Lab 1 — Primeiro serviço, em 60 segundos

> **Identificador:** `lab-1` · **Nível:** iniciante · **Jornada:** primeiro contacto

## Objectivo concreto

Executar um serviço web real com o Delonix e observar, com rigor, o ciclo
`run → ps → observar → logs → exec → rm` — e o que «daemonless» significa neste
motor (não há daemon central; há holders leves por recurso).

## Conhecimentos prévios

Terminal Linux e noção de porta TCP. Nada de containers é pressuposto.

## Versão e ambiente testados

> **Baseline histórica (linha 5.1.x):** os valores abaixo foram medidos na linha antiga e **ainda não foram re-medidos na linha 0.1.0 actual**.

`delonix 5.1.1`, Linux com cgroup v2 e user namespaces sem privilégio, execução
rootless. Validado a 2026-10-24.

## Recursos mínimos

~150 MiB de disco para a imagem `nginx:alpine`; rede para o primeiro `pull`.

## Privilégios e acessos necessários

Nenhum `sudo`. Corre como o teu utilizador (rootless). Precisa de uma porta TCP
local livre (o arnês escolhe a primeira a partir de 18080).

## Duração estimada

~2 minutos (dominado pelo primeiro `pull` da imagem).

## Ficheiros de preparação

Nenhum ficheiro externo. O arnês está nesta pasta:

| Ficheiro | Fase |
|---|---|
| `prepare.sh` | garante pré-requisitos e a imagem local |
| `run.sh`     | executa a jornada |
| `verify.sh`  | asserções de resultado funcional |
| `cleanup.sh` | remove só o que o lab criou |
| `lab.sh`     | orquestrador `prepare → run → verify → cleanup` |

## Como correr

```bash
# com o delonix no PATH (instalação normal):
bash docs/labs/lab-1/lab.sh

# apontando a um binário específico (ex.: build local):
DELONIX=/caminho/para/delonix bash docs/labs/lab-1/lab.sh

# deixar o serviço de pé para explorar (sem limpar):
bash docs/labs/lab-1/lab.sh --no-clean
```

## Passos (o que `run.sh` faz) e porquê

1. `container run -d --name web -p <porta>:80 nginx:alpine` — serviço em segundo
   plano, porta publicada no host.
2. `container ps` — o `STATUS` confirma que está `Up`.
3. `ps -o pid,ppid,comm,args -C delonix` — mostra os **holders** do recurso
   (`netns pin`, `netns control`, holder de `run`); a coluna PPID prova que são
   filhos do teu `systemd --user`, não de um daemon do sistema.
4. `curl localhost:<porta>` — fala com o serviço.
5. `container exec web …` — entra no container.
6. `container logs web` — lê o que o nginx escreveu.

## Checkpoints / asserções de sucesso (o que `verify.sh` exige)

- **A** — o container está `Up` (lido de `container ps -o json`).
- **B** — `GET localhost:<porta>` devolve **HTTP 200** (resultado funcional, com
  retry até 20 s; não um exit-code isolado).
- **C** — os logs do nginx não estão vazios.
- **D** — existem holders `delonix` residentes, filhos do init do utilizador —
  e **não** um daemon central.

## Limpeza

`cleanup.sh` remove o container `lab1-web` e o estado do lab. **Não** remove a
imagem `nginx:alpine` (outros labs reutilizam-na).

## Resultado aprendido

Um serviço corre e responde sem qualquer daemon central; o motor mantém-no vivo
por um holder mínimo por recurso, que desaparece com o `rm`.

## Próximo lab

**Lab 2 — Limites de recursos que pegam mesmo** (cgroups rootless).

# Lab 6 — A tua própria imagem de VM, com VMfile

> **Identificador:** `lab-6` · **Nível:** avançado · **Jornada:** primeiro contacto

## Objectivo concreto

Construir uma **imagem qcow2 à tua medida** a partir de um `VMfile` — como se
constrói uma imagem de container — e arrancar uma VM **a partir dela**:
`vm image init` (scaffold) → `vm image build` (assa a base com libguestfs) →
`vm image ls` → `vm create --disk`. A prova é funcional — lida de
`vm image ls -o json` e `vm ls -o json`, não de um exit-code solto.

## Conhecimentos prévios

O Lab 5 (microVM: ciclo de vida) e noção de imagem de container (um `Dockerfile`
é o paralelo do `VMfile`).

## Versão e ambiente testados

> **Baseline histórica (linha 5.1.x):** os valores abaixo foram medidos na linha antiga e **ainda não foram re-medidos na linha 0.1.0 actual**.

`delonix 5.1.1`, Linux com libguestfs (`virt-customize`) e libvirt, execução
rootless. Validado a 2026-10-24 (ciclo `init → build → create → Running+IP →
destroy`, positivo `EXIT=0`; modos de falha «imagem não construiu», «VM não
arrancou da imagem» e «órfão após destroy» comprovados a dar `EXIT=1`).

## Recursos mínimos

**libguestfs-tools** no host (`virt-customize`). O build descarrega a base
`FROM ubuntu:24.04` (algumas centenas de MiB) e produz uma qcow2 esparsa; o
overlay por-VM é pequeno. Conta **minutos** na primeira vez.

## Privilégios e acessos necessários

Nenhum `sudo` para o caminho rootless deste host. O build usa libguestfs em
modo utilizador.

## Duração estimada

~3–10 minutos na primeira vez (dominado pelo download da base e pelo
`virt-customize`); o arranque da VM leva segundos.

## Correção de documentação / achado deste lab

O scaffold do `vm image init` (e a sua dica «Next:») imprime
`delonix vm create dev --disk-image minha-base:1.0`, mas o flag real de
`vm create` é **`--disk`** (`--disk-image` não existe). O arnês usa `--disk`,
que é o correcto; o scaffold está por corrigir no binário (fora do âmbito deste
lab, que só mexe nos labs).

## Ficheiros de preparação

Nenhum ficheiro externo. O arnês gera o contexto de build (scaffold `VMfile`) e
uma chave SSH só do lab. Fica tudo nesta pasta:

| Ficheiro | Fase |
|---|---|
| `prepare.sh` | garante binário + libguestfs + chave do lab; `vm image init`; aponta `SSHKEY` à chave do lab; destrói restos |
| `run.sh`     | mostra o `VMfile` → `vm image build` → `vm image ls` → `vm create --disk` → espera Running+IP |
| `verify.sh`  | asserção: imagem no store, VM arranca da imagem, teardown sem órfãos, imagem sobrevive ao destroy |
| `cleanup.sh` | `vm destroy` + `vm image rm` + remove contexto e chave (idempotente) |
| `lab.sh`     | orquestrador `prepare → run → verify → cleanup` |

## Como correr

```bash
# com o delonix no PATH (instalação normal):
bash docs/labs/lab-6/lab.sh

# apontando a um binário específico (ex.: build local):
DELONIX=/caminho/para/delonix bash docs/labs/lab-6/lab.sh
```

> Nota: o `verify` destrói a VM no passo C (é a asserção de teardown), mas a
> **imagem construída fica no store**. Corre `./lab.sh --no-clean` para a
> manteres e arrancares outra VM dela.

## Passos (o que `run.sh` faz) e porquê

1. Mostra o `VMfile` de scaffold — `FROM ubuntu:24.04`, `SIZE 20G`, `RUN`s
   offline, `USER`/`SSHKEY`, `VCPUS`/`MEMORY`. Constrói **como está**.
2. `vm image build -t minha-base:1.0 .` — assa a base com libguestfs. Os `RUN`
   do scaffold são **offline**; só a base `FROM` é descarregada. (Um `RUN` com
   `apt-get install` precisa de `--network`.)
3. `vm image ls` — a tua imagem entrou no store de imagens de VM.
4. `vm create lab6vm --disk minha-base:1.0` — um overlay por-VM a partir da
   **tua** imagem.
5. Espera `Running` com IPv4 (cloud-init/DHCP demora) — lido de `vm ls -o json`.

## Checkpoints / asserções de sucesso (o que `verify.sh` exige)

- **A** — a imagem `minha-base:1.0` aparece em `vm image ls -o json` (o build
  produziu o disco).
- **B** — a VM arranca `Running` com IPv4 **a partir da tua imagem** (retry;
  distingue «nunca Running» de «Running sem IP»).
- **C** — `vm destroy` remove a VM e **não deixa órfãos** (`describe vm` deixa de
  resolver), e a **imagem sobrevive** ao destroy (o overlay era por-VM).

## Limpeza

`cleanup.sh` faz `vm destroy` da VM (se existir), `vm image rm` da imagem
construída, e remove o contexto de build e a chave do lab. **Não** toca nas
imagens de VM de outros labs.

## Resultado aprendido

Construir uma imagem de VM é como construir uma de container: um `VMfile` com um
`FROM`, um `SIZE`, `RUN`s e um `USER`, e um `build` que a assa. A imagem é um
disco bootável completo que vive no store e serve de base a quantas VMs quiseres
— cada `vm create --disk` dela tira o seu próprio overlay.

## Próximo lab

**Lab 7 — Kubernetes sem Docker** (um cluster local cujo runtime dos nós é o
Delonix).

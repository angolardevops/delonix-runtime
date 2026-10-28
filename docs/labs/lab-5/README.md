# Lab 5 — Uma microVM a sério

> **Identificador:** `lab-5` · **Nível:** intermédio · **Jornada:** primeiro contacto

## Objectivo concreto

Provar o **ciclo de vida completo** de uma microVM com o mesmo CLI dos
containers: criar a partir de uma imagem local, chegar a `Running` com IP, tirar
um **checkpoint de sistema** (memória + disco) com a VM **a correr**, voltar
atrás com `restore`, e desmontar com `vm destroy` **sem deixar órfãos**. A prova
é funcional — lida de `vm ls -o json`, não de um exit-code solto.

## Conhecimentos prévios

Terminal Linux e noção de VM. Os Labs 1–4 (containers) ajudam mas não são
obrigatórios.

## Versão e ambiente testados

> **Baseline histórica (linha 5.1.x):** os valores abaixo foram medidos na linha antiga e **ainda não foram re-medidos na linha 0.1.0 actual**.

`delonix 5.1.1`, Linux com libvirt, execução rootless. Validado a 2026-10-24
(ciclo `create → Running+IP → snapshot a correr → restore → destroy`, positivo
`EXIT=0`; modos de falha «VM nunca arrancou», «sem IP» e «snapshot/restore
falhou» comprovados a dar `EXIT=1`).

## Recursos mínimos

Uma **imagem de VM local** (ex.: `k8s-golden`, ~634 MiB) — o arnês escolhe uma
de `vm image ls` para não descarregar GiB. O overlay por-VM ronda ~350 MiB.

## Privilégios e acessos necessários

Nenhum `sudo` para o caminho rootless libvirt deste host.

## Duração estimada

~10–20 segundos com a imagem em cache (dominado pelo arranque e pelo DHCP).

## Correção de documentação que este lab trouxe

O texto antigo do Lab 5 tinha **dois erros** que o teste ao vivo expôs e que aqui
ficam corrigidos:

- o teardown é **`vm destroy <nome>`**, não `delonix delete vms <nome>` (este
  último erra `DX-4000 no such resource kind 'vms'`);
- um snapshot de uma VM **parada não falha** — é **disk-only** (suportado). O que
  `vm stop` faz *undefine* é o **domínio em execução**; disco, record e snapshots
  persistem.

## Ficheiros de preparação

Nenhum ficheiro externo. O arnês está nesta pasta:

| Ficheiro | Fase |
|---|---|
| `prepare.sh` | garante binário + imagem de VM local; destrói restos |
| `run.sh`     | `create` → espera Running+IP → `snapshot create` (a correr) → `restore` |
| `verify.sh`  | asserção do ciclo de vida e do teardown sem órfãos |
| `cleanup.sh` | `vm destroy` idempotente (o verify já destrói no passo D) |
| `lab.sh`     | orquestrador `prepare → run → verify → cleanup` |

## Como correr

```bash
# com o delonix no PATH (instalação normal):
bash docs/labs/lab-5/lab.sh

# apontando a um binário específico (ex.: build local):
DELONIX=/caminho/para/delonix bash docs/labs/lab-5/lab.sh
```

> Nota: o `verify` destrói a VM no passo D (é a asserção de teardown). Para
> explorar a VM (`vm console`/`vm ssh`), corre só `prepare.sh` + `run.sh` e pára
> antes do `verify`.

## Passos (o que `run.sh` faz) e porquê

1. `vm create lab5vm --disk <imagem>` — um overlay por-VM a partir da imagem
   local (fixada para ser reproduzível e não descarregar GiB).
2. Espera `Running` com IPv4 (cloud-init/DHCP demora) — lido de `vm ls -o json`.
3. `vm snapshot create lab5vm limpa` **com a VM a correr** → checkpoint de
   **memória + disco**.
4. `vm snapshot restore lab5vm limpa` — volta ao checkpoint.

## Checkpoints / asserções de sucesso (o que `verify.sh` exige)

- **A** — a VM está **`Running`** com **IPv4** válido (retry; distingue «nunca
  Running» de «Running sem IP»).
- **B** — o snapshot `limpa` aparece em `vm snapshot ls` (checkpoint vivo).
- **C** — depois do `restore`, a VM volta a `Running` com IP.
- **D** — `vm destroy` remove tudo e **não deixa órfãos**: `describe vm` deixa de
  resolver (`DX-4501`) e a VM sai do `vm ls`. Falha em voz alta se algo ficar
  para trás.

## Limpeza

`cleanup.sh` faz `vm destroy` da VM se ainda existir (idempotente — o `verify` já
a destrói no passo D). **Não** remove as imagens de VM locais (outros labs
reutilizam-nas).

## Resultado aprendido

Uma microVM tem o mesmo ciclo de vida de um container, pelo mesmo CLI, com uma
diferença de peso: o snapshot com a VM a correr é um **checkpoint de sistema**
(memória + disco), não só o disco. E o teardown correto — `vm destroy` — leva
disco, snapshots e record de uma vez, sem deixar órfãos.

## Próximo lab

**Lab 6 — A tua própria imagem de VM, com VMfile** (construir uma qcow2 à tua
medida).

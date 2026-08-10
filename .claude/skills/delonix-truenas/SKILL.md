---
name: delonix-truenas
description: Provisionar armazenamento numa NAS pela API do TrueNAS (ADR-0009, aceite) — criar o dataset, a quota, a partilha e as permissões a partir de um `kind: Volume`, em vez de os exigir feitos à mão. Usa quando o utilizador pedir para PROVISIONAR storage (dataset/quota/share/ACL) em vez de só consumir uma partilha já existente, ou quando mexeres no `delonix-truenas`/no bloco de provisionamento do `kind: Volume`.
---

# Provisionar no TrueNAS a partir de um `kind: Volume`

Decidido em [ADR-0009](../../../docs/adr/0009-truenas-storage-provisioner.md):
**aceite**, com duas condições que não são opcionais (ver o fim).

## O que já existe, e não se reescreve

`kind: Volume` com um bloco `nfs:`/`cifs:`/`webdav:` já **consome** uma partilha:
`storage::build_mount` monta-a e o `ensure_mounted` faz o `mount -t`, validado
ponta-a-ponta contra um servidor NFS real. O que falta é **criar** o que está do
outro lado.

Regra que decide metade do desenho: um volume provisionado é montado pelo
**mesmo** caminho de sempre. **Não há um segundo mecanismo de montagem.**

## Como

1. **Crate próprio** (`delonix-truenas`). O `delonix-volume` tem TRÊS
   dependências e **não ganha nenhuma** — cliente HTTP é tokio + hyper + TLS num
   crate de motor (guarda-rio #4). Verifica com `cargo tree -e normal -p
   delonix-volume` antes e depois.
2. **Bloco de provisionamento OPCIONAL** no `kind: Volume` (alvo, `dataset`,
   `quota`, `owner`, tipo de partilha). Sem ele, o comportamento é exactamente o
   de hoje — um manifesto existente não muda de significado.
3. **Credenciais de `kind: Secret`** (chave de API), nunca um literal no
   manifesto.
4. **A quota lê-se de volta, não se assume.** Depois de provisionar, guarda-se o
   que a NAS diz que impõe, e é isso que o `volumes inspect` mostra — a mesma
   disciplina do `Usage { bytes, unreadable }`: medição incompleta é
   *desconhecida*, nunca um número inventado.
5. **Pinar UM major da API** e falhar claro nos outros. A superfície do TrueNAS
   mudou de forma entre majors (lido em primeira mão a construir o appliance).
   Compatibilidade best-effort que faz a coisa errada em silêncio é pior que uma
   recusa.

## O caminho destrutivo é o que exige mais cuidado

**Remover um `kind: Volume` não pode destruir um dataset por omissão.** Flag
própria, confirmação própria. O precedente está no v0.37.0: o `volumes rm`
apagava a contabilidade ANTES dos dados e deixava o volume invisível com os
bytes lá — e isso era numa máquina só nossa. Aqui a destruição chega a outra
máquina.

**Apagar em último lugar, e nunca antes de saber que o objecto é nosso para
apagar.** É a regra que a auditoria dos 208 subcomandos deixou escrita.

## Duas condições da aceitação

1. **Passagem `delonix-runtime-sec` antes do merge.** Nenhuma fronteira de
   privilégio NOSSA se move, mas o raio de dano cresce: passamos a segurar uma
   credencial que destrói dados noutra máquina.
2. **O caminho destrutivo prova-se com um cenário de caos**, não por leitura — e
   com a regra do repo: tem de **falhar com a correcção revertida**.

## E a vantagem que este tem sobre o ADR-0008

**É testável aqui.** O appliance TrueNAS construído nesta série arranca e serve a
API neste host, por isso o CRUD, a quota e as permissões exercitam-se contra um
alvo REAL. Usa-o. Um provisionador validado só contra respostas gravadas não
cumpre a barra deste repositório.

---
name: delonix-testing
description: Disciplina de teste e validação-ao-vivo do delonix-runtime — como provar que um caminho funciona a sério (não só que compila), sem derrubar o host. Cobre a pirâmide (puro/proptest/concorrência/integração/E2E), as armadilhas de teste que este repo já pagou, e como validar fronteiras que o `cargo test` não alcança (rootless, cgroup delegado, netns do holder). Usa sempre que fores escrever/rever testes, validar uma feature antes de a dar por pronta, ou preparar a bateria E2E de uma release.
---

# Teste e validação-ao-vivo do Delonix Runtime

Nasceu da regra que se repete em todo o `CLAUDE.md`: **um `cargo test` verde
nunca prova que uma feature funciona.** Os bugs mais caros desta série passaram
todos os testes unitários porque a lógica pura estava certa e o caminho REAL não
lá chegava. Esta skill é o antídoto.

## A pergunta que decide se testaste

**"Que teste falha se eu reverter o fix?"** Se não souberes responder, ainda não
testaste — escreveste um teste que passa por acaso. Escreve/corre a falha
PRIMEIRO (reverte o fix mentalmente ou de facto, confirma que o teste a apanha),
só depois confirmas que passa com o fix.

## Pirâmide (do que o repo já tem para o que falta)

1. **Funções puras** — `#[cfg(test)]` no módulo. Piso mais barato e já denso
   (75+ módulos). Padrão: `crates/delonix-image/src/registry.rs::tests` constrói
   o struct/`Client` interno directamente, sem rede real. Todo o parser/validador/
   construtor de URL novo GANHA teste aqui.
2. **Propriedade** — `proptest` já é dependência do `delonix-net`. Usa-o onde o
   input é grande e a invariante é clara (CIDR/porta, nomes determinísticos,
   `expand_publish_range` recusa larguras diferentes). Uma invariante vale mais
   que mil casos à mão.
3. **Concorrência** — padrão canónico: N threads a mutar o mesmo `Store`/
   `JsonStore` com um `sleep` no meio da janela (`update_concorrente_nao_perde_
   escritas`, `jsonstore_update_concorrente_nao_perde_escritas`). Sem `flock`
   perde escritas; com ele, todas batem. O CRI é concorrente com a CLI — isto
   NÃO é teórico. Todo o read-modify-write novo sobre estado partilhado precisa
   deste teste.
4. **Integração** — só `crates/delonix-net/tests` existe hoje. Candidatos:
   `run→ps→stop→rm`, `compose up/down -v` (idempotência + limpeza), `apply`
   idempotente por Kind.
5. **E2E ao vivo** — `docs/RELATORIO-PRE-PRODUCAO.md` (139 PASS / 1 FAIL) é o
   norte. O binário real contra estado real. Ver secção abaixo.

## Comandos (sempre com `PROTOC`)

```bash
export PROTOC=<caminho-do-protoc>          # delonix-cri (tonic-build) precisa
cargo build -p <crate-tocada>              # feedback rápido, crate a crate
cargo clippy --workspace --all-targets     # zero warnings, SEMPRE
cargo test -p <crate-tocada> -p delonix-runtime-bin
```

## Validação-ao-vivo — o que o `cargo test` não pode provar

Fronteiras de privilégio (rootless, userns, cgroup delegado, netns do holder,
nft, re-exec) só se provam com o binário real:

```bash
cargo build -p delonix-runtime-bin
./target/debug/delonix <comando real contra estado real>
```

- **Caminhos triplicados**: valida os TRÊS, não um. `delonix vm ls-remote` E
  `delonix image vm ls-remote` E `delonix image --vm ls-remote` — o utilizador
  não sabe qual invocou.
- **A prova de que uma imagem importa não é "o comando saiu 0"** — é um `kubectl
  run --image-pull-policy=Never` a ficar `Running` (ver `cluster load`). Escolhe
  sempre a prova que o utilizador final veria, não o código de saída da
  ferramenta intermédia.

## Não podes respawnar o holder num host com containers vivos

**A armadilha operacional mais importante.** Muito código corre DENTRO do
processo do holder (`do_firewall`/`do_attach`/`dns_resolve`/`fw_chain_body`).
Reiniciar o holder derruba a SDN de TODOS os containers da SDN. Por isso, num
host de trabalho com containers vivos (odoo, registries, control-planes k8s):

- Prova o BUG ao vivo (é seguro — só observas), mas valida a CORRECÇÃO por teste
  unitário + leitura, e di-lo claramente no relatório ("corrigido, provado por
  teste; só toma efeito num respawn do holder"). É exactamente o que o `CLAUDE.md`
  faz repetidamente — segue o mesmo padrão de honestidade.
- Um upgrade in-place deixa o holder ANTIGO vivo (ver `stale_holder_message`) —
  um binário novo não fala com um holder velho por um verbo de socket novo. Testa
  a compatibilidade da linha de controlo (contagem de tokens) quando lhe mexeres.

## Armadilhas de teste que este repo já pagou

- **Um teste pode codificar o bug.** `default_project_name_normaliza_o_
  directorio` afirmava o comportamento que colapsava projectos compose — passava
  só porque usava caminhos absolutos, quando a invocação real é relativa. **Passa
  à função a forma que a produção lhe dá.**
- **`current_exe()`/re-exec num binário de teste re-entra no harness** → lê os
  args como filtros, corre zero testes, sai **0**. Falso sucesso que suprime o
  fallback.
- **rootless faz `chmod 700` sob userns mapeado** — `read_dir` que falha por
  EACCES e devolve 0 é indistinguível de vazio, e é o caso NORMAL. Medição
  incompleta é `unreadable`/`measured:false`, nunca zero.
- **`as u64` sobre `f64` satura** (`99999...t` → `u64::MAX`) — testa o overflow de
  qualquer conversão de tamanho vinda de input.
- **`let _ =`/`.ok()` sobre entropia é fail-open** — `random_token` a partir de
  `[0u8;16]` com o erro descartado dá token de zeros. Testa o caminho de falha da
  fonte de aleatoriedade quando ela guarda uma fronteira de segurança.
- **Género no i18n**: `msgid` partilhado com `msgstr` dependente do sujeito fica
  errado em silêncio, nunca no `cargo test`. Grepa o `msgid` antes de o adicionar.

## Antes de dar por pronto

O que ficou provado, o que ficou por provar e PORQUÊ (host sem GPU, sem 2.º nó,
holder não-respawnável), e que testes novos foram adicionados. Para uma release,
actualiza a bateria de `docs/RELATORIO-PRE-PRODUCAO.md` se a superfície E2E
mudou. **Nunca declares "testado" o que só compilou** — é a diferença entre esta
skill e um checklist qualquer.

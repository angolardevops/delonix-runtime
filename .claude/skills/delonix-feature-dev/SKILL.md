---
name: delonix-feature-dev
description: Checklist para adicionar uma feature nova à CLI do delonix-runtime (comando novo, flag nova, subcomando novo) — wiring de todos os pontos de entrada, build/clippy/test, i18n, validação ao vivo, e o que actualizar antes de a dar por terminada. Usa sempre que o utilizador pedir para adicionar/mudar um comando da CLI.
---

# Desenvolvimento de feature na CLI do Delonix Runtime

Nasceu do `ls-remote` (v0.11.0) e do fix do `vm pull` (v0.10.2) — os dois casos
desta série em que uma mesma funcionalidade lógica tinha de existir em vários
pontos de entrada da CLI e só um deles a ganhou primeiro, partindo os outros.

## 1. Identifica TODOS os pontos de entrada que a feature precisa

O `delonix-runtime-bin` tem grupos de comandos com **caminhos triplicados** de
propósito (compat + ergonomia), cada um com a sua própria definição `clap`:

- **Imagens VM**: `cmd/vm.rs::VmCmd` (`delonix vm <cmd>`) + `cmd/image.rs::VmSub`
  (`delonix image vm <cmd>`) + `cmd/image.rs::ImageCmd` com `--vm` (`delonix
  image --vm <cmd>`) — os três convergem em `cmd/vmimage.rs::VmImageCmd`/`run()`.
- Outros grupos podem ter o mesmo padrão (`network`/`volumes` com `apply` vs
  CLI directa, `container`/`pod` com flags partilhadas). **Antes de escrever
  código, grepa o nome do comando irmão mais próximo** (ex.: `grep -rn "Pull"
  crates/delonix-runtime-bin/src/cmd/`) para veres quantos `match`/`enum`
  precisam do mesmo braço.
- Uma feature só num dos três caminhos é um bug, não uma feature parcial — o
  utilizador não sabe (nem deve precisar de saber) qual dos três invocou.

## 2. Argumentos opcionais com default sensato: `Option<String>` + `unwrap_or_else`

Padrão já estabelecido (`source: Option<String>` + `OFFICIAL_VM_IMAGE`): se o
comando faz sentido sem argumento (usa a imagem/registo oficial por omissão),
o campo tem de ser `Option<T>` em TODOS os estados/`enum` do caminho — um só
`String` obrigatório num deles faz o `clap` recusar a invocação sem argumento
antes de qualquer código correr (foi exactamente o bug do v0.10.2).

## 3. Build, lint, testes — nesta ordem, sempre com `PROTOC`

```bash
export PROTOC=<caminho-do-protoc-no-scratchpad>   # delonix-cri (tonic-build) precisa
cargo build -p <crate-tocada>              # feedback rápido, crate a crate
cargo build -p delonix-runtime-bin         # o binário final
cargo clippy --workspace --all-targets     # zero warnings, sempre
cargo test -p <crate-tocada> -p delonix-runtime-bin
```

Escreve teste(s) para qualquer função pura nova (parsers, validadores, URLs
construídos) — ver `crates/delonix-image/src/registry.rs::tests` para o padrão
de construir um `Client`/struct interno directamente no teste sem rede real.

## 4. Validação ao vivo — nunca só testes unitários

Corre o binário local a sério contra o estado/serviço real (registo OCI, socket
de gestão, etc.) antes de declarar a feature pronta — testes unitários provam a
lógica pura, não que os três caminhos da CLI chegam lá. Ex.: `./target/debug/
delonix vm ls-remote`, `./target/debug/delonix image vm ls-remote`, `./target/
debug/delonix image --vm ls-remote` — os três, não só um.

## 5. i18n — strings novas de UI nunca hardcoded em PT

Fonte é sempre EN no código. Um `Error::Invalid("texto fixo".into())` novo
passa a `Error::Invalid(super::po::t("texto em inglês").into())` + entrada
correspondente em `crates/delonix-runtime-bin/data/pt.po`. Doc-comments `///`
de `clap` (o `--help`) traduzem-se sozinhos via `translate_help` — não
precisam de `po::t`, mas idealmente também ganham entrada no `pt.po` (senão
degradam para EN em `--l18n=pt`, que é aceitável mas não ideal).

## 6. Antes de terminar

- `docs/releases/vX.Y.Z.md` + `CLAUDE.md` (secção relevante do grupo de
  comandos) — ver a skill `delonix-release` para o pipeline completo de bump/
  tag/CI/validação. Decide MINOR (feature user-visible) vs PATCH (só fix).
- Se a superfície do `--help` mudou, os docs do site (`docs/*.html`) têm de
  ser regenerados contra o binário PUBLICADO — passo 4 da `delonix-release`.
- Revê o `git status`/`git diff --stat` antes de comitar: um wiring triplo
  bem feito toca tipicamente 2-4 ficheiros pequenos (`cmd/vm.rs`, `cmd/
  image.rs`, `cmd/vmimage.rs` + a crate de motor, se aplicável) — um diff
  muito maior ou muito menor do que isso é sinal de ter esquecido um caminho.

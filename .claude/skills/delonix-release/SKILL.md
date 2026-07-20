---
name: delonix-release
description: Pipeline completo de release do delonix-runtime — bump de versão, notas, tag, CI, validação dos assets publicados e sincronização da documentação. Usa sempre que o utilizador pedir "nova release", "publicar", "bump de versão", ou depois de features user-visible fundidas no main que mereçam sair.
---

# Release do Delonix Runtime

O build de release é FEITO PELO CI (`.github/workflows/release.yml`, disparado
pelo push de uma tag `v*`) — **nunca compilar a release localmente** (a máquina
de desenvolvimento não tem `protoc`; o runner instala-o e usa ubuntu-22.04 para
glibc 2.35). O trabalho local é: notas → bump → tag → monitorizar → validar.

## Passos

1. **Notas da release** — criar `docs/releases/vX.Y.Z.md`:
   - 1.ª linha: `## vX.Y.Z — <título curto>` (vira o TÍTULO da release no GitHub).
   - Conteúdo: as features por secção, no estilo das notas anteriores (ver
     `docs/releases/`). Honestidade primeiro: limitações conhecidas incluídas.
2. **Apêndice** — correr `bash scripts/gen-releases.sh` (regenera
   `docs/RELEASES.md`; o CI volta a fazê-lo pós-publicação, mas o commit local
   evita um diff pendente).
3. **Bump** — `version = "X.Y.Z"` no `[workspace.package]` do `Cargo.toml` raiz
   + `cargo update --workspace` (actualiza o lock; o CI compila `--locked`).
   O workflow ABORTA se `--version` do binário ≠ tag — o bump não é opcional.
4. **Documentação da CLI** (só se a superfície de comandos mudou): o site de
   docs embebe o `--help` real — depois de a release publicar, descarregar o
   binário publicado e regenerar:
   `curl -fL -o /tmp/dlx https://github.com/angolardevops/delonix-runtime/releases/latest/download/delonix-x86_64-linux && chmod +x /tmp/dlx && python3 docs/gen.py /tmp/dlx`
   — comitar as páginas alteradas. Armadilha: o `gen.py` importa o módulo
   `markdown` e o pip do sistema está bloqueado (PEP 668) — usar um venv
   descartável (`python3 -m venv /tmp/v && /tmp/v/bin/pip install markdown &&
   /tmp/v/bin/python docs/gen.py /tmp/dlx`).
5. **Commit + tag + push** —
   `git commit … && git push origin main && git tag vX.Y.Z && git push origin vX.Y.Z`.
   (Se o push der 403 "denied to <outra-conta>": o gh tem múltiplas contas; usar
   a credencial certa sem tocar na conta activa global — `GIT_ASKPASS` com
   `gh auth token -u angolardevops`, e `GH_TOKEN=$(gh auth token -u angolardevops)`
   para chamadas `gh`.)
6. **Monitorizar** o workflow `release.yml` (Monitor/`gh run watch`) até
   `completed success`. Em falha: `gh run view <id> --log-failed`, corrigir,
   apagar e re-push da tag se necessário.
7. **Validar como um utilizador real** — nunca declarar a release feita sem:
   - download dos assets via `releases/latest/download/`;
   - `sha256sum -c SHA256SUMS --ignore-missing` OK;
   - `./delonix-x86_64-linux --version` = X.Y.Z;
   - se houve mudanças de i18n: `--l18n=pt <grupo> -h` mostra o help traduzido.
8. **Confirmar a doc dinâmica** — o passo final do workflow comita
   `docs/RELEASES.md` actualizado no main (`[skip ci]`); fazer `git pull` para
   sincronizar o clone local.

## Convenções

- Assets com nomes ESTÁVEIS (o `install.sh` depende deles):
  `delonix-x86_64-linux`, `delonix-x86_64-v3-linux`, `delonix-cri-x86_64-linux`,
  `delonix-cri-x86_64-v3-linux`, `SHA256SUMS`, `install.sh`. Nunca renomear.
- Versionamento: MINOR para features user-visible, PATCH para fixes/instalador.
- Strings de UI novas: EN no código + entrada no `data/pt.po` (ver a secção
  i18n do CLAUDE.md) — uma release nunca sai com strings PT hardcoded novas.

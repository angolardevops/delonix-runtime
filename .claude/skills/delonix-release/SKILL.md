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
   Armadilha do `Edit`: a linha 34 do `Cargo.toml` tem `oci-spec = { version =
   "0.10.0", ... }` — uma dependência SEM RELAÇÃO com a versão do workspace que
   por vezes coincide em string com ela, causando "2 matches" no `old_string`.
   Inclui sempre `[workspace.package]\nversion = "X.Y.Z"` como contexto do
   `old_string` para apontar só à linha 17.
4. **Documentação da CLI** (só se a superfície de comandos mudou): o site de
   docs embebe o `--help` real — depois de a release publicar, descarregar o
   binário publicado e regenerar. Armadilha: o `gen.py` importa o módulo
   `markdown` e o pip do sistema está bloqueado (PEP 668) — usar um venv
   descartável. Armadilha maior: `gen.py` aceita um **2.º argumento** com o
   caminho do `delonixctl` (cliente PaaS privado, gera as páginas irmãs desse
   produto); sem ele cai no default `../target/release/delonixctl`, que **não
   existe** fora de um checkout do `delonix-paas` e faz o script rebentar com
   `FileNotFoundError` a meio — passa sempre os DOIS argumentos, mesmo que só
   te interesse a doc do `delonix`:

   ```bash
   curl -fL -o /tmp/delonix https://github.com/angolardevops/delonix-runtime/releases/latest/download/delonix-x86_64-linux
   chmod +x /tmp/delonix
   python3 -m venv /tmp/v && /tmp/v/bin/pip install markdown
   /tmp/v/bin/python docs/gen.py /tmp/delonix ~/.local/bin/delonixctl
   ```

   Se `~/.local/bin/delonixctl` também não existir neste host, procura em
   `delonix-paas/target/{release,debug}/delonixctl` antes de desistir da doc
   do `delonixctl` — mas a doc do `delonix` (o que importa a este repo) já
   gera correctamente com qualquer caminho válido no 2.º argumento.
   O `gen.py` usa o **argv[0]** do binário para as linhas `Usage:` — o ficheiro
   TEM de se chamar `delonix` (é o nome em toda a doc comitada; **esta nota
   dizia `dlx` e estava ERRADA**, o que na v0.46.0 produziu exactamente o diff
   enorme que ela existe para evitar — 26 ficheiros trocados só por causa do
   nome. Confirma sempre com `git show HEAD:docs/comandos/vm.html | grep -oE
   "Usage: [a-z0-9-]+" | sort -u` antes de gerar), senão a regeneração troca
   `Usage: delonix …` por `Usage: delonix-x86_64-linux …` em
   TODAS as páginas e produz um diff enorme e errado. Copia o asset para um
   ficheiro com esse nome antes de gerar.
   Comitar as páginas alteradas.

   `docs/comparacao.html` (Delonix vs Docker vs Podman) **É gerado** pelo
   `gen.py`, a partir da constante `COMPARE` (≈linha 1125). A nota anterior
   aqui dizia o contrário e custou uma quase-regressão na v0.36.0: a página
   comitada tinha sido corrigida À MÃO no `8e67a64` sem actualizar o `COMPARE`,
   por isso a regeneração REVERTIA-A para um texto stale que afirmava que os
   achados de segurança "ainda NÃO foram confirmados por uma 2.ª auditoria" e
   que o núcleo de syscalls "nunca teve revisão nenhuma" — ambos falsos desde
   2026-07-26. Numa página sobre postura de segurança isso é activamente
   enganador. **Nunca editar `docs/comparacao.html` directamente**: editar o
   `COMPARE` no `gen.py` e regenerar. Depois de gerar, confirmar sempre com
   `git diff docs/comparacao.html` que só mudou o que era suposto.
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

## No roteiro de auditoria

Uma release é o ponto de controlo do roteiro (`delonix-auditoria`): antes da tag,
os pontos **5 e 7** (`scripts/e2e.sh` + varredura da CLI, via `delonix-test-e2e`)
e **8** (aprendizados e gates, via `delonix-aprendizados`) têm de estar fechados,
e as notas dizem sempre o que **não** foi validado — nunca o implícito. Se a
release muda comportamento sob carga, `delonix-carga` com número antes e depois;
se muda a superfície comparável, `delonix-paridade` para o `COMPARE` do `gen.py`.

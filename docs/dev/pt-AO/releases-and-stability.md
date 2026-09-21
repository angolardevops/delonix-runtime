<!-- translated-from: releases-and-stability.md sha256:38c64cb15968ed86136ea555f0d3b7515baa85a8a5e7439fcf65700807c8df37 -->
# Releases e estabilidade

**Antes de leres:** [Fluxo de contribuição](contributing-workflow.md#version-alignment) (o gate de versão) e [Publicar a documentação](publishing-docs.md) (o que uma release regenera).

Esta página é sobre a outra metade de uma release: o que o `version` no `Cargo.toml` promete, o
que acontece quando uma tag `v*` é empurrada, e o que o motor garante que não parte sem uma versão
major. [Publicar a documentação](publishing-docs.md) já cobre o lado da documentação de uma
release (o que regenera, o que a CI verifica); esta página cobre o próprio número de versão e o
contrato de CLI/manifesto que ele suporta.

## O gate de versão

`scripts/version_gate.py` (job de CI `version`, `fetch-depth: 0` porque precisa de todas as tags)
permite que o `version` do workspace, no `Cargo.toml` da raiz, seja exactamente uma de duas
coisas:

1. **Igual à tag mais recente que este commit contém.** Trabalho normal entre releases. Duas
   builds com o mesmo número de versão distinguem-se por `delonix --version`, que imprime a
   distância dessa tag: `commit: <hash> (+N commits since vX.Y.Z)`.
2. **Maior do que a tag contida mais recente, só no commit de release** — e então
   `docs/releases/v<versão>.md` tem de existir, porque o workflow de release o publica como o
   corpo da GitHub Release.

Tudo o resto falha, cada um com uma razão ligada a um modo de falha real:

| O que o gate vê | Porque falha |
|---|---|
| Existe uma tag mais recente que este commit não contém | O ramo começou antes dessa release; fazer merge dele tal como está desfaz o que a release já publicou. Faz merge do `origin/main` primeiro. |
| O `Cargo.toml` está abaixo da tag contida mais recente | Um número de versão mais antigo seria publicado por cima de um mais recente. |
| O `Cargo.toml` está acima da tag contida mais recente, sem `docs/releases/v<versão>.md` nenhum | Um bump sem tag correspondente — manda o download do `delonix-cri` (que é resolvido a partir da própria versão do binário em execução, vê `vmimage.rs`) para uma release que nunca vai existir. |

A versão não é decoração: além do download do `delonix-cri`, é o `ServerVersion` da API Docker, o
`delonix_version` gravado em cada backup de recurso, e o que o workflow de release (abaixo)
verifica contra o binário construído antes de publicar seja o que for.

Não faças bump à versão num PR de funcionalidade, e não uses um sufixo `-dev` — a primeira regra
do gate já cobre o trabalho normal, e um sufixo `-dev` apontaria o download do CRI para uma
release que não existe.

## O que uma tag empurrada faz

`.github/workflows/release.yml` dispara em `push: tags: ["v*"]` e, num único job:

1. Constrói `delonix`, `delonix-cri`, `delonix-mcp` e `delonix-mgmt` duas vezes — uma genérica
   x86-64, outra com `-C target-cpu=x86-64-v3` (AVX2/BMI2/FMA) — especificamente em
   `ubuntu-22.04`, para a linha de base do glibc (2.35) ficar compatível com o RHEL 9 e o
   Debian 12, não só com o Ubuntu mais recente. O `scripts/install.sh` escolhe a build `-v3`
   automaticamente quando o CPU do host a suporta. Um job `build-arm64` separado constrói os mesmos
   quatro binários nativamente num runner aarch64 (um por componente, sem variante `-v3`), e são
   publicados como `<name>-aarch64-linux` sob o mesmo `SHA256SUMS`. O `install.sh` ainda não os
   instala. O job só corre numa tag `v*`, por isso a sua primeira execução foi a própria release
   v4.2.0; a CI corre a suite de testes nativamente em arm64 no job `test (arm64)` em cada PR.
2. **Regenera o site do utilizador contra esta build de release exacta e falha se `docs/`
   divergir.** Este gate existe porque uma vez não existia: um buraco no site saiu ao vivo na
   v0.48.0, escondendo um comando novo durante horas enquanto um job de CI paralelo já estava
   vermelho sobre isso — os dois workflows simplesmente não se olhavam um ao outro. Correr o
   `docs/gen.py` aqui, antes de publicar, fecha isso.
3. Constrói um `SHA256SUMS` com checksum, um SBOM SPDX 2.3 (`scripts/sbom.py`, a partir do
   `Cargo.lock`, ele próprio metido no hash do `SHA256SUMS`), e — quando o secret
   `MINISIGN_SECRET_KEY` está configurado — uma assinatura minisign sobre o `SHA256SUMS`. O
   `SHA256SUMS` sozinho só prova integridade de transferência (vem da mesma URL que o binário); a
   assinatura prova que a release veio deste projecto, já que o `install.sh` traz a chave pública
   embutida e recusa instalar uma release não assinada sem `--insecure-skip-signature`. Falhar a
   assinar quando uma chave **está** configurada é um erro rígido — partiria de vez a verificação
   de assinatura de todos os instaladores.
4. Verifica que o seu próprio binário reporta a versão da tag (`delonix --version | grep <tag>`)
   antes de publicar seja o que for — a mesma classe de verificação que o `version_gate.py` já
   correu antes, contra o artefacto que está de facto prestes a ser publicado.
5. Anexa proveniência SLSA de build (`actions/attest-build-provenance`, a própria acção do
   GitHub, mantida separada do minisign de propósito: a proveniência prova *onde e a partir de
   que commit* algo foi construído, para quem não confia no projecto à partida; o minisign prova
   que a release é *deste projecto*, para quem já confia na sua chave pública embutida).
6. Publica a GitHub Release, com `docs/releases/<tag>.md` como notas quando esse ficheiro existe,
   ou `--generate-notes` caso contrário.
7. Faz checkout à `main` e regenera o `docs/RELEASES.md` (`scripts/gen-releases.sh`) e os factos
   gerados do manual (`scripts/dev_docs.py`) e, à parte, o **site** do manual
   (`scripts/dev_docs_site.py`) — comitando o que tiver mudado, `[skip ci]`, para a documentação
   nunca ficar mais de um commit atrás de uma release. Uma falha do gerador aqui é um aviso alto,
   não uma release falhada: a release em si não depende disso.

A metade **narrativa** do manual — a prosa destas páginas — não é regenerada pela CI. Depois de
uma release ser publicada, uma revisão feita por um mantenedor lê o que mudou desde a tag
anterior (commits, notas de release) e actualiza só as páginas que essa mudança afecta,
exactamente como descrito em [Publicar a documentação § O que acontece no momento da
release](publishing-docs.md#what-happens-at-release-time).

## O que é estável, e onde vive essa promessa

`docs/cli-stability.md` é o contrato de facto, no mesmo repositório, lido pelo `delonix explain`
e pelas páginas geradas igualmente — esta secção só te orienta até lá, porque duplicar o
conteúdo dele aqui dar-lhe-ia uma segunda cópia a desalinhar-se do código. Aplica-se desde a
v0.42.3 e, desde a v1.0.0, lê-se como a promessa de semver real do projecto, em vez de uma nota
dentro do `0.x`.

**Estável — não parte sem um major:**

- Os verbos de ciclo de vida de container/imagem (`container run`, `ps`, `stop`, `exec`, …) e os
  verbos de imagem, com os nomes e a ordem de argumentos do próprio Docker/Podman, e as flags
  curtas/longas específicas que `docs/cli-stability.md` lista para `run`/`exec`.
- Os códigos de saída (`0` sucesso, `4` não encontrado, `5` conflito, `69` capacidade do host em
  falta, `124` prazo esgotado, …) e o número do dicionário `DX-CDNN` que cada falha carrega
  (ADR-0043) — o número identifica *qual* falha e nunca muda de significado nem é reutilizado;
  `delonix explain DX-4501` procura um.
- `-o json` em todo comando de listagem: campos podem ser acrescentados, nunca removidos nem
  mudar de tipo (ADR-0005).
- O **schema do manifesto** para os Kinds com spec tipada (`Container`, `Pod`, `Volume`,
  `Network`, e os restantes que `delonix manifest schema` lista): um campo nunca é removido, muda
  de tipo, ou muda de propósito; um campo novo é sempre opcional com um default que preserva o
  comportamento anterior; um campo renomeado mantém a grafia antiga como alias;
  `apiVersion: delonix.io/v1` continua a carregar mesmo depois de os grupos por domínio
  (`compute.delonix.io/v1alpha1`, …) se terem tornado canónicos. Esta é a promessa que mais
  importa na prática — protege o que as pessoas põem em git e revêem num PR, não só o que
  escrevem numa prompt.

**Não estável — pode mudar em qualquer versão:** `serve cri`/`serve api`/`serve docker-api` (a API
de gestão local em particular não tem contrato publicado nenhum e é explicitamente algo para não
automatizar contra — vê [Os crates § `delonix-mgmt`](crates.md#delonix-mgmt) e a ADR-0040/0041);
as superfícies imperativas `cluster`/`vm`/`pod`/`workload`/`net` (o **schema** de manifesto delas,
onde existe, está coberto acima — só os verbos e as flags à volta é que não); `compose`;
`backup`; `mcp`; `system`/`dashboard`/`completion`/`init`/`man`/`config`/`explain`; o formato do
estado em disco debaixo de `$DELONIX_ROOT`; e `stack history`/`stack rollback` (ADR-0019 — nada
lê esse histórico para decidir o que existe, por isso perdê-lo não muda nada do que o
reconciliador faz).

## Como uma quebra é feita, quando tem de acontecer

O precedente, já aplicado mais do que uma vez (a reorganização da CLI da v0.30.0, a reversão
`image list`→`image ls` da v2.0.0): **um corte limpo, sem alias de compatibilidade.** A grafia
antiga falha com `unrecognized subcommand`, alto, em toda versão a partir da quebra em diante —
nunca um alias silencioso que muda de comportamento mais tarde em silêncio. `docs/cli-stability.md
§ Como uma quebra é feita` regista uma lição real de o teres feito: uma renomeação pode deixar
para trás um chamador *interno* (o próprio servidor CRI continuou a invocar um `delonix netns
attach` já removido durante meses depois da reorganização da v0.30.0, partindo a criação de pods
rootless) — fazer grep ao workspace inteiro pela grafia antiga, não só à documentação e aos
testes, faz parte de fazer o corte.

Uma quebra a algo que esta página ou o `docs/cli-stability.md` marca como estável precisa de um
ADR primeiro ([Fluxo de contribuição § Quando escrever um
ADR](contributing-workflow.md#when-to-write-an-adr)), porque move uma fronteira estrutural por
definição — o mesmo raciocínio que se aplica a um backend novo ou a uma fronteira de privilégio
nova aplica-se também aqui.

---

**A seguir:** [Publicar a documentação](publishing-docs.md) — como o site e este manual são gerados, controlados por gate e publicados, e o que o teu PR tem de regenerar.

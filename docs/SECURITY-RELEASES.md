# Assinatura das releases (minisign)

## O problema que isto resolve

O `install.sh` sempre verificou os binários contra o `SHA256SUMS` da release. Isso
prova que a **transferência não corrompeu** — não prova **quem produziu o ficheiro**.
O `SHA256SUMS` vem da mesma URL que o binário, por isso quem consiga publicar uma
release adulterada (conta GitHub comprometida, token de CI roubado, insider) publica
também o `SHA256SUMS` a condizer. A verificação passa, com sucesso, e instala o
backdoor.

Uma assinatura fecha isso: a chave privada nunca está na release, e o `install.sh`
traz a pública embutida. Forjar passa a exigir a chave privada, não apenas acesso de
escrita à release.

## Configuração (uma vez)

**A chave privada é gerada por ti, na tua máquina.** Não a geres em CI, não a peças a
um assistente, não a coles em lado nenhum a não ser no secret do repositório.

```bash
# 1. Gerar o par. Escolhe uma password forte quando for pedida.
minisign -G -p delonix.pub -s delonix.key

# 2. Guardar a PRIVADA fora do repositório (gestor de passwords / cofre offline).
#    Se a perderes, não consegues assinar releases novas — só gerar uma chave nova
#    e atualizar o install.sh, o que quebra a verificação para quem tiver a antiga.

# 3. Ver a pública (2 linhas: comentário + base64):
cat delonix.pub
```

Depois:

1. **`scripts/install.sh`** — substituir `MINISIGN_PUBKEY="__POR_PREENCHER__"` pela
   **segunda linha** do `delonix.pub` (só o base64, sem o comentário).
2. **Secrets do repositório** (Settings → Secrets and variables → Actions):
   - `MINISIGN_SECRET_KEY` — o conteúdo INTEIRO do `delonix.key`.
   - `MINISIGN_PASSWORD` — a password escolhida no passo 1.

O workflow de release falha alto se o secret faltar, em vez de publicar uma release
sem assinatura que todos os instaladores recusariam.

## Como o instalador se comporta

| Situação | Comportamento |
|---|---|
| Assinatura válida | instala |
| `SHA256SUMS` adulterado | **aborta** — `SIGNATURE verification FAILED` |
| Assinado por outra chave | **aborta** — key id não bate |
| Release sem `.minisig` | **aborta**, com o `--insecure-skip-signature` na mensagem |
| `minisign` em falta | tenta instalar pelo gestor de pacotes; se não conseguir, **aborta** |
| `MINISIGN_PUBKEY` por preencher | avisa e segue (só integridade de transferência) |

Fail-closed em todos os caminhos de falha real. A verificação corre **antes** de
qualquer `verify_asset` — sem um `SHA256SUMS` autêntico, os hashes que ele contém não
valem nada.

## Verificar à mão

```bash
curl -fsSLO https://github.com/angolardevops/delonix-runtime/releases/latest/download/SHA256SUMS
curl -fsSLO https://github.com/angolardevops/delonix-runtime/releases/latest/download/SHA256SUMS.minisig
minisign -Vm SHA256SUMS -P '<a chave pública, do install.sh ou do site>'
sha256sum -c SHA256SUMS --ignore-missing
```

## O que isto NÃO cobre

- **O próprio `install.sh`.** Corrido por `curl … | bash`, é executado sem
  verificação — a sua autenticidade depende do TLS e do GitHub. Para fechar esse elo,
  descarrega-o primeiro, confere-o contra o `SHA256SUMS` assinado (o `install.sh` está
  lá listado) e só depois o corres.
- **Um compromisso da própria máquina de build.** A assinatura prova que o artefacto
  saiu do nosso pipeline, não que o pipeline estava íntegro. **Desde 2026-08-25 há
  proveniência SLSA** (`actions/attest-build-provenance`, assinada com uma identidade
  OIDC efémera do runner via Sigstore — sem chave privada a guardar), que diz de que
  commit e de que workflow o binário saiu:

  ```bash
  gh attestation verify delonix-x86_64-linux --repo angolardevops/delonix-runtime
  ```

  As duas assinaturas coexistem de propósito e respondem a perguntas diferentes: o
  minisign prova que a release é **nossa** a quem tem a chave pública embutida no
  `install.sh`; a proveniência prova **onde** foi construída, a quem não confia em nós
  à partida. Continua a não ser build reprodutível — isso é outra coisa, e nenhuma das
  duas a promete.
- **`cloud-hypervisor-static` e `hypervisor-fw`**, instalados do upstream só por HTTPS
  — o upstream não publica checksums num formato conveniente. Risco conhecido e
  registado; ver o comentário no `install.sh`.

## Rotação

Trocar de chave quebra a verificação para quem tiver um `install.sh` antigo em cache.
Publica a pública nova no site e nas notas da release **antes** de a usar, e mantém a
antiga documentada. O workflow verifica, antes de publicar, que a assinatura bate com
a pública embutida no `install.sh` — uma troca esquecida a meio falha em CI, não nas
máquinas dos utilizadores.

## SBOM

Cada release publica **`delonix-sbom.spdx.json`** — SPDX 2.3, gerado do
`Cargo.lock` por `scripts/sbom.py`. É o que responde a «esta CVE afecta-me?»:
380 pacotes com nome, versão e o checksum do registo.

**Entra no `SHA256SUMS`, e portanto na assinatura.** Publicá-lo fora dela daria
um inventário que qualquer um pode substituir — e um SBOM adulterado é pior que
nenhum, porque é acreditado.

Sai de um script nosso e não de um `syft`/`cyclonedx` pela mesma razão por que a
proveniência usa a acção do próprio GitHub: acrescentar uma ferramenta de
terceiros ao passo que existe para garantir a cadeia de fornecimento é aumentar
a superfície exactamente onde ela conta. O `Cargo.lock` **é** a árvore
resolvida; o script traduz, não descobre.

**O que ele não cobre, e está escrito no próprio documento:** o que é ligado do
sistema (libc, o que o `protoc` gera) e qualquer alegação de reprodutibilidade.


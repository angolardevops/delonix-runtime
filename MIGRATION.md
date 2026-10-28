# Migrar para o Delonix (linha 0.x)

O produto `delonix-runtime` passou a chamar-se **Delonix** e re-baseou a versão de
`5.1.1` para `0.1.0` (uma linha nova, de propósito — ver [VERSIONING.md](VERSIONING.md)).
Este guia tem duas partes: para quem **usa** o motor, e para quem **mantém** o repositório.

## Para quem usa — `5.1.1` → `0.1.0`

**O que muda:** o nome e os endereços (repo, Pages, imagem GHCR), e o número de
versão. O binário da CLI continua a chamar-se `delonix`.

**O que NÃO muda** (contratos operacionais estáveis na re-base):
- Estado persistido: `/var/lib/delonix/`, rootless `~/.local/share/delonix`.
- Config: `/etc/delonix/`. Sockets: `/run/delonix-{cri,mgmt,node}.sock`.
- Serviços systemd, variáveis `DELONIX_*`, grupos de schema `*.delonix.io`.
- A tua instalação mantém dados, imagens, volumes e configuração.

**Como actualizar:**
```
curl -fsSL https://github.com/ngolacloud/delonix/releases/latest/download/install.sh | bash
```
O "latest" do GitHub é por **data**, por isso resolve para `0.1.0` (não por ordem
SemVer). Os **cinco binários** (`delonix`, `-cri`, `-mcp`, `-mgmt`, `-node-api`)
têm de ficar todos na mesma versão — o motor recusa binários-irmãos desalinhados
(`check_version` por igualdade exacta) e o `install.sh` avisa se encontrar um no
PATH de outra versão.

**Auto-update:** uma ferramenta que compare versões por SemVer vê `0.1.0 < 5.1.1` e
**não** actualiza sozinha — reinstala pelo `install.sh` acima. É uma transição de
linha, anunciada, não uma actualização incremental.

**Rollback:** as releases `v1.*`…`v5.1.1` continuam publicadas (tags imutáveis,
nunca sobrescritas); dá para voltar a uma delas.

## Para quem mantém — runbook da transferência GitHub

Transferir o repositório `angolardevops/delonix-runtime` → `ngolacloud/delonix`.
**Preferir TRANSFERIR** (preserva histórico, issues, PRs, releases e redireccionamentos)
em vez de recriar.

**Pré-voo (verificar ANTES):**
- [ ] origem correcta; admin na origem e na org `ngolacloud`; nome `delonix` livre no destino.
- [ ] branch protections / rulesets (main, develop) a recriar no destino.
- [ ] Actions, environments, self-hosted runners.
- [ ] secrets / OIDC / credenciais de publicação (minisign `MINISIGN_SECRET`, GHCR token).
- [ ] packages e imagens GHCR; apps, webhooks, deploy keys.
- [ ] GitHub Pages e domínio personalizado (há `.nojekyll`, sem CNAME hoje).

**Transferir:** Settings → *Transfer ownership* → `ngolacloud`, e renomear
`delonix-runtime` → `delonix` (ou transferir já com o nome novo).

**Pós-transferência (VALIDAR — não assumir que os redireccionamentos bastam):**
- [ ] `git remote set-url` + `clone` do novo endereço.
- [ ] CI verde na nova org (permissões mínimas reconfiguradas).
- [ ] downloads de release (assets por `v{version}`) e `SHA256SUMS(.minisig)`.
- [ ] Pages na nova URL (`ngolacloud.github.io/delonix` ou domínio) e as
      `schema/v1/*.json` a resolver.
- [ ] GHCR `ghcr.io/ngolacloud/delonix`.
- [ ] `vmimage.rs` baixa `delonix-cri` da release nova.
- [ ] integrações externas re-apontadas.

**Só depois** completar a identidade e cortar a release:
1. `scripts/rebrand.sh build-coupled` (constantes Rust, `gen.py`, `docs/dev`, proto `go_package`).
2. rebuild dos 5 binários a `0.1.0` + `python3 docs/gen.py` + `scripts/dev_docs_site.py` + `buf generate`.
3. `docs/releases/v0.1.0.md` + `scripts/version_gate.py` verde (abre a linha delonix).
4. `scripts/release_verify.py` + pipeline (`release.yml`): SHA256SUMS, SBOM, minisign, SLSA.
5. etiquetar `v0.1.0` e publicar. Ver a checklist em [VERSIONING.md](VERSIONING.md#checklist-antes-de-etiquetar-v010).

## Estado desta transição (2026-10-28)

- **Feito e integrado em `develop`:** identidade segura em config/docs
  (`Cargo.toml`/`Makefile`/`README`/units/`sbom.py`), `version_gate` consciente de
  linha + testes, política de compatibilidade (VERSIONING.md), CHANGELOG
  consolidado, adendo ao ADR-0061, `scripts/rebrand.sh`.
- **Pendente (dependente da transferência e do build de release):** `rebrand.sh
  build-coupled`, regeneração das docs com a identidade nova, e a própria release
  `0.1.0`. A transferência é uma operação externa irreversível num repo público
  vivo (a cortar releases 5.x) — executar com os pré-voos satisfeitos.

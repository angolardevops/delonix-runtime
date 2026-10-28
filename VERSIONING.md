# Versões e compatibilidade do Delonix

## Duas linhas de versão

O produto renomeou-se de `delonix-runtime` para **Delonix** e **re-baseou o
número de versão de propósito**:

| Linha | Versões | Estado |
|---|---|---|
| `delonix-runtime` | `v1.0.0` … `v5.1.1` (2026-08-29 … 2026-10-15) | **congelada** — histórico, não continua |
| **Delonix** | `0.1.0` em diante | activa |

`0.1.0 < 5.1.1` em SemVer puro. Isto é **intencional** — uma linha nova, não uma
regressão. As ferramentas que comparam versões têm de o saber; ver abaixo.

## Política de compatibilidade (pré-1.0)

Enquanto a linha Delonix estiver em `0.x` (antes de `1.0.0`):

- **Patch** (`0.1.0 → 0.1.1`): sem mudanças incompatíveis de contrato.
- **Minor** (`0.1.0 → 0.2.0`): **pode** quebrar contratos de CLI/API/JSON/schema;
  cada quebra fica documentada no [CHANGELOG.md](CHANGELOG.md) como `BREAKING`.
- A estabilização de API para SemVer estrito chega com `1.0.0`
  (`docs/cli-stability.md` é o contrato que então se aplica).

**Contratos operacionais que NÃO mudam com a re-base** (eram estáveis na linha
5.x e continuam): directórios de estado (`/var/lib/delonix`, `/etc/delonix`,
`~/.local/share/delonix`), sockets (`/run/delonix-*.sock`), serviços systemd,
variáveis de ambiente `DELONIX_*`, e os grupos de schema `*.delonix.io`
(`config.delonix.io/v1`, `compute.delonix.io/v1alpha1`, …). Uma instalação 5.1.1
mantém o seu estado ao passar para 0.1.0.

## O que a descida 5.1.1 → 0.1.0 toca (medido)

| Mecanismo | Efeito da descida | Tratamento |
|---|---|---|
| `scripts/version_gate.py` | Partia: `0.1.0 < 5.1.1` (tag contida) | **Resolvido** — portão consciente de linha; só compara dentro da linha e exige conter a `v5.*` mais recente. Testes em `scripts/test_version_gate.py`. |
| `check_version` (`delonix-node/src/dispatch.rs`) | Igualdade exacta entre `delonix` e os binários-servidores co-instalados | Reconstruir **os 5 binários** (`delonix`, `-cri`, `-mcp`, `-mgmt`, `-node-api`) para `0.1.0` **em conjunto**. O `install.sh` já avisa de binários-irmãos de outra versão no PATH. |
| `vmimage.rs` (`:4799`, `:4925`) | Descarrega `releases/download/v{version}/delonix-cri-*` | A release `v0.1.0` tem de existir com os 5 binários + `SHA256SUMS(.minisig)`. |
| GitHub "latest release" | É por **data**, não SemVer | Uma instalação nova via `install.sh` (`VERSION=latest`) obtém `0.1.0` sem problema. |
| `install.sh` aviso de migração | Match de **prefixo** (`0.64.*`), não ordenação | Inofensivo na descida. |
| Comparadores SemVer de terceiros | Veriam `0.1.0 < 5.1.1` → não actualizam | **Transição explícita**: anunciar a nova linha/canal; não depender de auto-update por ordenação. Documentar o passo de migração. |

## Regras de etiquetagem

- **Nunca** sobrescrever as tags `v1.*`…`v5.*` nem republicar releases imutáveis.
- As tags da linha Delonix continuam no formato `v<ver>` (`v0.1.0`), porque
  `release.yml` e `vmimage.rs` compõem `v{version}`.
- A linha 5.x fica como releases históricas; a `v0.1.0` passa a ser a "latest"
  por data.

## Checklist antes de etiquetar `v0.1.0`

1. Identidade renomeada e repositório em `github.com/ngolacloud/delonix` (URLs,
   `repository`, imagem GHCR, Pages, workflows).
2. `Cargo.toml` `workspace.package.version = "0.1.0"`.
3. `docs/releases/v0.1.0.md` escrito (o `version_gate.py` exige-o para abrir a
   linha; `release.yml` publica-o).
4. Os 5 binários reconstruídos a `0.1.0` (dispatch por igualdade exacta).
5. `python3 scripts/version_gate.py` verde (abre a linha delonix em 0.1.0).
6. `python3 scripts/release_verify.py` e o pipeline (`release.yml`) validados:
   SHA256SUMS, SBOM SPDX, assinatura minisign, SLSA provenance.
7. Testar instalação limpa e actualização de uma 5.1.1 existente (estado
   preservado; binários co-instalados coerentes).

## Nota de implementação

`version_gate.line_of()` separa as linhas por major (0 = Delonix; 1..5 =
`delonix-runtime`). Quando a linha Delonix se aproximar de `1.0.0`, este corte
tem de ser revisto (as tags legadas `v1..v5` deixam de se distinguir por major) —
ver o aviso no topo de `scripts/version_gate.py`.

# Supply chain e release — `fa8c6297` / release v3.1.0

| Controlo | Evidência | Estado |
|---|---|---|
| `Cargo.lock` presente e `--locked` em todos os gates | baseline | PASS |
| Dependências git | nenhuma em `Cargo.toml` do workspace | PASS |
| Advisories / licenças / bans / fontes | `cargo deny check`: tudo ok (`raw/deny.log`) | PASS |
| `cargo audit` | ferramenta ausente | NOT TESTED |
| Actions pinadas por commit | **0 de 20** `uses:` com SHA (`@v4`, `@v2`, `@v1`) | FAIL (G-020) |
| Assinatura da release | `SHA256SUMS.minisig` publicado; install.sh verifica com chave pública embutida e falha fechado | PASS (desenho) |
| Provenance SLSA | `actions/attest-build-provenance@v1` no release.yml | PASS (desenho) |
| SBOM | `delonix-sbom.spdx.json` publicado e dentro do SHA256SUMS assinado | PASS (desenho) |
| Verificação real dos assets v3.1.0 (`minisign -V`, `gh attestation verify`) | exige descarregar ~100 MB — **não autorizado nesta sessão** | NOT TESTED |
| Instalador com hash/assinatura errada, download interrompido, arch errada | — | NOT TESTED |
| Artefactos por arquitectura | só `x86_64` e `x86_64-v3` | PARTIAL (G-021) |
| Downloads de terceiros no instalador (cloud-hypervisor, firmware) | só HTTPS, sem checksum (risco documentado) | PARTIAL |
| Builds reproduzíveis | — | NOT TESTED |
| Secret scanning | — | NOT TESTED |

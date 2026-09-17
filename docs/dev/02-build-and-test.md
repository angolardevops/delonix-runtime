# 2. Clone, build and test

## The gates CI runs

<!-- dev-docs:begin ci-gates -->
| CI job | What it checks |
|---|---|
| `fmt` | rustfmt |
| `lang` | lang ratchet |
| `arch` | arch fitness |
| `contract` | contract gate |
| `version` | version gate |
| `cli-surface` | cli surface |
| `clippy` | clippy -D warnings |
| `test` | test |
| `deny` | cargo-deny |
| `docs` | docs geradas e exemplos válidos |
<!-- dev-docs:end ci-gates -->

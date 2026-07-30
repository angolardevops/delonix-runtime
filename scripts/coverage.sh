#!/usr/bin/env bash
# Cobertura de testes do workspace via `cargo-llvm-cov` (source-based, stable Rust).
# NÃO instala nada global à força: se o `cargo-llvm-cov` faltar, diz como o obter e sai.
#
# Uso:
#   ./scripts/coverage.sh            # resumo por-ficheiro no terminal
#   ./scripts/coverage.sh --html     # relatório HTML em target/llvm-cov/html/
#   ./scripts/coverage.sh --lcov      # lcov.info (para CI/Codecov)
#
# Precisa do `PROTOC` (o delonix-cri/tonic-build), tal como os testes normais.
set -euo pipefail

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
  echo "cargo-llvm-cov não está instalado." >&2
  echo "Instala com:  cargo install cargo-llvm-cov" >&2
  echo "(precisa também do componente llvm-tools:  rustup component add llvm-tools-preview)" >&2
  exit 1
fi

: "${PROTOC:=$(command -v protoc || true)}"
if [[ -z "${PROTOC}" ]]; then
  echo "PROTOC não encontrado no PATH — o delonix-cri (tonic-build) precisa dele." >&2
  echo "Exporta PROTOC=<caminho> antes de correr." >&2
  exit 1
fi
export PROTOC

mode="${1:-summary}"
case "$mode" in
  --html)  cargo llvm-cov --workspace --html ;                 echo "→ target/llvm-cov/html/index.html" ;;
  --lcov)  cargo llvm-cov --workspace --lcov --output-path lcov.info ; echo "→ lcov.info" ;;
  *)       cargo llvm-cov --workspace ;;
esac

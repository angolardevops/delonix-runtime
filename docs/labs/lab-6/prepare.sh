#!/usr/bin/env bash
# Lab 6 · preparar — garante o binário, o libguestfs, uma chave SSH do lab e um
# contexto de build limpo com um VMfile de scaffold (que CONSTRÓI COMO ESTÁ).
# shellcheck disable=SC2034  # LAB_ID é lido por _lib/common.sh (ficheiro externo)
LAB_ID=lab-6
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../_lib/common.sh
. "${DIR}/../_lib/common.sh"

log "a verificar pré-requisitos"
need python3
# O build assa a imagem com libguestfs (virt-customize). Sem ele o lab não corre
# — e é melhor falhar aqui, em voz alta, do que a meio de um build de minutos.
command -v virt-customize >/dev/null 2>&1 \
  || die "falta o libguestfs (virt-customize) no PATH. Instala: sudo apt install libguestfs-tools"
delonix_ok

NAME="lab6vm"
TAG="minha-base:1.0"
BUILDDIR="${LAB_STATE_DIR}/build"
KEY="${LAB_STATE_DIR}/id_ed25519"
state_set NAME "$NAME"
state_set TAG  "$TAG"
state_set BUILDDIR "$BUILDDIR"
state_set KEY  "$KEY"

# Uma chave SÓ do lab, para o arnês não depender de o leitor ter uma — e para a
# imagem que construímos ficar autocontida e reproduzível.
if [ ! -f "${KEY}.pub" ]; then
  log "a gerar uma chave SSH ed25519 do lab (${KEY})"
  ssh-keygen -t ed25519 -N '' -C "delonix-lab-6" -f "$KEY" >/dev/null
fi

# Idempotência: desfaz restos de uma execução anterior (VM + imagem + contexto).
if [ -n "$(vm_field "$NAME" status)" ] || "${DELONIX}" describe vm "$NAME" >/dev/null 2>&1; then
  warn "VM '${NAME}' já existia — a destruir"
  "${DELONIX}" vm destroy "$NAME" -f >/dev/null 2>&1 || true
fi
if vm_image_exists "$TAG"; then
  warn "imagem '${TAG}' já existia — a remover"
  "${DELONIX}" vm image rm "$TAG" >/dev/null 2>&1 || true
fi
rm -rf "$BUILDDIR"; mkdir -p "$BUILDDIR"

log "a gerar o scaffold do VMfile (vm image init) em ${BUILDDIR}"
"${DELONIX}" vm image init minha-base -d "$BUILDDIR" >/dev/null

# O scaffold aponta o SSHKEY para ~/.ssh/id_ed25519.pub. Trocamos pela chave do
# lab para o build NÃO depender do ambiente do leitor — o resto do VMfile fica
# tal e qual (continua a "construir como está"). Falha em voz alta se a linha
# não existir (o scaffold mudou de forma).
VMFILE="${BUILDDIR}/VMfile"
grep -q '^SSHKEY ' "$VMFILE" \
  || die "o scaffold do VMfile não tem linha 'SSHKEY ' — o formato do 'vm image init' mudou; actualiza este lab."
sed -i -E "s|^SSHKEY .*|SSHKEY minha-base ${KEY}.pub|" "$VMFILE"

ok "preparado — contexto=${BUILDDIR} imagem=${TAG} vm=${NAME}"

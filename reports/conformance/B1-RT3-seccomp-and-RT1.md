# B1 gaps isolados — GAP-RT-1 e GAP-RT-3 (corrigidos)

**Data:** 2026-10-24 · **Binário:** `delonix 5.1.1` (árvore de `ee615f3b` + estes fixes), rootless.
Decisões: ambos vêm de [B1-oci-runtime-baseline.md](B1-oci-runtime-baseline.md).

## GAP-RT-1 — `ociVersion` do bundle exportado · CORRIGIDO + verificado

- `cmd/image.rs build_runtime_spec`: `.version("1.0.2")` → `.version("1.3.0")` (a versão que o
  `runc spec` 1.5.2 emite). Teste actualizado.
- **Verificado:** `oci-runtime-tool validate` passou de *recusar* ("only handles 1.3.0") para
  **"Bundle validation succeeded"**.

## GAP-RT-3 — seccomp no bundle exportado · CORRIGIDO + verificado

### Decisão de desenho (porque NÃO o perfil standard moby)

Ofereci duas vias: (a) a política **do próprio Delonix** ou (b) o perfil **standard containerd/moby**.
Ao implementar descobri que o repo já tem o material para (a) de forma limpa — `const SYSCALLS`
(tabela nome↔número, via `libc::SYS_*`) + `allowed_syscalls()` (a allowlist numérica). Logo (a)
tornou-se **a melhor escolha**: sem vendoring de ~300 linhas, sem risco de transcrição, e
**zero drift** — o bundle carrega **a mesma política que o init do Delonix impõe**, não uma política
upstream diferente. (b) traria uma política divergente da real e um asset externo a manter.

### Implementação

- `delonix-linux/seccomp_profile.rs`: novo `pub fn syscall_name(nr) -> Option<&str>` (o inverso de
  `syscall_number`).
- `delonix-linux/lib.rs`: novo `pub fn allowed_syscall_names()` — resolve `allowed_syscalls()`
  (números) para nomes via a tabela, ordenado e dedup.
- `cmd/image.rs build_runtime_spec`: constrói um `LinuxSeccomp` (`defaultAction = SCMP_ACT_ERRNO`,
  arches x86_64+x86, `syscalls = [{names: <allowlist>, action: SCMP_ACT_ALLOW}]`) e liga-o ao
  `linux.seccomp`. Teste de round-trip estende-se para exigir seccomp + default-deny + os essenciais.

### Verificado ao vivo (`delonix image export alpine`)

```
seccomp present: True
defaultAction:   SCMP_ACT_ERRNO            (default-deny)
architectures:   SCMP_ARCH_X86_64, SCMP_ARCH_X86
allow count:     263 syscalls
read/write/execve/openat presentes:  True
ptrace/bpf/mount EXCLUÍDOS:           True
oci-runtime-tool validate:           Bundle validation succeeded
```

Antes: o bundle não tinha secção `seccomp` → um `runc`/`crun` corria-o **sem filtro nenhum**. Agora
corre com a mesma allowlist default-deny que o Delonix aplica.

### Limitação documentada (follow-up)

As `architectures` são as do host de exportação (x86_64 + compat 32-bit). Um bundle para outra
arquitectura (aarch64) precisa da lista de arches correspondente — `allowed_syscalls()` já é
condicional por arch, falta só mapear o `Arch` do OCI. Follow-up pequeno, não bloqueia x86_64.

## Estado

Ambos **aplicados, compilam, verificados ao vivo**; na branch `fix/campaign-defects-doctor-rm-compose`,
**não empurrados**. Teste unitário `build_runtime_spec` (agora cobre RT-1 e RT-3) a correr.

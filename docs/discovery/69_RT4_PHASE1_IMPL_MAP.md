# RT-4 Fase 1 — mapa de implementação (`delonix oci-runtime`)

> Concretiza o ADR-0080 ao nível do código, para a implementação na janela de build ser mecânica.
> Baseado na investigação de 2026-10-24 do init do `delonix-linux`.

## A descoberta que torna isto tratável

`delonix_linux::create_with(store: &Store, container: &mut Container, rootfs: &str, spec: &RunSpec)`
(`lib.rs:6989`) é o **ponto de entrada reutilizável do launch**: arranca o init a partir de um
`Container` + um **caminho de rootfs** + `RunSpec` — **não** de uma imagem. E `ContainerInitSpec`
(`lib.rs:3876`) consome `rootfs: &str` + process/filesystem/security/namespaces/io — não é acoplado
a imagem. Um bundle OCI **já tem** `rootfs/` extraído + `config.json`, logo:

- **Não é preciso o image store nem unpack** — o rootfs do bundle usa-se directamente.
- O trabalho real é **traduzir `config.json` (oci_spec::runtime::Spec) → `Container` + `RunSpec`**.

## Estrutura (ficheiros a tocar)

| Ficheiro | O quê |
|---|---|
| `bins/delonix-runtime-bin/src/cmd/oci_runtime.rs` (novo) | o subcomando, os 5 handlers, o modelo de estado, o tradutor Spec→Container |
| `bins/delonix-runtime-bin/src/main.rs` | registar `OciRuntime(OciRuntimeArgs)` no enum de topo + dispatch; `--l18n`/pt.po da ajuda |
| `bins/delonix-runtime-bin/data/pt.po` | 3 textos da ajuda (flag clap nova bate ARG_HELP_PENDING) |

## CLI (clap)

```
delonix oci-runtime [--root <state-dir>] create <id> --bundle <dir>
delonix oci-runtime [--root <state-dir>] start  <id>
delonix oci-runtime [--root <state-dir>] state  <id>
delonix oci-runtime [--root <state-dir>] kill   <id> [signal]
delonix oci-runtime [--root <state-dir>] delete <id> [--force]
```
`--root` default: `$XDG_RUNTIME_DIR/delonix-runtime` (rootless) / `/run/delonix-runtime` (root).

## Estado bundle-keyed (em `--root`, NÃO no image store)

```rust
#[derive(Serialize, Deserialize)]
struct OciState { oci_version: String, id: String, status: String, pid: i32, bundle: String }
// status: "creating"|"created"|"running"|"stopped" (o enum do runtime-spec)
```
Ficheiro `--root/<id>/state.json`. Pure + unit-testável (round-trip serde + o shape que `state` imprime).

## Tradutor `config.json` → `Container` + `RunSpec` (o miolo)

Ler `oci_spec::runtime::Spec` de `<bundle>/config.json`. Mapear:

| OCI `config.json` | delonix |
|---|---|
| `process.args` | `Container.cmd` |
| `process.env` | `Container.config.env` |
| `process.cwd` | `Container.config.working_dir` |
| `process.user.{uid,gid}` | `RunOpts.user`/run_uid/run_gid |
| `root.path` (relativo ao bundle) | `rootfs = <bundle>/<root.path>` |
| `mounts[]` | `Container` mounts (reusar o tipo `Mount` do volume) |
| `linux.namespaces` (user/net/…) | `RunSpec.new_netns` + `Container.userns` + mappings |
| `linux.resources.{memory,cpu,pids}` | `Container` limits (os controladores delegados) |
| `linux.seccomp` | `seccomp_profile::parse` (JÁ consome OCI seccomp JSON) |
| `hostname` | `Container.hostname` |

**Recusar-por-nome (guardrail 6, nunca ignorar):** `hooks` (sem protocolo OCI-hook), `annotations`
que impliquem comportamento, `linux.resources` sem controlador delegado (como `system resources`
já faz), AppArmor em rootless. Cada recusa nomeia o campo + a razão.

## Handlers

- **create**: parse config.json → `Container` + guardar `OciState{status:"created",pid:0}` em `--root`;
  NÃO arrancar. (O rootfs do bundle já está pronto.)
- **start**: ler estado → `create_with(&store_em_root, &mut container, &rootfs, &RunSpec{detach:true,…})`;
  gravar `pid`/`status:"running"`.
- **state**: ler o estado, `reconcile_status`, imprimir o JSON OCI (`{ociVersion,id,status,pid,bundle}`).
- **kill**: `runtime::send_signal(&store, id, sig)`.
- **delete**: teardown (netns/cgroup/mounts via o caminho de `cmd_rm`) + remover `--root/<id>`.

## Verificação (na janela de build)

1. `cargo build --release -p delonix-runtime-bin` (incremental; disco ≥12G).
2. Unit: round-trip de `OciState` + o tradutor numa `config.json` mínima (fail-sem/passa-com).
3. Live: `delonix image export --rootless alpine /tmp/b` (já temos) → `delonix oci-runtime create c1 --bundle /tmp/b` → `start c1` → `state c1` (running) → `kill c1` → `delete c1`; zero órfãos.
4. Suite: `RUNTIME=<wrapper p/ delonix oci-runtime> scripts/runtime-tools.sh` → baseline vs o runc.

## Gate da Fase 1

Um número `runtime-tools: N ok / M not ok` contra `delonix oci-runtime`, publicado ao lado do do runc
(a referência). Fase 2 fecha os `not ok` de maior valor, um por fatia, com teste.

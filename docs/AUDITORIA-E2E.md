# Auditoria E2E — Delonix Runtime
> Auditoria ofensiva de todo o ecossistema (~50k LOC, 9 crates): bugs, gaps, erros de
> design, performance, paralelismo, memória e recursos. **14 finders por subsistema ×
> múltiplas lentes**, cada achado passado por **2 céticos adversariais independentes**
> que tentam refutá-lo lendo o código real e cruzando com as limitações já documentadas
> no `CLAUDE.md`. Data: 2026-07-21.

**Nota de cobertura:** a corrida bateu no limite de sessão a meio da fase de verificação.
Os **24 achados abaixo sobreviveram à verificação adversarial completa** (2 céticos, 0
refutações salvo nota). Outros **11 achados ficaram por verificar** por quota — sobretudo
nos subsistemas mais críticos (`delonix-runtime/lib.rs`, `delonix-net/infra.rs`,
`container.rs`) — e estão listados à parte, a precisar de confirmação.

## Resumo executivo

| Severidade | Confirmados | Não-verificados (quota) |
|---|---|---|
| 🟠 HIGH | 6 | 2 |
| 🟡 MEDIUM | 12 | 6 |
| ⚪ LOW | 6 | 3 |
| **Total** | **24** | **11** |

Por categoria (confirmados): bug 12, recurso 3, concorrência 3, gap 2, performance 2, memória 1, design 1.

**Veredicto:** a fronteira rootless→root e a validação de input de CLI/manifesto (auditadas
antes) mantêm-se sólidas — nenhum RCE novo. O padrão dominante dos achados **HIGH** é
**escrita/eliminação de ficheiros fora do sandbox por input não-confiável** (whiteouts OCI,
IDs de CRI, nome de VM, COPY com symlink, kubeconfig em `/tmp`) e **falta de autenticação
de peer no socket de gestão** — todos exploráveis sem privilégio, no threat-model normal de
correr uma imagem/manifesto de terceiros.

## Achados confirmados (verificados adversarialmente)

### 1. 🟠 HIGH — Path traversal in OCI whiteout handling deletes arbitrary files outside the rootfs

- **Local:** `crates/delonix-image/src/overlay.rs:81`
- **Categoria:** bug · confiança do finder: high

**Descrição.** In `apply_layer_flat`, non-whiteout entries are extracted safely via `entry.unpack_in(dest)` (tar 0.4.46 guards `..`/absolute escapes) and pre-created parents go through `safe_rel`. But the OCI whiteout branch (lines 62-84) takes the RAW `entry.path()` from the tar header — never passed through `safe_rel` — and joins it directly into `std::fs::remove_dir_all`/`remove_file`. `path.parent()` may contain `..` components, and `dest.join(<..-path>)` does NOT normalize (PathBuf::join appends literally); the OS resolves `..` at unlink time, escaping `dest`. The regular whiteout deletes `parent/<target>`; the opaque marker (`.wh..wh..opq`) does `read_dir(parent)` then `remove_dir_all` on every child. It also follows symlinks planted by an earlier layer (CVE-2019-14271 class). Reachable on the DEFAULT rootless path: `prepare_rootfs` (cmd/util.rs:83) -> `export_rootfs` (overlay.rs:162) -> `apply_layer_flat` runs on every rootless `container run`, plus `image export` (cmd/image.rs:632) and rootless `build`. Not covered by the documented `safe_rel`/`safe_join` mitigations (those cover file writes and Dockerfile COPY, not whiteout unlinks). Impact reaches critical (wiping the invoking user's home dir); rated high because it is deletion/integrity, not code execution.

**Cenário de falha.** User runs `delonix container run <malicious-image>` (rootless, default). The image's top layer tar contains an entry named `../../../../../../home/walter/.wh..wh..opq`. In `apply_layer_flat`, name=`.wh..wh..opq`, target after strip=`.wh..opq` matches the opaque case; parent = dest.join("../../../../../../home/walter") which resolves to /home/walter; the code `read_dir`s it and `remove_dir_all`s every entry -> the user's entire home directory contents are deleted. A regular-whiteout variant `../../../.ssh/.wh.authorized_keys` deletes ~/.ssh/authorized_keys.

**Correcção sugerida.** Run the whiteout `path` through `safe_rel` (reject any `..`/absolute/Prefix component) before computing `parent`/`victim`; return None -> skip the entry. Additionally resolve the victim within `dest` without following symlinks (e.g. openat-based or verify canonicalized parent starts_with(dest)) to close the planted-symlink vector.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev high) · confirma (sev high)

### 2. 🟠 HIGH — Path traversal via unvalidated CRI container/pod IDs → arbitrary *.json deletion and file reads

- **Local:** `crates/delonix-cri/src/runtime_svc/lifecycle.rs:745`
- **Categoria:** bug · confiança do finder: high

**Descrição.** Incoming `container_id`/`pod_sandbox_id` from CRI requests (StopContainer/RemoveContainer/ContainerStatus/RemovePodSandbox/PodSandboxStatus/CreateContainer) are used verbatim to build filesystem paths without any validation. `read_rec`/`write_rec`/the raw `remove_file` calls do `dir.join(format!("{id}.json"))` (lines 151, 157, 745, 390) and `read_rec(&sb_dir, &req.pod_sandbox_id)` (line 551). An `id` containing `../` escapes `<base>/cri/{containers,sandboxes}`. Only server-generated ids are safe (generate_id → hex), but the lifecycle mutation/read paths accept the id straight from the request. Note the inconsistency: `log_path` IS explicitly checked for `..`/absolute (line 546), but the id — which also becomes a path — is not. This is the same class as the ALTO VM-name traversal already fixed in delonix-vm (valid_vm_name), but the CRI store paths were never hardened.

**Cenário de falha.** A client on the CRI socket (malicious/compromised kubelet, or any process with access to /run/delonix-cri.sock) calls RemoveContainer with container_id = "../../../../home/walter/.config/delonix/somefile". `delonix container rm -f cri-<traversal>` returns "not found" → stderr_not_found=true → gone=true → `std::fs::remove_file(ct_dir(base).join("../../../../home/walter/.config/delonix/somefile.json"))` deletes an arbitrary *.json file owned by the runtime user (root on many k8s nodes). ContainerStatus/PodSandboxStatus with a traversal id similarly attempt arbitrary-path reads (parsed as a record).

**Correcção sugerida.** Validate every incoming CRI id with a strict whitelist (e.g. hex/[A-Za-z0-9._-], reject empty/`..`/`/`) at the top of each lifecycle handler before it touches any path, mirroring delonix_vm::valid_vm_name and the existing log_path check.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev high)

### 3. 🟠 HIGH — VM name path-traversal bypasses the valid_vm_name fix: generate_seed_iso writes seed files outside the state dir before create() rejects the name

- **Local:** `crates/delonix-runtime-bin/src/cmd/vm.rs:1043`
- **Categoria:** bug · confiança do finder: high

**Descrição.** The documented audit-#2 fix placed valid_vm_name at the ENGINE boundary (delonix_vm::create, lib.rs:1120) on the claim that "qualquer consumidor da API herda". But the bin's generate_seed_iso() is NOT an API consumer that goes through create() first — it runs BEFORE it and builds filesystem paths straight from the raw VM name: work_dir = state_root().join("vms").join(vm_name) followed by std::fs::create_dir_all(&work_dir) and fs::write of user-data / meta-data / network-config / seed.iso (vm.rs:1043-1099). Neither call site validates the name first: the CLI path calls generate_seed_iso(&name, ...) at vm.rs:492 (name is the raw positional arg) and the manifest apply path calls generate_seed_iso(name=&doc.metadata.name, ...) at vm.rs:384, both strictly BEFORE delonix_vm::create() (vm.rs:523 / 412). The manifest layer performs no name validation either (manifest.rs of_kind/spec_of do not check metadata.name). So a name containing '../' traverses out of the state directory: create_dir_all fabricates arbitrary directory trees and four fixed-basename files (including user-data/seed.iso whose content is fully attacker-controlled via --user-data, copied verbatim at vm.rs:1049) are written wherever the invoking user can write. create() then rejects the name, but the side-effecting writes have already happened. This is the SAME arbitrary-file-write class the project itself rated ALTO in audit #2 ("escrevia/sobrescrevia ficheiros FORA do directório de estado") — the fix is incomplete because it guards create()/remove() but not the seed-generation pre-step in the bin.

**Cenário de falha.** Untrusted manifest applied via `delonix stack apply -f evil.yaml` with kind: Vm, metadata.name: "../../../../home/walter/.config/pwn", spec.volumes referencing any existing volume (so resolve_vm_volumes succeeds and the generate_seed_iso branch at vm.rs:383 fires): create_dir_all + writes land under $HOME/.config/pwn, outside state_root, before create() rejects the name. Even without a manifest: `delonix vm create '../../../../tmp/pwned' --user-data attacker.yaml` writes an attacker-controlled `user-data` and a `seed.iso` into /tmp/pwned/ then errors out. Directory creation is fully arbitrary; file writes are limited to the four fixed basenames.

**Correcção sugerida.** Call delonix_vm::valid_vm_name(name) (or equivalent) at the top of generate_seed_iso and/or before every call site in the bin (vm.rs:384, vm.rs:492, and validate metadata.name during manifest load), so the seed-generation path enforces the same whitelist as the engine boundary rather than trusting create() to reject it after the writes.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev high)

### 4. 🟠 HIGH — kubeconfig cluster-admin exposto em /tmp com modo 0644 e caminho previsível no host remoto

- **Local:** `crates/delonix-runtime-bin/src/cmd/cluster.rs:1115`
- **Categoria:** bug · confiança do finder: high

**Descrição.** fetch_kubeconfig corre no host de control-plane, como root via `sudo -n bash -c`: `cp /etc/kubernetes/admin.conf /tmp/delonix-admin.conf && chmod 644 /tmp/delonix-admin.conf`. O admin.conf contém as credenciais cluster-admin embutidas (client-certificate-data + client-key-data). É copiado para um caminho FIXO e PREVISÍVEL (`/tmp/delonix-admin.conf`) e tornado world-readable (0644) durante a janela do scp. O `rm -f` de limpeza é `let _ = ...` (erro ignorado), pelo que uma falha deixa o ficheiro. Este é exactamente o mesmo padrão do achado MÉDIO já corrigido noutro sítio (ensure_libvirt_network → OpenOptions::create_new O_EXCL + mode(0o600)), mas aqui não foi aplicado e o conteúdo é MUITO mais sensível (admin do cluster inteiro).

**Cenário de falha.** Num host de control-plane multi-utilizador, um utilizador local não privilegiado: (a) simplesmente lê `/tmp/delonix-admin.conf` (0644) durante o cluster apply e obtém acesso cluster-admin total; ou (b) pré-cria `/tmp/delonix-admin.conf` como ficheiro seu antes do apply — o `cp` (sem -i, não-interactivo sob sudo) escreve o conteúdo do admin.conf nesse ficheiro pré-existente sem mudar o dono, e o atacante lê-o a seguir. Resultado: comprometimento total do cluster Kubernetes.

**Correcção sugerida.** Usar um nome de ficheiro não-previsível (mktemp) OU escrever com O_EXCL/0600 e chown para o utilizador SSH em vez de chmod 644; alternativamente ler o admin.conf via `sudo cat` para stdout do ssh_run e nunca o pousar num ficheiro world-readable. Verificar o resultado do `rm` de limpeza.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev high) · confirma (sev high)

### 5. 🟠 HIGH — safe_join in COPY is purely lexical — symlinks bypass it, re-opening the arbitrary host file read/write it was added to close

- **Local:** `crates/delonix-runtime-bin/src/cmd/build.rs:282`
- **Categoria:** bug · confiança do finder: high

**Descrição.** safe_join (build.rs:258) rejects only lexical `..`/absolute/prefix components; it never resolves symlinks, and copy_into_rootfs then uses raw std::fs::copy / std::fs::create_dir_all (lines 282, 292, 301-311) which FOLLOW symlinks. The security-audit fix documented in CLAUDE.md ('COPY ../../../etc/passwd read arbitrary host files; a dst with .. wrote outside the rootfs') is therefore incomplete: a symlink achieves the exact same escape that the lexical `..` check blocks. Confirmed there is zero canonicalize/symlink_metadata/is_symlink handling in build.rs. copy_dir_all (line 316) uses DirEntry::file_type() (does not follow the link) so a symlink entry falls to the else branch and std::fs::copy follows it, copying the target's contents. This affects both the src side (read host file into image) and the dst side (write host file), and build_from_spec is also reachable via `delonix image apply` (kind: Image, spec.build) from a manifest.

**Cenário de falha.** SRC read: build context contains a symlink `creds -> /home/walter/.ssh/id_rsa`; Dockerfile has `COPY creds /app/creds`. safe_join(context,"creds") = context/creds (single Normal component, passes), then std::fs::copy(context/creds, ...) follows the link and bakes the host private key into the image layer — exfiltrated on push/run. DST write: `FROM attacker/base` whose rootfs ships a symlink `/opt/hook -> ../../../../home/walter/.bashrc` (tar stores the link target verbatim, extraction keeps it inside rootfs); Dockerfile `COPY payload /opt/hook`. abs_dst=/opt/hook → safe_join(rootfs,"opt/hook") passes → create_dir_all + std::fs::copy traverse the symlink and overwrite /home/walter/.bashrc on the host as the build user during a rootless build.

**Correcção sugerida.** After safe_join, canonicalize the resolved src/dst and verify the real path is still inside context/rootfs (starts_with the canonical base); or lstat each path component and refuse symlinks, and use O_NOFOLLOW / symlink_metadata on the final component before copy. copy_dir_all must apply the same check per entry.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev high) · confirma (sev high)

### 6. 🟠 HIGH — Management API unix socket has no peer authentication and no restrictive file mode — full control-plane (incl. container exec) exposed to any local process, gated only by ambient umask

- **Local:** `crates/delonix-mgmt/src/lib.rs:63`
- **Categoria:** design · confiança do finder: high

**Descrição.** `serve_blocking`/`serve_over_uds` bind the management socket (default `unix:///run/delonix-mgmt.sock`, see delonix-runtime-bin/src/main.rs:266-267) with `UnixListener::bind` and NEVER (a) `set_permissions`/chmod the socket, nor (b) check the peer credential (SO_PEERCRED) on accept. The `router()` (lib.rs:102-150) installs no auth/tower middleware — every route (`/v1/containers/:id/exec`, `run`, `rm`, `/v1/images/build`, `network create`, `vm rm`, ...) is served to anyone who can `connect()`. `container_exec_ep` (lib.rs:439-462) runs `container exec <id> sh -c <cmd>` = arbitrary code execution inside any container (often running as root inside its userns). This is the HIGHEST-privilege surface in the runtime, yet it is the ONLY unix-socket server in the repo with zero auth: the holder control socket deliberately sets BOTH `from_mode(0o600)` AND validates `peer_uid() == own_uid` via SO_PEERCRED (delonix-net/src/infra.rs:545-616, `control_loop`), precisely to stop a non-privileged local user from driving the engine. The mgmt socket falls below that bar. Whether another local UID can actually open the socket is left entirely to the ambient umask at bind time (default 022 -> mode 0755 -> connect denied for other users, but a daemon launched with a permissive umask, e.g. systemd `UMask=000`, or a world-traversable `/run` placement, yields mode 0777 -> any local user obtains full control-plane RCE). The same gap exists in delonix-cri/src/lib.rs:283, though that socket is kubelet-facing.

**Cenário de falha.** The `delonix api` process is started under a service manager (or shell) with umask 0/002 -> `/run/delonix-mgmt.sock` is created mode 0777. Unprivileged local user `mallory` runs `curl --unix-socket /run/delonix-mgmt.sock -XPOST http://x/v1/containers/<id>/exec -d '{"cmd":"id > /root/pwned"}'` and executes arbitrary commands inside a root-in-userns container, or `POST /v1/containers` to launch new containers, `DELETE` to destroy them, `network create`, etc. No credential is ever checked.

**Correcção sugerida.** Mirror the holder's belt-and-suspenders: after bind, `set_permissions(path, from_mode(0o600))` on the socket, AND check `SO_PEERCRED` of each accepted connection against the server's own euid (reuse the `peer_uid` pattern from delonix-net infra.rs), rejecting mismatches before dispatching to the router. Apply the same to delonix-cri.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev medium)

### 7. 🟡 MEDIUM — Unbounded blob buffering with attacker-controlled Content-Length aborts/OOMs the pull

- **Local:** `crates/delonix-image/src/registry.rs:242`
- **Categoria:** recurso · confiança do finder: high

**Descrição.** `blob_with_progress` does `Vec::with_capacity(total.unwrap_or(0) as usize)` where `total = resp.content_length()` is the registry's raw HTTP Content-Length (untrusted). A malicious or MITM registry returning a huge Content-Length (e.g. near u64::MAX) makes `Vec::with_capacity` attempt a giant reservation, which aborts the process on allocation failure (non-recoverable). Independently, the read loop (245-257) accumulates the entire blob into `buf` with no size cap, and the digest verification for OCI artifacts happens only AFTER full buffering (pull_oci_artifact:899), so it cannot prevent exhaustion. A multi-GB (or lie-then-stream) blob OOM-kills the CLI. Same buffering pattern is used by `pull_from_registry_with_creds`, `pull_oci_artifact`, and `RegistryClient::get_blob`, i.e. every `image pull` / `vm pull`.

**Cenário de falha.** `delonix vm pull ghcr.io/attacker/img:tag` against a hostile registry that responds to the blob GET with `Content-Length: 9223372036854775807`: `Vec::with_capacity(9.2e18)` aborts the delonix process immediately. Alternatively the server streams an endless chunked body with no declared length -> `buf` grows until the machine OOMs.

**Correcção sugerida.** Cap the initial `with_capacity` to a sane bound (e.g. min(total, cap)); enforce a maximum blob size in the read loop and error out when exceeded before buffering the rest; ideally stream large blobs to a temp file in the CAS and hash incrementally instead of holding the whole blob in memory.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev medium)

### 8. 🟡 MEDIUM — exec/attach child process and threads leaked when the streaming client disconnects (no kill on stream close)

- **Local:** `crates/delonix-cri/src/spdy.rs:762`
- **Categoria:** recurso · confiança do finder: high

**Descrição.** The SPDY (`run_exec`, line 762 `input.close()`) and WebSocket (`streaming.rs` exec_tty line 453 / exec_pipes line 564) streaming handlers never kill the spawned `delonix exec`/`delonix logs -f` child when the connection drops. On disconnect the read loop ends and `input.close()` only closes the pty master (Tty branch); the Pipe branch closes nothing, and the child handle is owned by a detached `child.wait()` thread with no kill path. For attach the child is `delonix container logs -f`, which never exits on its own — the ONLY termination is client disconnect, which does not stop it.

**Cenário de falha.** kubectl/crictl attaches to a pod (or execs `sleep 3600`) then aborts (Ctrl-C / API-server timeout). writer_task dies → out_rx dropped → reader thread's send fails and it exits → nobody drains the child's stdout → the `delonix logs -f`/exec process blocks on a full 64 KB pipe forever, and the `child.wait()` thread blocks forever. Each aborted exec/attach permanently leaks one process (plus its in-container setns child) and at least one OS thread; repeated attaches exhaust PIDs/threads on the node.

**Correcção sugerida.** Hold the Child (or its pid) in the select loop and call child.start_kill()/libc::kill on the process group when the client stream closes (ws_rx None / rd.read_exact error), before joining the wait thread — as containerd/CRI-O do on exec stream teardown.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev medium)

### 9. 🟡 MEDIUM — ContainerStatus reports a fabricated finished_at = now() on every poll for exited containers

- **Local:** `crates/delonix-cri/src/runtime_svc/lifecycle.rs:795`
- **Categoria:** bug · confiança do finder: medium

**Descrição.** `container_status` sets `finished_at: if exit.is_some() { now_ns() } else { 0 }` — the real death time is not stored, so the CRI FinishedAt timestamp is recomputed to the current time on each ContainerStatus call. `started_at` is likewise fabricated as `created_at` (line 794) rather than the real start time. The kubelet uses FinishedAt for restart back-off and container age.

**Cenário de falha.** A crashed container (restartPolicy OnFailure) sits in Exited. Each time the kubelet polls ContainerStatus, finished_at jumps forward to now, so the container perpetually looks like it 'just died' (age ≈ 0). The kubelet's CrashLoopBackOff / age computations and `kubectl describe` FinishedAt flap forward in time instead of reflecting the actual termination instant, distorting back-off timing.

**Correcção sugerida.** Persist the real finished/started timestamps when the container transitions to a terminal state (from the Store's reconcile) and echo those fixed values, rather than now_ns() at read time.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev low)

### 10. 🟡 MEDIUM — SecretStore/CredVault path lookups don't sanitize the name → traversal on read/delete

- **Local:** `crates/delonix-runtime-core/src/secret.rs:78`
- **Categoria:** bug · confiança do finder: medium

**Descrição.** SecretStore::path() builds `<root>/secrets/{name}.json` via a raw format! with no sanitization. Only save() enforces valid_name(); load(), remove(), resolve_env() and materialize() accept an arbitrary name. This is inconsistent with the rest of the tree, which routes every externally-influenced key through safe_key (Store/JsonStore, store.rs:67), VolumeStore::valid_name (lib.rs:113) or safe_snapshot_name (lib.rs:572). CredVault has the identical gap: cred_path() (cred_vault.rs:102) is only guarded on put(), while get()/remove()/exists() take the name unchecked. The secret name flows in unvalidated from `container run --secret <name>` (container.rs:1346 → resolve_env) and from manifest `spec.secrets` under `stack apply -f <untrusted.yaml>` — the same untrusted-manifest surface the project already treats as hostile (cf. the VM `metadata.name` traversal finding in CLAUDE.md). resolve_env reads `<root>/secrets/../<path>.json` and injects any Secret-JSON-shaped file's `data` map as container env (bounded exfil); `secret rm <name>` / `secret unset --all <name>` reach remove() and unlink `<root>/secrets/../../<path>.json` if it ends in .json (arbitrary file delete within the invoking user's privileges).

**Cenário de falha.** `delonix secret rm ../../../home/walter/.local/share/delonix/vm-images/foo` (or any path resolving to an existing .json) → remove(name) → fs::remove_file(<root>/secrets/../../../.../foo.json) deletes a file outside the secrets dir. Or a manifest `kind: Container` with `spec.secrets: ["../../containers/<id>"]` applied via `stack apply` → resolve_env reads a file outside <root>/secrets and, if it parses as Secret JSON, injects its data as env.

**Correcção sugerida.** Route every name through valid_name()/safe_key in SecretStore::path (and valid_cred_name in CredVault::cred_path) on load/remove/exists/resolve_env/materialize, not just on save/put — reject or map the name before any PathBuf::join, matching Store/JsonStore/VolumeStore.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev low) · confirma (sev low)

### 11. 🟡 MEDIUM — SecretStore::save uses a fixed temp file and holds no lock → torn encrypted blob + lost updates

- **Local:** `crates/delonix-runtime-core/src/secret.rs:103`
- **Categoria:** concorrência

**Descrição.** SecretStore::save() writes to a per-name-fixed temp `.{name}.tmp` and renames it over the final file, with no flock anywhere in SecretStore. This is exactly the two failure modes Store deliberately engineered against: Store::save uses a per-writer temp `.{id}.{pid}.{seq}.tmp` (store.rs:140-143) precisely because "two processes writing the SAME container would write over each other in the same temp and the rename would publish an interleaved JSON", and Store::update wraps read-modify-write in FileLock/flock (store.rs:166-182) to prevent lost updates in this daemonless, concurrent-by-design runtime. SecretStore has neither. Two concurrent writers of the same secret name (e.g. two `delonix secret set` invocations, or `stack apply` racing an automation) both fs::write (O_TRUNC) the same `.{name}.tmp` and rename — the published file can contain interleaved/truncated bytes. Because the blob is XChaCha20-Poly1305 AEAD, any corruption makes decode() (secret.rs:82) fail permanently → the secret becomes undecryptable (data loss / DoS), not merely stale. Separately, the load-modify-save in `secret set`/`unset` (secret.rs:247-259, 271-278) has no lock, so concurrent edits silently drop keys (classic lost update). CredVault::write_0600 (cred_vault.rs:55-61) shares the fixed-temp issue via path.with_extension("tmp").

**Cenário de falha.** Two `delonix secret set db PASS=...` (or a `stack apply` re-run) touch secret `db` at the same instant: both fs::write `.db.tmp`, their writes interleave, the rename publishes a partially-written SEALED blob → subsequent `store.load("db")` fails to decrypt forever (secret lost). Milder path: concurrent `secret set db A=1` and `secret set db B=2` → one save overwrites the other's read-modify, losing a key with no error.

**Correcção sugerida.** Make SecretStore::save use a per-writer unique temp (pid+atomic seq, like Store::save) and add an flock-guarded read-modify-write path (mirror Store::update) for the CLI set/unset flows; apply the same unique-temp fix to CredVault::write_0600 and rotate_key.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev medium)

### 12. 🟡 MEDIUM — kubeadm join concatena `:6443` a um endpoint que pode já conter porta → comando malformado no caminho HA

- **Local:** `crates/delonix-runtime-bin/src/cmd/cluster.rs:1090`
- **Categoria:** bug · confiança do finder: high

**Descrição.** valid_endpoint aceita explicitamente `host:port` (o teste `valid_endpoint("10.0.0.10:6443")` passa; doc-comment diz `host[:port]`). kubeadm_init interpola o endpoint CRU em `--control-plane-endpoint={endpoint}` (kubeadm aceita host:port aí). Mas kubeadm_join faz `format!("kubeadm join {endpoint}:6443 ...")`, anexando `:6443` incondicionalmente. Tratamento inconsistente da porta entre init e join.

**Cenário de falha.** Cluster HA (mode: ssh) com `spec.controlPlaneEndpoint: lb.exemplo.com:8443` (LB/VIP numa porta não-6443, cenário legítimo já que a validação e o schema permitem porta). init corre `--control-plane-endpoint=lb.exemplo.com:8443` (OK); join corre `kubeadm join lb.exemplo.com:8443:6443 ...` — endereço malformado, o join de todos os control-planes secundários e workers falha. O caminho HA fica inutilizável com endpoint com porta customizada.

**Correcção sugerida.** Só anexar `:6443` quando o endpoint não contém já uma porta (ex.: detectar `:` num host não-IPv6, ou parsear host/porta), garantindo simetria com o uso em kubeadm_init.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev medium)

### 13. 🟡 MEDIUM — pick_route path-prefix match ignores segment boundary → route confusion to wrong backend

- **Local:** `crates/delonix-runtime-bin/src/cmd/ingress_proxy.rs:149`
- **Categoria:** bug · confiança do finder: high

**Descrição.** The request-time matcher uses a raw `path.starts_with(&r.path)` with no path-segment boundary check. The docstring and CLAUDE.md explicitly align the HTTPRoute semantics to the Kubernetes Gateway API, whose PathPrefix match of `/foo` matches `/foo` and `/foo/bar` but NOT `/foobar`. Here `/foobar` DOES match the route `/foo` because it is a plain string prefix. `valid_path_prefix` in httproute.rs only sanitizes config-time paths; it does nothing about how request paths are matched. Combined with the longest-prefix tie-break (`max_by_key` on `r.path.len()`), a request for an unrelated sibling path is silently dispatched to a more-specific backend it was never meant to reach.

**Cenário de falha.** HTTPRoute has two path rules on the same host: `/api` → internalApiBackend and `/` → publicBackend. A client requests `/api-docs` (intended for publicBackend). `"/api-docs".starts_with("/api")` is true and `/api` is the longest matching prefix, so pick_route routes the request to internalApiBackend instead of publicBackend — traffic reaches the wrong (internal) service, contradicting the documented k8s PathPrefix semantics.

**Correcção sugerida.** Match with a segment-boundary rule: accept only when `path == r.path` or `path[r.path.len()..]` begins with `/` (treating a trailing `/` in the configured prefix consistently), i.e. `path.starts_with(&r.path) && (r.path.ends_with('/') || path.len()==r.path.len() || path.as_bytes()[r.path.len()]==b'/')`.

> **Consenso dos céticos:** 1 refutação(ões) · refuta (sev low) · confirma (sev medium)

### 14. 🟡 MEDIUM — Composed proxy config (config.json) is built outside the flock → concurrent --expose auto-route silently dropped from the live proxy

- **Local:** `crates/delonix-runtime-bin/src/cmd/ingress_proxy.rs:666`
- **Categoria:** concorrência · confiança do finder: high

**Descrição.** CLAUDE.md states the `--expose` auto-registration uses a flock read-modify-write on auto.json to prevent lost updates. `with_auto_locked` does correctly serialize writes to auto.json. But the subsequent `rebuild()` (read manual+auto → compose → `ensure_running` writes config.json → SIGHUP) is called OUTSIDE that lock, in both auto_register (line 666) and auto_deregister (line 674), and rebuild re-reads auto.json unlocked (line 587) then writes config.json via non-atomic `std::fs::write`. The proxy serves config.json (via SIGHUP reload), NOT auto.json — so a stale composed config becomes the live state even though auto.json is correct. The flock guarantee therefore does not extend to the config the proxy actually serves.

**Cenário de falha.** Two `container run --expose` run in parallel (e.g. parallel launches or a stack). Thread A: with_auto_locked adds X (auto.json=[X]); A enters rebuild and reads auto.json=[X]. Thread B: with_auto_locked adds Y (auto.json=[X,Y]); B completes its whole rebuild, writing config.json=[X,Y] and SIGHUP. Thread A now finishes its rebuild with its stale snapshot, writing config.json=[X] and SIGHUP last. The proxy reloads config.json=[X] — container Y's route is missing from the live proxy despite auto.json=[X,Y] being correct, and no further trigger re-composes it, so Y stays unreachable via the FQDN indefinitely.

**Correcção sugerida.** Perform the whole compose+write-config (rebuild/ensure_running) inside the same flock as the auto.json mutation, or make config.json writes atomic (temp file + rename) and serialize rebuild under the lock so the last writer always reflects the final auto.json.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev medium)

### 15. 🟡 MEDIUM — Admission gate (DELONIX_SCAN_ON_PULL) fails OPEN on an unrecognized threshold — a typo silently disables the fail-closed CVE gate

- **Local:** `crates/delonix-runtime-bin/src/cmd/scan.rs:338`
- **Categoria:** gap · confiança do finder: high

**Descrição.** admission_scan_on_pull is documented as a 'fail-closed GATE'. But an invalid (non-empty, non-'warn', non-severity) policy value is not rejected: admission_rejects (line 303) does Severity::parse(policy) → None → returns false, so the image is admitted (line 338 not taken). Only afterward, at line 345, a warning is printed to stderr. There is no early validation of the policy string, so a misconfiguration silently downgrades a security control from enforcing to advisory. In automated CI the stderr warning is easily lost while the pull succeeds.

**Cenário de falha.** Operator sets DELONIX_SCAN_ON_PULL=criticl (typo for 'critical') intending to block critical-CVE images. On pull, scan_image runs, admission_rejects(worst, "criticl") returns false because Severity::parse fails, the image is NOT removed and the pull succeeds; a vulnerable image is admitted with only a stderr warning. Same for any casing/spelling slip (e.g. 'crit', 'high '→trimmed ok, but 'hihg').

**Correcção sugerida.** Validate the policy once after reading the env var: accept only {warn, low, medium, high, critical}; on any other non-empty value return Err (fail closed) instead of scanning-then-warning, so a misconfigured gate refuses rather than admits.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev low) · confirma (sev medium)

### 16. 🟡 MEDIUM — CIFS/SMB password embedded in world-readable mount argv (defeats --password-secret vault)

- **Local:** `crates/delonix-runtime-bin/src/cmd/storage.rs:163`
- **Categoria:** bug · confiança do finder: high

**Descrição.** build_mount() for the cifs/smb driver pushes the resolved password inline into the mount options string as `password=<secret>` (storage.rs:158-165). That string is later handed to `delonix-volume::VolumeStore::ensure_mounted`, which invokes `Command::new("mount").args(["-t","cifs",device,mountpoint,"-o",options])` (crates/delonix-volume/src/lib.rs:202-213). The password thus appears as a plain process argument. Mounting CIFS requires CAP_SYS_ADMIN, so in the supported/privileged path `mount.cifs` runs as root and its `/proc/<pid>/cmdline` is world-readable — any unprivileged local user can read the NAS credential while the mount runs. This directly undermines the documented purpose of `--password-secret`/`kind: Secret` (CLAUDE.md and storage.rs:47-49 claim the vault path avoids exposing the password). mount.cifs(8) explicitly warns against `password=` on the command line and provides `credentials=<file>` for exactly this reason; that safer path is not used.

**Cenário de falha.** Admin stores the NAS password in the vault and runs `delonix storage create nas --type cifs --server nas --share media --username alice --password-secret nascreds` (or `stack apply` with a Storage referencing passwordSecret). During the mount, an unprivileged local attacker runs `grep -a password= /proc/*/cmdline` (or `ps aux`) and reads `//nas/media -o username=alice,password=<cleartext>`, capturing the secret the vault was supposed to protect.

**Correcção sugerida.** Do not pass credentials via `-o password=`; write them to a mode-0600 temporary credentials file and pass `credentials=<file>` (or feed the password to mount.cifs on stdin). Keep the secret out of any process argument list.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev low) · confirma (sev medium)

### 17. 🟡 MEDIUM — CIFS credentials/options are comma-joined without escaping — commas in a password inject/break mount options

- **Local:** `crates/delonix-runtime-bin/src/cmd/storage.rs:177`
- **Categoria:** bug · confiança do finder: high

**Descrição.** build_mount() assembles the cifs option list by pushing `username={u}`, `password={p}`, `ro`, and the user's `extra` string, then joins them with ',' (storage.rs:157-178). CIFS mount options are delimited by commas and there is no escaping. A perfectly legitimate password (or username) that contains a comma is split by mount.cifs: everything after the comma is reinterpreted as further mount options. This both breaks correct credentials (a password like `a,b3!` fails to authenticate or the mount errors on an unknown option) and, when the value is attacker-influenced through an untrusted `stack apply -f` manifest Secret, lets the trailing text act as injected CIFS mount options (e.g. `uid=0,file_mode=0777,dir_mode=0777`). mount.cifs has no way to escape a comma inside `-o`; the canonical fix is a credentials file.

**Cenário de falha.** A user creates a Secret whose `password` key is `S3cr,et` and a `kind: Storage` (cifs) referencing it. resolve_password returns `S3cr,et`; build_mount produces options `username=...,password=S3cr,et`; mount.cifs treats `et` as a bogus mount option and fails — or, with a crafted value such as `x,file_mode=0777,dir_mode=0777`, silently mounts the share world-writable. The failure is confusing (looks like a wrong password) and the injection path exists.

**Correcção sugerida.** Move credentials out of the comma-joined `-o` string entirely (credentials file / stdin). At minimum, reject or reject-with-clear-error any username/password containing a comma or newline before building the option string.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev low)

### 18. 🟡 MEDIUM — Node DNS resolver does a full O(n) scan of every container+VM record on every A query, single-threaded and serialized behind blocking upstream forwards

- **Local:** `crates/delonix-net/src/infra.rs:3206`
- **Categoria:** performance · confiança do finder: high

**Descrição.** The holder's service-discovery DNS server (`dns_server_main`, infra.rs:3087) is a single std::thread with one blocking `recv_from` loop: it handles exactly one query at a time and only reads the next datagram after the current one is fully answered. Two costs compound on that serial path:

1. O(n) disk scan per query: `handle_dns` calls `dns_resolve(&name)` for EVERY A query (infra.rs:3132-3133), including external names. `parse_internal_name` returns `Some((whole_name, None))` for an external domain like `github.com`, so `dns_resolve` (infra.rs:3210-3251) still `read_dir`s `<base>/containers`, `fs::read`s and JSON-parses EVERY container record, then `read_dir`s+parses EVERY VM record, before giving up and forwarding. So all DNS traffic on the node — including every container's outbound internet lookup — pays a full re-read+parse of all container and VM JSON files, with no cache.

2. Head-of-line blocking on forward: on a miss, `forward_dns` (infra.rs:3158-3171) does a synchronous `send_to`+`recv_from` with a 3s read timeout against SLIRP_DNS and then 1.1.1.1 (up to ~6s total). Because the server thread is serial, that stall freezes DNS for the whole node — no other container can resolve anything until it returns.

**Cenário de falha.** On a Kubernetes node (kubelet + many pods) with, say, 50 container records, each pod DNS lookup re-reads and JSON-parses all 50 files single-threaded; concurrent lookups queue behind each other. If one container queries a black-holed/slow domain, `forward_dns` blocks the single server thread for up to ~6s, during which every other container's DNS (including internal service discovery <name>.<ns>.delonix.internal) is stalled — a single misbehaving client degrades DNS for all workloads.

**Correcção sugerida.** Handle each datagram in its own task/thread (or an async UDP loop) so a slow forward can't block other queries; and resolve internal names from an in-memory index refreshed on record change (or short-TTL cache / inotify) instead of a full directory scan per query. Fast-path: only scan the store when the name actually carries an internal suffix, forwarding external names immediately.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev medium) · confirma (sev medium)

### 19. ⚪ LOW — valid_version aceita versão patch (1.31.2) mas k8s_repo_version constrói um path pkgs.k8s.io inexistente

- **Local:** `crates/delonix-runtime-bin/src/cmd/k8s_recipes.rs:37`
- **Categoria:** gap · confiança do finder: medium

**Descrição.** k8s_repo_version faz `format!("stable:/v{v}")` com a versão CRUA. O repositório pkgs.k8s.io só publica directórios ao nível minor (`stable:/v1.31/deb/`), não patch. Porém valid_version (cluster.rs) aceita e o seu doc-comment afirma explicitamente que `1.31` OU `1.31.2` são válidos. Assim uma versão patch passa a validação mas gera uma URL de repositório 404.

**Cenário de falha.** `spec.k8sVersion: "1.31.2"` (aceite por valid_version) → recipe do repositório escreve `deb [...] https://pkgs.k8s.io/core:/stable:/v1.31.2/deb/ /` → `apt-get update && apt-get install` falha com 404 em TODOS os hosts do cluster no prepare_host, abortando o bootstrap. A doc diz que este valor é válido, criando expectativa errada.

**Correcção sugerida.** Truncar a versão para major.minor ao construir a URL do repositório (`stable:/vMAJOR.MINOR`), mantendo a versão completa apenas para `--kubernetes-version` do kubeadm; ou restringir valid_version/documentação a major.minor para o path do repo.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev low) · confirma (sev low)

### 20. ⚪ LOW — Concurrent first-time ensure_running double-spawns two proxies → orphaned proxy with no pidfile

- **Local:** `crates/delonix-runtime-bin/src/cmd/ingress_proxy.rs:720`
- **Categoria:** concorrência · confiança do finder: medium

**Descrição.** ensure_running checks running_pid() and, if None, calls spawn_proxy() + publish_listeners() with no lock. When the proxy does not yet exist and two callers (e.g. an httproute apply and a container run --expose, or two --expose launches) run concurrently, both observe running_pid()==None and both spawn a proxy. Both proxies try to bind the same listener port(s) inside the holder netns; one wins, the other crashes at bind. The pidfile is overwritten by whichever spawn writes last (line 807). If the crashed proxy's pid was written last, running_pid() later cleans it as dead while the surviving proxy has no pidfile recorded — it becomes an orphan that can no longer be targeted by SIGHUP (reload) or SIGTERM (stop).

**Cenário de falha.** No proxy is running. `delonix httproute apply` and `delonix container run --expose ... ` (or two --expose runs) execute at the same moment. Both reach spawn_proxy; proxy A binds :8080 and lives, proxy B fails to bind and exits after writing proxy.pid with B's pid. running_pid() finds B dead, removes the pidfile; proxy A keeps serving but is untracked — a later `httproute rm`/apply cannot signal it, and stop() leaves it running.

**Correcção sugerida.** Guard the running_pid()-check → spawn → pidfile-write sequence in ensure_running under the same lock file already used for auto.json (or a dedicated proxy.lock), so the spawn decision is serialized and only one proxy is ever started.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev low) · confirma (sev low)

### 21. ⚪ LOW — kube generate emits YAML with unescaped quoting — command args break or inject YAML

- **Local:** `crates/delonix-runtime-bin/src/cmd/kube.rs:79`
- **Categoria:** bug · confiança do finder: high

**Descrição.** pod_manifest() serializes container command/args by wrapping each token with `quote()` = `format!("\"{s}\"")` (kube.rs:59-63, 78-80), which does not escape embedded double-quotes, backslashes, or newlines. A container whose command contains a `"` yields a token like `"echo "x""`, which is invalid YAML; a token containing a newline injects raw line breaks into the document. Since the whole point of `kube generate` is to pipe the output into `kubectl apply -f -`, a container created with a quote/newline in its command produces a manifest that either fails to parse or is structurally altered (extra keys after a newline-carrying value).

**Cenário de falha.** User runs `delonix container run --name web alpine sh -c 'echo "hi"'` then `delonix kube generate web | kubectl apply -f -`. pod_manifest emits `args: ["-c", "echo "hi""]`, which kubectl rejects as malformed YAML; a token containing an embedded newline could instead inject sibling keys into the pod spec.

**Correcção sugerida.** Build the Pod value with a real YAML serializer (serde_yaml, already a dependency here) or properly escape/emit block/flow scalars, instead of hand-formatting with an unescaped `format!("\"{s}\"")`.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev low) · confirma (sev low)

### 22. ⚪ LOW — Dashboard reconciles every VM twice per 1s refresh, doubling virsh subprocess spawns

- **Local:** `crates/delonix-runtime-bin/src/cmd/dash.rs:112`
- **Categoria:** performance · confiança do finder: high

**Descrição.** `DashData::collect` (called every 1s in the TUI loop, dash.rs:429-430) builds the VM list as `delonix_vm::list(&root).into_iter().map(|v| delonix_vm::status(&root, &v.name)...)`. But `delonix_vm::list` (delonix-vm/src/lib.rs:1367-1374) ALREADY reconciles each VM by calling `status(base, &vm.name)` per entry. The `.map(status)` in dash therefore reconciles every VM a second time. For the libvirt backend each `status` is not cheap: `is_running`→`libvirt_domain_uri` probes up to two URIs via `virsh domstate` (infra.rs:664-668) plus another `domstate`/`domifaddr` for the IP — several `virsh` process spawns per VM. So every dashboard tick forks roughly twice as many `virsh` processes as needed.

**Cenário de falha.** A host running the libvirt backend with e.g. 8 VMs open in `delonix dash`: each 1s tick spawns ~4 virsh subprocesses per VM twice = ~64 forks/sec, half of them pure waste, making the dashboard laggy and adding steady CPU load for a read-only view.

**Correcção sugerida.** Drop the redundant `.map(|v| delonix_vm::status(...))` and use the already-reconciled result of `delonix_vm::list(&root)` directly.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev low) · confirma (sev low)

### 23. ⚪ LOW — Image pull buffers each layer fully in RAM before writing to CAS

- **Local:** `crates/delonix-image/src/registry.rs:662`
- **Categoria:** memória · confiança do finder: high

**Descrição.** During pull, each layer is fetched by `blob_with_progress` (registry.rs:227-259) which reads in 64KB chunks but accumulates the whole blob into a single `Vec<u8>` (`buf` pre-sized to Content-Length), then `store.cas().write(&data)` (cas.rs:50) takes `&[u8]` and writes it out — no streaming to disk. Peak resident memory equals the size of the largest layer. The chunked read exists only for progress reporting, not to bound memory.

**Cenário de falha.** Pulling a large image (e.g. `kindest/node` or the golden VM artifact, layers of hundreds of MB to multiple GB) on a small/memory-constrained node causes a RAM spike of one full layer, risking OOM-kill of the pull (or of co-located workloads) on nodes where that headroom isn't available.

**Correcção sugerida.** Stream the blob to a temp file while updating the running sha256, then rename into CAS (a `write_streaming(reader) -> digest` on the CAS), so peak memory is the 64KB chunk rather than the whole layer.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev low) · confirma (sev low)

### 24. ⚪ LOW — Management (and CRI) accept loop terminates the entire server on any transient accept() error, permanently downing the control-plane

- **Local:** `crates/delonix-mgmt/src/lib.rs:80`
- **Categoria:** recurso · confiança do finder: high

**Descrição.** In `serve_over_uds` the accept loop does `let (socket, _) = uds.accept().await.map_err(|e| Error::Runtime{...})?;` — the `?` propagates ANY accept error out of `serve_over_uds`, which returns from `serve_blocking`, which exits the `delonix api` process. `accept()` can fail with recoverable, per-connection errors — most importantly EMFILE/ENFILE (per-process/system fd exhaustion) and ECONNABORTED. Because each accepted connection is `tokio::spawn`ed with no concurrency/fd limit, a burst of connections (each holding an fd until its handler finishes; long ops like `image build`/`pull` hold them for minutes) can drive the process to its fd ceiling; the next `accept()` returns EMFILE and the whole management API dies permanently instead of shedding load and recovering. The identical pattern exists in delonix-cri/src/lib.rs (its accept path), so a single recoverable condition takes down the kubelet-facing runtime endpoint too.

**Cenário de falha.** A client (or bug) opens many concurrent connections / triggers several concurrent long builds until the process hits RLIMIT_NOFILE. The subsequent `uds.accept()` returns EMFILE; `serve_over_uds` returns Err; `serve_blocking` returns and the `delonix api` process exits. All subsequent control-plane calls fail with connection-refused until an operator manually restarts it — a transient, self-clearing condition became a hard outage.

**Correcção sugerida.** Do not `?` out of the accept loop on per-connection errors: log and `continue` on EMFILE/ENFILE/ECONNABORTED (optionally with a short backoff), reserving fatal exit for truly unrecoverable listener errors. Consider bounding in-flight connections. Apply the same fix to delonix-cri.

> **Consenso dos céticos:** 0 refutação(ões) · confirma (sev low) · confirma (sev low)

## Achados NÃO-verificados (verificação interrompida por limite de sessão)

⚠️ Estes vêm dos finders mas os seus dois céticos falharam por quota — **não** passaram pela verificação adversarial. Tratá-los como **candidatos a confirmar**, não como confirmados. Concentram-se nos subsistemas de maior risco (core de syscalls, holder da SDN, `container run`).

### 1. 🟠 HIGH — `container run --rm` leaks the entire rootfs directory in rootless mode (both foreground and watcher paths)

- **Local:** `crates/delonix-runtime-bin/src/cmd/container.rs:1597`
- **Categoria:** recurso · confiança do finder: high

**Descrição.** The two `--rm` auto-removal paths — the foreground branch (lines 1592-1598) and the detached `spawn_rm_watcher` (lines 1651-1666) — both do `runtime::remove` + `unpublish_ports` + `images.unmount_rootfs(&c.id)`, but NEITHER calls `images.remove_container_dir(&c.id)`. In rootless mode (the product's default/primary mode) the container's rootfs is a FLAT copy at `containers/<id>/rootfs`, and `unmount_rootfs` DELIBERATELY preserves that directory (overlay.rs:263-273: `if base.join("rootfs").exists()` it only removes merged/upper/work and keeps `rootfs/`). `runtime::remove` (lib.rs:4288-4303) only removes the cgroup and the store JSON record — it never touches the container directory. So every `--rm` container in rootless leaves its full rootfs behind forever. This is exactly the leak documented as fixed in `cmd_rm` (container.rs:2369-2375 — '49 directories (45 GiB) piled up ... kubelet marked disk-pressure'), where the fix WAS to add `remove_container_dir`; the fix was applied only to the explicit `rm` path, not to the `--rm` auto-remove paths that share the same intent. `delonix system prune` is the only backstop, but `--rm`'s entire contract is automatic full cleanup (as in Docker, which removes the container filesystem).

**Cenário de falha.** Rootless: `delonix container run --rm alpine echo hi` (foreground) or `delonix container run -d --rm ...` (detached, watcher path). Container exits → record removed, cgroup removed, but `~/.local/share/delonix/containers/<id>/rootfs` (tens–hundreds of MB per image) is never deleted. Repeated ephemeral runs (CI, `--rm` loops, build work containers) accumulate orphan rootfs dirs until the disk fills and, on a Delonix-hosted k8s node, the kubelet applies `disk-pressure` — the identical incident CLAUDE.md records as already fixed for `rm`.

**Correcção sugerida.** In both `--rm` branches call `images.remove_container_dir(&c.id)` after `unmount_rootfs`, mirroring `cmd_rm`/`remove_container` (container.rs:2355/2375). In rootless the flat `rootfs/` must be destroyed on `--rm`, not preserved.

### 2. 🟠 HIGH — Global `egress` command silently deletes all per-network egress restrictions (fail-open)

- **Local:** `crates/delonix-net/src/infra.rs:1531`
- **Categoria:** design · confiança do finder: high

**Descrição.** `do_egress` (the GLOBAL egress policy, reached from `set_egress_policy`/`delonix ... egress deny|allow`) begins by deleting EVERY rule in the `fwdeny` chain whose listing line contains both `oifname "tap0"` and `drop`. Its intent is to remove only the single global blanket rule `oifname "tap0" drop`, but the substring match is too broad: the PER-NETWORK egress restrictions installed by `apply_egress_from_state`/`egress_specs` (infra.rs:2179-2203) are of the form `iifname "<bridge>" oifname "tap0" ... drop` — they also contain `oifname "tap0"` and `drop`, so they are deleted too. Once the per-network terminal `drop` is gone, egress falls through `fwdeny` into the `forward` chain, which unconditionally accepts `oifname "tap0"` (infra.rs:152), so the affected network regains full unrestricted Internet egress. The interaction is asymmetric: `do_egress_net` scopes its own cleanup to `iifname "<bridge>"` and never touches the global rule, so only the global command corrupts per-network state, and it does so silently.

**Cenário de falha.** User configures a restricted network: `egress net secure deny` (or `allowlist:...`), installing `iifname "dlxnXXXX" oifname "tap0" drop`; containers on `secure` cannot reach the Internet. Later any global `egress deny` followed by `egress allow` (or a lone `egress allow`) invokes `do_egress`, whose removal loop deletes the `secure` drop rule (it matches `oifname "tap0"`+`drop`). The `allow` branch adds nothing back. Result: containers on `secure` now have unrestricted egress to the Internet, with no error and no indication the per-network policy was wiped — a data-exfiltration path reopened by an unrelated command.

**Correcção sugerida.** Restrict the removal loop to the global rule only: skip any listing line that also contains `iifname` (the global blanket rule has no `iifname`), or delete the global rule by a stored handle/tag rather than by broad substring match. Add a regression test that installs a per-network egress drop, runs global `egress allow`, and asserts the per-network drop survives.

### 3. 🟡 MEDIUM — VFIO PCI address interpolated into libvirt domain XML without hex-validation or xml_escape (manifest-reachable)

- **Local:** `crates/delonix-vm/src/lib.rs:862`
- **Categoria:** bug · confiança do finder: high

**Descrição.** In libvirt_domain_xml, every user-influenced value (name, overlay, seed, mac, net, bridge, cpuset) is passed through xml_escape() — except the PCI passthrough address. parse_pci_addr (lib.rs:914) merely splits the device string on '/', '.', ':' and returns the four raw substrings with NO validation that they are hexadecimal; libvirt_domain_xml then interpolates them directly into <address domain='0x{dom}' bus='0x{bus}' slot='0x{slot}' function='0x{func}'/> (lib.rs:862-864) with no escaping. cfg.devices is attacker-influenced: the manifest spec.devices list is passed through untouched (vm.rs:405 devices: spec.devices) and applies() reaches libvirt_domain_xml via create(). Because the first-colon token (dom) may contain any character except ':' '.' '/', a value like `0' foo='bar:00:00.0` yields dom=`0' foo='bar` and produces `<address domain='0x0' foo='bar' bus='0x00' .../>` — an injected XML attribute; a value like `a':00:00.0` yields a stray quote that makes the XML malformed so `virsh define` fails. Full new-element injection is blocked because closing tags need '/' (consumed by rsplit('/')), which caps the impact at attribute injection / define-time DoS rather than arbitrary XML, but the missing escape+validation is a real gap that breaks the module's own escaping discipline and is exactly the 'endereço' validation the VFIO lens asks about.

**Cenário de falha.** Manifest `spec: { devices: ["0' foo='x:00:00.0"] }` (or `--device "a':00:00.0"`) with backend libvirt → libvirt_domain_xml emits malformed/attribute-injected <address>, so `virsh define` fails (VM cannot be created) or an unintended attribute is injected into the hostdev address element. No hex sanity check means a bogus address also reaches libvirt unfiltered.

**Correcção sugerida.** In parse_pci_addr, reject any component that is not valid hex of the expected width (domain 4, bus 2, slot 2, func 1) and return None otherwise; additionally wrap the four values in xml_escape() at the interpolation site as defense-in-depth, matching every other field in libvirt_domain_xml.

### 4. 🟡 MEDIUM — Re-exec spec file written world-readable with plaintext `-e` secrets, persisting for the whole foreground container lifetime

- **Local:** `crates/delonix-runtime-bin/src/cmd/container.rs:1993`
- **Categoria:** recurso · confiança do finder: medium

**Descrição.** `reexec_into_netns` (used by every `--net <custom>` and `--pod` run) serializes the full `RunOpts` to `state_root()/.reexec-<id>.json` via `std::fs::write` (line 1991-1993). `std::fs::write` creates the file with mode 0666 & ~umask (typically 0644 = world-readable), unlike the rest of the project's security-sensitive writes which use `OpenOptions::create_new().mode(0o600)` (the fix applied to the libvirt XML temp file per Audit #2). `RunOpts.env` carries the raw `-e KEY=VALUE` pairs the user passed on the command line — commonly credentials (`-e DB_PASSWORD=...`, `-e API_TOKEN=...`). The spawn uses `.status()` which BLOCKS until the child exits; for a FOREGROUND container the child's `cmd_run` blocks in `create_with` until the container's process terminates, so the world-readable JSON containing plaintext secrets lives in the state root (rootless: `$XDG_DATA_HOME/delonix` or `$HOME/.local/share/delonix`, which are often 0755) for the container's entire lifetime, only removed at line 2003 after exit.

**Cenário de falha.** On a shared host: user runs `delonix container run --net mynet -e DB_PASSWORD=s3cr3t postgres` (foreground). A co-tenant local user does `readdir` on the state root and `cat ~victim/.local/share/delonix/.reexec-*.json` for the whole time the container runs, reading `s3cr3t` in cleartext. In root mode the file lands in `/var/lib/delonix` (0755) — readable by any local user.

**Correcção sugerida.** Create the spec file with `OpenOptions::new().write(true).create_new(true).mode(0o600)` (same pattern the project already adopted for `ensure_libvirt_network`), or write it under a 0700 per-user runtime dir. Consider excluding raw secret-bearing env from the serialized spec and re-resolving from the SecretStore in the child.

### 5. 🟡 MEDIUM — DNS external-domain hijack via dotted container name (unenforced anti-hijack assumption)

- **Local:** `crates/delonix-net/src/infra.rs:3199`
- **Categoria:** bug · confiança do finder: high

**Descrição.** parse_internal_name (infra.rs:3179) maps any name that is NOT suffixed .delonix.internal/.delonix.io to a whole-name match with no namespace: `api.github.com` -> ("api.github.com", None) (infra.rs:3199-3203, and the test at 3780 asserts exactly this). dns_resolve (infra.rs:3206) then iterates ALL container records in <base>/containers/*.json and returns the container's `ip` whenever v["name"].to_lowercase() == the queried name (infra.rs:3217-3229), short-circuiting BEFORE forward_dns (handle_dns, infra.rs:3132-3148). The documented and unit-tested anti-hijack guarantee (CLAUDE.md 'DNS interno'; test parse_internal_name_handles_all_schemes comment: 'matches no container foo.com -> forwards') is load-bearing on the claim that container names contain no dots. That claim is NOT enforced anywhere: `--name`/manifest `metadata.name` flows raw into Container.name (cmd/container.rs:1228-1248, cmd/container.rs:787) with no character validation (no valid_name in delonix-runtime-core for containers; sanitize() is only applied to netns/device paths, never to the stored name). A container literally named `api.github.com` therefore matches, and because the whole-name/legacy path resolves in ANY namespace, the hijack crosses the namespace boundary that CLAUDE.md presents as a security guarantee ('isolation also in DNS'). This EXTENDS the documented guarantee: the docs assert the hijack is impossible; the missing name validation makes it possible. The DNS server is always spawned in the holder (infra.rs:560) and serves every container/VM on the SDN.

**Cenário de falha.** Operator applies an innocuous-looking untrusted manifest `delonix stack apply -f third-party.yaml` containing `kind: Container` with `metadata.name: api.github.com` (a syntactically valid name). The container is created with Container.name = "api.github.com" and gets an SDN IP. Any other container on the shared SDN (including one in a different namespace, e.g. `prod`) that resolves `api.github.com` receives an A record pointing at the attacker container's internal IP instead of forwarding to the real upstream (dns_resolve returns before forward_dns) -> silent MITM of the external service for the whole SDN. The repo's own trust model treats `stack apply -f` manifest metadata.name as untrusted (see the VM path-traversal finding in CLAUDE.md), so this is in-scope for that model.

**Correcção sugerida.** Enforce the assumption the resolver depends on: reject/validate container (and VM) names against a whitelist that forbids '.' (and other DNS-significant characters) at the engine boundary (delonix-runtime-core Container::new or cmd_run, analogous to delonix_vm::valid_vm_name), OR make dns_resolve only match names that cannot contain a '.' by construction. At minimum, in the whole-name/legacy path of parse_internal_name, refuse to treat a multi-label name as an internal name unless it carries a delonix suffix, so a dotted external FQDN can never match a container record.

### 6. 🟡 MEDIUM — Bind-mount target não é resolvido com securejoin — symlink na imagem escapa o rootfs (mkdir/create de ficheiro no host como root)

- **Local:** `crates/delonix-runtime/src/lib.rs:967`
- **Categoria:** bug · confiança do finder: medium

**Descrição.** `bind_volume` (chamado por `setup_rootfs` no laço `for m in mounts` a lib.rs:1193, ANTES do `pivot_root`) constrói o destino por concatenação crua `dst = format!("{rootfs}{}", m.target)` e a seguir faz `std::fs::create_dir_all(&dst)` / `OpenOptions...open(&dst)` (branch de ficheiro) e finalmente `mount(source, dst)`. O `mount_target_safe` só rejeita `..`/relativos no path LÓGICO — NÃO confina a resolução de symlinks que existam DENTRO do rootfs da imagem. Como estas operações correm antes do pivot, `/` ainda é o filesystem do host, partilhado (o `MS_PRIVATE` só isola propagação de mounts, não os inodes). No modo ROOT (motor com sudo, sem userns — `userns=false`), `setup_rootfs`/`bind_volume` correm como root REAL do host. O mesmo padrão aplica-se a `apply_tmpfs` (create_dir_all do alvo) e aos mount points criados noutros helpers. A auditoria de path-traversal documentada no CLAUDE.md cobre `mount_target_safe` (só `..`), o COPY do `build` e o nome da VM — mas NÃO cobre a resolução de symlinks do alvo de bind no rootfs, logo é um vector novo. runc/Docker usam securejoin (resolução confinada com O_NOFOLLOW por componente) exactamente para fechar isto.

**Cenário de falha.** Imagem maliciosa traz `/var` como symlink para `/etc` (ou `/var -> /`). Operador corre em modo root `delonix container run -v data:/var/data <imagem-maliciosa>`. Ao preparar o rootfs, `create_dir_all("{rootfs}/var/data")` segue o symlink `{rootfs}/var` (alvo absoluto `/etc`, resolvido contra a raiz do host pré-pivot) e cria `/etc/data` NO HOST como root; o branch de ficheiro cria um ficheiro vazio num caminho escolhido pela imagem (ex.: `/etc/nologin` → DoS de login). Primitiva de integridade/DoS controlada pela imagem (mkdir + criação de ficheiro vazio em caminho absoluto arbitrário) no host, como root. Em rootless (default) o uid mapeado (100000) não é root no host, o que mitiga; por isso é MEDIUM e limitado ao modo root.

**Correcção sugerida.** Resolver o alvo de bind (e de --tmpfs/--device) com um join confinado ao rootfs (O_NOFOLLOW por componente / openat2 RESOLVE_IN_ROOT, ou o mesmo padrão `safe_join`/`safe_rel` já usado em `delonix-image::overlay` e `cmd::build`), rejeitando qualquer componente que seja symlink a sair do rootfs, antes de create_dir_all/File::create/mount.

### 7. 🟡 MEDIUM — Blocking `read` after a failed `poll` defeats the 10s guard and can hang `run` forever

- **Local:** `crates/delonix-net/src/infra.rs:508`
- **Categoria:** concorrência · confiança do finder: high

**Descrição.** `start_slirp` guards the slirp ready-fd with `wait_readable(rd, 10_000)` (a `poll`), whose entire documented purpose is to avoid a bare blocking `read` that hangs forever when the child never signals and never closes the write end (a grandchild inheriting `wr` — created by `libc::pipe` WITHOUT O_CLOEXEC — is enough to prevent EOF). However, when `wait_readable` returns `false` (10s timeout: the fd is neither readable nor hung-up), the code only logs a warning and then unconditionally executes `libc::read(rd, ...)` on the still-blocking fd (lines 511-516). Since there is no data and no EOF, that `read` blocks indefinitely, reintroducing the exact deadlock the poll was added to prevent — and because `slirp_attach` now runs before the container entrypoint is released, this hangs the whole `run` with no log and no exit. The sibling per-container path `slirp_attach` (lib.rs:2232) has an even barer version: a blocking `read` with no poll guard at all.

**Cenário de falha.** slirp4netns starts but stalls before writing its ready byte (slow host, resource pressure, or a forked helper) while keeping the inherited write-end open. `wait_readable` times out at 10s and returns false; execution falls through to `libc::read(rd,...)`, which blocks forever because no ready byte is written and `wr` is still held open (no EOF). `delonix container run` hangs indefinitely instead of surfacing the warned error downstream.

**Correcção sugerida.** Only perform the `read` when `wait_readable` returned true; on timeout, skip the read (and `close(rd)` regardless) so the flow proceeds to surface the error. Apply the same `wait_readable` guard to `slirp_attach` in lib.rs, and/or create the pipe with `O_CLOEXEC`/`O_NONBLOCK` so an inherited copy cannot pin the write end open.

### 8. 🟡 MEDIUM — `system prune` can tear down the entire ingress infra mid-`run` (ref-marker TOCTOU)

- **Local:** `crates/delonix-net/src/infra.rs:304`
- **Categoria:** concorrência · confiança do finder: medium

**Descrição.** `attach_container` writes a container's ingress ref-marker via `acquire(id)` (container.rs:1429) well before the container record is persisted to the Store (container.rs:1575). `reap_orphan_refs` (infra.rs:296-306), invoked by `system prune` (system.rs:239), builds its `live` set exclusively from the Store (running containers) plus `cri-*`/`vm-*` prefixed markers. A marker written by an in-flight `run` whose record is not yet saved is therefore classified as an orphan, removed, and — if it is the only remaining marker — triggers `teardown()`, which SIGTERMs the holder and slirp and destroys the shared netns. This is under `FileLock`, so it is not a data race, but the reaper's view of "live" is stale relative to the acquire-before-save window, and teardown destroys networking for the racing container (and drops all veths/nft state of any others, since the holder netns is shared).

**Cenário de falha.** A single container is being created: `attach_container` has written its ref-marker but `store.save` has not yet run. Concurrently the operator runs `delonix system prune`; `live_refs` is assembled from the Store and does not contain the new id, so `reap_orphan_refs` removes its marker, finds the refs dir now empty, and calls `teardown()` — killing the holder and slirp. The container's subsequent `control_send(attach ...)` hits a dead holder and the run fails; if other containers were relying on the ingress they lose networking too.

**Correcção sugerida.** Have `attach_container` acquire the ref-marker under a broader guarantee (e.g., persist a minimal Store placeholder before acquire, or have prune take the ingress `FileLock` and additionally cross-check `attached_refs` against in-flight leases in the IPAM registry), so a marker for an id with a live IPAM lease is never treated as orphan.

### 9. ⚪ LOW — `find` silently resolves an ambiguous id prefix to the newest container instead of erroring

- **Local:** `crates/delonix-runtime-bin/src/cmd/util.rs:72`
- **Categoria:** design · confiança do finder: high

**Descrição.** `find` matches with `c.id == q || c.id.starts_with(q) || c.name == q` and returns the FIRST hit from `store.list()`, which is sorted by `created_unix` descending (store.rs:211). When a short id prefix matches more than one container, `find` silently picks the most-recently-created one rather than reporting ambiguity. Docker/Podman refuse an ambiguous prefix (`multiple ... found`). This drives destructive verbs — `container stop`/`rm` all resolve via `find` — so an ambiguous prefix can act on an unintended container without warning. Also, an id prefix match takes precedence position-wise over a container whose exact NAME equals `q` only by list ordering, so `q` intended as a name could resolve to a different container whose id happens to start with `q`.

**Cenário de falha.** Two containers with ids `a1f3...` and `a1f9...`; user runs `delonix container rm a1` intending the older one. `find` returns the newest matching container (list is created-desc) and `rm` destroys the wrong container, silently — no ambiguity error.

**Correcção sugerida.** Collect all matches; if more than one distinct container matches (and there is no exact id/name equality that uniquely disambiguates), return an error listing the candidates, matching Docker semantics. Prefer exact `id ==`/`name ==` matches before falling back to prefix.

### 10. ⚪ LOW — `exec` fuga os fds das namespaces do container no processo-pai (namespaces ficam pinned após o container morrer)

- **Local:** `crates/delonix-runtime/src/lib.rs:3591`
- **Categoria:** recurso · confiança do finder: high

**Descrição.** Em `exec`, os fds `/proc/<pid>/ns/{user,uts,net,pid,mnt}` são abertos como i32 CRUS no pai (lib.rs:3585-3591) e guardados em `fds: Vec<(&str,i32)>`. Só o FILHO do 1.º fork os fecha (via `OwnedFd::from_raw_fd(*fd)` que fecha no drop após o setns, 3624-3631). No braço `ForkResult::Parent` (3734-3763) os fds NUNCA são fechados; o `Vec<(&str,i32)>` ao ser dropado não fecha i32 crus, e o `O_CLOEXEC` só ajuda num `execve` do próprio pai (que não acontece num chamador persistente). Contrasta com `mount_live`/`unmount_live`, que usam `OwnedFd` (via `open_container_ns`) e por isso fecham correctamente no braço do pai — a assimetria confirma o bug. Não há UAF/double-free, é fuga pura. Impacto real hoje é baixo (os chamadores — CLI e o subprocess `delonix exec` do CRI — são de vida curta e saem logo); mas `delonix_runtime::exec` é API pública e um supervisor in-process que faça exec repetidos esgota fds E, pior, cada fd de ns MANTÉM viva a namespace (mnt/net/pid/user) mesmo depois de o container morrer, impedindo a libertação dos recursos associados.

**Cenário de falha.** Um chamador in-process de vida longa (ex.: um futuro supervisor, ou se o CRI deixasse de delegar e chamasse `exec` directamente) faz N `exec` num container e depois o container é removido/morre. Os até 5 fds de ns por chamada acumulam-se no pai (esgotamento de fds) e os fds de mnt/net/pid pinned impedem o teardown das namespaces do container morto (leak de netns/mount-tree). No CLI actual o processo sai e limpa, logo severidade baixa.

**Correcção sugerida.** Envolver os fds em `OwnedFd` (como faz `open_container_ns`), ou fechar explicitamente todos os `fds` no braço `ForkResult::Parent` antes de retornar.

### 11. ⚪ LOW — `apply_sysctls` escreve sysctls `net.*` no netns do HOST quando o container usa `--net host`

- **Local:** `crates/delonix-runtime/src/lib.rs:1728`
- **Categoria:** design · confiança do finder: high

**Descrição.** `sysctl_namespaced` (lib.rs:1728) devolve `true` para todos os `net.*`, partindo do princípio de que sysctls de rede são sempre namespaced e portanto seguros para o container mudar. Isso só é verdade se o container tiver a SUA PRÓPRIA netns (`CLONE_NEWNET`). Num container `--net host` (sem `CLONE_NEWNET`, ver `spawn` lib.rs:3062 — a flag NEWNET não é adicionada), `/proc/sys/net` reflecte a netns do HOST. `apply_sysctls` corre em `setup_rootfs` (chamado a lib.rs:1227) ANTES de `/proc/sys` ficar read-only (`mask_proc_paths`) e ainda com todas as caps, escrevendo directamente em `/proc/sys/net/...`. Faltando a validação cruzada que o Docker faz (o Docker recusa `--sysctl net.*` combinado com host networking exactamente por isto), o valor é aplicado ao host. É input do operador (não do container), por isso baixo; ainda assim é uma falha silenciosa de contenção (o operador julga estar a afinar o container).

**Cenário de falha.** `delonix container run --net host --sysctl net.ipv4.conf.all.rp_filter=0 <img>` (ou `net.ipv6.conf.all.forwarding=1`) escreve o knob GLOBAL da netns do host, desligando o reverse-path filtering / ligando forwarding a nível de máquina, em vez de afectar apenas o container.

**Correcção sugerida.** Recusar `--sysctl net.*` (com erro claro, coerente com o invariante 'sem falha silenciosa') quando o container partilha a netns do host (sem `CLONE_NEWNET`), à semelhança do Docker; ou aplicar os net.* apenas no caminho com netns própria.

## Refutados na verificação

- ~~write_rec is not atomic for concurrent writes to the same record id (temp name only per-process)~~ (`crates/delonix-cri/src/runtime_svc/lifecycle.rs:152`) — abatido pelos céticos.

## Crítico de completude (próximo passo)

A verificação adversarial NÃO cobriu, por quota, estes subsistemas — merecem uma 2.ª corrida
(a partir das 18:10 Luanda, quando a sessão reset):

- `delonix-runtime/src/lib.rs` — o core de 104 `unsafe` (clone/mount/setns/seccomp): a lente
  de **memory-safety** ficou praticamente por verificar. É o ponto de maior risco do repo.
- `delonix-net/src/infra.rs` — holder + control socket + nft: os 5 achados de finder aqui
  (incluindo 1 HIGH de `egress` global destrutivo) ficaram sem verificação adversarial.
- `container.rs` e `vm.rs` — o achado HIGH de fuga de rootfs no `--rm` e o VFIO-em-XML.

Além disso, a fase de finders correu 1 passagem por subsistema; um 2.º passo com a lente de
**concorrência sob carga** (CRI + mgmt + proxy em simultâneo) e um **fuzzing dos parsers**
(DNS, .po, manifesto YAML, tar de layer) apanharia o que a leitura estática não vê.

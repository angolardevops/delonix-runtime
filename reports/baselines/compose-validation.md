# Baseline — Compose file validation path (CLI `compose config`)

**Status: VALIDADA** (this environment, offline, no engine required)
Date: 2026-10-21 · HEAD `71ff8a21` (v5.1.1) + 3 uncommitted deliveries (G-011, G-017).

## What this baseline covers

The operational path **`delonix compose config -f <file>` → raw-YAML parse → allowlist
refusal → auditable error / resolved project**. It proves that the Compose parser refuses
unrecognized keys — at the top level, at the service level, and (new, G-011) *inside* a
service's nested maps — naming the full dotted path, instead of parsing them into a typed
struct that drops them in silence. The companion G-017 delivery (MCP `name` validation) is
confirmed by its own tests.

Included behaviour:
- Refusal of an unknown nested key (`healthcheck.disable`, `deploy.resources.reservations`,
  `deploy.mode`, `deploy.resources.limits.pids`, `depends_on.<svc>.restart`, a volume mount's
  `bind:`), each by its full dotted path.
- Acceptance of a fully-supported nested file, printed as a resolved project.
- Acceptance of `x-` extension fields at every level, including inside a nested map.
- Idempotent (pure read): repeated runs are byte-identical.

Explicitly **out** of this baseline: implementing any refused key as behaviour (they are
refused, not wired); `compose up`/running containers; the Docker-API/OCI/provider gaps.

## Environment / versions

- Rust toolchain: per `rust-toolchain.toml` (repo-pinned).
- Binary: `target/debug/delonix` built from this tree.
- No network, no running engine, no root required — `compose config` is a pure validator.

## Reproduce

```bash
R=/path/to/delonix-runtime
cd "$R"

# 1. Build the real CLI
cargo build -p delonix-runtime-bin --bin delonix
BIN="$R/target/debug/delonix"

# 2. Fixtures
D=$(mktemp -d)
cat > "$D/invalid-healthcheck.yml" <<'EOF'
services:
  web:
    image: nginx
    healthcheck:
      disable: true
EOF
cat > "$D/invalid-deploy.yml" <<'EOF'
services:
  web:
    image: nginx
    deploy:
      resources:
        reservations:
          cpus: '0.1'
EOF
cat > "$D/valid.yml" <<'EOF'
name: demo
services:
  web:
    image: nginx
    healthcheck:
      test: ["CMD", "true"]
      interval: 5s
      timeout: 3s
      retries: 2
      start_period: 1s
    deploy:
      replicas: 1
      resources:
        limits:
          cpus: '0.5'
          memory: 256m
    depends_on:
      db:
        condition: service_started
    volumes:
      - type: bind
        source: /tmp
        target: /data
        read_only: true
  db:
    image: postgres
EOF

# 3. Exercise
"$BIN" compose config -f "$D/invalid-healthcheck.yml"; echo "exit=$?"
"$BIN" compose config -f "$D/invalid-deploy.yml";      echo "exit=$?"
"$BIN" compose config -f "$D/valid.yml";               echo "exit=$?"

# 4. Cleanup
rm -rf "$D"
```

## Expected vs observed

| Input | Expected | Observed |
|-------|----------|----------|
| `invalid-healthcheck.yml` | exit 1, names `healthcheck.disable` | `error[DX-1000] … service 'web': `healthcheck.disable:` is not understood and was refused rather than ignored — `healthcheck:` reads test, interval, timeout, retries, start_period` · exit=1 |
| `invalid-deploy.yml` | exit 1, names `deploy.resources.reservations` | `error[DX-1000] … `deploy.resources.reservations:` … refused … — `deploy.resources:` reads limits` · exit=1 |
| `valid.yml` | exit 0, resolved project; long volume mount → `/tmp:/data:ro` | project `demo`, services `db`+`web`, `web` volume `"/tmp:/data:ro"` (read_only honoured) · exit=0 |
| repeated run of `valid.yml` | byte-identical | identical (idempotent read) |
| `deploy.x-note` extension | exit 0, accepted | accepted; `replicas: 2` expands to two `web` instances |
| unknown top-level `bogus:` | exit 1 | refused, lists the 7 top-level keys · exit=1 |

## Regression proof (test verifies behaviour, not implementation)

With the descent call `check_nested_service_fields(svc, svc_map)?` disabled in
`check_unsupported_value` (compose.rs ~line 2011), the behaviour test
`a_nested_key_outside_its_allowlist_is_refused_by_full_path` **FAILS**
(`this compose file must be refused: ()`); restoring the call makes it **PASS**.

Gates (all green at close):
- `cargo test -p delonix-runtime-bin --bin delonix` → 1415 passed, 0 failed.
- `cargo test -p delonix-mcp -p delonix-mgmt` → 23 + 36 passed, 0 failed (G-017).
- `cargo clippy -p delonix-runtime-bin -p delonix-mcp -p delonix-mgmt` → clean.

## Logs / locations

- Source of refusal: `bins/delonix-runtime-bin/src/cmd/compose.rs`
  — `check_unsupported_value` → `check_nested_service_fields` → `refuse_unknown_nested`.
- Allowlist-vs-struct drift guard: `nested_struct_fields_are_all_in_the_allowlist`
  (reads the file's own source, honours `#[serde(rename)]`).
- G-017 validator: `delonix-mgmt::valid_arg` (shared), called by `delonix-mcp`
  `logs.query`/`container.restart`.

## Known limitations

- The nested walk now covers every nested service body with sub-keys: `deploy`,
  `healthcheck`, long `depends_on`, long volume mount, `build`, service `networks` entry,
  `ulimits`, and (resilience/security pass, 2026-10-21) **long-form `ports`**.
- Refused keys are **refused, not implemented** — naming a key here is not a promise the
  engine honours it. The one exception added in the resilience pass is `ports.host_ip`,
  which is now *honoured* (see below), at parity with the short form.
- Validated offline; `compose up` against a live engine is a separate baseline.
- Pathological deep-nested flow YAML (`[`×100k) is refused cleanly (`recursion limit
  exceeded`, exit 1) but the refusal time grows ~quadratically — a minor local-only DoS,
  not a crash or state corruption. Not fixed; recorded as a limitation.

## Resilience/security pass (2026-10-21, HEAD `633d7de0` + this delivery)

**Finding (CONFIRMED, isolation/silent-drop):** long-form `ports` entries were parsed into
`ComposePort::Long`, which read only `target`/`published`/`protocol`. A `host_ip:` or
`mode:` reached that struct (no `deny_unknown_fields`) and was **dropped in silence** —
`ports: [{target: 80, published: 8080, host_ip: 127.0.0.1}]` resolved to `"8080:80"`,
publishing on EVERY interface a port the author scoped to loopback. The short form
(`127.0.0.1:8080:80`) was fixed for exactly this leak one commit earlier; the long form
still had it, and the two shapes disagreed about who could reach the port.

**Fix:** `host_ip` is now a field of `ComposePort::Long`, prepended verbatim in
`resolve_ports` just as the short form carries it; `check_nested_service_fields` walks the
long-form `ports` list and refuses any key outside `SUPPORTED_PORT_LONG`
(`target, published, protocol, host_ip`) by full dotted path. `mode`/`name`/`app_protocol`
are now named-refused, not dropped.

**Evidence (real CLI, `target/debug/delonix`):**
| Input | Before | After |
|-------|--------|-------|
| long `{target:80, published:8080, host_ip:127.0.0.1}` | `"8080:80"` (leak) · exit 0 | `"127.0.0.1:8080:80"` · exit 0 |
| short `127.0.0.1:8080:80` | `"127.0.0.1:8080:80"` | `"127.0.0.1:8080:80"` (parity) |
| long `{…, mode: host}` | accepted, dropped · exit 0 | `` `ports.mode:` … refused `` · exit 1 |
| long `{target:53, published:5353, protocol:udp}` | `"5353:53/udp"` | `"5353:53/udp"` (unchanged) |

**Regression gates added** (`bins/delonix-runtime-bin/src/cmd/compose.rs`):
- `resolve_ports_long_form_honours_host_ip_like_the_short_form`
- `an_unsupported_long_port_key_is_refused_by_name`
- `nested_struct_fields_are_all_in_the_allowlist` extended with `ComposePort::Long` ↔
  `SUPPORTED_PORT_LONG` (drift guard: the parser's field set must equal the allowlist).

Gates green: `cargo test -p delonix-runtime-bin --bin delonix` → 1485 passed, 0 failed;
`cargo clippy -p delonix-runtime-bin` clean; `lang_ratchet.py` rc 0; `dev_docs.py --check`
rc 0.

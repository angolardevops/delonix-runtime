# __NAME__ (HAProxy __TEMPLATE_VERSION__)

An HAProxy L7 reverse proxy / load balancer scaffolded by
`delonix init -t haproxy -v __TEMPLATE_VERSION__`. It listens on `__PORT__`
and, until you point it at real servers, answers requests itself so it runs
out of the box. Infrastructure only — there is no application code here.

What is already configured, and where (`haproxy.cfg`):

| Concern | What this template does |
|---|---|
| Health | `GET /healthz` → `200 ok`, answered by the proxy, kept out of the log |
| Access log | one JSON object per request on stdout, with `request_id`, backend, server and termination state |
| Correlation | keeps the caller's `X-Request-ID` or mints one; forwards it to the backend and returns it |
| Security headers | `X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`; `Server` removed — on proxied AND proxy-generated responses |
| Timeouts | connect 5 s, client/server 30 s, request headers 10 s, keep-alive 5 s, queue 30 s |
| Metrics | Prometheus exporter on `:8405/metrics`, not published by the manifest |
| Config check | `haproxy -c` runs at build time; a typo fails the build |

## Quickstart

```bash
delonix build -t __NAME__:dev .     # runs `haproxy -c` as part of the build
delonix stack apply                 # starts it with restart: always, 128M, 0.5 CPU
sh scripts/smoke.sh                 # health, headers, request id
```

Expected: `smoke: all checks passed`.

## Commands

| Task | Command |
|---|---|
| Build (and validate) | `delonix build -t __NAME__:dev .` |
| Run | `delonix stack apply` |
| Smoke test | `sh scripts/smoke.sh [http://127.0.0.1:__PORT__]` |
| Validate the running config | `delonix container exec __NAME__ haproxy -c -f /usr/local/etc/haproxy/haproxy.cfg` |
| Logs | `delonix container logs -f __NAME__` |
| Metrics | `delonix container exec __NAME__ wget -qO- http://127.0.0.1:8405/metrics` |
| Stop and remove | `delonix stack destroy` |

## Layout

| Path | Purpose |
|---|---|
| `haproxy.cfg` | the whole configuration |
| `Delonixfile` | image build: config, `haproxy -c`, healthcheck |
| `delonix-manifest.yaml` | how it runs: port, restart policy, memory and CPU |
| `scripts/smoke.sh` | the checks above, against a running container |

## Load-balance real servers

In `backend app`: uncomment `option httpchk` and the `server` lines, point them
at your app containers, and delete the standalone `http-request return`. The
servers have to be reachable from this container: run them on the same
Delonix network (`network:` in the manifests) and use their container names or
`networkAlias`. `X-Request-ID` and `X-Forwarded-For` reach them already.

## TLS

Terminate TLS here only when nothing in front of this container does. The
commented `bind *:8443 ssl …` line expects one PEM with the certificate and
key at `/usr/local/etc/haproxy/tls/site.pem` — mount it as a volume (never
`COPY` a key into the image), publish `8443` in `delonix-manifest.yaml`, and
enable `Strict-Transport-Security` only once the site is HTTPS-only.

## Troubleshooting

- **The build fails at `haproxy -c`** — the message names the line.
- **`503 Service Unavailable`** — every `server` in `backend app` is failing
  its health check, or none is declared and the standalone `return` was removed.
- **A host port below 1024** — publishing one needs the host to allow it
  (`net.ipv4.ip_unprivileged_port_start`); `delonix stack apply` says so and
  names the fix. Keep `__PORT__`, or publish a high host port onto it.
- **The process runs as `haproxy` (uid 99), the user the official image
  declares.** It cannot bind a port below 1024 inside the container either,
  which is why the frontends listen on `__PORT__` and 8405. On a host with no
  subordinate uid range the engine cannot apply that user and says so; the
  process then runs as root inside the container.

## Choosing an HAProxy version

The default is `__TEMPLATE_VERSION__`, the line the official image tags as
`lts` when this template was last updated. Regenerate with `-v` to pin another
`haproxy:<version>-alpine` tag, e.g. `delonix init -t haproxy -v 3.2 --force .`
(`--force`: `init` never overwrites without it). `http-after-response` needs
HAProxy 2.2 or newer.

Reference: <https://docs.haproxy.org/>.

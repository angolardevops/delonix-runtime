# __NAME__ (nginx __TEMPLATE_VERSION__)

An nginx edge scaffolded by `delonix init -t nginx -v __TEMPLATE_VERSION__`:
serves the static site in `html/` on port `__PORT__`, and is ready to become a
reverse proxy in front of an app container. Infrastructure only — there is no
application code here.

What is already configured, and where (`nginx.conf`):

| Concern | What this template does |
|---|---|
| Health | `GET /healthz` → `200 ok`, answered by nginx itself, excluded from the access log |
| Access log | one JSON object per request on stdout, with `request_id` |
| Correlation | keeps the caller's `X-Request-ID` or mints one; returns it in the response |
| Security headers | `X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, `Content-Security-Policy`; no version in `Server` |
| Limits/timeouts | 10 MiB request body, 15 s header/body read, 30 s send and keep-alive |
| Metrics | `stub_status` at `/nginx_status`, reachable only from inside the container |
| Config check | `nginx -t` runs at build time; a typo fails the build |

## Quickstart

```bash
delonix build -t __NAME__:dev .     # runs `nginx -t` as part of the build
delonix stack apply                 # starts it with restart: always, 128M, 0.5 CPU
sh scripts/smoke.sh                 # health, headers, request id, 404, status page
```

Expected: `smoke: all checks passed`.

## Commands

| Task | Command |
|---|---|
| Build (and syntax-check) | `delonix build -t __NAME__:dev .` |
| Run | `delonix stack apply` |
| Smoke test | `sh scripts/smoke.sh [http://127.0.0.1:__PORT__]` |
| Check a live edit | `delonix container exec __NAME__ nginx -t` |
| Reload without a restart | `delonix container exec __NAME__ nginx -s reload` |
| Logs | `delonix container logs -f __NAME__` |
| Connection counters | `delonix container exec __NAME__ wget -qO- http://127.0.0.1:__PORT__/nginx_status` |
| Stop and remove | `delonix stack destroy` |

A live edit needs the file inside the container: `nginx.conf` is baked into
the image, so the normal path is edit → `delonix build` → `delonix stack apply`.

## Layout

| Path | Purpose |
|---|---|
| `nginx.conf` | the whole configuration (replaces the image's default) |
| `html/` | the static site; `style.css` is separate because the CSP forbids inline styles |
| `Delonixfile` | image build: base image, config, site, `nginx -t`, healthcheck |
| `delonix-manifest.yaml` | how it runs: port, restart policy, memory and CPU |
| `scripts/smoke.sh` | the checks above, against a running container |

## Put it in front of an app

Uncomment the `location /api/` block in `nginx.conf` and point `proxy_pass` at
the app. The app has to be reachable from this container: run both on the same
Delonix network (`network:` in both manifests) and use the app's container
name or `networkAlias`. The block already forwards `X-Request-ID` and the
`X-Forwarded-*` headers and sets connect/send/read timeouts.

## TLS

Terminate TLS here only when nothing in front of this container does it. The
commented `server { listen 8443 ssl; … }` block expects `tls.crt`/`tls.key`
under `/etc/nginx/tls/` — mount them as a volume (never `COPY` a key into the
image), publish `8443` in `delonix-manifest.yaml`, and enable the
`Strict-Transport-Security` header only once the site is HTTPS-only.

## Troubleshooting

- **The build fails at `RUN nginx -t`** — the message names the file and line.
- **`403` on `/nginx_status`** — expected from outside; it only answers `127.0.0.1`.
- **A page renders unstyled** — the Content-Security-Policy allows same-origin
  resources only; move inline `<style>`/`<script>` into files, or relax the
  policy for your site deliberately.
- **Port `__PORT__` already in use** — `delonix stack apply` names the process
  holding it; change the port in both `nginx.conf` and `delonix-manifest.yaml`.

## Choosing an nginx version

The default is `__TEMPLATE_VERSION__`, the line the official image tags as
`stable` when this template was last updated. Regenerate with `-v` to pin
another `nginx:<version>-alpine` tag, e.g. `delonix init -t nginx -v 1.31
--force .` (`--force`: `init` never overwrites without it). The config uses
nothing newer than nginx 1.25 (`http2 on;` in the TLS example is 1.25.1+).

Reference: <https://nginx.org/en/docs/>.

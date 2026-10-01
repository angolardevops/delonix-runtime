# __NAME__ (Apache httpd __TEMPLATE_VERSION__)

An Apache httpd edge scaffolded by `delonix init -t httpd -v __TEMPLATE_VERSION__`:
serves the static site in `public/` on port `__PORT__`, and is ready to become
a reverse proxy in front of an app container. Infrastructure only — there is
no application code here.

The image's own `conf/httpd.conf` is kept; this template adds
`conf/delonix.conf` at its end, so every setting it changes is in one file:

| Concern | What this template does |
|---|---|
| Health | `GET /healthz` → `ok` (a static file), excluded from the access log |
| Access log | one JSON object per request on stdout, with `request_id` |
| Correlation | keeps the caller's `X-Request-ID` or uses `mod_unique_id`'s; returns it |
| Security headers | `X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, `Content-Security-Policy`; `Server: Apache` with no version; `TRACE` off |
| Limits/timeouts | 10 MiB request body, `RequestReadTimeout` 15–30 s, `Timeout 30`, `KeepAliveTimeout 5` |
| Metrics | `mod_status` at `/server-status`, only from inside the container |
| Config check | `httpd -t` runs at build time; a typo fails the build |

## Quickstart

```bash
delonix build -t __NAME__:dev .     # runs `httpd -t` as part of the build
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
| Check the running config | `delonix container exec __NAME__ httpd -t` |
| Reload without a restart | `delonix container exec __NAME__ httpd -k graceful` |
| Logs | `delonix container logs -f __NAME__` |
| Status counters | `delonix container exec __NAME__ wget -qO- 'http://127.0.0.1:__PORT__/server-status?auto'` |
| Stop and remove | `delonix stack destroy` |

## Layout

| Path | Purpose |
|---|---|
| `conf/delonix.conf` | everything this template sets, included last |
| `public/` | the static site (`healthz` is the health file); `style.css` is separate because the CSP forbids inline styles |
| `Delonixfile` | image build: port, drop the plain-text log, include, `httpd -t` |
| `delonix-manifest.yaml` | how it runs: port, restart policy, memory and CPU |
| `scripts/smoke.sh` | the checks above, against a running container |

## Put it in front of an app

Uncomment the `mod_proxy` block at the end of `conf/delonix.conf` and point it
at the app. The app has to be reachable from this container: run both on the
same Delonix network (`network:` in both manifests) and use the app's
container name or `networkAlias`. `X-Request-ID` is forwarded with the other
request headers.

## TLS

Terminate TLS here only when nothing in front of this container does. The
commented `mod_ssl` block expects `tls.crt`/`tls.key` under
`/usr/local/apache2/tls/` — mount them as a volume (never `COPY` a key into the
image), publish `8443` in `delonix-manifest.yaml`, and enable
`Strict-Transport-Security` only once the site is HTTPS-only.

## Troubleshooting

- **The build fails at `httpd -t`** — the message names the file and line.
- **`403` on `/server-status`** — expected from outside; `Require local`.
- **A page renders unstyled** — the Content-Security-Policy allows same-origin
  resources only; move inline styles and scripts into files, or relax the
  policy for your site deliberately.
- **Two lines per request in the log** — the Delonixfile's `sed` did not find
  the image's `CustomLog … common` line (a different base image version);
  comment it out in `conf/httpd.conf` by hand.

## Choosing an httpd version

`2.4` is the only maintained Apache httpd line. Regenerate with `-v` to pin a
patch release, e.g. `delonix init -t httpd -v 2.4.68 --force .` (`--force`:
`init` never overwrites without it).

Reference: <https://httpd.apache.org/docs/2.4/>.

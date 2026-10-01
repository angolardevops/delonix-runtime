# __NAME__ (Apache httpd __TEMPLATE_VERSION__)

An Apache httpd edge scaffolded by `delonix init -t httpd -v __TEMPLATE_VERSION__`:
serves the static site in `public/` over HTTPS on port `__TLS_PORT__` (port
`__PORT__` redirects there), and is ready to become
a reverse proxy in front of an app container. Infrastructure only — there is
no application code here.

The image's own `conf/httpd.conf` is kept; this template adds
`conf/delonix.conf` at its end, so every setting it changes is in one file:

| Concern | What this template does |
|---|---|
| Health | `GET /healthz` → `ok` (a static file) on both ports, excluded from the access log |
| TLS | HTTPS with HTTP/2 on `__TLS_PORT__`; `__PORT__` redirects; certificate mounted from `./tls` |
| Capacity | event MPM, 4 × 64 = 256 request threads, `nofile` 65 535, keep-alive |
| Access log | one JSON object per request on stdout, with `request_id` |
| Correlation | keeps the caller's `X-Request-ID` or uses `mod_unique_id`'s; returns it |
| Security headers | `X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, `Content-Security-Policy`; `Server: Apache` with no version; `TRACE` off |
| Limits/timeouts | 10 MiB request body, `RequestReadTimeout` 15–30 s, `Timeout 30`, `KeepAliveTimeout 5` |
| Metrics | `mod_status` at `/server-status`, only from inside the container |
| Config check | `httpd -t` runs at build time; a typo fails the build |

## Quickstart

```bash
delonix build -t __NAME__:dev .     # runs `httpd -t` as part of the build
delonix stack apply                 # starts it with restart: always, 512M, 2 CPUs
sh scripts/smoke.sh                 # health, HTTPS, redirect, headers, request id, 404
```

Expected: `smoke: all checks passed`. Or, in one command from an empty
directory: `delonix stack init -t httpd --up __NAME__`, which ends by printing
the address to open.

## Commands

| Task | Command |
|---|---|
| Build (and syntax-check) | `delonix build -t __NAME__:dev .` |
| Run | `delonix stack apply` |
| Smoke test | `sh scripts/smoke.sh [https://127.0.0.1:__TLS_PORT__] [http://127.0.0.1:__PORT__]` |
| New or renewed certificate | `sh scripts/tls.sh` (see TLS below) |
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
| `delonix-manifest.yaml` | how it runs: ports, the `./tls` and `./acme` mounts, restart policy, memory, CPU, descriptors |
| `tls/` | certificate and key — generated, never committed, never in the image |
| `acme/` | Let's Encrypt HTTP-01 webroot |
| `scripts/tls.sh` | local certificate, install one, or Let's Encrypt |
| `scripts/smoke.sh` | the checks above, against a running container |

## Put it in front of an app

Uncomment the `mod_proxy` block at the end of `conf/delonix.conf` and point it
at the app. The app has to be reachable from this container: run both on the
same Delonix network (`network:` in both manifests) and use the app's
container name or `networkAlias`. `X-Request-ID` is forwarded with the other
request headers.

## TLS

HTTPS is on from the first start: `__TLS_PORT__` serves the site (HTTP/2, TLS
1.2 and 1.3, forward-secret suites only) and `__PORT__` redirects to it.

The certificate and key live in `./tls` on the host and are mounted read-only.
They are in `.gitignore` and `.dockerignore`: the key never reaches git or the
image.

| You want | Run |
|---|---|
| A local certificate (what `delonix init` already made) | `sh scripts/tls.sh` |
| More names on it | `sh scripts/tls.sh localhost 127.0.0.1 shop.test` |
| Browsers on this machine to trust it | install [mkcert](https://github.com/FiloSottile/mkcert), `mkcert -install` once, then `sh scripts/tls.sh` |
| To use a certificate you already have | `sh scripts/tls.sh install fullchain.pem privkey.pem` |
| A publicly trusted certificate | `sh scripts/tls.sh letsencrypt example.org you@example.org` |

`tls.sh` reloads the running server; no restart, no dropped connection.

With mkcert the certificate is signed by a CA that exists only on this
machine; without it the certificate is self-signed, HTTPS works and browsers
warn. Neither is for the public internet.

### Let's Encrypt

`scripts/tls.sh letsencrypt <domain> <email>` runs certbot's webroot
challenge: certbot writes the challenge file into `./acme`, this server
answers it over plain HTTP at `/.well-known/acme-challenge/` (the one path
that is not redirected), and the certificate is installed into `./tls` and
loaded. It needs certbot on this host, the domain's DNS pointing at this host,
and port 80 of the host reaching the container (`"0.0.0.0:80:__PORT__"`).
Everything certbot keeps goes to `./letsencrypt`, so no root is involved. The
script prints the `certbot renew` line to put in cron.

`Strict-Transport-Security` is off on purpose: it pins the host name, every
port of it, and on `localhost` that would force every other local service to
HTTPS. Turn it on in `conf/delonix.conf` once this serves a real domain.

To serve on the standard ports, publish `"80:__PORT__"` and `"443:__TLS_PORT__"` in
`delonix-manifest.yaml` (a rootless host needs `install.sh --low-ports`
first) and drop `:__TLS_PORT__` from the redirect in `conf/delonix.conf`. A published
port binds `127.0.0.1`; write `"0.0.0.0:443:__TLS_PORT__"` to accept other machines.

## Sized for load

| Setting | Value | Why |
|---|---|---|
| MPM | event | idle keep-alive connections do not hold a thread |
| `ServerLimit` × `ThreadsPerChild` | 4 × 64 | 256 request threads (`MaxRequestWorkers`) |
| `AsyncRequestWorkerFactor` | 4 | connections accepted beyond the busy threads |
| `ListenBackLog` | 4096 | a burst waits in the kernel instead of being refused |
| `MaxKeepAliveRequests` | 1000 | fewer handshakes per client |
| `SSLSessionCache` | 4 MiB, on a tmpfs | resumable sessions, no disk I/O |

The manifest gives it 2 CPUs, 512 MiB and 65 535 file descriptors. Raise the
numbers together: more CPUs without more workers is idle capacity, more
connections without more descriptors is `Too many open files`.

## Troubleshooting

- **The build fails at `httpd -t`** — the message names the file and line.
- **`403` on `/server-status`** — expected from outside; `Require local`.
- **A page renders unstyled** — the Content-Security-Policy allows same-origin
  resources only; move inline styles and scripts into files, or relax the
  policy for your site deliberately.
- **The browser warns about the certificate** — expected with a self-signed
  one; see the TLS table for the two ways out.
- **`SSLCertificateFile: file … does not exist or is empty`** at start —
  `./tls` is empty: run `sh scripts/tls.sh`.
- **Two lines per request in the log** — the Delonixfile's `sed` did not find
  the image's `CustomLog … common` line (a different base image version);
  comment it out in `conf/httpd.conf` by hand.

## Choosing an httpd version

`2.4` is the only maintained Apache httpd line. Regenerate with `-v` to pin a
patch release, e.g. `delonix init -t httpd -v 2.4.68 --force .` (`--force`:
`init` never overwrites without it).

Reference: <https://httpd.apache.org/docs/2.4/>.

# __NAME__ (HAProxy __TEMPLATE_VERSION__)

An HAProxy L7 reverse proxy / load balancer scaffolded by
`delonix init -t haproxy -v __TEMPLATE_VERSION__`. It serves HTTPS on `__TLS_PORT__` (port `__PORT__` redirects there)
and, until you point it at real servers, answers requests itself so it runs
out of the box. Infrastructure only — there is no application code here.

What is already configured, and where (`haproxy.cfg`):

| Concern | What this template does |
|---|---|
| Health | `GET /healthz` → `200 ok` on both ports, answered by the proxy, kept out of the log |
| TLS | HTTPS with HTTP/2 on `__TLS_PORT__`; `__PORT__` redirects; one PEM mounted from `./tls` |
| Capacity | `maxconn 20000`, 2 threads, `nofile` 65 535, keep-alive and connection reuse to the backends |
| Privilege | starts as root to read the key, handles traffic as `haproxy` |
| Access log | one JSON object per request on stdout, with `request_id`, backend, server and termination state |
| Correlation | keeps the caller's `X-Request-ID` or mints one; forwards it to the backend and returns it |
| Security headers | `X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`; `Server` removed — on proxied AND proxy-generated responses |
| Timeouts | connect 5 s, client/server 30 s, request headers 10 s, keep-alive 5 s, queue 30 s |
| Metrics | Prometheus exporter on `:8405/metrics`, not published by the manifest |
| Config check | `haproxy -c` runs at build time; a typo fails the build |

## Quickstart

```bash
delonix build -t __NAME__:dev .     # runs `haproxy -c` as part of the build
delonix stack apply                 # starts it with restart: always, 512M, 2 CPUs
sh scripts/smoke.sh                 # health, HTTPS, redirect, headers, request id
```

Expected: `smoke: all checks passed`. Or, in one command from an empty
directory: `delonix stack init -t haproxy --up __NAME__`, which ends by
printing the address to open.

## Commands

| Task | Command |
|---|---|
| Build (and validate) | `delonix build -t __NAME__:dev .` |
| Run | `delonix stack apply` |
| Smoke test | `sh scripts/smoke.sh [https://127.0.0.1:__TLS_PORT__] [http://127.0.0.1:__PORT__]` |
| New or renewed certificate | `sh scripts/tls.sh` (see TLS below) |
| Reload config and certificate, no dropped connection | `delonix container kill -s USR2 __NAME__` |
| Validate the running config | `delonix container exec __NAME__ haproxy -c -f /usr/local/etc/haproxy/haproxy.cfg` |
| Logs | `delonix container logs -f __NAME__` |
| Metrics | `delonix container exec __NAME__ wget -qO- http://127.0.0.1:8405/metrics` |
| Stop and remove | `delonix stack destroy` |

## Layout

| Path | Purpose |
|---|---|
| `haproxy.cfg` | the whole configuration |
| `Delonixfile` | image build: config, `haproxy -c`, healthcheck |
| `delonix-manifest.yaml` | how it runs: ports, the `./tls` mount, restart policy, memory, CPU, descriptors |
| `tls/` | certificate, key and `tls.pem` (both) — generated, never committed, never in the image |
| `scripts/tls.sh` | local certificate, install one, or Let's Encrypt (DNS-01) |
| `scripts/smoke.sh` | the checks above, against a running container |

## Load-balance real servers

In `backend app`: uncomment `option httpchk` and the `server` lines, point them
at your app containers, and delete the standalone `http-request return`. The
servers have to be reachable from this container: run them on the same
Delonix network (`network:` in the manifests) and use their container names or
`networkAlias`. `X-Request-ID` and `X-Forwarded-For` reach them already.

## TLS

HTTPS is on from the first start: `__TLS_PORT__` serves the site (HTTP/2, TLS
1.2 and 1.3, forward-secret suites only) and `__PORT__` redirects to it. HAProxy reads `tls/tls.pem`,
the certificate and the key in one file.

The certificate and key live in `./tls` on the host and are mounted read-only.
They are in `.gitignore` and `.dockerignore`: the key never reaches git or the
image.

| You want | Run |
|---|---|
| A local certificate (what `delonix init` already made) | `sh scripts/tls.sh` |
| More names on it | `sh scripts/tls.sh localhost 127.0.0.1 shop.test` |
| Browsers on this machine to trust it | install [mkcert](https://github.com/FiloSottile/mkcert), `mkcert -install` once, then `sh scripts/tls.sh` |
| To use a certificate you already have | `sh scripts/tls.sh install fullchain.pem privkey.pem` |
| A publicly trusted certificate | `sh scripts/tls.sh letsencrypt-dns example.org you@example.org` |

`tls.sh` reloads the running server; no restart, no dropped connection.

With mkcert the certificate is signed by a CA that exists only on this
machine; without it the certificate is self-signed, HTTPS works and browsers
warn. Neither is for the public internet.

### Let's Encrypt

HAProxy does not serve files, so the HTTP-01 webroot challenge is not wired
in this template; the certificate comes by DNS-01:
`sh scripts/tls.sh letsencrypt-dns <domain> <email>`. The script prints one
TXT record (`_acme-challenge.<domain>`), you create it at your DNS provider,
and it carries on by itself once the zone's own name servers answer with it.
Nothing has to reach this host, so it works behind NAT or CGNAT, and
`<domain>` may be a wildcard. The certificate is installed into `./tls` and
loaded with the same process id. Everything certbot keeps goes to
`./letsencrypt`, so no root is involved.

A certificate issued this way is renewed by running the command again (the CA
asks for a new record each time); set `DNS_AUTH_HOOK=<command>` (and
`DNS_CLEANUP_HOOK`) to have a script create the record through your
provider's API, and the script then prints the `certbot renew` line to put in
cron. Try it first with `LETSENCRYPT_STAGING=1`; `<email>` may be `-`, and
`ACME_SERVER=<directory-url>` points the script at another ACME CA. It needs
certbot and one of `dig`, `host` or `nslookup`.

`Strict-Transport-Security` is off on purpose: it pins the host name, every
port of it, and on `localhost` that would force every other local service to
HTTPS. Turn it on in `haproxy.cfg` once this serves a real domain.

To generate the project on other ports or with your own name on the certificate:
`delonix init -t haproxy --port 9080 --tls-port 9443 --hostname shop.test`.

To serve on the standard ports, publish `"80:__PORT__"` and `"443:__TLS_PORT__"` in
`delonix-manifest.yaml` (a rootless host needs `install.sh --low-ports`
first) and drop `:__TLS_PORT__` from the redirect in `haproxy.cfg`. A published
port binds `127.0.0.1`; write `"0.0.0.0:443:__TLS_PORT__"` to accept other machines.

## Sized for load

| Setting | Value | Why |
|---|---|---|
| `maxconn` (global) | 20 000 | concurrent connections for the whole process |
| `nbthread` | 2 | one per CPU the container is allowed; the default counts the host's cores |
| `ulimit nofile` | 65 535 | a proxied connection is two descriptors |
| `http-reuse safe` + keep-alive | on | idle backend connections are shared instead of reopened |
| `retries 2` + `option redispatch` | on | a request is retried on another server when one stops answering |
| `tune.ssl.cachesize` | 40 000 | resumable TLS sessions |

The manifest gives it 2 CPUs, 512 MiB and 65 535 file descriptors. Raise the
numbers together: more CPUs without more workers is idle capacity, more
connections without more descriptors is `Too many open files`.

## Troubleshooting

- **The build fails at `haproxy -c`** — the message names the line.
- **`503 Service Unavailable`** — every `server` in `backend app` is failing
  its health check, or none is declared and the standalone `return` was removed.
- **A host port below 1024** — publishing one needs the host to allow it
  (`net.ipv4.ip_unprivileged_port_start`); `delonix stack apply` says so and
  names the fix. Keep `__PORT__`, or publish a high host port onto it.
- **`ps` shows a root process** — the master, which reads the private key and
  reloads the workers. The worker that handles traffic runs as `haproxy`
  (`user haproxy` in `haproxy.cfg`); `scripts/smoke.sh` checks it.
- **`unable to load certificate from file '…/tls.pem'`** at start — `./tls` is
  empty: run `sh scripts/tls.sh`.
- **The browser warns about the certificate** — expected with a self-signed
  one; see the TLS table for the two ways out.

## Choosing an HAProxy version

The default is `__TEMPLATE_VERSION__`, the line the official image tags as
`lts` when this template was last updated. Regenerate with `-v` to pin another
`haproxy:<version>-alpine` tag, e.g. `delonix init -t haproxy -v 3.2 --force .`
(`--force`: `init` never overwrites without it). `http-after-response` needs
HAProxy 2.2 or newer.

Reference: <https://docs.haproxy.org/>.

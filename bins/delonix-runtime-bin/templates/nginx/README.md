# __NAME__ (nginx __TEMPLATE_VERSION__)

An nginx edge scaffolded by `delonix init -t nginx -v __TEMPLATE_VERSION__`:
serves the static site in `html/` over HTTPS on port `__TLS_PORT__` (port `__PORT__`
redirects there), and is ready to become a
reverse proxy in front of an app container. Infrastructure only — there is no
application code here.

What is already configured, and where (`nginx.conf`):

| Concern | What this template does |
|---|---|
| Health | `GET /healthz` → `200 ok` on both ports, answered by nginx itself, excluded from the access log |
| TLS | HTTPS with HTTP/2 on `__TLS_PORT__`; `__PORT__` redirects; certificate mounted from `./tls` |
| Capacity | 2 workers × 8192 connections, `nofile` 65 535, keep-alive, open-file cache |
| Access log | one JSON object per request on stdout, with `request_id` |
| Correlation | keeps the caller's `X-Request-ID` or mints one; returns it in the response |
| Security headers | `X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, `Content-Security-Policy`; no version in `Server` |
| Limits/timeouts | 10 MiB request body, 15 s header/body read, 30 s send and keep-alive |
| Metrics | `stub_status` at `/nginx_status`, reachable only from inside the container |
| Config check | `nginx -t` runs at build time; a typo fails the build |

## Quickstart

```bash
delonix build -t __NAME__:dev .     # runs `nginx -t` as part of the build
delonix stack apply                 # starts it with restart: always, 512M, 2 CPUs
sh scripts/smoke.sh                 # health, HTTPS, redirect, headers, request id, 404
```

Expected: `smoke: all checks passed`. Or, in one command from an empty
directory: `delonix stack init -t nginx --up __NAME__`, which ends by printing
the address to open.

## Commands

| Task | Command |
|---|---|
| Build (and syntax-check) | `delonix build -t __NAME__:dev .` |
| Run | `delonix stack apply` |
| Smoke test | `sh scripts/smoke.sh [https://127.0.0.1:__TLS_PORT__] [http://127.0.0.1:__PORT__]` |
| New or renewed certificate | `sh scripts/tls.sh` (see TLS below) |
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
| `delonix-manifest.yaml` | how it runs: ports, the `./tls` and `./acme` mounts, restart policy, memory, CPU, descriptors |
| `tls/` | certificate and key — generated, never committed, never in the image |
| `acme/` | Let's Encrypt HTTP-01 webroot |
| `scripts/tls.sh` | local certificate, install one, or Let's Encrypt |
| `scripts/smoke.sh` | the checks above, against a running container |

## Put it in front of an app

Uncomment the `location /api/` block in `nginx.conf` and point `proxy_pass` at
the app. The app has to be reachable from this container: run both on the same
Delonix network (`network:` in both manifests) and use the app's container
name or `networkAlias`. The block already forwards `X-Request-ID` and the
`X-Forwarded-*` headers and sets connect/send/read timeouts.

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
| The same, with no port 80 reaching this host | `sh scripts/tls.sh letsencrypt-dns example.org you@example.org` |

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

Behind NAT or CGNAT, where port 80 of the public address cannot reach this
host, use DNS-01 instead: `sh scripts/tls.sh letsencrypt-dns <domain> <email>`.
The script prints one TXT record (`_acme-challenge.<domain>`), you create it
at your DNS provider, and it carries on by itself once the zone's own name
servers answer with it — nothing has to reach this host, and `<domain>` may be
a wildcard. A certificate issued this way is renewed by running the command
again (the CA asks for a new record each time); set `DNS_AUTH_HOOK=<command>`
(and `DNS_CLEANUP_HOOK`) to have a script create the record through your
provider's API, and renewal then needs nobody. It needs certbot and one of
`dig`, `host` or `nslookup`.

Try it first with `LETSENCRYPT_STAGING=1 sh scripts/tls.sh letsencrypt …`: the
staging CA exercises the whole path without counting against the domain's
rate limits (its certificate is not trusted by browsers). `<email>` may be `-`
to register without one, and `ACME_SERVER=<directory-url>` points the script
at another ACME CA. If the CA answers `Timeout during connect`, port 80 of
the public address is not reaching this host — a router or firewall in front
of it, not this project.

`Strict-Transport-Security` is off on purpose: it pins the host name, every
port of it, and on `localhost` that would force every other local service to
HTTPS. Turn it on in `nginx.conf` once this serves a real domain.

To generate the project on other ports or with your own name on the certificate:
`delonix init -t nginx --port 9080 --tls-port 9443 --hostname shop.test`.

To serve on the standard ports, publish `"80:__PORT__"` and `"443:__TLS_PORT__"` in
`delonix-manifest.yaml` (a rootless host needs `install.sh --low-ports`
first) and drop `:__TLS_PORT__` from the redirect in `nginx.conf`. A published
port binds `127.0.0.1`; write `"0.0.0.0:443:__TLS_PORT__"` to accept other machines.

## Sized for load

| Setting | Value | Why |
|---|---|---|
| `worker_processes` | 2 | one per CPU the container is allowed; `auto` would count the host's cores |
| `worker_connections` | 8192 | per worker: about 16 000 concurrent connections |
| `worker_rlimit_nofile` / `ulimit nofile` | 65 535 | a connection is one descriptor, two when proxying |
| `keepalive_requests` | 1000 | fewer handshakes per client |
| `open_file_cache` | 10 000 entries | no `stat()`/`open()` per static request |
| `ssl_session_cache` | 10 MiB shared | about 40 000 resumable sessions |

The manifest gives it 2 CPUs, 512 MiB and 65 535 file descriptors. Raise the
numbers together: more CPUs without more workers is idle capacity, more
connections without more descriptors is `Too many open files`.

## On the internet without a public IP

`delonix-tunnel.yaml` opens an OUTBOUND tunnel: __NAME__ gets an `https`
address on the internet with no public IP, nothing opened on the router, and
no certificate to manage — the provider serves HTTPS with its own. It runs
only when applied; `stack apply` on `delonix-manifest.yaml` does not touch it.
Anyone on the internet can then reach the service.

The tunnel talks to the HTTPS port (`__TLS_PORT__`), because the plain-HTTP
port redirects to a port the public address does not have. A visitor who
comes in over plain HTTP is redirected to `https://<host>/` by the HTTPS
server, from the scheme the tunnel reports. `insecureSkipTlsVerify: true` in
the file covers only the hop from the tunnel agent to `localhost`: the local
certificate (mkcert or self-signed) cannot be checked there, and visitors get
the provider's certificate.

### Just a tunnel (no account, no domain)

1. Install `cloudflared` ([releases](https://github.com/cloudflare/cloudflared/releases),
   or your distribution's package) somewhere on `PATH`.
2. `delonix stack apply -f delonix-tunnel.yaml`
3. `delonix get gateways` — the address is in `PUBLIC URL`.

The address is random (`https://<words>.trycloudflare.com`) and changes every
time the tunnel opens; Cloudflare offers these quick tunnels for testing,
without an uptime guarantee. Close it with
`delonix delete gateways __NAME__-tunnel`.

### Your own domain (a stable address)

The domain's DNS has to be on Cloudflare (a free account is enough: point the
domain's name servers, at your registrar, to the two Cloudflare gives you).

1. Cloudflare dashboard → Zero Trust → Networks → Tunnels → **Create a
   tunnel** → Cloudflared. Name it and copy the token from the install
   command it shows (`--token eyJ…`).
2. In the tunnel, add a **public hostname**: `app.example.org` → service
   `https://localhost:__TLS_PORT__`. Cloudflare creates the DNS record.
   Under the hostname's additional application settings → TLS, turn on
   **No TLS Verify** (the local certificate is mkcert or self-signed).
3. Keep the token in a Delonix secret, typed in so it lands in no file and
   no shell history: run `delonix secret create __NAME__-tunnel --from-env-file -`,
   type `token=<the token>`, then Enter and Ctrl-D.
4. In `delonix-tunnel.yaml`, uncomment `tokenSecretRef` and `hostname`, then
   `delonix stack apply -f delonix-tunnel.yaml`.

Visitors get Cloudflare's certificate for `app.example.org`. To change the
route later, change it in the dashboard: with a token the tunnel takes its
routes from there, not from this file.

### Other providers

`provider:` also takes:

- `ngrok`: the ngrok agent on `PATH` and an account
  (`ngrok config add-authtoken <token>`). A free account has one fixed name
  (`<words>.ngrok-free.dev`): a stable address without a domain of your own.
  One ngrok tunnel at a time per machine.
- `pinggy` talks plain HTTP to the port, so it does not fit this template, which the tunnel reaches on its HTTPS port.

## Tests and CI

```bash
sh scripts/ci-check.sh        # builds the image, runs it, runs the smoke against it
sh scripts/smoke.sh           # just the smoke, against something already up
```

`scripts/ci-check.sh` is what `.github/workflows/ci.yml` and `.gitlab-ci.yml`
both run, so a green CI and a green local run mean the same thing. It needs
`docker`, `curl` and `openssl`, and it probes the server from OUTSIDE the
container on purpose — the status endpoint is restricted to the local machine,
so a probe from inside answers 200 and the check written as "this must NOT be
public" would fail against a configuration that is exactly right.

`sonar-project.properties` is ready for a `sonar-scanner` run; `CONTRIBUTING.md`
has the branching model, Conventional Commits and SemVer.

## Architecture: where to take this

**A rule before the menu: a configuration no test exercises is a guess.** This
project ships the test — `scripts/smoke.sh`, run in CI by `scripts/ci-check.sh` against the real image — and it is the thing to extend when you take one
of the paths below. An edge config is deployed far more often than it is read;
the smoke is what stands between a typo and a silent outage.

### What this project is today

An edge: TLS termination, HTTP→HTTPS, security headers, request-id
correlation, the ACME challenge path and a static site. It has no application
logic, and that is the architectural decision — everything here is about what
arrives and what leaves, never about what the answer means.

### Path 1 — reverse proxy in front of one application

Uncomment the `upstream`/`proxy_pass` block and point it at a container on the
same Delonix network by name. *Patterns:* Reverse proxy, health-checked
upstream. **Keep the request id**: it is already minted here, and forwarding it
is what makes one request traceable across the edge and the app.

### Path 2 — API gateway for several services

One `location` per service, each with its own upstream and timeouts.
*Patterns:* API Gateway, Bulkhead (separate upstream pools so one slow service
does not exhaust the others). *Watch for:* the gateway becoming the place
routing rules are invented — keep it a map, not a program.

### Path 3 — edge cache / static front for a dynamic app

`proxy_cache` for what is safe to cache, with explicit `Cache-Control` from the
application. *Patterns:* Cache-aside at the edge, stale-while-revalidate.
**Let the application own the policy**; a cache whose rules live only here is a
cache nobody can reason about.
### Choosing

| If your problem is… | Put it… | The sign you chose wrong |
|---|---|---|
| TLS, HTTP/2, compression, redirects | here, at the edge | the application has TLS code in it |
| who may call what, per user | in the application | the proxy config encodes business rules |
| rate limiting a whole site | here | each service re-implements it, differently |
| rate limiting one tenant's quota | in the application | the proxy needs to know about accounts |
| routing to several services | here (gateway) | the browser knows every service's address |
| shaping a response for one screen | in a BFF service behind this | the proxy rewrites JSON |

### What not to do yet

- **Do not put business rules in the proxy config.** It is not tested like code
  and it is deployed on another cadence; the day the rule is wrong, nobody
  looks here.
- **Do not terminate TLS twice.** Terminate once, here, and speak plain HTTP
  (or a separately-trusted mTLS) to the backends.
- **Do not grow one shared config for every service.** One file, many teams, is
  the merge conflict that becomes an outage.
- **Do not skip the smoke after a config change.** `scripts/smoke.sh` is the
  only thing standing between a typo and a silent 500.

## Troubleshooting

- **The build fails at `RUN nginx -t`** — the message names the file and line.
- **`403` on `/nginx_status`** — expected from outside; it only answers `127.0.0.1`.
- **A page renders unstyled** — the Content-Security-Policy allows same-origin
  resources only; move inline `<style>`/`<script>` into files, or relax the
  policy for your site deliberately.
- **Port `__PORT__` or `__TLS_PORT__` already in use** — `delonix stack apply` names the
  process holding it; change the port in both `nginx.conf` and
  `delonix-manifest.yaml` (the redirect in `nginx.conf` names the HTTPS port).
- **The browser warns about the certificate** — expected with a self-signed
  one; see the TLS table for the two ways out.
- **`cannot load certificate "/etc/nginx/tls/tls.crt"`** at start — `./tls`
  is empty: run `sh scripts/tls.sh`.

## Choosing an nginx version

The default is `__TEMPLATE_VERSION__`, the line the official image tags as
`stable` when this template was last updated. Regenerate with `-v` to pin
another `nginx:<version>-alpine` tag, e.g. `delonix init -t nginx -v 1.31
--force .` (`--force`: `init` never overwrites without it). The config needs
nginx 1.25.1 or newer (`http2 on;`).

Reference: <https://nginx.org/en/docs/>.

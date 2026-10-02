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
HTTPS. Turn it on in `conf/delonix.conf` once this serves a real domain.

To generate the project on other ports or with your own name on the certificate:
`delonix init -t httpd --port 9080 --tls-port 9443 --hostname shop.test`.

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

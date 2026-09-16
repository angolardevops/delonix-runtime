# __NAME__ (HAProxy __TEMPLATE_VERSION__)

A production-ready HAProxy L7 proxy/load-balancer, scaffolded by
`delonix init -t haproxy -v __TEMPLATE_VERSION__`: binds `__PORT__`, a
proxy-answered `/healthz`, and an `app` backend ready for real servers. The
config is validated (`haproxy -c`) at build time.

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/healthz
curl localhost:__PORT__/
```

Edit `haproxy.cfg`: uncomment the `server …` lines in `backend app` to load-balance
your real app containers, and remove the standalone `http-request return`.

## Choosing an HAProxy version

Regenerate with `-v <version>` to pin another `haproxy:<version>-alpine`
image, e.g. `delonix init -t haproxy -v 2.9 --force .` (`--force`: `init`
never overwrites without it). Omit `-v` and it falls back to this template's
own default (`__TEMPLATE_VERSION__`, from `template.meta`).

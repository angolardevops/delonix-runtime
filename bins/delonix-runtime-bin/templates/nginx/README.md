# __NAME__ (nginx __TEMPLATE_VERSION__)

A production-ready nginx, scaffolded by
`delonix init -t nginx -v __TEMPLATE_VERSION__`: gzip, security headers, a
`/healthz` endpoint, static serving from `html/`, and a commented
reverse-proxy block. Listens on `__PORT__`.

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/healthz
```

Edit `nginx.conf` to proxy to your app (uncomment the `/api/` block) or drop your
site into `html/`.

## Choosing an nginx version

Regenerate with `-v <version>` to pin another `nginx:<version>-alpine` image,
e.g. `delonix init -t nginx -v 1.26 --force .` (`--force`: `init` never
overwrites without it). Omit `-v` and it falls back to this template's own
default (`__TEMPLATE_VERSION__`, from `template.meta`).

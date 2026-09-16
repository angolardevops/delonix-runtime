# __NAME__ (Apache httpd __TEMPLATE_VERSION__)

A production-ready Apache httpd, scaffolded by
`delonix init -t httpd -v __TEMPLATE_VERSION__`: listens on `__PORT__`,
version banner off, a `/healthz` file, static serving from `public/`.

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/healthz
```

Drop your site into `public/`, or add a `VirtualHost`/`mod_proxy` config to
proxy to your app.

## Choosing an httpd version

Regenerate with `-v <version>` to pin another `httpd:<version>-alpine` image,
e.g. `delonix init -t httpd -v 2.4.62 --force .` (`--force`: `init` never
overwrites without it). Omit `-v` and it falls back to this template's own
default (`__TEMPLATE_VERSION__`, from `template.meta`).

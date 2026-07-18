# __NAME__ (Apache httpd)

A production-ready Apache httpd, scaffolded by `delonix init --template httpd`:
listens on `__PORT__`, version banner off, a `/healthz` file, static serving from
`public/`.

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/healthz
```

Drop your site into `public/`, or add a `VirtualHost`/`mod_proxy` config to
proxy to your app.

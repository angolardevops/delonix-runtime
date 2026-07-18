# __NAME__ (nginx)

A production-ready nginx, scaffolded by `delonix init --template nginx`:
gzip, security headers, a `/healthz` endpoint, static serving from `html/`, and
a commented reverse-proxy block. Listens on `__PORT__`.

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/healthz
```

Edit `nginx.conf` to proxy to your app (uncomment the `/api/` block) or drop your
site into `html/`.

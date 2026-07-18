# __NAME__ (HAProxy)

A production-ready HAProxy L7 proxy/load-balancer, scaffolded by
`delonix init --template haproxy`: binds `__PORT__`, a proxy-answered `/healthz`,
and an `app` backend ready for real servers. The config is validated (`haproxy -c`)
at build time.

```bash
delonix build -t __NAME__:dev .
delonix stack apply
curl localhost:__PORT__/healthz
curl localhost:__PORT__/
```

Edit `haproxy.cfg`: uncomment the `server …` lines in `backend app` to load-balance
your real app containers, and remove the standalone `http-request return`.

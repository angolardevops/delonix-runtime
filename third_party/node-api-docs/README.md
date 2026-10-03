# Embedded UI assets of the node API docs (ADR-0042 D3)

`GET /docs` (Swagger UI) and `GET /redoc` (ReDoc) on the `delonix-node-api` socket are served
from these files, compiled into the binary. A node may be offline, and a page that drives the
engine's API must not load a script from a CDN: that is a supply-chain hole (ADR-0042 D3).

The files live under `third_party/` because they are third-party code (the language ratchet and other scans skip it). They are **byte-identical to the upstream npm packages** — nothing is edited, so anyone can
check them against the registry. `crates/interfaces/delonix-node-api/build.rs` verifies every file against `SHA256SUMS` and the
build fails on any difference.

| Directory | Package | Version | License | Files taken |
|---|---|---|---|---|
| `swagger-ui/` | [`swagger-ui-dist`](https://www.npmjs.com/package/swagger-ui-dist) | 5.33.1 | Apache-2.0 | `swagger-ui-bundle.js`, `swagger-ui.css`, `swagger-ui-bundle.js.LICENSE.txt`, `LICENSE`, `NOTICE` |
| `redoc/` | [`redoc`](https://www.npmjs.com/package/redoc) | 2.5.4 | MIT | `bundles/redoc.standalone.js`, `bundles/redoc.standalone.js.LICENSE.txt`, `LICENSE` |

Provenance (fetched 2026-10-02; the tarball's `sha512` matched the registry's `dist.integrity`):

- `https://registry.npmjs.org/swagger-ui-dist/-/swagger-ui-dist-5.33.1.tgz`
- `https://registry.npmjs.org/redoc/-/redoc-2.5.4.tgz`

## Updating

1. Download the new tarball from the registry and check its `sha512` against
   `https://registry.npmjs.org/<package>/<version>` → `dist.integrity`. Do not take the files from
   a CDN mirror.
2. Copy the files listed above, unchanged, over these.
3. `sha256sum swagger-ui/* redoc/* > SHA256SUMS` in this directory.
4. Update the table above, and check the pages in a browser through a local proxy to the socket
   (the Content-Security-Policy the server sends blocks anything that is not served from it).

# 6. The crates

## Reference table

<!-- dev-docs:begin crates-table -->
| Crate | Layer | Path | Binaries | Depends on (engine crates) | Used by |
|---|---|---|---|---|---|
| `delonix-model` | Foundation | `crates/foundation/delonix-model` | — | `delonix-runtime-core` | `delonix-runtime-bin` |
| `delonix-net-rules` | Foundation | `crates/foundation/delonix-net-rules` | — | — | `delonix-net`, `delonix-vm` |
| `delonix-runtime-core` | Foundation | `crates/foundation/delonix-runtime-core` | — | — | `delonix-compute`, `delonix-cri`, `delonix-image`, `delonix-mcp`, `delonix-mcp-bin`, `delonix-mgmt`, `delonix-mgmt-bin`, `delonix-model`, `delonix-net`, `delonix-proxmox`, `delonix-runtime`, `delonix-runtime-bin`, `delonix-scan`, `delonix-security-runtime`, `delonix-stack`, `delonix-truenas`, `delonix-vm`, `delonix-volume` |
| `delonix-compute` | Contexts | `crates/contexts/delonix-compute` | — | `delonix-runtime-core` | `delonix-cri`, `delonix-image`, `delonix-net`, `delonix-runtime`, `delonix-runtime-bin`, `delonix-vm`, `delonix-volume` |
| `delonix-security-runtime` | Contexts | `crates/contexts/delonix-security-runtime` | — | `delonix-runtime-core` | `delonix-runtime-bin` |
| `delonix-stack` | Contexts | `crates/contexts/delonix-stack` | — | `delonix-runtime-core` | `delonix-runtime-bin` |
| `delonix-image` | Adapters | `crates/adapters/delonix-image` | — | `delonix-compute`, `delonix-runtime-core` | `delonix-cri`, `delonix-mgmt`, `delonix-runtime-bin`, `delonix-scan` |
| `delonix-net` | Adapters | `crates/adapters/delonix-net` | — | `delonix-compute`, `delonix-net-rules`, `delonix-runtime-core` | `delonix-cri`, `delonix-mcp`, `delonix-mgmt`, `delonix-runtime-bin` |
| `delonix-runtime` | Adapters | `crates/adapters/delonix-runtime` | — | `delonix-compute`, `delonix-runtime-core` | `delonix-cri`, `delonix-mcp`, `delonix-mgmt`, `delonix-runtime-bin` |
| `delonix-scan` | Adapters | `crates/adapters/delonix-scan` | — | `delonix-image`, `delonix-runtime-core` | `delonix-mgmt`, `delonix-runtime-bin` |
| `delonix-telemetry` | Adapters | `crates/adapters/delonix-telemetry` | — | — | `delonix-cri`, `delonix-mcp-bin`, `delonix-mgmt`, `delonix-mgmt-bin`, `delonix-runtime-bin` |
| `delonix-vm` | Adapters | `crates/adapters/delonix-vm` | — | `delonix-compute`, `delonix-net-rules`, `delonix-runtime-core` | `delonix-mcp`, `delonix-mgmt`, `delonix-proxmox`, `delonix-runtime-bin` |
| `delonix-volume` | Adapters | `crates/adapters/delonix-volume` | — | `delonix-compute`, `delonix-runtime-core` | `delonix-mcp`, `delonix-mgmt`, `delonix-runtime-bin` |
| `delonix-proxmox` | Providers | `crates/providers/delonix-proxmox` | — | `delonix-runtime-core`, `delonix-vm` | `delonix-runtime-bin` |
| `delonix-truenas` | Providers | `crates/providers/delonix-truenas` | — | `delonix-runtime-core` | `delonix-runtime-bin` |
| `delonix-cri` | Interfaces | `crates/interfaces/delonix-cri` | `delonix-cri` | `delonix-compute`, `delonix-image`, `delonix-net`, `delonix-runtime`, `delonix-runtime-core`, `delonix-telemetry` | — |
| `delonix-mcp` | Interfaces | `crates/interfaces/delonix-mcp` | — | `delonix-mgmt`, `delonix-net`, `delonix-runtime`, `delonix-runtime-core`, `delonix-vm`, `delonix-volume` | `delonix-mcp-bin` |
| `delonix-mgmt` | Interfaces | `crates/interfaces/delonix-mgmt` | — | `delonix-image`, `delonix-net`, `delonix-runtime`, `delonix-runtime-core`, `delonix-scan`, `delonix-telemetry`, `delonix-vm`, `delonix-volume` | `delonix-mcp`, `delonix-mgmt-bin`, `delonix-runtime-bin` |
| `delonix-mcp-bin` | Binaries | `bins/delonix-mcp-bin` | `delonix-mcp` | `delonix-mcp`, `delonix-runtime-core`, `delonix-telemetry` | — |
| `delonix-mgmt-bin` | Binaries | `bins/delonix-mgmt-bin` | `delonix-mgmt` | `delonix-mgmt`, `delonix-runtime-core`, `delonix-telemetry` | — |
| `delonix-runtime-bin` | Binaries | `bins/delonix-runtime-bin` | `delonix` | `delonix-compute`, `delonix-image`, `delonix-mgmt`, `delonix-model`, `delonix-net`, `delonix-proxmox`, `delonix-runtime`, `delonix-runtime-core`, `delonix-scan`, `delonix-security-runtime`, `delonix-stack`, `delonix-telemetry`, `delonix-truenas`, `delonix-vm`, `delonix-volume` | — |
<!-- dev-docs:end crates-table -->

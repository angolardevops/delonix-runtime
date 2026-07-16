# Delonix Runtime

A daemonless, rootless-first, kernel-native container and microVM runtime, written in
Rust. Delonix Runtime creates and manages Linux containers directly via
namespaces/cgroups/nftables (no supervising daemon), plus declarative microVMs on
Cloud Hypervisor or libvirt/KVM — in the same spirit as Docker/Podman/containerd, with
rootless operation as a first-class design goal rather than an afterthought.

## Crates

| Crate | Responsibility |
|---|---|
| `delonix-runtime-core` | Shared domain types: `Container`, `Vm`, `Status` (6-state lifecycle), `Store`/`JsonStore` (persistence), typestate, virtualization detection, and the Secret Manager (`Secret`/`SecretStore`/`CredVault`) used for `--secret`/`--secret-files`. |
| `delonix-runtime` / `delonix-runtime-bin` | The container runtime itself (clone/namespaces/cgroups, create/stop/exec, status reconciliation) plus a lean standalone binary (no API/orchestrator) for node/CRI contexts. |
| `delonix-net` | Rootless SDN: a single holder network namespace + bridge + `slirp4netns`, ingress/DNAT/firewall via nftables, CNI-plugin compatibility, and inter-node overlay encryption (WireGuard over VXLAN). |
| `delonix-image` | OCI images: pull/registry/build (Dockerfile), Cloud Native Buildpacks, an internal content-addressed registry, and image signature verification. |
| `delonix-vm` | Declarative microVMs via a pluggable `VmBackend` trait — Cloud Hypervisor or libvirt/KVM. |
| `delonix-volume` | Named volumes and bind mounts, Docker-compatible `-v` syntax. |
| `delonix-cri` | A Kubernetes CRI (`runtime.v1`) server, so Delonix Runtime can act as a node's container runtime under `kubelet`. |

## Status

This project was extracted from a larger platform's monorepo, where these crates already
implement a working rootless runtime. It has not yet been packaged as a standalone CLI —
that's the next milestone. Expect API churn.

## License

Apache-2.0. See [LICENSE](LICENSE).

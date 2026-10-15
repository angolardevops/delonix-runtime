//! The reference CNI plugins the golden image carries, pinned by content.
//!
//! **The gap this closes.** Until now the only thing that put anything in
//! `/opt/cni/bin` was the `kubernetes-cni` `.deb`, pulled in as a `kubelet`
//! dependency. Nothing in the build named it, nothing asserted which plugins
//! it contains, and — measured in `download_k8s_debs` — its version is
//! deliberately NOT pinned (`VERSIONED` covers only
//! `kubeadm`/`kubelet`/`kubectl`), because `kubernetes-cni` versions on its own
//! track. So the set of plugin binaries on a node, and their versions, were
//! whatever `pkgs.k8s.io` happened to publish on build day. The only in-repo
//! evidence that `portmap` lands at all is an ADR's measurement and an
//! `scripts/e2e.sh` check that SKIPS when the file is absent.
//!
//! This module replaces that with an explicit, versioned, digest-verified
//! install from `containernetworking/plugins` — the upstream reference
//! implementation — fetched and verified on the HOST and copied into the image,
//! so the offline build stays offline.
//!
//! **Why a named subset and not the whole tarball.** The brief this answers is
//! explicit, and it is also the right engineering: a binary being present is
//! not a reason for it to be in a chain. The tarball ships 18 plugins; a
//! kubeadm node running one of this engine's profiles execs ten of them at
//! most. `dhcp` (a daemon), `static`, `vlan`, `vrf`, `ptp`, `sbr`, `tap` and
//! `dummy` are not reachable from any profile's conflist, so they are not
//! installed — unreachable code on a machine handed to someone else is attack
//! surface, which is the same reasoning `k8s_node_debloat_steps` already
//! applies to the distro's own packages.
//!
//! **Who owns which file.** A CNI add-on installs its OWN plugin (Cilium's
//! `cilium-cni`, Calico's `calico`/`calico-ipam`) into the same directory, at
//! install time, from its agent pod. The two must not fight: this module
//! installs only the names below, which no add-on ships, and it overwrites
//! nothing it did not put there. The add-on's own binaries are its business
//! and this build never touches them.

use std::path::{Path, PathBuf};

use delonix_model::{Error, Result};

use super::vmimage::{hex_sha256_file, stream_download, CustomizeOp};

/// The pinned upstream release. Moved deliberately, in its own commit, with
/// the digests below re-fetched — never floated to "latest".
pub(crate) const PLUGINS_VERSION: &str = "1.9.1";

/// SHA-256 of each architecture's release tarball.
///
/// Obtained from the publisher's own `.sha256` sidecar and re-verified against
/// a local `sha256sum` of the downloaded file on 2026-10-15. Both halves
/// matter: the sidecar says what upstream published, the local hash says what
/// actually arrived.
const PLUGINS_SHA256: &[(&str, &str)] = &[
    (
        "amd64",
        "b98f74a0f8522f0a83867178729c1aa70f2158f90c45a2ca8fa791db1c76b303",
    ),
    (
        "arm64",
        "56171987d3947707c3563db2f4001bccaf50fd63468611b9f3cbecb1375ee7ec",
    ),
];

/// The plugins installed into `/opt/cni/bin`, and what each one is FOR.
///
/// Every name here is reachable from at least one profile's chain (see
/// `net_profiles::Profile::required_plugins`) or from the engine's own
/// single-node conflist. The test below proves that claim rather than trusting
/// this comment.
pub(crate) const REFERENCE_PLUGINS: &[(&str, &str)] = &[
    // Gives a pod `127.0.0.1`. Every profile needs it; a pod without it breaks
    // in ways that look like application bugs.
    ("loopback", "brings `lo` up inside the sandbox"),
    // The engine's own single-node conflist and Flannel's are literally
    // bridge + host-local + portmap.
    (
        "bridge",
        "the node-local bridge of the `default` and flannel chains",
    ),
    ("host-local", "the IPAM those chains use"),
    // ADR-0074: this is what publishes a `hostPort`. Without it the CRI
    // refuses a hostPort by name rather than attaching and staying quiet.
    ("portmap", "publishes hostPort (ADR-0074)"),
    // Calico's own manifest declares the `bandwidth` capability.
    ("bandwidth", "traffic shaping, chained by Calico"),
    // Chained by several add-ons to set per-interface sysctls.
    ("tuning", "per-interface sysctls in a chain"),
    ("firewall", "the admit-from-host rules a chain may add"),
    // The Multus profile's secondary networks: these are the attachment types
    // supported in the first delivery (see the ADR), and without them a
    // `NetworkAttachmentDefinition` fails at the first ADD.
    ("macvlan", "a Multus secondary network on a parent NIC"),
    ("ipvlan", "a Multus secondary network, L3 flavour"),
    (
        "host-device",
        "a Multus secondary network moving a whole NIC in",
    ),
];

/// Where a verified tarball is cached, next to the other build downloads.
fn cache_path(store_dir: &Path, arch: &str) -> PathBuf {
    store_dir.join(format!(
        "_cni-plugins/cni-plugins-linux-{arch}-v{PLUGINS_VERSION}.tgz"
    ))
}

fn asset_url(arch: &str) -> String {
    format!(
        "https://github.com/containernetworking/plugins/releases/download/\
         v{PLUGINS_VERSION}/cni-plugins-linux-{arch}-v{PLUGINS_VERSION}.tgz"
    )
}

/// The digest this release pins for `arch`, or an error NAMING the
/// architectures it does know.
///
/// An unknown architecture is refused rather than downloaded unverified: an
/// arch with no pinned digest is an arch whose artefact nobody checked, and
/// "we could not verify it so we installed it" is the failure this whole
/// module exists to remove.
pub(crate) fn pinned_sha256(arch: &str) -> Result<&'static str> {
    PLUGINS_SHA256
        .iter()
        .find(|(a, _)| *a == arch)
        .map(|(_, s)| *s)
        .ok_or_else(|| {
            Error::Invalid(super::po::tf(
                "no pinned CNI-plugins digest for architecture '{arch}' — this release pins \
                 {known}. An architecture whose artefact digest is not pinned would have to be \
                 installed unverified, which this build does not do",
                &[
                    ("arch", arch),
                    (
                        "known",
                        &PLUGINS_SHA256
                            .iter()
                            .map(|(a, _)| *a)
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                ],
            ))
        })
}

/// Downloads (once) and verifies the reference plugin tarball on the HOST.
///
/// Host-side on purpose, like every other download this build does: it is what
/// lets `virt-customize` run with `--no-network`, so the appliance never needs
/// DHCP or DNS. Fails CLOSED on a digest mismatch, and deletes the file — the
/// same non-negotiable as `download_k8s_debs` and `fetch_node_exporter`.
pub(crate) fn fetch_reference_plugins(store_dir: &Path, arch: &str) -> Result<PathBuf> {
    let want = pinned_sha256(arch)?;
    let dest = cache_path(store_dir, arch);
    if dest.exists() {
        if let Ok(got) = hex_sha256_file(&dest) {
            if got == want {
                return Ok(dest);
            }
        }
        // A cached file that does not match is a bad download, not a decision.
        let _ = std::fs::remove_file(&dest);
    }
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d)?;
    }
    eprintln!(
        "{}",
        super::po::tf(
            "downloading the reference CNI plugins v{version} ({arch})...",
            &[("version", PLUGINS_VERSION), ("arch", arch)]
        )
    );
    let tmp = dest.with_extension("part");
    stream_download(&asset_url(arch), &tmp)?;
    let got = hex_sha256_file(&tmp)?;
    if got != want {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Invalid(super::po::tf(
            "invalid checksum for the CNI plugins v{version} ({arch}): expected {want}, got \
             {got} — download discarded, nothing was installed",
            &[
                ("version", PLUGINS_VERSION),
                ("arch", arch),
                ("want", want),
                ("got", &got),
            ],
        )));
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// The `virt-customize` steps that put the reference plugins in the image.
///
/// Pure (returns the ops, runs nothing), so the exact command line is
/// unit-tested without building an image — the thing that went wrong
/// historically on this build was always the ASSEMBLED command, not a function
/// in isolation.
///
/// Extraction is by NAME, one `tar` invocation listing exactly the members
/// wanted — a tarball that grows a new member must not silently start
/// shipping it.
///
/// **And then the directory is PRUNED to that same set.** Extraction alone was
/// not enough, and only building the image showed it: `kubernetes-cni` arrives
/// as a `kubelet` dependency and owns 21 paths under `/opt/cni/bin` — all 18
/// upstream plugins plus `LICENSE` and `README.md`. So the first version of
/// this function extracted its ten verified binaries over the deb's copies and
/// left the other eight (`dhcp`, `dummy`, `ptp`, `sbr`, `static`, `tap`,
/// `vlan`, `vrf`) sitting there, while this module's own header and ADR-0078
/// both claimed they do not ship. The claim was false on the built artefact,
/// and no unit test could see it: the tests assert the CONTENTS of
/// `REFERENCE_PLUGINS`, and the extra files come from a `.deb` the test never
/// runs. Measured on a real golden (`virt-ls /opt/cni/bin` → 20 entries).
///
/// The prune makes the delivered image match the documented claim, which is
/// the direction this repo resolves that kind of mismatch in. The cost, said
/// plainly: `dpkg` still believes `kubernetes-cni` owns the removed files, so
/// `dpkg --verify` reports them missing and a reinstall of that package would
/// bring them back. That is the same trade `k8s_node_debloat_steps` already
/// makes when it purges packages out of the image, and it is preferable to
/// shipping binaries nothing can reach on a machine handed to a tenant.
pub(crate) fn install_ops(tarball: &Path) -> Vec<CustomizeOp> {
    let names: Vec<&str> = REFERENCE_PLUGINS.iter().map(|(n, _)| *n).collect();
    let members = names
        .iter()
        .map(|n| format!("./{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    // The keep-list for the prune, as a space-delimited haystack so the `case`
    // match below cannot partially match a name (`" bridge "` never matches
    // `host-bridge`).
    let keep = format!(" {} ", names.join(" "));
    let name = tarball
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    vec![
        CustomizeOp::RunCommand("mkdir -p /opt/cni/bin /tmp/cni-plugins".into()),
        CustomizeOp::CopyIn(tarball.to_path_buf(), "/tmp/cni-plugins".to_string()),
        CustomizeOp::RunCommand(format!(
            "tar -xzf /tmp/cni-plugins/{name} -C /opt/cni/bin {members} && \
             rm -rf /tmp/cni-plugins"
        )),
        // Prune, then chmod what remains. `for f in *` with a `case` test is
        // POSIX sh and needs no `find`; the quotes keep a name with a space
        // (none today) from splitting.
        CustomizeOp::RunCommand(format!(
            "cd /opt/cni/bin && for f in *; do \
             case \"{keep}\" in *\" $f \"*) ;; *) rm -rf \"$f\" ;; esac; done && \
             chmod 0755 /opt/cni/bin/*"
        )),
    ]
}

/// A line for the image's own `/etc/delonix-image-release`, so a node can say
/// which plugin set it carries without anyone having to mount the qcow2.
pub(crate) fn provenance(arch: &str) -> String {
    let digest = pinned_sha256(arch).unwrap_or("unknown");
    format!("v{PLUGINS_VERSION} ({arch}, sha256:{})", &digest[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::net_profiles::Profile;

    /// A digest that is not 64 hex characters is not a digest, and a pin that
    /// does not cover the architecture the build runs on is a download nobody
    /// checked.
    #[test]
    fn every_pinned_architecture_has_a_real_digest() {
        assert!(!PLUGINS_SHA256.is_empty());
        for (arch, sha) in PLUGINS_SHA256 {
            assert_eq!(sha.len(), 64, "{arch}");
            assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()), "{arch}");
        }
        // amd64 is the architecture the golden image is built for (ADR-0018).
        assert!(pinned_sha256("amd64").is_ok());
        // And an unknown one is refused by name, listing what IS pinned.
        let e = pinned_sha256("riscv64").unwrap_err();
        assert!(e.to_string().contains("riscv64"), "{e}");
        assert!(e.to_string().contains("amd64"), "{e}");
    }

    /// The URL carries the pinned version, and nothing on this path says
    /// `latest`.
    #[test]
    fn the_asset_url_is_pinned() {
        let u = asset_url("amd64");
        assert!(u.contains(&format!("v{PLUGINS_VERSION}")), "{u}");
        assert!(!u.contains("latest"), "{u}");
        assert!(u.starts_with("https://"), "{u}");
    }

    /// The claim in this module's header, as a test: every plugin a profile
    /// says it needs is a plugin the image installs. A profile that needs a
    /// binary the golden does not carry is a cluster that fails at the first
    /// pod, with `NetworkReady` blaming a missing plugin.
    #[test]
    fn the_installed_set_covers_every_profile_that_needs_one() {
        let installed: Vec<&str> = REFERENCE_PLUGINS.iter().map(|(n, _)| *n).collect();
        for p in [
            Profile::Default,
            Profile::Cilium,
            Profile::Calico,
            Profile::Flannel,
        ] {
            for need in p.required_plugins() {
                assert!(
                    installed.contains(need),
                    "{} needs `{need}` and the golden image does not install it",
                    p.name()
                );
            }
        }
        // And the other direction: nothing is installed "just in case". These
        // are the names deliberately NOT shipped, and a tarball member
        // becoming reachable has to be a decision, not a default.
        for unwanted in [
            "dhcp", "static", "vlan", "vrf", "ptp", "sbr", "tap", "dummy",
        ] {
            assert!(
                !installed.contains(&unwanted),
                "`{unwanted}` is not reachable from any profile's chain and should not ship"
            );
        }
        // Every entry says what it is for — an undocumented binary on a
        // tenant's node is one nobody can justify later.
        for (n, why) in REFERENCE_PLUGINS {
            assert!(!why.is_empty(), "{n} has no stated purpose");
        }
    }

    /// The assembled command is what historically went wrong on this build, so
    /// it is the thing asserted: extraction by NAME, into the standard
    /// directory, with the temp directory created before the `--copy-in` that
    /// needs it to exist.
    #[test]
    fn the_install_steps_extract_only_the_named_plugins() {
        let ops = install_ops(Path::new("/cache/cni-plugins-linux-amd64-v1.9.1.tgz"));
        let cmds: Vec<String> = ops
            .iter()
            .map(|o| match o {
                CustomizeOp::RunCommand(c) => c.clone(),
                CustomizeOp::CopyIn(src, dst) => format!("COPYIN {} {dst}", src.display()),
                other => format!("{other:?}"),
            })
            .collect();
        // `--copy-in` requires the destination directory to already exist.
        assert!(
            cmds[0].contains("mkdir -p /opt/cni/bin /tmp/cni-plugins"),
            "{cmds:?}"
        );
        assert!(
            cmds[1].starts_with("COPYIN /cache/cni-plugins-linux-amd64-v1.9.1.tgz"),
            "{cmds:?}"
        );
        let tar = &cmds[2];
        assert!(tar.contains("-C /opt/cni/bin"), "{tar}");
        for (n, _) in REFERENCE_PLUGINS {
            assert!(tar.contains(&format!("./{n}")), "{n} not extracted: {tar}");
        }
        // Not a blanket extract — the member list is what bounds it.
        assert!(!tar.contains("-xzf /tmp/cni-plugins/*"), "{tar}");
        for unwanted in ["./dhcp", "./vlan", "./vrf"] {
            assert!(!tar.contains(unwanted), "{unwanted} extracted: {tar}");
        }
        // The tarball does not stay in the image.
        assert!(tar.contains("rm -rf /tmp/cni-plugins"), "{tar}");

        // And the PRUNE, which is what makes the claim true on the built
        // artefact: `kubernetes-cni` puts all 18 plugins there as a kubelet
        // dependency, so extracting ten over them leaves eight behind.
        let prune = &cmds[3];
        assert!(prune.contains("cd /opt/cni/bin"), "{prune}");
        assert!(prune.contains("rm -rf \"$f\""), "{prune}");
        assert!(prune.contains("chmod 0755"), "{prune}");
        // Every kept name is in the keep-list, space-delimited on both sides
        // so no name can partially match another.
        for (n, _) in REFERENCE_PLUGINS {
            assert!(prune.contains(&format!(" {n} ")), "{n} not kept: {prune}");
        }
        // And nothing outside the set is named as kept.
        for unwanted in ["dhcp", "vlan", "vrf", "LICENSE", "README.md"] {
            assert!(
                !prune.contains(&format!(" {unwanted} ")),
                "{unwanted} is in the keep-list: {prune}"
            );
        }
    }

    /// The prune is shell that runs in a `virt-customize` chroot, where a
    /// syntax error is a failed build with a confusing message. Run the real
    /// thing against a directory that mimics what the image actually has:
    /// the deb's 18 plugins + LICENSE + README.md.
    #[test]
    fn the_prune_keeps_exactly_the_reference_set() {
        let tmp = match tempfile::tempdir() {
            Ok(t) => t,
            Err(e) => panic!("tempdir: {e}"),
        };
        let bin = tmp.path().join("opt/cni/bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        // What `kubernetes-cni` really ships, measured off a built golden.
        let shipped = [
            "bandwidth", "bridge", "dhcp", "dummy", "firewall", "host-device", "host-local",
            "ipvlan", "LICENSE", "loopback", "macvlan", "portmap", "ptp", "README.md", "sbr",
            "static", "tap", "tuning", "vlan", "vrf",
        ];
        for f in shipped {
            std::fs::write(bin.join(f), b"x").expect("write");
        }
        // The prune command, with the absolute path swapped for the fixture.
        let ops = install_ops(Path::new("/cache/x.tgz"));
        let prune = match &ops[3] {
            CustomizeOp::RunCommand(c) => c.replace("/opt/cni/bin", &bin.to_string_lossy()),
            other => panic!("expected a RunCommand, got {other:?}"),
        };
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&prune)
            .output()
            .expect("sh");
        assert!(
            out.status.success(),
            "the prune failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut left: Vec<String> = std::fs::read_dir(&bin)
            .expect("readdir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut want: Vec<String> = REFERENCE_PLUGINS
            .iter()
            .map(|(n, _)| (*n).to_string())
            .collect();
        want.sort();
        assert_eq!(left, want, "the directory is not exactly the reference set");
    }

    /// Provenance has to name the version AND enough of the digest to tell two
    /// builds apart; a version alone is the pin that floated.
    #[test]
    fn the_provenance_line_names_the_version_and_the_digest() {
        let p = provenance("amd64");
        assert!(p.contains(PLUGINS_VERSION), "{p}");
        assert!(p.contains("sha256:b98f74a0f8522f0a"), "{p}");
        assert!(p.contains("amd64"), "{p}");
    }
}

//! `VMfile` — a declarative recipe for building a bootable qcow2 VM image.
//!
//! Same shape as a `Dockerfile`/`Delonixfile` on purpose, because the shape is
//! the whole value: anyone who has written one can read this. What it is NOT is
//! the same *mechanism* — a container image is layers of a filesystem, a VM
//! image is a disk that has to boot, so the verbs differ where the substrate
//! differs:
//!
//! * `FROM` names a **cloud image**, not an OCI image — a distro's published
//!   qcow2 (`ubuntu:24.04`), an absolute URL, or a VM image already in the local
//!   store. Downloaded and checksum-verified, never taken on trust.
//! * There is no `CMD`/`ENTRYPOINT`: a VM boots an init, it does not run a
//!   process. `CLOUDINIT` is the equivalent — it is how a cloud image is told
//!   what to be.
//! * `SIZE` exists because a disk has one and a layer does not.
//!
//! **Multi-stage means something narrower here, and saying so matters.** In a
//! container build, a stage is a throwaway filesystem you copy artefacts out
//! of. Here a stage is a whole bootable disk, so `COPY --from=<stage>` extracts
//! files from that disk (`virt-copy-out`) and injects them into the next. That
//! covers the case people actually want — compile in a fat image, ship a thin
//! one — and nothing else. A stage is not a cache layer and does not become an
//! image unless it is the last one.

use std::path::PathBuf;

use delonix_runtime_core::{Error, Result};

/// One build step of a stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// `RUN <shell>` — executed inside the guest, offline.
    Run(String),
    /// `COPY <src> <dst>` — from the build context into the guest.
    Copy { src: String, dst: String },
    /// `COPY --from=<stage> <src> <dst>` — out of a previously built stage.
    CopyFrom {
        stage: String,
        src: String,
        dst: String,
    },
    /// `ENV KEY=value` — appended to `/etc/environment`, so it survives a
    /// reboot and applies to every login. A container's `ENV` lives in the
    /// image config, which a VM has no equivalent of.
    Env { key: String, value: String },
    /// `USER <name>[:<group>]` — creates the account (with a home and a shell).
    User(String),
    /// `PASSWORD <user>:<password>`.
    Password { user: String, password: String },
    /// `ROOTPASSWORD <password>`.
    RootPassword(String),
    /// `CLOUDINIT <path>` — user-data baked in as the image's default.
    CloudInit(String),
    /// `SSHKEY <user> <path-or-literal>` — authorized_keys for that user.
    SshKey { user: String, key: String },
}

/// A stage: one base image plus the steps applied to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stage {
    /// `AS <name>`, when given.
    pub(crate) name: Option<String>,
    /// The `FROM` reference, unresolved.
    pub(crate) from: String,
    pub(crate) steps: Vec<Step>,
    /// `SIZE <n>G` — grow the disk before anything else runs. Growing after a
    /// `RUN` that filled it is too late, which is why it is a stage property
    /// and not a step.
    pub(crate) size: Option<String>,
    /// `HOSTNAME <name>`.
    pub(crate) hostname: Option<String>,
}

/// A parsed `VMfile`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct VmFile {
    /// Every stage in order; the LAST one produces the image.
    pub(crate) stages: Vec<Stage>,
    /// `VCPUS`/`MEMORY` — defaults recorded in the image's metadata so
    /// `vm create` from it needs no flags. They describe the image's intent,
    /// not a hard limit.
    pub(crate) vcpus: Option<u32>,
    pub(crate) memory: Option<String>,
    /// `HYPERVISOR <cloud-hypervisor|libvirt>` — which backend `vm create`
    /// should prefer for images built from this recipe, canonicalized at
    /// parse time (`delonix_vm::valid_backend_name`). Same spirit as
    /// `vcpus`/`memory`: a recorded intent, not a hard requirement — an
    /// explicit `--backend`/`DELONIX_VM_BACKEND`/persisted default at `vm
    /// create` time still wins (see `delonix_vm::create_with`'s precedence).
    pub(crate) backend: Option<String>,
    /// `LABEL k=v`.
    pub(crate) labels: Vec<(String, String)>,
}

impl VmFile {
    /// The stage that produces the image.
    pub(crate) fn final_stage(&self) -> &Stage {
        self.stages.last().expect("parse guarantees >= 1 stage")
    }
}

/// Splits a line into instruction and argument, honouring `\` continuations
/// and `#` comments. Returns the logical lines.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut acc = String::new();
    let mut start = 0usize;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        // A comment is only a comment on its OWN line. Stripping from a `#`
        // anywhere would mutilate `RUN sed -i 's/#foo/bar/' x`, which is
        // ordinary shell and not a comment at all.
        if acc.is_empty() && (line.is_empty() || line.starts_with('#')) {
            continue;
        }
        if acc.is_empty() {
            start = i + 1;
        }
        if let Some(cont) = line.strip_suffix('\\') {
            acc.push_str(cont.trim_end());
            acc.push(' ');
            continue;
        }
        acc.push_str(line);
        out.push((start, std::mem::take(&mut acc)));
    }
    if !acc.is_empty() {
        out.push((start, acc));
    }
    out
}

fn err(line: usize, msg: &str) -> Error {
    Error::Invalid(format!("VMfile:{line}: {msg}"))
}

/// Parses a `VMfile`.
///
/// **Fail-closed on anything it does not understand.** An unknown instruction
/// is an error, not a skipped line: a recipe half-applied produces an image
/// that boots and is missing something, which is discovered much later and far
/// from here. Same rule the seccomp profile parser follows.
pub(crate) fn parse(text: &str) -> Result<VmFile> {
    let mut vf = VmFile::default();
    let mut cur: Option<Stage> = None;

    for (ln, line) in logical_lines(text) {
        let (kw, rest) = match line.split_once(char::is_whitespace) {
            Some((k, r)) => (k.to_ascii_uppercase(), r.trim().to_string()),
            None => (line.to_ascii_uppercase(), String::new()),
        };
        if kw != "FROM" && cur.is_none() {
            return Err(err(ln, &format!("{kw} before any FROM")));
        }
        match kw.as_str() {
            "FROM" => {
                if rest.is_empty() {
                    return Err(err(ln, "FROM needs an image (see `delonix vm init`)"));
                }
                if let Some(s) = cur.take() {
                    vf.stages.push(s);
                }
                let (from, name) = match rest.split_once(" AS ") {
                    Some((f, n)) => (f.trim().to_string(), Some(n.trim().to_string())),
                    None => match rest.to_ascii_uppercase().split_once(" AS ") {
                        // `as` in lower case is ordinary in a Dockerfile and
                        // refusing it would be pedantry.
                        Some(_) => {
                            let idx = rest.to_ascii_uppercase().find(" AS ").unwrap_or(0);
                            (
                                rest[..idx].trim().to_string(),
                                Some(rest[idx + 4..].trim().to_string()),
                            )
                        }
                        None => (rest.clone(), None),
                    },
                };
                cur = Some(Stage {
                    name,
                    from,
                    steps: Vec::new(),
                    size: None,
                    hostname: None,
                });
            }
            "RUN" => cur.as_mut().unwrap().steps.push(Step::Run(rest)),
            "COPY" => {
                let step = parse_copy(&rest).map_err(|m| err(ln, &m))?;
                if let Step::CopyFrom { stage, .. } = &step {
                    let known = vf
                        .stages
                        .iter()
                        .any(|s| s.name.as_deref() == Some(stage.as_str()));
                    if !known {
                        // Naming a stage that does not exist YET (or at all) is
                        // a typo that would otherwise surface as an empty copy.
                        return Err(err(ln, &format!("no earlier stage named '{stage}'")));
                    }
                }
                cur.as_mut().unwrap().steps.push(step);
            }
            "ENV" => {
                let (k, v) = rest
                    .split_once('=')
                    .ok_or_else(|| err(ln, "ENV needs KEY=value"))?;
                cur.as_mut().unwrap().steps.push(Step::Env {
                    key: k.trim().to_string(),
                    value: v.trim().to_string(),
                });
            }
            "USER" => cur.as_mut().unwrap().steps.push(Step::User(rest)),
            "PASSWORD" => {
                let (u, p) = rest
                    .split_once(':')
                    .ok_or_else(|| err(ln, "PASSWORD needs <user>:<password>"))?;
                cur.as_mut().unwrap().steps.push(Step::Password {
                    user: u.trim().to_string(),
                    password: p.trim().to_string(),
                });
            }
            "ROOTPASSWORD" => cur.as_mut().unwrap().steps.push(Step::RootPassword(rest)),
            "CLOUDINIT" => cur.as_mut().unwrap().steps.push(Step::CloudInit(rest)),
            "SSHKEY" => {
                let (u, k) = rest
                    .split_once(char::is_whitespace)
                    .ok_or_else(|| err(ln, "SSHKEY needs <user> <path-or-key>"))?;
                cur.as_mut().unwrap().steps.push(Step::SshKey {
                    user: u.trim().to_string(),
                    key: k.trim().to_string(),
                });
            }
            "SIZE" => cur.as_mut().unwrap().size = Some(rest),
            "HOSTNAME" => cur.as_mut().unwrap().hostname = Some(rest),
            "VCPUS" => {
                vf.vcpus = Some(
                    rest.parse()
                        .map_err(|_| err(ln, "VCPUS needs a whole number"))?,
                )
            }
            "MEMORY" => vf.memory = Some(rest),
            "HYPERVISOR" => {
                vf.backend = Some(
                    delonix_vm::valid_backend_name(&rest)
                        .map_err(|e| err(ln, &e.to_string()))?
                        .to_string(),
                )
            }
            "LABEL" => {
                let (k, v) = rest
                    .split_once('=')
                    .ok_or_else(|| err(ln, "LABEL needs key=value"))?;
                vf.labels.push((k.trim().to_string(), v.trim().to_string()));
            }
            other => {
                return Err(err(
                    ln,
                    &format!(
                        "unknown instruction '{other}' — supported: FROM RUN COPY ENV USER \
                         PASSWORD ROOTPASSWORD CLOUDINIT SSHKEY SIZE HOSTNAME VCPUS MEMORY \
                         HYPERVISOR LABEL"
                    ),
                ))
            }
        }
    }
    if let Some(s) = cur.take() {
        vf.stages.push(s);
    }
    if vf.stages.is_empty() {
        return Err(Error::Invalid(
            "VMfile has no FROM — nothing to build".to_string(),
        ));
    }
    Ok(vf)
}

fn parse_copy(rest: &str) -> std::result::Result<Step, String> {
    let (from, tail) = match rest.strip_prefix("--from=") {
        Some(t) => {
            let (stage, tail) = t
                .split_once(char::is_whitespace)
                .ok_or_else(|| "COPY --from=<stage> needs <src> <dst>".to_string())?;
            (Some(stage.to_string()), tail.trim())
        }
        None => (None, rest),
    };
    let parts: Vec<&str> = tail.split_whitespace().collect();
    if parts.len() != 2 {
        return Err("COPY needs exactly <src> <dst>".to_string());
    }
    Ok(match from {
        Some(stage) => Step::CopyFrom {
            stage,
            src: parts[0].to_string(),
            dst: parts[1].to_string(),
        },
        None => Step::Copy {
            src: parts[0].to_string(),
            dst: parts[1].to_string(),
        },
    })
}

/// How a `FROM` reference should be obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Base {
    /// A distro's published cloud image (`ubuntu:24.04`, `debian:bookworm`,
    /// `rocky:9`) — downloaded and checksum-verified by the existing
    /// per-distro code, which already knows each publisher's quirks (three
    /// different checksum formats between them).
    Distro { distro: String, release: String },
    /// An absolute URL to a qcow2. Verified against `<url>.sha256` when the
    /// publisher offers one; otherwise downloaded over TLS with that stated.
    Url(String),
    /// A VM image already in the local store.
    Local(String),
}

/// Classifies a `FROM`. **Pure**, so every branch is testable without a network.
pub(crate) fn classify_base(from: &str) -> Base {
    if from.starts_with("http://") || from.starts_with("https://") {
        return Base::Url(from.to_string());
    }
    if let Some((d, r)) = from.split_once(':') {
        // Only the distros whose cloud-image layout the builder actually knows.
        // Anything else with a colon is a local tag (`my-image:1.0`), which is
        // the far more common shape — treating it as an unknown distro would
        // turn a working reference into an error.
        if matches!(d, "ubuntu" | "debian" | "rocky") {
            return Base::Distro {
                distro: d.to_string(),
                release: r.to_string(),
            };
        }
    }
    Base::Local(from.to_string())
}

/// The `VMfile` a `vm init` writes.
///
/// A scaffold is documentation people actually read, so this one is a working
/// recipe rather than a list of placeholders: it builds a real, bootable image
/// as written, and every line it contains is one the reader can delete.
pub(crate) fn scaffold(name: &str) -> String {
    format!(
        r#"# VMfile — a bootable qcow2, built from a distro's cloud image.
#
#   delonix vm build -t {name}:1.0 .
#   delonix vm create dev --disk-image {name}:1.0
#
# Builds as written. Delete what you do not need.

# The base. Three forms:
#   ubuntu:24.04 | debian:bookworm | rocky:9   published cloud image (verified)
#   https://…/whatever.qcow2                   any absolute URL
#   my-other-image:1.0                         one already in `delonix vm ls`
FROM ubuntu:24.04

# The disk a cloud image ships with is small (a couple of GB). Grow it BEFORE
# anything runs — growing after a RUN that filled it is too late.
SIZE 20G

HOSTNAME {name}

# Runs inside the guest. Offline by default — everything a RUN needs comes from
# the base image or from a COPY — because a build that reaches the internet
# gives a different image depending on when it ran.
#
# These two build as written, with no network:
RUN echo "built by delonix vm build" > /etc/motd
RUN systemctl enable ssh

# To install packages the guest needs the network, and you ask for it:
#
#   delonix vm build --network -t {name}:1.0 .
#
# with the RUN you actually want, for example:
#
#   RUN apt-get update && apt-get install -y --no-install-recommends nginx
#   RUN systemctl enable nginx

# From the build context into the guest.
# COPY ./site /var/www/html

ENV APP_ENV=production

# An account you can log in as. Give it a key rather than a password when you
# can — a password baked into an image is in every copy of that image.
USER {name}
SSHKEY {name} ~/.ssh/id_ed25519.pub
# PASSWORD {name}:change-me

# Baked-in cloud-init user-data: what the image does on FIRST boot of each
# instance. `vm create` still generates its own per-instance seed (hostname,
# keys) — this is the default underneath it.
# CLOUDINIT ./cloud-init/user-data.yaml

# Defaults recorded in the image, so `vm create` needs no flags.
VCPUS 2
MEMORY 2G
# HYPERVISOR cloud-hypervisor   # or `libvirt` — omit to let `vm create` decide
LABEL org.opencontainers.image.title={name}

# ---------------------------------------------------------------------------
# Multi-stage: build fat, ship thin.
#
# A stage here is a whole bootable disk, not a filesystem layer, so
# `COPY --from=` extracts files out of it. Uncomment to try:
#
# FROM ubuntu:24.04 AS builder
# RUN apt-get update && apt-get install -y build-essential
# COPY ./src /src
# RUN make -C /src
#
# FROM ubuntu:24.04
# COPY --from=builder /src/myapp /usr/local/bin/myapp
"#
    )
}

/// The cloud-init `user-data` a `vm init` writes alongside the `VMfile`.
pub(crate) fn scaffold_cloud_init(name: &str) -> String {
    format!(
        r#"#cloud-config
# Runs on the FIRST boot of each instance created from this image.
#
# Baked into the image by `CLOUDINIT ./cloud-init/user-data.yaml`. `vm create`
# generates its own seed on top (hostname, ssh keys) — this is the default it
# sits on.

hostname: {name}
package_update: false

runcmd:
  - [ systemctl, enable, --now, nginx ]

final_message: "{name} up after $UPTIME seconds"
"#
    )
}

/// Where `vm init` writes the scaffold.
pub(crate) fn init_dir(dir: Option<PathBuf>) -> PathBuf {
    dir.unwrap_or_else(|| PathBuf::from("."))
}

// ===========================================================================
// The builder
// ===========================================================================

use super::vmimage::{CustomizeOp, Distro, VmImage, VmImageStore};

/// Builds the image described by `path`.
///
/// Each stage is a full disk: the base is flattened into a working qcow2,
/// grown if asked, customized offline with `virt-customize`, and kept for
/// later stages to copy out of. Only the last one is stored.
///
/// `virt-customize` runs `--no-network` unless `network` is set. Offline is the
/// default deliberately: a build that reaches the internet from inside the
/// guest is a build whose result depends on when it ran, and it is the reason
/// the offline golden path avoids the DHCP/passt host workarounds entirely.
///
/// It is opt-in rather than absent because the most common thing a `VMfile`
/// wants to do is install a package, and an engine that makes that impossible
/// is not offering a choice — it is just refusing. The reproducibility cost is
/// the caller's to take, once they are told about it.
pub(crate) fn build(
    store: &VmImageStore,
    path: &std::path::Path,
    context: &std::path::Path,
    tag: &str,
    compress: bool,
    network: bool,
) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    let vf = parse(&text)?;

    let work_dir = std::env::temp_dir().join(format!("delonix-vmfile-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir)?;
    // Every stage's disk, by name, so `COPY --from=` can reach it. Unnamed
    // stages are not addressable and so are not kept — matching the Dockerfile
    // rule, and avoiding a directory that grows with every anonymous stage.
    let mut built: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    let total = vf.stages.len();
    let mut last: Option<PathBuf> = None;
    // The FINAL stage's base, when it is a local image — what the result
    // inherits its distro and kernel from. `None` for a URL or a `--from=`
    // stage, and that absence is the honest answer rather than a guess.
    let base_meta = store.get(&vf.final_stage().from).ok();

    for (i, stage) in vf.stages.iter().enumerate() {
        let label = stage
            .name
            .clone()
            .unwrap_or_else(|| format!("stage-{}", i + 1));
        eprintln!(
            "{}",
            super::po::tf(
                "[{n}/{total}] {label}: FROM {from}",
                &[
                    ("n", &(i + 1).to_string()),
                    ("total", &total.to_string()),
                    ("label", &label),
                    ("from", &stage.from),
                ],
            )
        );
        let base = resolve_base(store, &stage.from, &built)?;
        let disk = work_dir.join(format!("{label}.qcow2"));
        // Flatten: no backing file in the artifact, so the result does not
        // depend on a base still being on this machine later.
        super::vmimage::run_tool(
            "qemu-img",
            &[
                "convert",
                "-O",
                "qcow2",
                &base.to_string_lossy(),
                &disk.to_string_lossy(),
            ],
        )?;
        if let Some(size) = &stage.size {
            // BEFORE the customization, always: growing after a `RUN` that
            // filled the disk is too late, and that is why `SIZE` is a stage
            // property rather than a step you can misplace.
            super::vmimage::run_tool("qemu-img", &["resize", &disk.to_string_lossy(), size])?;
        }

        let ops = stage_ops(stage, context, &built, &work_dir, &label)?;
        if !ops.is_empty() {
            let mut args = super::vmimage::customize_args(&disk, &ops);
            if !network {
                args.push("--no-network".into());
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            super::vmimage::run_tool("virt-customize", &argv)?;
        }
        if let Some(n) = &stage.name {
            built.insert(n.clone(), disk.clone());
        }
        last = Some(disk);
    }

    let final_disk = last.expect("parse guarantees >= 1 stage");
    let out = finalize(&work_dir, &final_disk, compress)?;
    let data = std::fs::read(&out)?;
    let digest = format!("sha256:{}", super::vmimage::hex_sha256(&data));
    let size = data.len() as u64;
    std::fs::rename(&out, store.qcow2_path(tag))
        .or_else(|_| std::fs::copy(&out, store.qcow2_path(tag)).map(|_| ()))?;
    let _ = std::fs::remove_dir_all(&work_dir);

    let img = VmImage {
        name: tag.to_string(),
        tag: tag.to_string(),
        digest,
        size,
        // The recipe's own base, not a distro release this builder chose —
        // recording `ubuntu 24.04` for an image built `FROM https://…` would
        // be a guess presented as a fact.
        // Inherited from a local base, and the recipe's own `FROM` otherwise.
        //
        // Half-inheriting was measurably worse than not inheriting: taking the
        // distro from the base while leaving this as the FROM ref made
        // `image vm ls` print `debian/delonix-vm-base:debian-bookworm` in the
        // DISTRO column — a distro glued to an image name. Either both come
        // from the base (which knows them, so it is not a guess) or neither
        // does.
        ubuntu_release: base_meta
            .as_ref()
            .and_then(|b| b.ubuntu_release.clone())
            .or_else(|| Some(vf.final_stage().from.clone())),
        k8s_version: None,
        created_unix: super::vmimage::now_unix(),
        // INHERITED from the base when the `FROM` is a local image, and left
        // unknown otherwise.
        //
        // A build customizes a rootfs; it does not swap the distro or the
        // kernel. Recording `None` for both meant `image vm ls` — the command
        // whose whole job is to say what an image IS — showed `-` under KERNEL
        // and DISTRO for every image this engine built, while showing them for
        // the very base it was built from. Measured: building `FROM
        // delonix-vm-base:debian-bookworm` (kernel `6.1.0-52-cloud-amd64`,
        // distro `debian/bookworm`) produced a row with both columns empty.
        //
        // Inheriting only from a LOCAL base is the honest half: a `FROM
        // https://…` gives nothing to inherit, and guessing a distro from a URL
        // would be a guess presented as a fact — the same reason
        // `ubuntu_release` records the recipe's own `FROM` rather than a
        // release this builder picked.
        kernel_version: base_meta.as_ref().and_then(|b| b.kernel_version.clone()),
        distro: base_meta.as_ref().and_then(|b| b.distro.clone()),
        // `VCPUS`/`MEMORY`/`HYPERVISOR` from the VMfile, if given — `vm
        // create` applies them when the disk resolves to this image AND the
        // caller did not already say otherwise (CLI flag, env var, or
        // persisted default). Previously parsed but never wired anywhere —
        // fixed here, the same "dead code with no caller yet" class of gap
        // this repo's own audits keep finding (see CLAUDE.md).
        default_vcpus: vf.vcpus,
        default_memory: vf.memory.clone(),
        default_backend: vf.backend.clone(),
        // A VMfile build starts `FROM` a cloud image and is customized by
        // cloud-init on first boot — that is the whole premise of the format.
        cloud_init: Some(true),
    };
    store.save(&img)?;
    println!("{tag}");
    Ok(())
}

/// Gets the qcow2 for a `FROM`, downloading it when needed.
fn resolve_base(
    store: &VmImageStore,
    from: &str,
    built: &std::collections::HashMap<String, PathBuf>,
) -> Result<PathBuf> {
    // A previous stage by name wins over everything: it is the one reference
    // that cannot mean anything else.
    if let Some(p) = built.get(from) {
        return Ok(p.clone());
    }
    match classify_base(from) {
        Base::Distro { distro, release } => {
            let d = match distro.as_str() {
                "ubuntu" => Distro::Ubuntu,
                "debian" => Distro::Debian,
                _ => Distro::Rocky,
            };
            super::vmimage::download_base(store, d, &release)
        }
        Base::Url(url) => super::vmimage::download_url_base(store, &url),
        Base::Local(name) => {
            let p = store.qcow2_path(&name);
            if p.exists() {
                return Ok(p);
            }
            Err(Error::Invalid(super::po::tf(
                "FROM {name}: no such local VM image, and it is not a URL nor a known cloud image (ubuntu:/debian:/rocky:) — see `delonix vm ls`",
                &[("name", &name)],
            )))
        }
    }
}

/// Translates a stage's steps into `virt-customize` operations.
fn stage_ops(
    stage: &Stage,
    context: &std::path::Path,
    built: &std::collections::HashMap<String, PathBuf>,
    work_dir: &std::path::Path,
    label: &str,
) -> Result<Vec<CustomizeOp>> {
    let mut ops = Vec::new();
    if let Some(h) = &stage.hostname {
        ops.push(CustomizeOp::RunCommand(format!("echo {h} > /etc/hostname")));
    }
    for (i, step) in stage.steps.iter().enumerate() {
        match step {
            Step::Run(cmd) => ops.push(CustomizeOp::RunCommand(cmd.clone())),
            Step::Copy { src, dst } => {
                // Confined to the context, like the container build's `COPY`.
                // Without this a recipe could read `../../.ssh` off the machine
                // doing the build — the same traversal this repo already closed
                // for `delonix build`.
                let real = super::build::safe_join(context, src)?;
                ops.push(CustomizeOp::CopyIn(real, dst.clone()));
            }
            Step::CopyFrom {
                stage: from,
                src,
                dst,
            } => {
                let disk = built.get(from).ok_or_else(|| {
                    Error::Invalid(format!("COPY --from={from}: stage was not built"))
                })?;
                // Out of that disk, then into this one. There is no way to copy
                // between two qcow2s directly, and going through the host is
                // what makes the artefact independent of the stage afterwards.
                let staging = work_dir.join(format!("{label}-copy-{i}"));
                std::fs::create_dir_all(&staging)?;
                super::vmimage::run_tool(
                    "virt-copy-out",
                    &[
                        "-a",
                        &disk.to_string_lossy(),
                        src,
                        &staging.to_string_lossy(),
                    ],
                )?;
                let base = std::path::Path::new(src)
                    .file_name()
                    .ok_or_else(|| Error::Invalid(format!("COPY --from: bad source '{src}'")))?;
                ops.push(CustomizeOp::CopyIn(staging.join(base), dst.clone()));
            }
            Step::Env { key, value } => ops.push(CustomizeOp::RunCommand(format!(
                "printf '%s=%s\\n' {key} {value} >> /etc/environment"
            ))),
            Step::User(name) => ops.push(CustomizeOp::RunCommand(format!(
                "id -u {name} >/dev/null 2>&1 || useradd -m -s /bin/bash {name}"
            ))),
            Step::Password { user, password } => ops.push(CustomizeOp::Password {
                user: user.clone(),
                password: password.clone(),
            }),
            Step::RootPassword(p) => ops.push(CustomizeOp::RootPassword(p.clone())),
            Step::CloudInit(p) => {
                let real = super::build::safe_join(context, p)?;
                // `/etc/cloud/cloud.cfg.d` and not the NoCloud seed: the seed is
                // per-INSTANCE and `vm create` writes its own. This is the
                // image's default, underneath it.
                ops.push(CustomizeOp::CopyIn(
                    real,
                    "/etc/cloud/cloud.cfg.d".to_string(),
                ));
            }
            Step::SshKey { user, key } => {
                let expanded = expand_tilde(key);
                let material = if std::path::Path::new(&expanded).exists() {
                    std::fs::read_to_string(&expanded)
                        .map_err(|e| Error::Invalid(format!("SSHKEY {expanded}: {e}")))?
                } else {
                    key.clone()
                };
                let material = material.trim().to_string();
                if !material.starts_with("ssh-") && !material.starts_with("ecdsa-") {
                    return Err(Error::Invalid(super::po::tf(
                        "SSHKEY {key}: not a public key, and no such file — pass a path or the key itself",
                        &[("key", key)],
                    )));
                }
                ops.push(CustomizeOp::RunCommand(format!(
                    "install -d -m 700 -o {user} -g {user} /home/{user}/.ssh && \
                     printf '%s\\n' '{material}' >> /home/{user}/.ssh/authorized_keys && \
                     chown {user}:{user} /home/{user}/.ssh/authorized_keys && \
                     chmod 600 /home/{user}/.ssh/authorized_keys"
                )));
            }
        }
    }
    Ok(ops)
}

/// `~` expansion for a path typed by a human. Only a leading `~/`; `~user` is
/// deliberately not supported (it needs passwd lookups for a case nobody hits).
fn expand_tilde(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(h) => format!("{}/{rest}", h.to_string_lossy()),
            None => p.to_string(),
        },
        None => p.to_string(),
    }
}

/// Sparsify + compress, exactly as the golden build does — the reason is the
/// same and it is not obvious: this image becomes the read-only backing file
/// of every VM made from it, so decompression speed is a runtime property, not
/// a build one. zstd over zlib for that.
fn finalize(work_dir: &std::path::Path, disk: &std::path::Path, compress: bool) -> Result<PathBuf> {
    if !compress {
        return Ok(disk.to_path_buf());
    }
    eprintln!(
        "{}",
        super::po::t("compacting the image (sparsify + zstd compression)...")
    );
    if let Err(e) =
        super::vmimage::run_tool("virt-sparsify", &["--in-place", &disk.to_string_lossy()])
    {
        eprintln!(
            "{} {}",
            super::po::t("warning:"),
            super::po::tf(
                "virt-sparsify failed ({err}); compressing anyway",
                &[("err", &e.to_string())]
            )
        );
    }
    let out = work_dir.join("final.qcow2");
    super::vmimage::run_tool(
        "qemu-img",
        &[
            "convert",
            "-c",
            "-O",
            "qcow2",
            "-o",
            "compression_type=zstd",
            &disk.to_string_lossy(),
            &out.to_string_lossy(),
        ],
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parseia_o_scaffold_que_escrevemos() {
        // O scaffold é documentação que as pessoas correm — se não parseia,
        // o primeiro contacto de alguém com a funcionalidade é um erro.
        let vf = parse(&scaffold("demo")).expect("o scaffold tem de parsear");
        assert_eq!(vf.stages.len(), 1);
        assert_eq!(vf.final_stage().from, "ubuntu:24.04");
        assert_eq!(vf.final_stage().size.as_deref(), Some("20G"));
        assert_eq!(vf.vcpus, Some(2));
        assert_eq!(vf.memory.as_deref(), Some("2G"));
    }

    #[test]
    fn multi_stage_com_copy_from() {
        let vf = parse(
            "FROM ubuntu:24.04 AS builder\n\
             RUN make\n\
             FROM debian:bookworm\n\
             COPY --from=builder /src/app /usr/local/bin/app\n",
        )
        .unwrap();
        assert_eq!(vf.stages.len(), 2);
        assert_eq!(vf.stages[0].name.as_deref(), Some("builder"));
        assert_eq!(
            vf.final_stage().steps[0],
            Step::CopyFrom {
                stage: "builder".into(),
                src: "/src/app".into(),
                dst: "/usr/local/bin/app".into()
            }
        );
    }

    #[test]
    fn copy_from_de_um_estagio_inexistente_e_erro() {
        // Sem isto, o engano aparece como uma cópia vazia numa imagem que
        // arranca na mesma — descoberto muito depois e longe daqui.
        let e = parse("FROM ubuntu:24.04\nCOPY --from=nada /a /b\n").unwrap_err();
        assert!(
            e.to_string().contains("no earlier stage named 'nada'"),
            "{e}"
        );
    }

    #[test]
    fn hypervisor_normaliza_alias_e_recusa_desconhecido() {
        let vf = parse("FROM ubuntu:24.04\nHYPERVISOR ch\n").unwrap();
        assert_eq!(vf.backend.as_deref(), Some("cloud-hypervisor"));

        let vf = parse("FROM ubuntu:24.04\nHYPERVISOR KVM\n").unwrap();
        assert_eq!(vf.backend.as_deref(), Some("libvirt"));

        let e = parse("FROM ubuntu:24.04\nHYPERVISOR hyperv\n").unwrap_err();
        assert!(e.to_string().contains("unknown VM backend"), "{e}");
    }

    #[test]
    fn instrucao_desconhecida_falha_fechado() {
        let e = parse("FROM ubuntu:24.04\nENTRYPOINT /bin/sh\n").unwrap_err();
        assert!(e.to_string().contains("unknown instruction"), "{e}");
        // E diz quais são as válidas, em vez de mandar ler a documentação.
        assert!(e.to_string().contains("CLOUDINIT"), "{e}");
    }

    #[test]
    fn instrucao_antes_do_from_e_erro() {
        let e = parse("RUN echo oi\nFROM ubuntu:24.04\n").unwrap_err();
        assert!(e.to_string().contains("before any FROM"), "{e}");
    }

    #[test]
    fn continuacao_de_linha_e_comentarios() {
        let vf = parse(
            "# comentário\n\
             FROM ubuntu:24.04\n\
             \n\
             RUN apt-get install -y \\\n\
                 nginx \\\n\
                 curl\n",
        )
        .unwrap();
        assert_eq!(
            vf.final_stage().steps[0],
            Step::Run("apt-get install -y nginx curl".into())
        );
    }

    #[test]
    fn cardinal_dentro_de_um_run_nao_e_comentario() {
        // `sed -i 's/#foo/bar/'` é shell corrente. Cortar no primeiro `#`
        // mutilava o comando em silêncio.
        let vf =
            parse("FROM ubuntu:24.04\nRUN sed -i 's/#PermitRoot/PermitRoot/' /etc/ssh\n").unwrap();
        assert_eq!(
            vf.final_stage().steps[0],
            Step::Run("sed -i 's/#PermitRoot/PermitRoot/' /etc/ssh".into())
        );
    }

    #[test]
    fn classify_base_distingue_as_tres_formas() {
        assert_eq!(
            classify_base("ubuntu:24.04"),
            Base::Distro {
                distro: "ubuntu".into(),
                release: "24.04".into()
            }
        );
        assert_eq!(
            classify_base("https://x/y.qcow2"),
            Base::Url("https://x/y.qcow2".into())
        );
        // Uma tag local COM dois pontos não é uma distro desconhecida — é a
        // forma mais comum de todas, e tratá-la como distro transformaria uma
        // referência boa num erro.
        assert_eq!(
            classify_base("my-image:1.0"),
            Base::Local("my-image:1.0".into())
        );
        assert_eq!(classify_base("golden"), Base::Local("golden".into()));
    }

    #[test]
    fn as_em_minusculas_tambem_nomeia_o_estagio() {
        let vf = parse("FROM ubuntu:24.04 as builder\nRUN true\nFROM debian:bookworm\n").unwrap();
        assert_eq!(vf.stages[0].name.as_deref(), Some("builder"));
    }

    #[test]
    fn env_password_e_label_exigem_a_forma_certa() {
        assert!(parse("FROM ubuntu:24.04\nENV SEM_IGUAL\n").is_err());
        assert!(parse("FROM ubuntu:24.04\nPASSWORD semdoispontos\n").is_err());
        assert!(parse("FROM ubuntu:24.04\nLABEL semigual\n").is_err());
        assert!(parse("FROM ubuntu:24.04\nVCPUS muitos\n").is_err());
    }
}

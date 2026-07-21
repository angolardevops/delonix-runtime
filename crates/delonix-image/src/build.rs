//! Image building: a minimal `Dockerfile` (`FROM`/`RUN`/`CMD`) and
//! capturing the *diff* of a build as a new content-addressed *layer*.

use crate::cas::strip;
use crate::image::{now_unix, Image, ImageConfig, ImageStore};
use delonix_runtime_core::{Error, Result};
use std::path::PathBuf;

/// A build step, in order (order matters: `COPY` before the `RUN` that uses it).
#[derive(Debug, Clone)]
pub enum Step {
    /// `RUN <cmd>` — executes a command (with the accumulated `ENV`/`WORKDIR`).
    Run(String),
    /// `COPY [--from=<stage>] <src> <dst>` — copies from the build *context* (or from
    /// a previous stage, if `from` is present — multi-stage build).
    Copy {
        src: String,
        dst: String,
        from: Option<String>,
    },
    /// `ENV K=V` (or `ENV K V`) — sets a variable (affects the following `RUN`s).
    Env { key: String, val: String },
    /// `WORKDIR <dir>` — working directory of the following `RUN`s.
    Workdir(String),
}

/// An intermediate stage of a multi-stage build (`FROM x AS name`).
#[derive(Debug, Clone)]
pub struct Stage {
    /// Stage name (`AS <name>`), or `None`.
    pub name: Option<String>,
    /// The stage's base image (`FROM`).
    pub from: String,
    /// The stage's steps.
    pub steps: Vec<Step>,
}

/// A parsed `Dockerfile` — Docker-compatible, with Delonix extensions.
/// The fields `from`/`steps`/`cmd`/… describe the **final stage** (the resulting
/// image); `stages` holds the intermediate stages (multi-stage build).
#[derive(Debug, Default)]
pub struct Dockerfile {
    /// Intermediate stages (all but the last), in order.
    pub stages: Vec<Stage>,
    /// The base image of the final stage (`FROM`).
    pub from: String,
    /// The steps of the final stage, in order.
    pub steps: Vec<Step>,
    /// The default command (`CMD`).
    pub cmd: Vec<String>,
    /// The entry point (`ENTRYPOINT`).
    pub entrypoint: Vec<String>,
    /// The accumulated `ENV` (for the image config).
    pub env: Vec<String>,
    /// The final `WORKDIR` (for the image config).
    pub workdir: Option<String>,
    // --- Delonix extensions (which the Dockerfile does NOT have) ---
    /// `SCAN fail-on=<sev>` — vulnerability gate before the build.
    pub scan_fail_on: Option<String>,
    /// `CPUS <n>` — CPU limit embedded in the image (mandatory in Delonix).
    pub cpus: Option<String>,
    /// `MEMORY <n>` — memory limit embedded in the image.
    pub memory: Option<String>,
    /// `SECURITY <option>...` — default security posture (e.g. `userns`).
    pub security: Vec<String>,
    /// `HEALTHCHECK ... CMD <cmd>` — health command (the part after `CMD`).
    pub healthcheck: Option<String>,
}

/// Parses a Dockerfile. **Docker-compatible** (FROM/RUN/CMD/ENTRYPOINT/
/// ENV/WORKDIR/COPY; LABEL/EXPOSE/USER/ARG/ADD/MAINTAINER/VOLUME accepted and
/// ignored) **plus Delonix extensions** (SCAN/CPUS/MEMORY/SECURITY).
/// Joins physical lines ending in `\` into a single logical line (continuations,
/// like Docker) — returns `(0-based index of the 1st physical line, logical line)`.
/// A continuation line is concatenated with a space in place of the `\<newline>`.
fn join_continuations(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0usize;
    for (i, raw) in text.lines().enumerate() {
        if cur.is_empty() {
            start = i;
        }
        let t = raw.trim_end();
        if let Some(head) = t.strip_suffix('\\') {
            cur.push_str(head);
            cur.push(' ');
        } else {
            cur.push_str(t);
            out.push((start, std::mem::take(&mut cur)));
        }
    }
    if !cur.is_empty() {
        out.push((start, cur)); // trailing `\` with no line after — do not lose the content
    }
    out
}

pub fn parse_dockerfile(text: &str) -> Result<Dockerfile> {
    let mut df = Dockerfile::default();
    let mut stages: Vec<Stage> = Vec::new(); // all stages, in order
    for (n, line) in join_continuations(text) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (instr, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let instr_up = instr.to_ascii_uppercase();
        // The steps go to the current STAGE (the last of `stages`); FROM opens a new one.
        if instr_up != "FROM" && stages.is_empty() {
            // allow extensions/metadata before the 1st FROM to be ignored? No:
            // any step before FROM is an error (like in Docker).
            return Err(Error::Invalid(format!(
                "Dockerfile line {}: `{instr}` before any FROM",
                n + 1
            )));
        }
        match instr_up.as_str() {
            "FROM" => {
                // `FROM <img> [AS <name>]`
                let mut it = rest.split_whitespace();
                let from = it.next().unwrap_or("").to_string();
                let name = match (it.next(), it.next()) {
                    (Some(kw), Some(nm)) if kw.eq_ignore_ascii_case("as") => Some(nm.to_string()),
                    _ => None,
                };
                stages.push(Stage {
                    name,
                    from,
                    steps: Vec::new(),
                });
            }
            "RUN" => stages
                .last_mut()
                .unwrap()
                .steps
                .push(Step::Run(rest.to_string())),
            "CMD" => df.cmd = parse_cmd(rest),
            "ENTRYPOINT" => df.entrypoint = parse_cmd(rest),
            "ENV" => {
                // `ENV k1=v1 k2="v 2" …` (multiple vars) OR the legacy `ENV k v`.
                for (key, val) in parse_env_pairs(rest) {
                    stages
                        .last_mut()
                        .unwrap()
                        .steps
                        .push(Step::Env { key, val });
                }
            }
            "WORKDIR" => {
                stages
                    .last_mut()
                    .unwrap()
                    .steps
                    .push(Step::Workdir(rest.to_string()));
            }
            "COPY" | "ADD" => {
                // `COPY [--from=<stage>] <src> <dst>`
                let mut from_stage: Option<String> = None;
                let parts: Vec<&str> = rest
                    .split_whitespace()
                    .filter(|t| {
                        if let Some(f) = t.strip_prefix("--from=") {
                            from_stage = Some(f.to_string());
                            false
                        } else {
                            true
                        }
                    })
                    .collect();
                if parts.len() < 2 {
                    return Err(Error::Invalid(format!(
                        "line {}: {instr} requires src and dst",
                        n + 1
                    )));
                }
                stages.last_mut().unwrap().steps.push(Step::Copy {
                    src: parts[0].to_string(),
                    dst: parts[parts.len() - 1].to_string(),
                    from: from_stage,
                });
            }
            // --- Delonix extensions (apply to the final image) ---
            "SCAN" => {
                df.scan_fail_on = rest
                    .split_whitespace()
                    .find_map(|t| t.strip_prefix("fail-on=").map(|s| s.to_string()))
                    .or(Some("high".to_string()));
            }
            "CPUS" => df.cpus = Some(rest.to_string()),
            "MEMORY" => df.memory = Some(rest.to_string()),
            "SECURITY" => df.security = rest.split_whitespace().map(|s| s.to_string()).collect(),
            // HEALTHCHECK [options] CMD <cmd> | HEALTHCHECK NONE (A17).
            "HEALTHCHECK" => {
                let r = rest.trim();
                if r.eq_ignore_ascii_case("NONE") {
                    df.healthcheck = None;
                } else if let Some(idx) = r.find("CMD ").or_else(|| r.find("cmd ")) {
                    df.healthcheck = Some(r[idx + 4..].trim().to_string());
                }
            }
            // compatibility: accepted but with no build effect (metadata)
            "LABEL" | "EXPOSE" | "USER" | "ARG" | "MAINTAINER" | "VOLUME" | "STOPSIGNAL"
            | "SHELL" | "ONBUILD" => {}
            other => {
                return Err(Error::Invalid(format!(
                    "Dockerfile line {}: unknown instruction `{other}`",
                    n + 1
                )))
            }
        }
    }
    // The last stage is the final one (the resulting image); the earlier ones are intermediate.
    let last = stages
        .pop()
        .ok_or_else(|| Error::Invalid("Dockerfile has no FROM instruction".into()))?;
    df.from = last.from;
    df.steps = last.steps;
    df.stages = stages;
    // ENV/WORKDIR of the final stage → image config.
    for s in &df.steps {
        match s {
            Step::Env { key, val } => df.env.push(format!("{key}={val}")),
            Step::Workdir(d) => df.workdir = Some(d.clone()),
            _ => {}
        }
    }
    Ok(df)
}

/// `ENV K=V` or `ENV K V` → (key, value).
/// Parses an `ENV` into one or more `(key, value)` pairs, Docker-style:
/// - **legacy** `ENV key value with spaces` (without `=`): a single pair, the rest is the value.
/// - **multi-var** `ENV k1=v1 k2="v 2" k3=v3`: tokenizes by spaces RESPECTING
///   quotes (for values with spaces), each token splits on the 1st `=`.
fn parse_env_pairs(rest: &str) -> Vec<(String, String)> {
    let rest = rest.trim();
    if !rest.contains('=') {
        return match rest.split_once(char::is_whitespace) {
            Some((k, v)) => vec![(k.trim().to_string(), v.trim().to_string())],
            None => vec![(rest.to_string(), String::new())],
        };
    }
    let mut out = Vec::new();
    let mut tok = String::new();
    let mut in_quote = false;
    let mut push = |t: &mut String| {
        if let Some((k, v)) = t.split_once('=') {
            out.push((k.trim().to_string(), v.to_string()));
        }
        t.clear();
    };
    for ch in rest.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !tok.is_empty() {
                    push(&mut tok);
                }
            }
            c => tok.push(c),
        }
    }
    if !tok.is_empty() {
        push(&mut tok);
    }
    out
}

/// `CMD ["a","b"]` (JSON) or `CMD a b` (shell) → vector of arguments.
fn parse_cmd(rest: &str) -> Vec<String> {
    if rest.starts_with('[') {
        serde_json::from_str::<Vec<String>>(rest).unwrap_or_default()
    } else {
        rest.split_whitespace().map(|s| s.to_string()).collect()
    }
}

/// The architecture in OCI vocabulary (`amd64`, `arm64`, ...).
fn oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => other,
    }
}

impl ImageStore {
    /// The `diff_ids` (UNCOMPRESSED digests) of the base's layers, read from its
    /// ORIGINAL config in the CAS (a pulled image brings them correct). Falls back to the
    /// stored digests if the config lacks them (e.g. a `scratch` base).
    fn base_diff_ids(&self, base: &Image) -> Vec<String> {
        if let Ok(bytes) = self.cas().read(&base.id) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(ids) = v
                    .get("rootfs")
                    .and_then(|r| r.get("diff_ids"))
                    .and_then(|d| d.as_array())
                {
                    let out: Vec<String> = ids
                        .iter()
                        .filter_map(|x| x.as_str().map(|s| format!("sha256:{}", strip(s))))
                        .collect();
                    if out.len() == base.layers.len() {
                        return out;
                    }
                }
            }
        }
        base.layers
            .iter()
            .map(|l| format!("sha256:{}", strip(l)))
            .collect()
    }

    /// Packs a build container's `upperdir` as a tar -> CAS.
    pub fn commit_upper(&self, container_id: &str) -> Result<String> {
        let upper: PathBuf = self
            .root()
            .join("containers")
            .join(container_id)
            .join("upper");
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            builder
                .append_dir_all(".", &upper)
                .map_err(|e| Error::Invalid(format!("failed to pack the diff: {e}")))?;
            builder
                .finish()
                .map_err(|e| Error::Invalid(format!("failed to close the tar: {e}")))?;
        }
        self.cas().write(&buf)
    }

    /// Creates the final image: layers(base) + new layer, with the config derived from the
    /// Dockerfile (Cmd, Env and — Delonix extensions — CPU/memory limits).
    pub fn build_image(
        &self,
        base: &Image,
        new_layer: String,
        df: &Dockerfile,
        tag: &str,
    ) -> Result<Image> {
        let mut layers = base.layers.clone();
        layers.push(new_layer);
        let cmd = if df.cmd.is_empty() {
            base.config.cmd.clone()
        } else {
            df.cmd.clone()
        };
        let entrypoint = if df.entrypoint.is_empty() {
            base.config.entrypoint.clone()
        } else {
            df.entrypoint.clone()
        };
        // Env = the base's + the Dockerfile's.
        let mut env = base.config.env.clone();
        env.extend(df.env.iter().cloned());
        // inherit the base's limits if the Dockerfile does not redefine them.
        let cpus = df.cpus.clone().or_else(|| base.config.cpus.clone());
        let memory = df.memory.clone().or_else(|| base.config.memory.clone());

        let created = now_unix();
        // `diff_ids` VALID for Docker/OCI: the base's layers come from the base's
        // ORIGINAL config (UNCOMPRESSED digests, which Docker validates by
        // decompressing each blob); the new layer is an uncompressed tar, so
        // its diff_id = the digest itself. This way an image built by
        // Delonix is pullable by Docker (not only by Delonix). See A1 (push).
        let mut diff_ids: Vec<String> = self.base_diff_ids(base);
        diff_ids.push(format!("sha256:{}", strip(layers.last().unwrap())));
        let security = if df.security.is_empty() {
            base.config.security.clone()
        } else {
            df.security.clone()
        };
        let healthcheck = df
            .healthcheck
            .clone()
            .or_else(|| base.config.healthcheck.clone());
        let workdir = df.workdir.clone().unwrap_or_default();
        let config_json = serde_json::json!({
            // Standard OCI/Docker image config fields (interop).
            "architecture": oci_arch(),
            "os": "linux",
            "config": { "Cmd": cmd, "Entrypoint": entrypoint, "Env": env, "Cpus": cpus, "Memory": memory, "Security": security },
            "rootfs": { "type": "layers", "diff_ids": diff_ids },
            // Delonix extension (ignored by Docker):
            "created_unix": created,
        });
        let id = self.cas().write(&serde_json::to_vec(&config_json)?)?;

        let repo_tags = self.merged_tags(&id, tag);
        let img = Image {
            id,
            repo_tags,
            layers,
            config: ImageConfig {
                cmd,
                entrypoint,
                env,
                cpus,
                memory,
                security,
                healthcheck,
                user: String::new(),
                working_dir: workdir.clone(),
            },
            created_unix: created,
        };
        self.enforce_tag_uniqueness(&img)?;
        self.save(&img)?;
        Ok(img)
    }

    /// Creates an image from a FLAT rootfs (*rootless*/vfs mode): packs
    /// the ENTIRE directory as **a single layer** (squash) — there is no overlay, hence
    /// no diff. Valid OCI config (1 diff_id). Used by the rootless `build`.
    pub fn commit_flat_rootfs(
        &self,
        rootfs: &std::path::Path,
        cmd: Vec<String>,
        env: Vec<String>,
        workdir: String,
        tag: &str,
    ) -> Result<Image> {
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            b.follow_symlinks(false);
            b.append_dir_all(".", rootfs)
                .map_err(|e| Error::Invalid(format!("empacotar rootfs: {e}")))?;
            b.finish()
                .map_err(|e| Error::Invalid(format!("fechar tar: {e}")))?;
        }
        self.commit_flat_rootfs_from_tar(buf, cmd, env, workdir, tag)
    }

    /// Like [`commit_flat_rootfs`], but receives the rootfs tar **already built**
    /// (uncompressed). It exists because, in rootless with subuid, the rootfs of a
    /// `RUN` running `apt`/`dpkg` has restricted-mode files that the
    /// real user cannot read — the tar has to be made INSIDE the mapped userns
    /// (`delonix __buildtar`, see `cmd::mapped::buildtar`) and the result delivered
    /// here. The in-process path of [`commit_flat_rootfs`] is for when there is
    /// no subuid (rootless single-uid) or for root.
    pub fn commit_flat_rootfs_from_tar(
        &self,
        tar_bytes: Vec<u8>,
        cmd: Vec<String>,
        env: Vec<String>,
        workdir: String,
        tag: &str,
    ) -> Result<Image> {
        let layer = self.cas().write(&tar_bytes)?; // uncompressed tar → diff_id = digest
        let diff_ids = vec![format!("sha256:{}", strip(&layer))];
        let created = now_unix();
        let config_json = serde_json::json!({
            "architecture": oci_arch(),
            "os": "linux",
            "config": { "Cmd": cmd, "Env": env, "WorkingDir": workdir },
            "rootfs": { "type": "layers", "diff_ids": diff_ids },
            "created_unix": created,
        });
        let id = self.cas().write(&serde_json::to_vec(&config_json)?)?;
        let repo_tags = self.merged_tags(&id, tag);
        let img = Image {
            id,
            repo_tags,
            layers: vec![layer],
            config: ImageConfig {
                cmd,
                entrypoint: Vec::new(),
                env,
                cpus: None,
                memory: None,
                security: Vec::new(),
                healthcheck: None,
                user: String::new(),
                working_dir: workdir,
            },
            created_unix: created,
        };
        self.enforce_tag_uniqueness(&img)?;
        self.save(&img)?;
        Ok(img)
    }

    /// Creates an image from a container's state (`docker commit`): the
    /// `new_layer` is the diff (upperdir) already packed by [`commit_upper`], over
    /// the `base`'s layers; `cmd`/`env` are the container's. Valid OCI config
    /// (pullable by Docker), just like `build`.
    pub fn commit_container(
        &self,
        base: &Image,
        new_layer: String,
        cmd: Vec<String>,
        env: Vec<String>,
        tag: &str,
    ) -> Result<Image> {
        let mut layers = base.layers.clone();
        layers.push(new_layer);
        let mut diff_ids: Vec<String> = self.base_diff_ids(base);
        diff_ids.push(format!("sha256:{}", strip(layers.last().unwrap())));
        let entrypoint = base.config.entrypoint.clone();
        let cpus = base.config.cpus.clone();
        let memory = base.config.memory.clone();
        let security = base.config.security.clone();
        let healthcheck = base.config.healthcheck.clone();
        let workdir = base.config.working_dir.clone();
        let created = now_unix();
        let config_json = serde_json::json!({
            "architecture": oci_arch(),
            "os": "linux",
            "config": { "Cmd": cmd, "Entrypoint": entrypoint, "Env": env, "Cpus": cpus, "Memory": memory, "Security": security },
            "rootfs": { "type": "layers", "diff_ids": diff_ids },
            "created_unix": created,
        });
        let id = self.cas().write(&serde_json::to_vec(&config_json)?)?;
        let repo_tags = self.merged_tags(&id, tag);
        let img = Image {
            id,
            repo_tags,
            layers,
            config: ImageConfig {
                cmd,
                entrypoint,
                env,
                cpus,
                memory,
                security,
                healthcheck,
                user: String::new(),
                working_dir: workdir.clone(),
            },
            created_unix: created,
        };
        self.enforce_tag_uniqueness(&img)?;
        self.save(&img)?;
        Ok(img)
    }
}

#[cfg(test)]
mod tests {
    use super::{join_continuations, parse_env_pairs};

    #[test]
    fn join_continuations_coalesces_backslash_lines() {
        let df = "RUN apt install \\\n    a \\\n    b\nENV X=1";
        let lines: Vec<String> = join_continuations(df).into_iter().map(|(_, l)| l).collect();
        assert_eq!(
            lines.len(),
            2,
            "3 linhas físicas do RUN → 1 lógica, + o ENV"
        );
        assert!(
            lines[0].starts_with("RUN apt install")
                && lines[0].contains(" a ")
                && lines[0].contains(" b")
        );
        assert_eq!(lines[1], "ENV X=1");
    }

    #[test]
    fn parse_env_pairs_handles_multi_var_and_quotes() {
        // Multi-var on one line — the bug that lost the PATH.
        let p = parse_env_pairs("A=1 B=2 PATH=/app/.venv/bin:$PATH");
        assert_eq!(
            p,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "2".into()),
                ("PATH".into(), "/app/.venv/bin:$PATH".into()),
            ]
        );
        // Value with spaces between quotes.
        assert_eq!(
            parse_env_pairs(r#"MSG="hello world" K=v"#),
            vec![
                ("MSG".into(), "hello world".into()),
                ("K".into(), "v".into()),
            ]
        );
        // Legacy `ENV key value` (without `=`).
        assert_eq!(
            parse_env_pairs("GREETING olá mundo"),
            vec![("GREETING".into(), "olá mundo".into())]
        );
    }
}

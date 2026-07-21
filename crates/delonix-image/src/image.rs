//! The model of an image and its local store.

use crate::cas::{strip, Cas};
use delonix_runtime_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// The subset of the OCI config that Delonix uses (Cmd/Env) + Delonix extensions
/// (resource limits embedded in the image — something Docker does not have).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ImageConfig {
    /// The default command (Docker: `Cmd`).
    #[serde(default)]
    pub cmd: Vec<String>,
    /// The entry executable (Docker: `Entrypoint`). Many images set only
    /// this (without `Cmd`); the final command is `entrypoint + cmd` (or `entrypoint +
    /// user args`).
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// The default environment variables (Docker: `Env`).
    #[serde(default)]
    pub env: Vec<String>,
    /// Embedded CPU limit (`CPUS` in the Delonix Dockerfile), e.g. "0.5".
    #[serde(default)]
    pub cpus: Option<String>,
    /// Embedded memory limit (`MEMORY`), e.g. "96M".
    #[serde(default)]
    pub memory: Option<String>,
    /// Embedded security posture (`SECURITY`): e.g. `["userns","apparmor"]`.
    #[serde(default)]
    pub security: Vec<String>,
    /// Dockerfile `HEALTHCHECK` command (the part after `CMD`), if any.
    #[serde(default)]
    pub healthcheck: Option<String>,
    /// The default user (Docker/OCI: `User`), e.g. `"elasticsearch"`,
    /// `"1000"` or `"1000:1000"`. Empty = root (uid 0). Images such as
    /// Elasticsearch refuse to run as root, so the runtime switches to this
    /// uid/gid before the `exec` (in rootless, via the `newuidmap` subuid map).
    #[serde(default)]
    pub user: String,
    /// Default working directory (Docker/OCI: `WorkingDir`), e.g. `"/data"`,
    /// `"/app"`. Empty = `/`. The runtime `chdir`s here before the `exec` — without
    /// this, entrypoints that operate on the CWD (e.g. redis/postgres's `chown -R`) run
    /// from `/` and touch `/sys` (RO). [[delonix-rootless-user]]
    #[serde(default)]
    pub working_dir: String,
}

/// A locally registered image.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Image {
    /// Image id = digest of the config blob (`sha256:...`).
    pub id: String,
    /// The tags (`name:tag`) that point to this image.
    pub repo_tags: Vec<String>,
    /// Digests of the *layers*, from base to top.
    pub layers: Vec<String>,
    /// The resolved config (Cmd/Env).
    pub config: ImageConfig,
    /// Import/build instant (Unix seconds).
    pub created_unix: u64,
}

impl Image {
    /// The first 12 characters of the id (hex).
    pub fn short_id(&self) -> String {
        strip(&self.id).chars().take(12).collect()
    }

    /// The `scratch` pseudo-image (Docker): an EMPTY base, with no layers or config.
    /// It does NOT resolve in the store nor pull from a registry — it is the empty
    /// starting point for `FROM scratch`. `export_rootfs` over it produces an empty rootfs.
    pub fn scratch() -> Self {
        Image {
            id: "sha256:scratch".into(), // sentinel; there is no real config blob
            repo_tags: vec!["scratch:latest".into()],
            layers: Vec::new(),
            config: ImageConfig::default(),
            created_unix: 0,
        }
    }
}

/// The image store: JSON records + blobs in the CAS.
pub struct ImageStore {
    root: PathBuf,
    cas: Cas,
}

impl ImageStore {
    /// Opens (creating) the store. `$DELONIX_ROOT` or `/var/lib/delonix`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("images"))?;
        fs::create_dir_all(root.join("layers"))?;
        fs::create_dir_all(root.join("containers"))?;
        let cas = Cas::open(&root)?;
        Ok(Self { root, cas })
    }

    /// The default directory of the image store.
    pub fn default_root() -> PathBuf {
        if let Some(root) = std::env::var_os("DELONIX_ROOT") {
            return PathBuf::from(root);
        }
        // Rootless (A13): without privileges, root's store (`/var/lib/delonix`)
        // is not writable → use the user's one (`$XDG_DATA_HOME/delonix`).
        if !nix::unistd::geteuid().is_root() {
            let base = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
                .unwrap_or_else(|| PathBuf::from("."));
            return base.join("delonix");
        }
        PathBuf::from("/var/lib/delonix")
    }

    /// Access to the underlying CAS.
    pub fn cas(&self) -> &Cas {
        &self.cas
    }

    /// The root of the store.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.root.join("images").join(format!("{}.json", strip(id)))
    }

    /// Tags to store for an image with this `id`: the new one first, and
    /// then those that ALREADY exist for the same `id` (identical content ⇒ same id,
    /// can have several tags — like Docker). Avoids losing the previous tag when
    /// two builds produce the same config (e.g. a cache hit in the same second).
    pub(crate) fn merged_tags(&self, id: &str, new_tag: &str) -> Vec<String> {
        let mut tags = vec![normalise_tag(new_tag)];
        if let Ok(data) = fs::read(self.record_path(id)) {
            if let Ok(existing) = serde_json::from_slice::<Image>(&data) {
                for t in existing.repo_tags {
                    if !tags.contains(&t) {
                        tags.push(t);
                    }
                }
            }
        }
        tags
    }

    /// Persists an image (atomic write).
    pub fn save(&self, img: &Image) -> Result<()> {
        let p = self.record_path(&img.id);
        let tmp = p.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(img)?)?;
        fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// Lists all images, from newest to oldest.
    pub fn list(&self) -> Result<Vec<Image>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("images"))? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(bytes) = fs::read(&path) {
                    if let Ok(img) = serde_json::from_slice::<Image>(&bytes) {
                        out.push(img);
                    }
                }
            }
        }
        out.sort_by_key(|i| std::cmp::Reverse(i.created_unix));
        Ok(out)
    }

    /// Resolves `name:tag`, `name` (→`:latest`) or an id prefix.
    pub fn resolve(&self, name: &str) -> Result<Image> {
        let want = normalise_tag(name);
        for img in self.list()? {
            if img.repo_tags.contains(&want) || strip(&img.id).starts_with(strip(name)) {
                return Ok(img);
            }
        }
        Err(Error::NotFound(format!("image {name}")))
    }

    /// Ensures each tag of `img` points ONLY to `img`: removes it from
    /// any OTHER record that still has it (and deletes the record if it is left with no
    /// tags). This is what makes a re-tag MOVE the tag (like Docker), instead
    /// of leaving it pointing to two images.
    pub(crate) fn enforce_tag_uniqueness(&self, img: &Image) -> Result<()> {
        let tags: std::collections::HashSet<&String> = img.repo_tags.iter().collect();
        for other in self.list()? {
            if other.id == img.id {
                continue;
            }
            let kept: Vec<String> = other
                .repo_tags
                .iter()
                .filter(|t| !tags.contains(t))
                .cloned()
                .collect();
            if kept.len() == other.repo_tags.len() {
                continue; // nothing to remove
            }
            if kept.is_empty() {
                let _ = fs::remove_file(self.record_path(&other.id));
            } else {
                let mut moved = other.clone();
                moved.repo_tags = kept;
                self.save(&moved)?;
            }
        }
        Ok(())
    }

    /// Adds a new tag to an existing image (moving it from another
    /// image if it is already there — the tag stays unique).
    pub fn tag(&self, source: &str, new_tag: &str) -> Result<()> {
        let mut img = self.resolve(source)?;
        let tag = normalise_tag(new_tag);
        if !img.repo_tags.contains(&tag) {
            img.repo_tags.push(tag);
        }
        self.enforce_tag_uniqueness(&img)?;
        self.save(&img)?;
        Ok(())
    }

    /// Removes a tag; if it is the last one, deletes the image record.
    pub fn remove(&self, name: &str) -> Result<String> {
        let mut img = self.resolve(name)?;
        let want = normalise_tag(name);
        let id = img.short_id();
        if img.repo_tags.len() > 1 && img.repo_tags.contains(&want) {
            img.repo_tags.retain(|t| *t != want);
            self.save(&img)?;
            Ok(format!("untagged: {want}"))
        } else {
            fs::remove_file(self.record_path(&img.id))?;
            Ok(format!("deleted: {id}"))
        }
    }
}

/// Normalises an image name: `alpine` → `alpine:latest`.
pub fn normalise_tag(name: &str) -> String {
    if name.contains(':') || name.contains('@') {
        name.to_string()
    } else {
        format!("{name}:latest")
    }
}

/// The current instant in Unix seconds.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

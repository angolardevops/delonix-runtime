//! The compute context's `RunHost` port: what resolving a run needs to know about
//! this node — its defaults, its confinement, and where state lives.

use delonix_runtime_core::{Error, Result};
use std::path::PathBuf;

pub struct HostRuntime<'a> {
    /// The engine's state root.
    pub state_root: PathBuf,
    /// Words the refusal of an AppArmor profile on a host without AppArmor.
    pub apparmor_disabled: &'a dyn Fn(&str) -> Error,
}

impl delonix_compute::ports::RunHost for HostRuntime<'_> {
    fn read_file(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn default_memory(&self) -> String {
        crate::default_memory_max()
    }

    fn default_cpus(&self) -> String {
        crate::default_cpus()
    }

    fn default_masked_paths(&self) -> Vec<String> {
        crate::DEFAULT_MASKED_PATHS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn default_readonly_paths(&self) -> Vec<String> {
        crate::DEFAULT_READONLY_PATHS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn rootless(&self) -> bool {
        crate::is_rootless()
    }

    fn load_seccomp_profile(
        &self,
        path: &str,
    ) -> std::result::Result<(String, Vec<String>), String> {
        let json = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let (_, unknown) = crate::seccomp_profile::parse(&json).map_err(|e| e.to_string())?;
        Ok((json, unknown))
    }

    fn ensure_apparmor(&self, profile: &str) -> Result<()> {
        ensure_apparmor(profile, self.apparmor_disabled)
    }

    fn check_secret(&self, name: &str) -> Result<()> {
        delonix_runtime_core::SecretStore::open(&self.state_root)?
            .load(name)
            .map(|_| ())
    }

    fn default_log_path(&self, id: &str) -> String {
        self.state_root
            .join("containers")
            .join(id)
            .join("log")
            .to_string_lossy()
            .into_owned()
    }
}

/// Ensure the AppArmor profile `profile` is loaded. `unconfined` does nothing;
/// `delonix-default` is loaded from the embedded profile; any other name is
/// assumed already loaded on the host (we don't invent it).
fn ensure_apparmor(profile: &str, disabled: &dyn Fn(&str) -> Error) -> Result<()> {
    if profile == "unconfined" {
        return Ok(());
    }
    if profile == "delonix-default" {
        const PROFILE: &str = include_str!("../data/apparmor-delonix-default");
        // Unique name + O_EXCL + 0600, not a fixed path under a world-writable
        // `/tmp`: this file is handed to `apparmor_parser`, which loads a KERNEL
        // security policy from it. Whoever pre-creates the predictable path owns
        // the file and can rewrite it between our write and that read. Exactly
        // the class already fixed in `delonix-sdn::bpf` for the BPF object, and
        // the reason `write_private_temp` exists.
        let path =
            delonix_runtime_core::write_private_temp("delonix-default.aa", PROFILE.as_bytes())?;
        let out = std::process::Command::new("apparmor_parser")
            .arg("-r")
            .arg(&path)
            .output()
            .map_err(|_| {
                Error::Invalid(
                    "apparmor_parser unavailable (AppArmor not supported on this host?)".into(),
                )
            });
        let _ = std::fs::remove_file(&path);
        let out = out?;
        if !out.status.success() {
            return Err(Error::Invalid(format!(
                "failed to load AppArmor profile: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        return Ok(());
    }
    // ANY other name used to fall through to `Ok(())`, which meant a container
    // asked to run under a profile that does not exist started happily and came
    // out UNCONFINED — measured. A confinement flag that silently does nothing is
    // worse than no flag: the operator believes the container is confined. Same
    // fail-closed rule the sibling `--security-opt seccomp=<profile>` already
    // follows, and what Docker and Podman both do.
    if !crate::apparmor_enabled() {
        return Err(disabled(profile));
    }
    // Whether the profile is LOADED cannot be checked here: the kernel's list is
    // root-only (measured: `Permission denied` for an ordinary user), so a
    // preflight over it would refuse every profile in rootless — including the
    // ones that work. The kernel answers at the transition instead, and the
    // container now refuses to start unconfined (`apply_apparmor`).
    Ok(())
}

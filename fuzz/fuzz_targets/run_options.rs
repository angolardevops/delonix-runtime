#![no_main]

//! Fuzzes the hand-written parsers behind a container's run options, each fed
//! with text a user or a client controls:
//!
//! - `parse_env_file` — `--env-file`, `secret create --from-env-file -` (stdin)
//!   and a compose `env_file:`;
//! - `parse_env0` — the NUL-separated environment the CRI writes for a pod;
//! - `parse_add_host` — `--add-host`, compose `extra_hosts:`, the Docker API's
//!   `ExtraHosts`;
//! - `parse_security_opts` — `--security-opt`, the Docker API's `SecurityOpt`;
//! - `parse_mem_mib` — a VM's `memory:`;
//! - `parse_idle_timeout` — `serve node-api --idle-timeout`.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = delonix_compute::run::parse_env0(data, "fuzz");
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = delonix_model::secret::parse_env_file(text);
    for line in text.lines().take(8) {
        let _ = delonix_compute::pod::parse_add_host(line);
        let _ = delonix_compute::vm_backend::parse_mem_mib(line);
        let _ = delonix_node::dispatch::parse_idle_timeout(line);
    }
    let opts: Vec<String> = text.lines().take(16).map(str::to_string).collect();
    let _ = delonix_compute::run::parse_security_opts(&opts);
});

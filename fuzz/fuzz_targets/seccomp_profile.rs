#![no_main]

//! Fuzzes `delonix_linux::seccomp_profile::parse` — a custom seccomp profile
//! (`--security-opt seccomp=<file>`, a CRI `Localhost` profile). The profile is
//! the policy the operator wrote, read in the parent before the clone, and a
//! panic there is a crashed start. Also the size and CPU grammars that turn
//! `-m`/`--cpus` (and a manifest's `memory:`/`cpus:`) into cgroup values, and
//! the cgroup files the engine reads back (`cpu.pressure`, `cgroup.controllers`).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = delonix_linux::seccomp_profile::parse(text);
    for line in text.lines().take(8) {
        if let Some(b) = delonix_linux::try_mem_bytes(line) {
            // The value written into `memory.max` is the bytes, verbatim.
            assert_eq!(
                delonix_linux::mem_limit_write_value(line).as_deref(),
                Some(b.to_string().as_str())
            );
        }
        let _ = delonix_linux::cpu_max_for(line);
        let _ = delonix_linux::ulimit_name_known(line);
    }
    let _ = delonix_linux::parse_psi_some(text);
    let _ = delonix_linux::parse_controller_list(text);
});

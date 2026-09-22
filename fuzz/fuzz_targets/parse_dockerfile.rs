#![no_main]

//! Fuzzes `delonix_oci::build::parse_dockerfile` — a hand-written parser fed
//! with the untrusted content of a cloned repository's `Dockerfile`/
//! `Delonixfile`, which `delonix build` reads before any container exists to
//! sandbox it. A panic here is a denial of service on `delonix build`, not a
//! container escape, but it is still attacker-controlled input reaching a
//! parser this codebase wrote by hand instead of pulling in a crate for it —
//! exactly the class of surface this repo's supply-chain-minimalism rule
//! trades a dependency for, and pays for with the CPU cycles fuzzing costs
//! instead.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Invalid UTF-8 is rejected by the caller (`std::fs::read_to_string`)
    // long before this function sees it — fuzzing that boundary would be
    // fuzzing the standard library, not this parser.
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = delonix_oci::build::parse_dockerfile(text);
});

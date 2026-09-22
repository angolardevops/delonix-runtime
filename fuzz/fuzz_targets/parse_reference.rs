#![no_main]

//! Fuzzes `delonix_oci::registry::parse_reference` — splits an
//! attacker-influenceable image reference string (a `kind: Image`/`spec.pull`
//! field in a manifest, `container run <ref>`, `--from-image` on the Docker
//! API's `/images/create`) into `(host, repo, tag_or_digest)`. This exact
//! function has already had a real, shipped bug — the combined
//! `repo:tag@digest` form was mis-split before a fix and regression test
//! landed (see AGENTS.md, "2 bugs corrigidos em `delonix image pull`") — which
//! is what makes it worth the fuzzer's time over a less battle-tested parser.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = delonix_oci::registry::parse_reference(text);
});

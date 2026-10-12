#![no_main]

//! Fuzzes the published-port grammar of `delonix_sdn`: `[hostIp:]hostPort:
//! contPort[/proto]`, its ranges, and the normalised form the record keeps.
//! The text comes from `container run -p`, a manifest's `ports:`, a compose
//! file's `ports:`, the Docker API's `PortBindings` (a socket client) and the
//! CRI's port mappings. The normalised spec is what a `start` publishes again,
//! so it must be a fixed point: normalising it twice gives the same string.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = delonix_sdn::parse_publish(text);
    let _ = delonix_sdn::parse_publish_addr(text);
    if let Ok(specs) = delonix_sdn::expand_publish_range(text) {
        // A range expands to one spec per port; a huge range is the caller's
        // to refuse, and the expansion itself must stay bounded by 65535.
        assert!(specs.len() <= 65536);
        for s in specs.iter().take(4) {
            let _ = delonix_sdn::parse_publish_addr(s);
        }
    }
    if let Ok(n) = delonix_sdn::normalize_publish_spec(text) {
        let again = delonix_sdn::normalize_publish_spec(&n).expect("a normalised spec parses");
        assert_eq!(again, n, "normalising is idempotent");
    }
    let (rate, burst) = text.split_once(' ').map_or((text, None), |(r, b)| (r, Some(b)));
    let _ = delonix_sdn::parse_net_rate(rate, burst);
});

//! The dependency rules as a test, so `cargo test` fails the moment a module
//! reaches in the wrong direction. The use cases (`notes`) must not know the
//! transport, telemetry, caching, messaging or a storage technology; adapters
//! must not call the transport.

use std::path::Path;

/// `(module file, forbidden `use` fragments)`.
const RULES: &[(&str, &[&str])] = &[
    // The use cases know their ports and the standard library — nothing else.
    (
        "notes.rs",
        &[
            "crate::httpapi",
            "crate::memstore",
            "crate::webhook",
            "crate::telemetry",
            "crate::config",
            "crate::cache",
            "crate::messaging",
            "crate::health",
            "use axum",
            "use reqwest",
            "use tracing",
        ],
    ),
    // Adapters implement ports; they do not call the transport.
    ("memstore.rs", &["crate::httpapi", "crate::webhook", "crate::cache", "crate::messaging"]),
    ("webhook.rs", &["crate::httpapi", "crate::memstore"]),
    ("messaging.rs", &["crate::httpapi", "crate::memstore"]),
    // The transport reaches the use cases through a trait, never a storage adapter.
    ("httpapi.rs", &["crate::memstore"]),
];

#[test]
fn dependency_direction_holds() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for (file, forbidden) in RULES {
        let text = std::fs::read_to_string(src.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        for frag in *forbidden {
            assert!(
                !text.contains(frag),
                "{file} contains {frag:?} — forbidden by the dependency rules in ARCHITECTURE.md"
            );
        }
    }
}

//! The contract drift gate: `api/openapi.yaml` and the route table must list
//! the same method+path pairs. A route added to or removed from the code
//! without being added here (and the other way round) fails here.

use __MODULE__::httpapi::routes_table;

#[test]
fn openapi_matches_routes() {
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/api/openapi.yaml"))
        .expect("api/openapi.yaml");

    let mut documented: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in raw.lines() {
        if let Some(path) = line.strip_prefix("  ").filter(|l| !l.starts_with(' ')) {
            if let Some(p) = path.strip_suffix(':') {
                if p.starts_with('/') {
                    current = p.to_string();
                    continue;
                }
            }
        }
        if let Some(method) = line.strip_prefix("    ").filter(|l| !l.starts_with(' ')) {
            if let Some(m) = method.strip_suffix(':') {
                if matches!(m, "get" | "post" | "put" | "patch" | "delete") && !current.is_empty() {
                    documented.push(format!("{} {current}", m.to_uppercase()));
                    continue;
                }
            }
        }
        // A top-level key (no indentation) ends the current path block.
        if !line.starts_with(' ') && !line.is_empty() {
            current.clear();
        }
    }

    let mut served: Vec<String> = routes_table(true)
        .into_iter()
        .map(|(method, path)| format!("{method} {path}"))
        .collect();

    documented.sort();
    served.sort();
    assert_eq!(documented, served, "contract drift between openapi.yaml and the routes");
}

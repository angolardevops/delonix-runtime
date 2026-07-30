//! Criterion micro-benchmark for `registry::parse_reference` — the pure image-ref
//! parser run on every image operation. Dev-only (criterion is a dev-dependency);
//! it never enters the release tree. Run: `cargo bench -p delonix-image`.
//!
//! First exemplar of the repo's bench harness (see the `delonix-testing` skill /
//! `performance-engineer` agent): measure a real hot pure function, keep the
//! subject the SAME as the robustness (proptest) target so fuzz and bench share
//! ground truth.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use delonix_image::registry::parse_reference;

fn bench_parse_reference(c: &mut Criterion) {
    // Representative refs: short, registry-qualified, and the combined
    // repo:tag@digest form that once broke the parser.
    let cases = [
        "nginx:alpine",
        "ghcr.io/angolardevops/delonix-vm-k8s:1.34",
        "kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac1b8f0e0a5d2f3b4c5d6e7f8a9b0c1d2e3f",
        "registry.k8s.io/pause:3.10.1",
    ];
    let mut group = c.benchmark_group("parse_reference");
    for case in cases {
        group.bench_with_input(case, case, |b, input| {
            b.iter(|| parse_reference(black_box(input)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parse_reference);
criterion_main!(benches);

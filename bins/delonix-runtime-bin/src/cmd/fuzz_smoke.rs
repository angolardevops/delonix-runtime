//! Mutation fuzzing of the parsers that live in this binary crate, on stable.
//!
//! The `fuzz/` workspace (cargo-fuzz, nightly) reaches only library crates: a
//! fuzz target cannot depend on a `[[bin]]`. The manifest loader and the compose
//! reader live here, and they read the two documents this engine takes from
//! someone else — a `stack apply -f` manifest and a `docker-compose.yml` from a
//! cloned repository. So they are fuzzed here, as an ordinary test: a seed
//! corpus (the published `examples/`), a deterministic mutator, and every input
//! run under `catch_unwind`. A refusal is fine; a panic is a defect, and the
//! test prints the input that caused it.
//!
//! The budget is `DELONIX_FUZZ_ITERS` per target (default 1500, a few seconds),
//! so it runs in every `cargo test`; a longer campaign is the same test with a
//! bigger number. The mutator is seeded by `DELONIX_FUZZ_SEED` (default fixed),
//! so a failure reproduces with the seed it printed.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

/// xorshift64*: enough to drive a mutator, and the same sequence everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// Tokens that change what a YAML document means, so a mutation reaches past
/// the first syntax error more often than a random byte would.
const TOKENS: &[&str] = &[
    ":",
    "- ",
    "\n",
    "  ",
    "{",
    "}",
    "[",
    "]",
    ",",
    "*a",
    "&a ",
    "<<: ",
    "'",
    "\"",
    "!!str ",
    "~",
    "null",
    "0x1f",
    "99999999999999999999",
    "-1",
    "\\",
    "#",
    "---\n",
    "...\n",
    "|\n",
    "?",
    "kind: Stack\n",
    "kind: Pod\n",
    "spec:\n",
    "containers:\n",
    "ports:\n",
    "- \"80:80\"\n",
    "${A:-b}",
    "$$",
    "\t",
    "\u{0}",
    "é",
    "😀",
];

fn mutate(rng: &mut Rng, seeds: &[Vec<u8>], input: &[u8]) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..=rng.below(4) {
        let len = out.len();
        match rng.below(7) {
            0 if len > 0 => {
                let i = rng.below(len);
                out[i] ^= 1 << rng.below(8);
            }
            1 => {
                let t = TOKENS[rng.below(TOKENS.len())].as_bytes();
                let i = rng.below(len + 1);
                out.splice(i..i, t.iter().copied());
            }
            2 if len > 0 => {
                let a = rng.below(len);
                let b = (a + rng.below(64)).min(len);
                out.drain(a..b);
            }
            3 if len > 0 => {
                let a = rng.below(len);
                let b = (a + rng.below(128)).min(len);
                let chunk = out[a..b].to_vec();
                let i = rng.below(len + 1);
                out.splice(i..i, chunk);
            }
            4 if len > 0 => out.truncate(rng.below(len)),
            5 => {
                let other = &seeds[rng.below(seeds.len())];
                if !other.is_empty() {
                    let a = rng.below(other.len());
                    let b = (a + rng.below(256)).min(other.len());
                    let i = rng.below(len + 1);
                    out.splice(i..i, other[a..b].iter().copied());
                }
            }
            _ => {
                let i = rng.below(len + 1);
                out.insert(i, rng.next() as u8);
            }
        }
    }
    out
}

fn iterations() -> usize {
    std::env::var("DELONIX_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500)
}

fn seed() -> u64 {
    std::env::var("DELONIX_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x005e_ed23)
        | 1
}

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// Every file under `dir` (one level, plus one sub-directory level) whose name
/// ends in one of `exts`.
fn corpus(dir: &Path, exts: &[&str]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(dir) {
        dirs.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let ok = f
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| exts.iter().any(|e| n.ends_with(e)));
            if ok {
                if let Ok(b) = std::fs::read(&f) {
                    out.push(b);
                }
            }
        }
    }
    out
}

/// Runs `target` over the seeds and `iterations()` mutations of them, and
/// fails with the first input that made it panic.
fn campaign(name: &str, seeds: Vec<Vec<u8>>, target: impl Fn(&str)) {
    assert!(!seeds.is_empty(), "{name}: empty seed corpus");
    let mut rng = Rng(seed());
    let run = |input: &[u8]| -> Option<String> {
        let text = String::from_utf8_lossy(input);
        match catch_unwind(AssertUnwindSafe(|| target(&text))) {
            Ok(()) => None,
            Err(_) => Some(text.chars().take(600).collect()),
        }
    };
    for s in &seeds {
        if let Some(bad) = run(s) {
            panic!("{name}: a seed panicked:\n{bad:?}");
        }
    }
    let mut current = seeds[0].clone();
    for i in 0..iterations() {
        let base = if rng.below(4) == 0 {
            &current
        } else {
            &seeds[rng.below(seeds.len())]
        };
        let input = mutate(&mut rng, &seeds, base);
        if let Some(bad) = run(&input) {
            panic!(
                "{name}: input #{i} panicked (DELONIX_FUZZ_SEED={}):\n{bad:?}",
                seed()
            );
        }
        current = input;
    }
}

#[test]
fn the_manifest_loader_never_panics_on_a_mutated_manifest() {
    let seeds = corpus(&examples_dir(), &[".yaml", ".yml"]);
    campaign("manifest::load_str", seeds, |text| {
        let _ = super::manifest::load_str(text, "fuzz.yaml");
    });
}

#[test]
fn the_compose_reader_never_panics_on_a_mutated_compose_file() {
    let mut seeds = corpus(&examples_dir().join("compose-webapp"), &[".yaml", ".yml"]);
    seeds.push(
        b"services:\n  web:\n    image: nginx\n    ports: [\"127.0.0.1:8080:80\", \"80\"]\n    \
          command: sh -c 'echo \"$A\" && sleep 1'\n    environment: {A: b}\n    \
          depends_on: {db: {condition: service_healthy}}\n  db:\n    image: postgres\n    \
          extends: {service: web}\nx-app: &app {restart: always}\n"
            .to_vec(),
    );
    campaign("compose", seeds, |text| {
        let _ = super::compose::check_unsupported_fields(text);
        if let Ok(v) = super::compose::parse_compose_value(text) {
            let _ = serde_yaml::to_string(&v);
        }
        for line in text.lines().take(16) {
            let _ = super::compose::shlex_split(line);
        }
    });
}

/// The harness itself: a planted panic behind a token the mutator inserts is
/// found and reported. Without this, a mutator that never left the seeds (or a
/// `catch_unwind` that swallowed everything) would read as «no crashes».
#[test]
#[should_panic(expected = "input #")]
fn the_harness_finds_a_planted_panic() {
    let seeds = vec![b"kind: Network\nmetadata: {name: a}\n".to_vec()];
    campaign("planted", seeds, |text| {
        if text.contains("<<: ") {
            panic!("planted");
        }
    });
}

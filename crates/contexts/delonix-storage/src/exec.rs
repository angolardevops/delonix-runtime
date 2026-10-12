//! How a storage driver runs the tools of its backend (`lvcreate`, `zfs`,
//! `btrfs`, `mount`): an argv, never a shell line, through a port the tests
//! replace.
//!
//! A driver builds the argv; [`Run`] executes it. The tests hand a driver a
//! recorder instead of [`System`], so every command a driver would run is
//! checked as data — which is how the drivers are proven without a disk.

/// What a command left behind. `code` is `None` when a signal ended it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Exit status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The first non-empty line of stderr, else of stdout — the part of a
    /// failure a message quotes.
    pub fn why(&self) -> String {
        self.stderr
            .lines()
            .chain(self.stdout.lines())
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("no output")
            .to_string()
    }
}

/// Runs one argv. `Err` only when the program could not be started (it is not
/// installed: `ErrorKind::NotFound`); a program that ran and failed is an `Ok`
/// with a non-zero code, and the caller reads it.
pub trait Run: Send + Sync {
    fn run(&self, argv: &[String]) -> std::io::Result<Output>;
}

/// The search path the tools are taken from. Fixed: the privileged helper
/// never takes a program from a directory its caller could write.
pub const TOOL_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// An argv from string slices.
pub fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// A scripted [`Run`] for tests: records every argv and answers each from the
/// first rule whose prefix matches. An argv no rule matches answers «command
/// not found» (`Err(NotFound)`), so a driver that runs something the test did
/// not expect fails loudly.
#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use std::sync::Mutex;

    use super::{Output, Run};

    /// One answer: argv prefix → output (or `None` for «not installed»).
    type Rule = (Vec<String>, Option<Output>);

    #[derive(Default)]
    pub struct Script {
        rules: Mutex<Vec<Rule>>,
        pub calls: Mutex<Vec<Vec<String>>>,
    }

    impl Script {
        pub fn new() -> Self {
            Self::default()
        }

        /// `prefix` answers `code`, `stdout`, `stderr`. A later rule with the
        /// same prefix is used only after the earlier ones were consumed — see
        /// [`Script::once`].
        pub fn on(&self, prefix: &[&str], code: i32, stdout: &str, stderr: &str) -> &Self {
            self.rules.lock().unwrap().push((
                prefix.iter().map(|s| s.to_string()).collect(),
                Some(Output {
                    code: Some(code),
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                }),
            ));
            self
        }

        /// `prefix` is a program that is not installed.
        pub fn missing(&self, prefix: &[&str]) -> &Self {
            self.rules
                .lock()
                .unwrap()
                .push((prefix.iter().map(|s| s.to_string()).collect(), None));
            self
        }

        /// The argvs run so far.
        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        /// The argvs run so far whose program is `prog`.
        pub fn calls_of(&self, prog: &str) -> Vec<Vec<String>> {
            self.calls()
                .into_iter()
                .filter(|c| c.first().map(String::as_str) == Some(prog))
                .collect()
        }
    }

    impl Run for Script {
        fn run(&self, argv: &[String]) -> std::io::Result<Output> {
            self.calls.lock().unwrap().push(argv.to_vec());
            let rules = self.rules.lock().unwrap();
            // The LAST matching rule wins, so a test can override an answer
            // set up earlier by adding a more recent one.
            let hit = rules.iter().rev().find(|(prefix, _)| {
                argv.len() >= prefix.len() && argv[..prefix.len()] == prefix[..]
            });
            match hit {
                Some((_, Some(out))) => Ok(out.clone()),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("not scripted: {}", argv.join(" ")),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn why_quotes_the_first_line_that_says_something() {
        let o = Output {
            code: Some(5),
            stdout: "ignored\n".into(),
            stderr: "\n  Volume group \"vg9\" not found\nmore\n".into(),
        };
        assert_eq!(o.why(), "Volume group \"vg9\" not found");
        assert_eq!(Output::default().why(), "no output");
    }
}

//! The command runner the storage pool drivers are given (ADR-0067).
//!
//! The drivers build an argv and hand it to a `delonix_storage::exec::Run`;
//! this is the one that runs a real program. It lives in the adapter because a
//! context spawns no process. The privileged helper uses it too, which is why
//! it clears the environment: a program is taken only from
//! [`TOOL_PATH`](delonix_storage::exec::TOOL_PATH), never from a directory the
//! caller chose, and the tools speak the C locale the drivers parse.

use std::process::{Command, Stdio};

use delonix_storage::exec::{Output, Run, TOOL_PATH};

/// Runs real programs: a cleared environment, a fixed `PATH`, the C locale,
/// no stdin.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRun;

impl Run for SystemRun {
    fn run(&self, argv: &[String]) -> std::io::Result<Output> {
        let (prog, args) = argv
            .split_first()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"))?;
        let out = Command::new(prog)
            .args(args)
            .env_clear()
            .env("PATH", TOOL_PATH)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()?;
        Ok(Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use delonix_storage::exec::argv;

    use super::*;

    #[test]
    fn the_runner_clears_the_environment_and_fixes_the_path() {
        // The test process has a HOME; the child must not inherit it.
        assert!(std::env::var_os("HOME").is_some());
        let out = SystemRun.run(&argv(&["env"])).unwrap();
        assert!(out.success());
        assert!(
            out.stdout.contains(&format!("PATH={TOOL_PATH}")),
            "{}",
            out.stdout
        );
        assert!(out.stdout.contains("LC_ALL=C"));
        assert!(!out.stdout.contains("HOME="), "{}", out.stdout);
    }

    #[test]
    fn a_program_that_is_not_installed_is_an_err_and_a_failure_is_an_ok() {
        let e = SystemRun
            .run(&argv(&["dlx-no-such-program-anywhere"]))
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(SystemRun.run(&argv(&["false"])).unwrap().code, Some(1));
    }
}

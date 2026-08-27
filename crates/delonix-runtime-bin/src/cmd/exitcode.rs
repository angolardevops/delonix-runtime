//! Exit codes: telling «the resource is not there» apart from «it blew up».
//!
//! # The problem this exists to fix
//!
//! Measured before this module existed: `container inspect <missing>` exited
//! **1**, `volumes rm <missing>` exited **1**, and a syscall that died halfway
//! through the work also exited **1**. Only clap's own usage error (2) stood
//! apart. So the two answers a reconciler most needs to tell apart — «it is
//! absent, create it» and «something broke, stop» — were the same number.
//!
//! The only remaining signal was the error MESSAGE, and that is exactly the
//! wrong thing to build on here: this CLI translates its output
//! (`--l18n=pt`/`$DELONIX_L18N`), so a script that greps for `no such` works on
//! the machine it was written on and silently stops classifying on a node with
//! another locale. A wrong classification in a reconciler is not a cosmetic
//! bug: «I could not tell it was missing» becomes «I did not create it», and
//! «I could not tell it broke» becomes «I created it again on top of a broken
//! one».
//!
//! # The map, and why it is this small
//!
//! | code | meaning                                   |
//! |------|-------------------------------------------|
//! | 0    | success                                   |
//! | 1    | any other failure (unchanged default)     |
//! | 2    | invalid usage — clap's, and `stack plan --detailed-exitcode`'s «there are changes» |
//! | 3    | the resource exists but is NOT RUNNING    |
//! | 4    | no such resource                          |
//! | 5    | conflict — it already exists              |
//! | 69   | a capability this host does not have      |
//! | 124  | the deadline passed                       |
//!
//! **3 and 4 are not invented numbers.** They are the LSB init-script status
//! codes that `systemctl` still speaks today: 3 = «program is not running»,
//! 4 = «program or service status is unknown» (`systemctl status <no-such-unit>`
//! answers 4 on this very host). Anyone writing service automation already has
//! those two in their fingers. 5 has no convention behind it; it is the next
//! free number below the shell's own range.
//!
//! **69 and 124 are not invented either.** 69 is `EX_UNAVAILABLE` from
//! `sysexits.h`; 124 is what `timeout(1)` returns when the deadline passes, and
//! is therefore already in the fingers of anyone who wraps a command in one.
//!
//! Both were added because they had **real producers being misclassified**, not
//! because a table looked incomplete. `stack wait` answered 1 on a timeout —
//! the same number as a broken apply, on the command whose entire job is to be
//! read by CI. A missing `wg`/`virt-customize`/`ngrok` answered 1 too,
//! indistinguishable from a typo in a flag. The two calls a reconciler makes
//! next are opposite: wait longer, or stop and install something.

//! **What is deliberately NOT here**, because each one would be a number that
//! can never come back:
//!
//! * *Host precondition unsatisfied* (a session without cgroup delegation, say)
//!   is not an error variant at all — the engine warns and carries on, so there
//!   is nothing to classify. Giving it a code would mean inventing the
//!   condition first.
//! * *Retryable* (75), which the CLI restructuring proposed, has **no producer
//!   today**: the retrying that exists (`publish_with_retry`) happens inside the
//!   engine and never reaches a caller. Publishing it would repeat the mistake
//!   `Error::Conflict` documents in its own doc-comment: a code nothing
//!   constructs, that can never be observed.
//!
//!   *Permission denied* (77) was on this list for the same reason, and the
//!   reason was WRONG — corrected by measurement rather than by re-reading.
//!   There is no `Error::PermissionDenied` variant, which is what the earlier
//!   note checked; but the failure does reach a caller, wrapped as
//!   `Error::Io` whose `kind()` is `PermissionDenied`. Measured against a state
//!   root with the write bit cleared: `volumes create` and `secret create` both
//!   come back exactly that way. «No variant» and «no producer» are different
//!   claims, and the first was used to conclude the second.
//! * `Error::Invalid` and `Error::Registry` keep 1. Splitting «your argument is
//!   wrong» from «the registry answered badly» is defensible, but neither
//!   changes what a reconciler does next, and every extra number is a promise
//!   that has to hold for the rest of the `0.x`.
//!
//!   The CLI restructuring asks for `65` (invalid manifest or data) here, and
//!   that is NOT a rename of this arm: `Error::Invalid` is constructed at 643
//!   sites in this binary, covering both «your manifest is wrong» (65 in that
//!   proposal) and «your flag is wrong» (64 in the same proposal). Mapping the
//!   variant wholesale would give one number to two classes the proposal itself
//!   separates. Splitting it needs the variant split first.
//!
//! # Where the numbers must NOT reach
//!
//! `run` in the foreground returns the WORKLOAD's code and `exec` the COMMAND's
//! (`docs/cli-stability.md` promises both). Those paths never come through
//! here: they call `container::propagate_exit_status`/`process::exit` while
//! this module only ever sees an `Error` travelling up to `main`. A container
//! that exits 4 still exits 4, and that is also why a script must not read
//! `$?` from a `run`/`exec` as an ENGINE class — the engine did not choose that
//! number.
//!
//! # Why the match is exhaustive
//!
//! No `_ =>` arm on purpose. A variant added to `delonix_runtime_core::Error`
//! tomorrow stops the build here and forces someone to decide, which is the
//! opposite of what a catch-all does: quietly file the new class under
//! «generic» and never tell anyone.

use delonix_runtime_core::Error;

/// Any failure without a class of its own. The historical behaviour, and what
/// every path returned before this module existed.
pub const GENERIC: i32 = 1;

/// The resource exists, but is not running (LSB/`systemctl`: 3).
pub const NOT_RUNNING: i32 = 3;

/// No such resource (LSB/`systemctl`: 4).
pub const NOT_FOUND: i32 = 4;

/// The desired state conflicts with what is already there (it already exists).
pub const CONFLICT: i32 = 5;

/// A capability this host does not have (`sysexits.h`: `EX_UNAVAILABLE`).
pub const UNAVAILABLE: i32 = 69;

/// The filesystem said no on a path this engine needs (`sysexits.h`: `EX_IOERR`).
pub const IO: i32 = 74;

/// The operating system refused, and the fix is a permission (`EX_NOPERM`).
///
/// Carved out of [`IO`] rather than given a variant of its own, because that is
/// where it actually arrives: measured against a state root with the write bit
/// cleared, both `volumes create` and `secret create` come back as
/// `Error::Io(Permission denied (os error 13))`. It is the single most
/// actionable failure a caller can get — fix the permission and retry — and
/// answering the same `1` as every other I/O problem threw that away.
pub const NO_PERMISSION: i32 = 77;

/// The deadline passed with the work unfinished (`timeout(1)`'s own code).
pub const TIMEOUT: i32 = 124;

/// The single place an engine error becomes an exit code.
///
/// Pure on purpose: two places deciding the same number is how they start
/// disagreeing, and a table this small is only worth anything if it is the
/// whole truth.
pub fn for_error(e: &Error) -> i32 {
    match e {
        Error::NotFound(_) | Error::VmNotFound(_) => NOT_FOUND,
        Error::NotRunning(_) => NOT_RUNNING,
        Error::Conflict(_) => CONFLICT,
        Error::Unavailable(_) => UNAVAILABLE,
        Error::Timeout(_) => TIMEOUT,
        // The KIND is inspected rather than the variant: `Error::Io` is the
        // wrapper every filesystem refusal arrives in, and EACCES inside it is
        // a different answer for the caller than a full disk or a bad path.
        Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => NO_PERMISSION,
        Error::Io(_) => IO,
        Error::Json(_) | Error::Runtime { .. } => GENERIC,
        Error::Invalid(_) | Error::Registry(_) => GENERIC,
    }
}

/// The code for a batch (`stop a b c`, `rm a b c`) where several ids failed.
///
/// One class for every failure keeps that class — `rm missing1 missing2` is
/// still «none of them is there», and answering 1 would throw away information
/// the caller could act on. A MIXED batch falls back to the generic code:
/// there is no honest single answer, and picking the first failure's class
/// would make the result depend on the order the ids were typed in.
pub fn merge(codes: &[i32]) -> i32 {
    match codes.first() {
        None => 0,
        Some(&first) if codes.iter().all(|&c| c == first) => first,
        _ => GENERIC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nao_existe_e_rebentou_deixam_de_ser_o_mesmo_numero() {
        // The whole point: these two used to be indistinguishable at 1.
        assert_eq!(for_error(&Error::NotFound("container: web".into())), 4);
        assert_eq!(
            for_error(&Error::Runtime {
                context: "clone",
                message: "EPERM".into()
            }),
            1
        );
    }

    #[test]
    fn uma_vm_em_falta_conta_como_recurso_em_falta() {
        // `VmNotFound` only exists because the shared `NotFound` used to say
        // "no such container"; for a caller they are the same question.
        assert_eq!(for_error(&Error::VmNotFound("dev".into())), NOT_FOUND);
    }

    #[test]
    fn existe_mas_parado_tem_codigo_proprio() {
        assert_eq!(for_error(&Error::NotRunning("web".into())), NOT_RUNNING);
        // The KIND decides, not the variant: two `Error::Io` values give
        // different numbers, and that distinction is what 77 exists for.
        assert_eq!(
            for_error(&Error::Io(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))),
            NO_PERMISSION
        );
        assert_eq!(
            for_error(&Error::Io(std::io::Error::from(
                std::io::ErrorKind::NotFound
            ))),
            IO
        );
    }

    #[test]
    fn ja_existe_tem_codigo_proprio() {
        assert_eq!(
            for_error(&Error::Conflict("network 'app' already exists".into())),
            CONFLICT
        );
    }

    #[test]
    fn a_missing_capability_is_not_a_wrong_argument() {
        // Both used to be `Invalid` → 1. The caller's next move is opposite:
        // fix the flag, or go install something.
        assert_eq!(
            for_error(&Error::Unavailable("'wg' is not available".into())),
            UNAVAILABLE
        );
        assert_eq!(for_error(&Error::Invalid("bad port".into())), GENERIC);
    }

    #[test]
    fn an_expired_deadline_is_not_a_failure() {
        // `stack wait` answered 1 on a timeout — the same number as a broken
        // apply, on the command whose whole job is to be read by CI.
        assert_eq!(
            for_error(&Error::Timeout("waiting for 2 resource(s)".into())),
            TIMEOUT
        );
    }

    /// The `DX_*` code and the exit code are two spellings of ONE
    /// classification, for two kinds of caller. Nothing in the type system
    /// makes them agree, and two tables answering the same question is exactly
    /// how they start disagreeing.
    ///
    /// The invariant is **asymmetric, and deliberately so**:
    ///
    /// * every `DX_*` code maps to exactly ONE number — a code that spanned two
    ///   would make `$?` and the text contradict each other;
    /// * every number EXCEPT [`GENERIC`] maps to exactly one code;
    /// * [`GENERIC`] (1) spans several codes on purpose. It is the «no class of
    ///   its own» bucket, and the text is allowed to be finer there because a
    ///   text namespace is not scarce: every NUMBER is a promise that has to
    ///   hold for the rest of the `0.x`, while `DX_REGISTRY` costs nothing
    ///   beside `DX_INVALID_ARGUMENT`. That is the whole reason both exist.
    #[test]
    fn the_text_class_and_the_number_cannot_diverge() {
        use std::collections::HashMap;
        let sample = [
            Error::NotFound("container: web".into()),
            Error::VmNotFound("dev".into()),
            Error::NotRunning("web".into()),
            Error::Conflict("taken".into()),
            Error::Unavailable("no wg".into()),
            Error::Timeout("still waiting".into()),
            Error::Invalid("bad port".into()),
            Error::Registry("401".into()),
            Error::Runtime {
                context: "clone",
                message: "EPERM".into(),
            },
            Error::Io(std::io::Error::other("boom")),
            // The sample held ONE `Error::Io` and therefore could not see the
            // divergence it exists to forbid: the permission kind gives another
            // number, so it must give another DX_ code. A test blind to half the
            // case passes for the wrong reason.
            Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        ];

        let mut by_exit_code: HashMap<i32, &str> = HashMap::new();
        let mut by_dx_code: HashMap<&str, i32> = HashMap::new();
        for e in &sample {
            let (n, c) = (for_error(e), e.code());
            assert!(
                c.starts_with("DX_"),
                "{c}: every code is part of the published DX_ namespace"
            );
            // A code that spanned two numbers would make `$?` and the text
            // contradict each other for the same failure.
            if let Some(prev) = by_dx_code.insert(c, n) {
                assert_eq!(prev, n, "{c} maps to two different exit codes");
            }
            if n != GENERIC {
                if let Some(prev) = by_exit_code.insert(n, c) {
                    assert_eq!(prev, c, "exit code {n} maps to two different DX codes");
                }
            }
        }

        // And the bucket really is a bucket — if this ever became 1:1, one of
        // the two classifications silently grew a promise the other did not.
        let in_the_bucket = sample
            .iter()
            .filter(|e| for_error(e) == GENERIC)
            .map(|e| e.code())
            .collect::<std::collections::HashSet<_>>();
        assert!(
            in_the_bucket.len() > 1,
            "GENERIC is meant to carry several DX codes; found {in_the_bucket:?}"
        );
    }

    #[test]
    fn tudo_o_resto_continua_a_ser_um() {
        // The conservative half of the contract: nothing that used to be 1 and
        // has no class moves.
        //
        // `Error::Io` LEFT this list when 74/77 landed, and that is the whole
        // point of the list — it is the gate that made the move visible instead
        // of letting it happen quietly. What stays here is what still has no
        // class of its own.
        for e in [
            Error::Invalid("bad port".into()),
            Error::Registry("401".into()),
            Error::Json(serde_json::from_str::<u8>("{").unwrap_err()),
        ] {
            assert_eq!(for_error(&e), GENERIC, "{e}");
        }
    }

    #[test]
    fn nenhum_codigo_colide_com_uma_convencao_instalada() {
        // 2 is clap's usage error AND `stack plan --detailed-exitcode`'s "there
        // are changes"; 126/127 are the shell's; 128+N is a signal. A class
        // landing on any of those would be read as something else entirely.
        for c in [
            NOT_RUNNING,
            NOT_FOUND,
            CONFLICT,
            UNAVAILABLE,
            IO,
            NO_PERMISSION,
            TIMEOUT,
        ] {
            assert_ne!(c, 0);
            assert_ne!(c, 2);
            // 126 = "found but not executable", 127 = "not found", 128+N = a
            // signal. 124 sits just below and is `timeout(1)`'s own code, which
            // is the reason to pick it rather than a free number.
            assert!((3..126).contains(&c), "code {c} collides with the shell");
        }
    }

    #[test]
    fn um_lote_todo_em_falta_mantem_a_classe_e_um_lote_misto_nao() {
        assert_eq!(merge(&[]), 0);
        assert_eq!(merge(&[NOT_FOUND]), NOT_FOUND);
        assert_eq!(merge(&[NOT_FOUND, NOT_FOUND]), NOT_FOUND);
        // Mixed: no single honest answer, and the order of the ids must not
        // decide it.
        assert_eq!(merge(&[NOT_FOUND, NOT_RUNNING]), GENERIC);
        assert_eq!(merge(&[NOT_RUNNING, NOT_FOUND]), GENERIC);
    }
}

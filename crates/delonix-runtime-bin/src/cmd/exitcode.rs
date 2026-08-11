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
//!
//! **3 and 4 are not invented numbers.** They are the LSB init-script status
//! codes that `systemctl` still speaks today: 3 = «program is not running»,
//! 4 = «program or service status is unknown» (`systemctl status <no-such-unit>`
//! answers 4 on this very host). Anyone writing service automation already has
//! those two in their fingers. 5 has no convention behind it; it is the next
//! free number below the shell's own range.
//!
//! **What is deliberately NOT here**, because each one would be a number that
//! can never come back:
//!
//! * *Host precondition unsatisfied* (a session without cgroup delegation, say)
//!   is not an error variant at all — the engine warns and carries on, so there
//!   is nothing to classify. Giving it a code would mean inventing the
//!   condition first.
//! * `Error::Invalid` and `Error::Registry` keep 1. Splitting «your argument is
//!   wrong» from «the registry answered badly» is defensible, but neither
//!   changes what a reconciler does next, and every extra number is a promise
//!   that has to hold for the rest of the `0.x`.
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
        Error::Io(_) | Error::Json(_) | Error::Runtime { .. } => GENERIC,
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
    }

    #[test]
    fn ja_existe_tem_codigo_proprio() {
        assert_eq!(
            for_error(&Error::Conflict("network 'app' already exists".into())),
            CONFLICT
        );
    }

    #[test]
    fn tudo_o_resto_continua_a_ser_um() {
        // The conservative half of the contract: nothing that used to be 1 and
        // has no class moves.
        for e in [
            Error::Invalid("bad port".into()),
            Error::Registry("401".into()),
            Error::Json(serde_json::from_str::<u8>("{").unwrap_err()),
            Error::Io(std::io::Error::other("boom")),
        ] {
            assert_eq!(for_error(&e), GENERIC, "{e}");
        }
    }

    #[test]
    fn nenhum_codigo_colide_com_uma_convencao_instalada() {
        // 2 is clap's usage error AND `stack plan --detailed-exitcode`'s "there
        // are changes"; 126/127 are the shell's; 128+N is a signal. A class
        // landing on any of those would be read as something else entirely.
        for c in [NOT_RUNNING, NOT_FOUND, CONFLICT] {
            assert_ne!(c, 0);
            assert_ne!(c, 2);
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

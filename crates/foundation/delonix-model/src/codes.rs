//! The dictionary of numbered codes, `DX-CDNN` (ADR-0043).
//!
//! A code is four digits: the thousands digit is the [`Class`] — what the caller
//! does next —, the hundreds digit is the [`Domain`] — where it happened —, and the
//! last two number the failure within that class and domain (`00` is the class
//! itself). `DX-4201` reads «not found, storage» before anyone opens this table.
//!
//! **A number never changes meaning** and is never reused. The message may be
//! reworded and is translated; the number is the contract. Everything that shows a
//! code — the CLI error line, `delonix explain`, the generated page, `-o json`, the
//! node API — reads this one table.

use crate::Error;

/// What the caller does next. The thousands digit of a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// It worked, or a notice that is not a failure.
    Success,
    /// An argument is wrong: fix it and run again.
    InvalidArgument,
    /// The command line itself is wrong, or a plan has changes pending.
    Usage,
    /// The resource exists but is not running.
    NotRunning,
    /// There is no such resource.
    NotFound,
    /// The name is already taken.
    Conflict,
    /// A capability this host does not have.
    Unavailable,
    /// The operating system refused on a permission.
    PermissionDenied,
    /// The deadline passed with the work unfinished.
    Timeout,
    /// The kernel, the filesystem, the state records or a registry failed.
    SystemFailure,
}

impl Class {
    /// Every class, in digit order.
    pub const ALL: [Class; 10] = [
        Class::Success,
        Class::InvalidArgument,
        Class::Usage,
        Class::NotRunning,
        Class::NotFound,
        Class::Conflict,
        Class::Unavailable,
        Class::PermissionDenied,
        Class::Timeout,
        Class::SystemFailure,
    ];

    /// The thousands digit.
    pub fn digit(self) -> u16 {
        Class::ALL.iter().position(|c| *c == self).unwrap_or(0) as u16
    }

    /// The exit code a failure of this class answers. For [`Class::SystemFailure`]
    /// it is the default; an entry may say `74` (I/O) instead.
    pub fn exit_code(self) -> i32 {
        use crate::exitcode as x;
        match self {
            Class::Success => 0,
            Class::InvalidArgument => x::GENERIC,
            Class::Usage => 2,
            Class::NotRunning => x::NOT_RUNNING,
            Class::NotFound => x::NOT_FOUND,
            Class::Conflict => x::CONFLICT,
            Class::Unavailable => x::UNAVAILABLE,
            Class::PermissionDenied => x::NO_PERMISSION,
            Class::Timeout => x::TIMEOUT,
            Class::SystemFailure => x::GENERIC,
        }
    }

    /// The class name, as printed by `delonix explain`.
    pub fn name(self) -> &'static str {
        match self {
            Class::Success => "success",
            Class::InvalidArgument => "invalid argument",
            Class::Usage => "invalid usage",
            Class::NotRunning => "not running",
            Class::NotFound => "not found",
            Class::Conflict => "conflict",
            Class::Unavailable => "unavailable",
            Class::PermissionDenied => "permission denied",
            Class::Timeout => "timeout",
            Class::SystemFailure => "system failure",
        }
    }
}

/// Where it happened. The hundreds digit of a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// The engine as a whole, or no domain in particular.
    Engine,
    /// Containers and pods.
    Container,
    /// Volumes and storage.
    Volume,
    /// Networks, routes, policies, ingress.
    Network,
    /// Images, builds and scans.
    Image,
    /// Virtual machines.
    Vm,
    /// Stacks, manifests and compose files.
    Stack,
    /// Clusters and the CRI.
    Cluster,
    /// Secrets and security policy.
    Security,
    /// The host, the CLI and external tools.
    Host,
}

impl Domain {
    /// Every domain, in digit order.
    pub const ALL: [Domain; 10] = [
        Domain::Engine,
        Domain::Container,
        Domain::Volume,
        Domain::Network,
        Domain::Image,
        Domain::Vm,
        Domain::Stack,
        Domain::Cluster,
        Domain::Security,
        Domain::Host,
    ];

    /// The hundreds digit.
    pub fn digit(self) -> u16 {
        Domain::ALL.iter().position(|d| *d == self).unwrap_or(0) as u16
    }

    /// The domain name, as printed by `delonix explain`.
    pub fn name(self) -> &'static str {
        match self {
            Domain::Engine => "engine",
            Domain::Container => "container",
            Domain::Volume => "volume",
            Domain::Network => "network",
            Domain::Image => "image",
            Domain::Vm => "vm",
            Domain::Stack => "stack",
            Domain::Cluster => "cluster",
            Domain::Security => "security",
            Domain::Host => "host",
        }
    }
}

/// One entry of the dictionary.
#[derive(Debug)]
pub struct Code {
    /// The number, `CDNN`.
    pub number: u16,
    /// A stable, readable id (`volume.not_found`).
    pub id: &'static str,
    /// What the caller does next.
    pub class: Class,
    /// Where it happened.
    pub domain: Domain,
    /// The exit code the CLI answers with it.
    pub exit: i32,
    /// The message, in English (translated by the CLI's catalogue).
    pub message: &'static str,
    /// What it means.
    pub meaning: &'static str,
    /// What to do about it.
    pub remedy: &'static str,
}

impl Code {
    /// `DX-4201`.
    pub fn label(&self) -> String {
        label(self.number)
    }
}

/// `DX-4201` for `4201`.
pub fn label(number: u16) -> String {
    format!("DX-{number:04}")
}

/// Reads `DX-4201`, `dx-4201`, `DX4201` or `4201`. `None` for anything else.
pub fn parse(text: &str) -> Option<u16> {
    let t = text.trim();
    let digits = t
        .strip_prefix("DX-")
        .or_else(|| t.strip_prefix("dx-"))
        .or_else(|| t.strip_prefix("DX"))
        .or_else(|| t.strip_prefix("dx"))
        .unwrap_or(t);
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The entry for a number, if the dictionary has one.
pub fn lookup(number: u16) -> Option<&'static Code> {
    CATALOG.iter().find(|c| c.number == number)
}

macro_rules! code {
    ($n:literal, $id:literal, $class:ident, $domain:ident, $exit:expr, $msg:literal, $meaning:literal, $remedy:literal) => {
        Code {
            number: $n,
            id: $id,
            class: Class::$class,
            domain: Domain::$domain,
            exit: $exit,
            message: $msg,
            meaning: $meaning,
            remedy: $remedy,
        }
    };
}

/// The whole dictionary, sorted by number.
pub static CATALOG: &[Code] = &[
    code!(0, "success", Success, Engine, 0,
        "success",
        "The command did what it was asked.",
        "Nothing."),
    code!(1000, "invalid_argument", InvalidArgument, Engine, 1,
        "invalid argument",
        "An argument, a flag value or a manifest field is not acceptable. The message names which.",
        "Correct the argument the message names and run the command again."),
    code!(1401, "image.empty_sbom", InvalidArgument, Image, 1,
        "empty SBOM",
        "The image has no apk or dpkg package database, so there is nothing to scan for vulnerabilities.",
        "Scan an image built on a distribution with a package manager, or scan its base image instead."),
    code!(1402, "image.advisory_db_invalid", InvalidArgument, Image, 1,
        "invalid advisory database",
        "The advisory database the scanner loaded is not valid JSON.",
        "Sync it again with `delonix image scan --update --feed <url>`."),
    code!(1403, "image.osv_feed_not_json", InvalidArgument, Image, 1,
        "OSV feed is not JSON",
        "The feed given to the scanner could not be parsed as JSON.",
        "Check the --feed URL points at an OSV JSON export, not an HTML page."),
    code!(1404, "image.osv_feed_shape", InvalidArgument, Image, 1,
        "OSV feed has the wrong shape",
        "The feed is JSON but neither an array of advisories nor an object with a vulns array.",
        "Point --feed at an OSV export: an array of advisories, or an object with a vulns array."),
    code!(1405, "image.not_an_odoo_module", InvalidArgument, Image, 1,
        "not an Odoo module",
        "The directory named as a module has no __manifest__.py.",
        "Point the scan at the module's own directory, the one holding __manifest__.py."),
    code!(1406, "image.no_odoo_module", InvalidArgument, Image, 1,
        "no Odoo module found",
        "No directory under the path given has a __manifest__.py.",
        "Point the scan at the directory that contains the modules."),
    code!(2000, "usage", Usage, Engine, 2,
        "invalid usage",
        "The command line does not parse: an unknown subcommand, a missing argument or a flag this command does not take.",
        "Run the command with --help and follow its usage line."),
    code!(2601, "stack.changes_pending", Usage, Stack, 2,
        "the plan has changes",
        "`stack plan --detailed-exitcode` found differences between the manifest and what is running. Not a failure.",
        "Review the plan, then run `delonix stack apply` if the changes are wanted."),
    code!(3000, "not_running", NotRunning, Engine, 3,
        "not running",
        "The resource exists but is not running, and the operation needs it running.",
        "Start it (`delonix container start <name>`, `delonix vm start <name>`) and try again."),
    code!(4000, "not_found", NotFound, Engine, 4,
        "no such resource",
        "Nothing with that name or id exists in this scope.",
        "Check the name and the namespace with the matching `ls`, or create the resource first."),
    code!(4501, "vm.not_found", NotFound, Vm, 4,
        "no such VM",
        "There is no virtual machine with that name in this state root.",
        "List them with `delonix vm ls`, or create it with `delonix vm create`."),
    code!(5000, "conflict", Conflict, Engine, 5,
        "conflict: already exists",
        "The name is already taken, or the desired state conflicts with what is there.",
        "Pick another name, remove the existing resource, or use the command's --force/--replace if it has one."),
    code!(6000, "unavailable", Unavailable, Engine, 69,
        "unavailable on this host",
        "A tool, a backend or a kernel feature the operation needs is not present. Nothing about the arguments is wrong.",
        "Install what the message names (`delonix system info` shows what this host has) and run the command again."),
    code!(7000, "permission_denied", PermissionDenied, Engine, 77,
        "permission denied",
        "The operating system refused on a permission: a file, a directory or a capability.",
        "Fix the permission on the path the message names, or run from a session that has it, and repeat."),
    code!(8000, "timeout", Timeout, Engine, 124,
        "timed out",
        "The deadline passed with the work unfinished. Nothing said no; it may still be finishing.",
        "Wait and ask again, or raise the --timeout. Do not recreate the resource on top of one still coming up."),
    code!(9000, "system_call_failed", SystemFailure, Engine, 1,
        "a system call failed",
        "The kernel refused an operation (clone, mount, setns…). The message carries the operation and the errno.",
        "Read the errno in the message; `delonix system info` checks the host requirements (user namespaces, cgroup delegation)."),
    code!(9001, "io", SystemFailure, Engine, 74,
        "I/O error",
        "The filesystem said no on a path the engine needs: a full disk, a missing directory, a bad mount.",
        "Check the path in the message and the free space of the state root (`delonix system df`)."),
    code!(9002, "invalid_state", SystemFailure, Engine, 1,
        "invalid state record",
        "A state record on disk could not be read or written as JSON: it is corrupt or from an incompatible version.",
        "Inspect the record the message names; restore it from a snapshot (`delonix system snapshot restore`) if it is corrupt."),
    code!(9401, "image.registry", SystemFailure, Image, 1,
        "registry error",
        "An OCI registry answered with an error, or could not be reached.",
        "Check the image reference, your login (`delonix image login`) and the network path to the registry."),
    code!(9402, "image.module_scan_failed", SystemFailure, Image, 1,
        "a module tree could not be read",
        "Reading the module directory failed: a permission, a missing path or an I/O error.",
        "Check the path in the message is readable by this user."),
];

impl Error {
    /// The dictionary number of this failure (ADR-0043). A variant built with free
    /// text reports its class's generic entry until it has its own.
    ///
    /// The match is exhaustive on purpose, like [`Error::code`]'s.
    pub fn number(&self) -> u16 {
        match self {
            Error::NotFound(_) => 4000,
            Error::VmNotFound(_) => 4501,
            Error::NotRunning(_) => 3000,
            Error::Conflict(_) => 5000,
            Error::Unavailable(_) => 6000,
            Error::Timeout(_) => 8000,
            Error::Invalid(_) => 1000,
            Error::Registry(_) => 9401,
            Error::Runtime { .. } => 9000,
            Error::Json(_) => 9002,
            Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 7000,
            Error::Io(_) => 9001,
            Error::Coded { number, .. } => *number,
        }
    }

    /// `inner`, carrying its specific dictionary number (ADR-0043 D4).
    ///
    /// # Panics
    ///
    /// When the number's class digit is not `inner`'s class: a code that says
    /// «not found» on a failure that exits as «invalid argument» would make the
    /// number and the exit code tell two stories. Every crate's conversion has a
    /// test that walks all its variants through here, so this fires in tests,
    /// not in front of an operator.
    pub fn coded(number: u16, inner: Error) -> Error {
        assert_eq!(
            number / 1000,
            inner.class().digit(),
            "DX-{number:04} does not belong to the class of «{inner}»"
        );
        Error::Coded {
            number,
            inner: Box::new(inner),
        }
    }

    /// The failure itself, for the rare caller that needs a variant's payload.
    ///
    /// Today every error is its own root. When a crate's error carries its specific
    /// code inside the shared class (ADR-0043 D4), this is what looks through the
    /// carrier — so a `match e.root()` keeps matching where a `match e` would not.
    /// For «which class is this», ask [`Error::class`] instead.
    pub fn root(&self) -> &Error {
        match self {
            Error::Coded { inner, .. } => inner.root(),
            e => e,
        }
    }

    /// [`Error::root`] by value, to move a payload out.
    pub fn into_root(self) -> Error {
        match self {
            Error::Coded { inner, .. } => inner.into_root(),
            e => e,
        }
    }

    /// The class of this failure — what the caller does next.
    ///
    /// **Ask this, not the variant.** A crate's own error travels as a shared class
    /// that carries its specific code (ADR-0043 D4), and a `match` on
    /// `Err(Error::NotFound(_))` stops matching it without a word from the
    /// compiler. A class question keeps its answer.
    pub fn class(&self) -> Class {
        let root = self.root();
        Class::ALL[usize::from(root.number() / 1000)]
    }

    /// «No such resource» — the caller creates it or reports it missing.
    pub fn is_not_found(&self) -> bool {
        self.class() == Class::NotFound
    }

    /// «Already exists» — the caller adopts it, skips, or picks another name.
    pub fn is_conflict(&self) -> bool {
        self.class() == Class::Conflict
    }

    /// «Exists but is not running».
    pub fn is_not_running(&self) -> bool {
        self.class() == Class::NotRunning
    }

    /// «A capability this host does not have».
    pub fn is_unavailable(&self) -> bool {
        self.class() == Class::Unavailable
    }

    /// «The deadline passed».
    pub fn is_timeout(&self) -> bool {
        self.class() == Class::Timeout
    }

    /// «An argument is wrong».
    pub fn is_invalid_argument(&self) -> bool {
        self.class() == Class::InvalidArgument
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_unique_and_sorted() {
        for w in CATALOG.windows(2) {
            assert!(
                w[0].number < w[1].number,
                "{} before {}",
                w[0].label(),
                w[1].label()
            );
        }
    }

    #[test]
    fn the_digits_are_the_class_and_the_domain() {
        for c in CATALOG {
            assert!(c.number <= 9999, "{}", c.label());
            assert_eq!(
                c.number / 1000,
                c.class.digit(),
                "{}: class digit",
                c.label()
            );
            assert_eq!(
                c.number / 100 % 10,
                c.domain.digit(),
                "{}: domain digit",
                c.label()
            );
            if c.number % 100 == 0 {
                assert_eq!(
                    c.domain,
                    Domain::Engine,
                    "{}: only a class entry ends in 00",
                    c.label()
                );
            }
        }
    }

    #[test]
    fn every_class_has_its_generic_entry() {
        for class in Class::ALL {
            let n = class.digit() * 1000;
            let c = lookup(n).unwrap_or_else(|| panic!("no entry {}", label(n)));
            assert_eq!(c.class, class);
        }
    }

    #[test]
    fn the_exit_code_of_an_entry_is_its_class() {
        for c in CATALOG {
            if c.class == Class::SystemFailure {
                assert!(
                    c.exit == 1 || c.exit == crate::exitcode::IO,
                    "{}",
                    c.label()
                );
            } else {
                assert_eq!(c.exit, c.class.exit_code(), "{}", c.label());
            }
        }
    }

    #[test]
    fn ids_are_unique_and_every_text_is_written() {
        let mut ids: Vec<&str> = CATALOG.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), CATALOG.len());
        for c in CATALOG {
            assert!(
                !c.message.is_empty() && !c.meaning.is_empty() && !c.remedy.is_empty(),
                "{}",
                c.label()
            );
        }
    }

    fn one_of_each() -> Vec<Error> {
        vec![
            Error::NotFound("volume x".into()),
            Error::VmNotFound("dev".into()),
            Error::NotRunning("web".into()),
            Error::Conflict("x".into()),
            Error::Unavailable("wg".into()),
            Error::Timeout("x".into()),
            Error::Invalid("x".into()),
            Error::Registry("x".into()),
            Error::Runtime {
                context: "clone",
                message: "EPERM".into(),
            },
            Error::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
            Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            Error::Io(std::io::Error::other("x")),
        ]
    }

    /// The number, the exit code and the `DX_*` identity of one failure can never
    /// tell three different stories.
    #[test]
    fn the_number_agrees_with_the_exit_code_of_the_same_error() {
        for e in one_of_each() {
            let entry =
                lookup(e.number()).unwrap_or_else(|| panic!("{} has no entry", label(e.number())));
            assert_eq!(entry.exit, crate::exitcode::for_error(&e), "{e}");
        }
    }

    #[test]
    fn the_class_question_answers_what_the_variant_did() {
        assert!(Error::NotFound("x".into()).is_not_found());
        assert!(Error::VmNotFound("x".into()).is_not_found());
        assert!(Error::Conflict("x".into()).is_conflict());
        assert!(Error::NotRunning("x".into()).is_not_running());
        assert!(Error::Unavailable("x".into()).is_unavailable());
        assert!(Error::Timeout("x".into()).is_timeout());
        assert!(Error::Invalid("x".into()).is_invalid_argument());
        assert!(!Error::Registry("x".into()).is_not_found());
        for e in one_of_each() {
            assert!(
                e.class().exit_code() == crate::exitcode::for_error(&e)
                    || e.class() == Class::SystemFailure,
                "{e}"
            );
        }
    }

    /// The carrier changes the number and nothing else: same message, same class,
    /// same exit code, same `DX_*` identity — and a `root()` match still matches.
    #[test]
    fn a_coded_error_is_its_inner_error_with_a_finer_number() {
        let plain = Error::NotFound("volume db".into());
        let coded = Error::coded(4201, Error::NotFound("volume db".into()));
        assert_eq!(coded.number(), 4201);
        assert_eq!(coded.to_string(), plain.to_string());
        assert_eq!(coded.class(), plain.class());
        assert_eq!(coded.code(), plain.code());
        assert_eq!(
            crate::exitcode::for_error(&coded),
            crate::exitcode::for_error(&plain)
        );
        assert!(coded.is_not_found());
        assert!(matches!(coded.root(), Error::NotFound(m) if m == "volume db"));
    }

    #[test]
    #[should_panic(expected = "does not belong to the class")]
    fn a_number_from_another_class_is_refused() {
        let _ = Error::coded(1201, Error::NotFound("volume db".into()));
    }

    #[test]
    fn a_code_is_read_in_the_spellings_people_write() {
        for s in ["DX-4201", "dx-4201", "DX4201", "4201", " 4201 "] {
            assert_eq!(parse(s), Some(4201), "{s}");
        }
        for s in [
            "",
            "42",
            "DX-42010",
            "Container",
            "Pod.image",
            "DX-42a1",
            "-4201",
        ] {
            assert_eq!(parse(s), None, "{s}");
        }
        assert_eq!(label(7), "DX-0007");
    }
}

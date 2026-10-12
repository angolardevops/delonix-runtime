//! The storage context's own failures, each with its class and its number in
//! the dictionary (ADR-0043). The volume domain's block (`DX-C2NN`).

use thiserror::Error;

/// A failure of a storage pool, its driver or the administrator's allowlist.
#[derive(Debug, Error)]
pub enum Error {
    // ---- invalid argument --------------------------------------------------
    /// A `kind: StoragePool`, a `spec.pool` or a `--pool` that cannot be
    /// served as written: a field a manifest may never carry (a device, a
    /// path, a command), a size missing or out of range, a pool volume asked
    /// for together with another backing.
    #[error("{0}")]
    InvalidPoolRequest(String),

    /// The allowlist file exists and cannot be used: it does not parse, names
    /// a field nobody defined, or an entry lacks what its driver needs.
    #[error("{0}")]
    InvalidAllowlist(String),

    /// A driver registration with no id, or with an id another driver has.
    #[error("{0}")]
    DriverRegistrationRefused(String),

    /// The privileged helper refused a request it does not serve: malformed,
    /// an unknown field or operation, or a value outside the protocol.
    #[error("{0}")]
    HelperRequestInvalid(String),

    // ---- not found ---------------------------------------------------------
    /// No `kind: StoragePool` with that name is registered on this node.
    #[error("{0}")]
    PoolNotRegistered(String),

    // ---- conflict ----------------------------------------------------------
    /// The pool still holds volumes of this engine, or the object the driver
    /// was asked to create or release exists without this engine's stamp.
    #[error("{0}")]
    PoolConflict(String),

    /// The allocation would pass the pool's over-allocation ceiling, or the
    /// pool is too full to take another volume (ADR-0067 D6).
    #[error("{0}")]
    PoolExhausted(String),

    /// `destroy-pool` on a pool the helper did not create: no `create:` block
    /// in the allowlist, or no creation mark on the pool itself.
    #[error("{0}")]
    PoolNotCreatedHere(String),

    /// `create-pool` refused a device: it carries a signature, is not a block
    /// device, or what it would create already exists.
    #[error("{0}")]
    DeviceInUse(String),

    // ---- unavailable -------------------------------------------------------
    /// The pool cannot be used on this host as it is: no allowlist, the
    /// driver is not built, the backing object is missing or not writable.
    #[error("{0}")]
    PoolUnavailable(String),

    /// The state of the pool could not be determined — the listing needs a
    /// privilege this process does not have. Never read as «no pools».
    #[error("{0}")]
    PoolUndetermined(String),

    /// The operation needs the privileged helper and none is configured or
    /// reachable on this node.
    #[error("{0}")]
    HelperUnavailable(String),

    // ---- permission denied -------------------------------------------------
    /// The administrator's allowlist does not name this pool, or caps a
    /// volume below what was asked.
    #[error("{0}")]
    NotAllowed(String),

    /// The helper does not serve the calling uid for this pool (`allowUsers`).
    #[error("{0}")]
    CallerNotAllowed(String),

    /// A pool administration command was run without root.
    #[error("{0}")]
    AdminNeedsRoot(String),

    /// A failure of the layers underneath, with its own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

type Dx = delonix_model::Error;

impl Error {
    /// The storage failure a dictionary number names, carrying `message` —
    /// how a refusal crosses the helper's socket and keeps its class. A number
    /// this context does not own becomes the engine error of that class.
    pub fn from_number(number: u16, message: String) -> Self {
        match number {
            1217 => Error::InvalidPoolRequest(message),
            1218 => Error::InvalidAllowlist(message),
            1219 => Error::DriverRegistrationRefused(message),
            1221 => Error::HelperRequestInvalid(message),
            4203 => Error::PoolNotRegistered(message),
            5201 => Error::PoolConflict(message),
            5202 => Error::PoolExhausted(message),
            5204 => Error::PoolNotCreatedHere(message),
            5205 => Error::DeviceInUse(message),
            6203 => Error::PoolUnavailable(message),
            6204 => Error::PoolUndetermined(message),
            6206 => Error::HelperUnavailable(message),
            7201 => Error::NotAllowed(message),
            7202 => Error::CallerNotAllowed(message),
            7203 => Error::AdminNeedsRoot(message),
            n => {
                let class = match n / 1000 {
                    1 => Dx::Invalid(message),
                    4 => Dx::NotFound(message),
                    5 => Dx::Conflict(message),
                    6 => Dx::Unavailable(message),
                    7 => Dx::PermissionDenied(message),
                    _ => return Error::PoolUnavailable(message),
                };
                if delonix_model::codes::lookup(n).is_some() && n % 1000 != 0 {
                    Error::Engine(Dx::coded(n, class))
                } else {
                    Error::Engine(class)
                }
            }
        }
    }

    /// The dictionary number of this failure. Exhaustive on purpose: a
    /// variant added tomorrow stops the build here.
    pub fn number(&self) -> u16 {
        match self {
            Error::InvalidPoolRequest(_) => 1217,
            Error::InvalidAllowlist(_) => 1218,
            Error::DriverRegistrationRefused(_) => 1219,
            Error::HelperRequestInvalid(_) => 1221,
            Error::PoolNotRegistered(_) => 4203,
            Error::PoolConflict(_) => 5201,
            Error::PoolExhausted(_) => 5202,
            Error::PoolNotCreatedHere(_) => 5204,
            Error::DeviceInUse(_) => 5205,
            Error::PoolUnavailable(_) => 6203,
            Error::PoolUndetermined(_) => 6204,
            Error::HelperUnavailable(_) => 6206,
            Error::NotAllowed(_) => 7201,
            Error::CallerNotAllowed(_) => 7202,
            Error::AdminNeedsRoot(_) => 7203,
            Error::Engine(e) => e.number(),
        }
    }
}

impl From<Error> for Dx {
    fn from(e: Error) -> Self {
        let number = e.number();
        let class = match e {
            Error::InvalidPoolRequest(t)
            | Error::InvalidAllowlist(t)
            | Error::DriverRegistrationRefused(t)
            | Error::HelperRequestInvalid(t) => Dx::Invalid(t),
            Error::PoolNotRegistered(t) => Dx::NotFound(t),
            Error::PoolConflict(t)
            | Error::PoolExhausted(t)
            | Error::PoolNotCreatedHere(t)
            | Error::DeviceInUse(t) => Dx::Conflict(t),
            Error::PoolUnavailable(t)
            | Error::PoolUndetermined(t)
            | Error::HelperUnavailable(t) => Dx::Unavailable(t),
            Error::NotAllowed(t) | Error::CallerNotAllowed(t) | Error::AdminNeedsRoot(t) => {
                Dx::PermissionDenied(t)
            }
            Error::Engine(e) => return e,
        };
        Dx::coded(number, class)
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    fn every_variant() -> Vec<Error> {
        vec![
            Error::InvalidPoolRequest("StoragePool 'x': `devices` is not a field".into()),
            Error::InvalidAllowlist("storage-pools.yaml: pool 'x' has no `path`".into()),
            Error::DriverRegistrationRefused("a storage driver needs an id".into()),
            Error::PoolNotRegistered("storage pool x".into()),
            Error::PoolConflict("storage pool 'x' still holds 2 volume(s)".into()),
            Error::PoolExhausted("storage pool 'x' is 97% full".into()),
            Error::PoolUnavailable("storage pool 'x': /srv/x does not exist".into()),
            Error::PoolUndetermined("storage pool 'x': LVM state needs root".into()),
            Error::NotAllowed("storage pool 'x' is not in the allowlist".into()),
            Error::HelperRequestInvalid("op 'create-pool' is not served".into()),
            Error::PoolNotCreatedHere("pool 'x' has no create: block".into()),
            Error::DeviceInUse("/dev/sdb carries an ext4 signature".into()),
            Error::HelperUnavailable("no storage helper".into()),
            Error::CallerNotAllowed("uid 1001 not in allowUsers".into()),
            Error::AdminNeedsRoot("create-pool needs root".into()),
            Error::Engine(delonix_model::Error::Conflict("x".into())),
        ]
    }

    #[test]
    fn every_failure_keeps_its_number_through_the_conversion() {
        for e in every_variant() {
            let number = e.number();
            let shown = e.to_string();
            let converted = delonix_model::Error::from(e);
            assert_eq!(converted.number(), number, "{shown}");
            assert!(
                delonix_model::codes::lookup(number).is_some(),
                "DX-{number:04} ({shown}) has no dictionary entry"
            );
        }
    }

    /// A storage failure that crosses the helper's socket as `(number,
    /// message)` comes back as the same variant.
    #[test]
    fn a_failure_survives_the_round_trip_by_number() {
        for e in every_variant() {
            let (n, text) = (e.number(), e.to_string());
            let back = Error::from_number(n, text.clone());
            assert_eq!(back.number(), n, "{text}");
            // The engine's own errors print their class before the message,
            // so only this context's variants keep the text byte for byte.
            if !matches!(back, Error::Engine(_)) {
                assert_eq!(back.to_string(), text);
            }
        }
    }

    /// The refusal for a pool that cannot be used is class 69 — the same class
    /// whether the engine knows why (unavailable) or cannot tell (undetermined).
    #[test]
    fn an_unusable_pool_is_unavailable_whether_or_not_the_reason_is_known() {
        for e in [
            Error::PoolUnavailable("x".into()),
            Error::PoolUndetermined("x".into()),
            Error::HelperUnavailable("x".into()),
        ] {
            let c = delonix_model::Error::from(e);
            assert_eq!(delonix_model::exitcode::for_error(&c), 69, "{c}");
        }
    }
}

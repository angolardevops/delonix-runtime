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

    // ---- unavailable -------------------------------------------------------
    /// The pool cannot be used on this host as it is: no allowlist, the
    /// driver is not built, the backing object is missing or not writable.
    #[error("{0}")]
    PoolUnavailable(String),

    /// The state of the pool could not be determined — the listing needs a
    /// privilege this process does not have. Never read as «no pools».
    #[error("{0}")]
    PoolUndetermined(String),

    // ---- permission denied -------------------------------------------------
    /// The administrator's allowlist does not name this pool, or caps a
    /// volume below what was asked.
    #[error("{0}")]
    NotAllowed(String),

    /// A failure of the layers underneath, with its own class.
    #[error(transparent)]
    Engine(#[from] delonix_model::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

type Dx = delonix_model::Error;

impl Error {
    /// The dictionary number of this failure. Exhaustive on purpose: a
    /// variant added tomorrow stops the build here.
    pub fn number(&self) -> u16 {
        match self {
            Error::InvalidPoolRequest(_) => 1217,
            Error::InvalidAllowlist(_) => 1218,
            Error::DriverRegistrationRefused(_) => 1219,
            Error::PoolNotRegistered(_) => 4203,
            Error::PoolConflict(_) => 5201,
            Error::PoolExhausted(_) => 5202,
            Error::PoolUnavailable(_) => 6203,
            Error::PoolUndetermined(_) => 6204,
            Error::NotAllowed(_) => 7201,
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
            | Error::DriverRegistrationRefused(t) => Dx::Invalid(t),
            Error::PoolNotRegistered(t) => Dx::NotFound(t),
            Error::PoolConflict(t) | Error::PoolExhausted(t) => Dx::Conflict(t),
            Error::PoolUnavailable(t) | Error::PoolUndetermined(t) => Dx::Unavailable(t),
            Error::NotAllowed(t) => Dx::PermissionDenied(t),
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

    /// The refusal for a pool that cannot be used is class 69 — the same class
    /// whether the engine knows why (unavailable) or cannot tell (undetermined).
    #[test]
    fn an_unusable_pool_is_unavailable_whether_or_not_the_reason_is_known() {
        for e in [
            Error::PoolUnavailable("x".into()),
            Error::PoolUndetermined("x".into()),
        ] {
            let c = delonix_model::Error::from(e);
            assert_eq!(delonix_model::exitcode::for_error(&c), 69, "{c}");
        }
    }
}

//! `delonix-vm`'s error type lives in `delonix-compute` since P4b.2 (a provider
//! crate builds its variants); re-exported here with the two conversions whose
//! source types are this adapter's.

pub use delonix_compute::vm_error::{Error, Result};

impl From<crate::cloudinit::Error> for delonix_compute::vm_error::Error {
    fn from(e: crate::cloudinit::Error) -> Self {
        delonix_compute::vm_error::Error::Engine(e.into())
    }
}

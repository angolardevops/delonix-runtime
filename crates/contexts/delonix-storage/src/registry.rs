//! The storage-pool drivers, by id.
//!
//! The composition root registers each driver once; nothing else in the engine
//! branches on a driver's name. Registering does no I/O (ADR-0008): a driver's
//! probe runs when a pool is used or listed.

use std::sync::{Arc, OnceLock, RwLock};

use crate::pool::StoragePoolDriver;
use crate::{Error, Result};

type Drivers = RwLock<Vec<Arc<dyn StoragePoolDriver>>>;

fn drivers() -> &'static Drivers {
    static DRIVERS: OnceLock<Drivers> = OnceLock::new();
    DRIVERS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Registers a driver. Registering the same id again replaces it.
pub fn register(driver: Arc<dyn StoragePoolDriver>) -> Result<()> {
    register_in(drivers(), driver)
}

/// The driver with this id, if one is registered.
pub fn driver(id: &str) -> Option<Arc<dyn StoragePoolDriver>> {
    driver_in(drivers(), id)
}

fn register_in(reg: &Drivers, driver: Arc<dyn StoragePoolDriver>) -> Result<()> {
    if driver.id().is_empty() {
        return Err(Error::DriverRegistrationRefused(
            "a storage pool driver needs an id".into(),
        ));
    }
    let mut all = reg.write().unwrap_or_else(|p| p.into_inner());
    all.retain(|d| d.id() != driver.id());
    all.push(driver);
    Ok(())
}

fn driver_in(reg: &Drivers, id: &str) -> Option<Arc<dyn StoragePoolDriver>> {
    reg.read()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .find(|d| d.id() == id)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ownership::Owner;
    use crate::pool::{
        Allocation, PoolOp, PoolProbe, PoolRef, PoolUsage, Privilege, VolumeRequest,
    };

    struct Fake(&'static str, u8);

    impl StoragePoolDriver for Fake {
        fn id(&self) -> &'static str {
            self.0
        }
        fn probe(&self, _: &PoolRef) -> PoolProbe {
            PoolProbe::Available
        }
        fn required(&self, _: PoolOp) -> Privilege {
            Privilege::Unprivileged
        }
        fn allocate(&self, _: &PoolRef, _: &VolumeRequest, _: &Owner) -> Result<Allocation> {
            Err(Error::PoolUnavailable("fake".into()))
        }
        fn release(&self, _: &PoolRef, _: &str, _: &Owner) -> Result<()> {
            Ok(())
        }
        fn usage(&self, _: &PoolRef) -> PoolUsage {
            PoolUsage {
                metadata_pct: Some(self.1),
                ..Default::default()
            }
        }
    }

    fn mark(reg: &Drivers, id: &str) -> Option<u8> {
        let entry = crate::allowlist::Entry::default();
        let pool = PoolRef {
            name: "p",
            entry: &entry,
        };
        driver_in(reg, id).and_then(|d| d.usage(&pool).metadata_pct)
    }

    #[test]
    fn a_driver_is_found_by_id_and_a_second_registration_replaces_it() {
        let reg = Drivers::default();
        register_in(&reg, Arc::new(Fake("dir", 1))).unwrap();
        register_in(&reg, Arc::new(Fake("zfs", 2))).unwrap();
        assert_eq!(mark(&reg, "dir"), Some(1));
        assert_eq!(mark(&reg, "lvm-thin"), None);
        register_in(&reg, Arc::new(Fake("dir", 3))).unwrap();
        assert_eq!(mark(&reg, "dir"), Some(3));
        assert_eq!(reg.read().unwrap().len(), 2);
    }

    #[test]
    fn a_driver_without_an_id_is_refused() {
        let reg = Drivers::default();
        assert!(register_in(&reg, Arc::new(Fake("", 0))).is_err());
    }
}

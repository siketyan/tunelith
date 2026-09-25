use crate::{Device, DeviceInfo, Driver, Result};

/// The drivers in use, in order of priority: a device model's own driver goes
/// before the generic one that would take the device as well.
pub struct Registry {
    drivers: Vec<Box<dyn Driver>>,
}

/// A device found by [`Registry::probe`].
#[derive(Clone, Debug)]
pub struct Found {
    driver: usize,
    pub info: DeviceInfo,
}

impl Registry {
    pub fn new(drivers: Vec<Box<dyn Driver>>) -> Self {
        Self { drivers }
    }

    /// Lists every device, each given to the first driver that reports it.
    pub async fn probe(&self) -> Result<Vec<Found>> {
        let mut found = Vec::<Found>::new();
        for (driver, d) in self.drivers.iter().enumerate() {
            for info in d.probe().await? {
                if !found.iter().any(|f| f.info.id == info.id) {
                    found.push(Found { driver, info });
                }
            }
        }

        Ok(found)
    }

    pub async fn open(&self, found: &Found) -> Result<Box<dyn Device>> {
        self.drivers[found.driver].open(&found.info).await
    }
}

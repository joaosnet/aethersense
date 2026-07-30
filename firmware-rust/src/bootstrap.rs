//! Single ownership point for ESP-IDF process-wide resources.
//!
//! Peripheral managers may use explicit GPIO numbers after the profile's pin
//! plan has been validated, but `Peripherals::take()` itself must happen once.

use esp_idf_hal::modem::Modem;
use esp_idf_hal::spi::SPI2;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::EspError;

pub struct BootstrapResources {
    pub modem: Modem<'static>,
    pub spi2: Option<SPI2<'static>>,
    pub system_event_loop: EspSystemEventLoop,
    pub default_nvs: EspDefaultNvsPartition,
}

impl BootstrapResources {
    pub fn take() -> Result<Self, EspError> {
        let peripherals = Peripherals::take()?;
        Ok(Self {
            modem: peripherals.modem,
            spi2: Some(peripherals.spi2),
            system_event_loop: EspSystemEventLoop::take()?,
            default_nvs: EspDefaultNvsPartition::take()?,
        })
    }

    pub fn take_spi2(&mut self) -> Option<SPI2<'static>> {
        self.spi2.take()
    }
}

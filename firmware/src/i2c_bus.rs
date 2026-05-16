use esp_idf_sys::{
    self as sys, esp, gpio_config_t, gpio_int_type_t_GPIO_INTR_DISABLE,
    gpio_mode_t_GPIO_MODE_INPUT_OUTPUT_OD, gpio_num_t, gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
    gpio_pullup_t_GPIO_PULLUP_ENABLE, EspError,
};

const DELAY_US: u32 = 5;
const STRETCH_LIMIT: usize = 1_000;

#[derive(Clone, Copy)]
pub struct I2cBus {
    sda: gpio_num_t,
    scl: gpio_num_t,
}

impl I2cBus {
    pub fn new(sda: gpio_num_t, scl: gpio_num_t) -> Result<Self, EspError> {
        let config = gpio_config_t {
            pin_bit_mask: (1u64 << sda) | (1u64 << scl),
            mode: gpio_mode_t_GPIO_MODE_INPUT_OUTPUT_OD,
            pull_up_en: gpio_pullup_t_GPIO_PULLUP_ENABLE,
            pull_down_en: gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
            intr_type: gpio_int_type_t_GPIO_INTR_DISABLE,
        };
        esp!(unsafe { sys::gpio_config(&config) })?;

        let bus = Self { sda, scl };
        bus.release_sda()?;
        bus.release_scl()?;
        bus.delay();
        bus.recover()?;
        Ok(bus)
    }

    pub fn probe(&self, address: u8) -> Result<(), EspError> {
        self.add_device(address)?.write(&[])
    }

    pub fn add_device(&self, address: u8) -> Result<I2cDevice, EspError> {
        Ok(I2cDevice {
            bus: *self,
            address,
        })
    }

    fn recover(&self) -> Result<(), EspError> {
        self.release_sda()?;
        self.release_scl()?;
        self.wait_scl_high()?;

        for _ in 0..9 {
            if self.sda_high() {
                break;
            }
            self.drive_scl_low()?;
            self.delay();
            self.release_scl()?;
            self.wait_scl_high()?;
            self.delay();
        }

        self.stop()
    }

    fn start(&self) -> Result<(), EspError> {
        self.release_sda()?;
        self.release_scl()?;
        self.wait_scl_high()?;
        self.delay();
        self.drive_sda_low()?;
        self.delay();
        self.drive_scl_low()?;
        self.delay();
        Ok(())
    }

    fn stop(&self) -> Result<(), EspError> {
        self.drive_sda_low()?;
        self.delay();
        self.release_scl()?;
        self.wait_scl_high()?;
        self.delay();
        self.release_sda()?;
        self.delay();
        Ok(())
    }

    fn write_byte(&self, mut byte: u8) -> Result<(), EspError> {
        for _ in 0..8 {
            if byte & 0x80 != 0 {
                self.release_sda()?;
            } else {
                self.drive_sda_low()?;
            }

            self.delay();
            self.release_scl()?;
            self.wait_scl_high()?;
            self.delay();
            self.drive_scl_low()?;
            self.delay();
            byte <<= 1;
        }

        self.release_sda()?;
        self.delay();
        self.release_scl()?;
        self.wait_scl_high()?;
        self.delay();
        let acked = !self.sda_high();
        self.drive_scl_low()?;
        self.delay();

        if acked {
            Ok(())
        } else {
            Err(i2c_nack())
        }
    }

    fn read_byte(&self, ack: bool) -> Result<u8, EspError> {
        let mut byte = 0u8;
        self.release_sda()?;

        for _ in 0..8 {
            self.release_scl()?;
            self.wait_scl_high()?;
            self.delay();
            byte = (byte << 1) | u8::from(self.sda_high());
            self.drive_scl_low()?;
            self.delay();
        }

        if ack {
            self.drive_sda_low()?;
        } else {
            self.release_sda()?;
        }

        self.delay();
        self.release_scl()?;
        self.wait_scl_high()?;
        self.delay();
        self.drive_scl_low()?;
        self.release_sda()?;
        self.delay();

        Ok(byte)
    }

    fn release_sda(&self) -> Result<(), EspError> {
        self.set_level(self.sda, true)
    }

    fn drive_sda_low(&self) -> Result<(), EspError> {
        self.set_level(self.sda, false)
    }

    fn release_scl(&self) -> Result<(), EspError> {
        self.set_level(self.scl, true)
    }

    fn drive_scl_low(&self) -> Result<(), EspError> {
        self.set_level(self.scl, false)
    }

    fn set_level(&self, pin: gpio_num_t, high: bool) -> Result<(), EspError> {
        esp!(unsafe { sys::gpio_set_level(pin, u32::from(high)) })
    }

    fn sda_high(&self) -> bool {
        unsafe { sys::gpio_get_level(self.sda) != 0 }
    }

    fn scl_high(&self) -> bool {
        unsafe { sys::gpio_get_level(self.scl) != 0 }
    }

    fn wait_scl_high(&self) -> Result<(), EspError> {
        for _ in 0..STRETCH_LIMIT {
            if self.scl_high() {
                return Ok(());
            }
            self.delay();
        }
        Err(i2c_timeout())
    }

    fn delay(&self) {
        unsafe {
            sys::esp_rom_delay_us(DELAY_US);
        }
    }
}

pub struct I2cDevice {
    bus: I2cBus,
    address: u8,
}

impl I2cDevice {
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), EspError> {
        let result = (|| {
            self.bus.start()?;
            self.bus.write_byte(self.address << 1)?;
            for byte in bytes {
                self.bus.write_byte(*byte)?;
            }
            Ok(())
        })();

        self.finish(result)
    }

    pub fn write_read(&mut self, bytes: &[u8], buffer: &mut [u8]) -> Result<(), EspError> {
        let result = (|| {
            self.bus.start()?;
            self.bus.write_byte(self.address << 1)?;
            for byte in bytes {
                self.bus.write_byte(*byte)?;
            }
            self.bus.start()?;
            self.bus.write_byte((self.address << 1) | 1)?;

            let last = buffer.len().saturating_sub(1);
            for (index, byte) in buffer.iter_mut().enumerate() {
                *byte = self.bus.read_byte(index != last)?;
            }

            Ok(())
        })();

        self.finish(result)
    }

    fn finish(&self, result: Result<(), EspError>) -> Result<(), EspError> {
        let stop = self.bus.stop();
        match (result, stop) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(err), _) => Err(err),
            (Ok(()), Err(err)) => Err(err),
        }
    }
}

fn i2c_nack() -> EspError {
    EspError::from(sys::ESP_ERR_NOT_FOUND).unwrap()
}

fn i2c_timeout() -> EspError {
    EspError::from(sys::ESP_ERR_TIMEOUT).unwrap()
}

use esp_idf_sys::EspError;

use crate::i2c_bus::I2cDevice;

pub struct Es8311<'a> {
    i2c: &'a mut I2cDevice,
}

impl<'a> Es8311<'a> {
    pub fn new(i2c: &'a mut I2cDevice) -> Self {
        Self { i2c }
    }

    pub fn enable_adc(&mut self) -> Result<(), EspError> {
        for (register, value) in [
            (0x00, 0x80), // RESET / CSM power on
            (0x01, 0xBA), // clock manager / MCLK = BCLK
            (0x02, 0x18), // clock manager / MULT_PRE = 3
            (0x0D, 0x01), // power up analog circuitry
            (0x0E, 0x02), // enable analog PGA and ADC modulator
            (0x14, 0x10), // select Mic1p-Mic1n / minimum PGA gain
            (0x17, 0xFF), // ADC volume max gain
            (0x1C, 0x6A), // bypass ADC EQ, cancel DC offset
        ] {
            self.write_reg(register, value)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn disable(&mut self) -> Result<(), EspError> {
        for (register, value) in [
            (0x0D, 0xFC), // power down analog circuitry
            (0x0E, 0x6A),
            (0x00, 0x00), // CSM power down
        ] {
            self.write_reg(register, value)?;
        }
        Ok(())
    }

    fn write_reg(&mut self, register: u8, value: u8) -> Result<(), EspError> {
        self.i2c.write(&[register, value])
    }
}

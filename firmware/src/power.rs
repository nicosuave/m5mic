use esp_idf_hal::delay::FreeRtos;
use esp_idf_sys::EspError;

use crate::i2c_bus::I2cDevice;

const REG_PWR_CFG: u8 = 0x06;
const REG_I2C_CFG: u8 = 0x09;
const REG_GPIO_MODE: u8 = 0x10;
const REG_GPIO_OUT: u8 = 0x11;
const REG_GPIO_DRV: u8 = 0x13;
const REG_GPIO_FUNC0: u8 = 0x16;
const REG_PWR_SRC: u8 = 0x04;
const REG_VBAT_L: u8 = 0x22;
const REG_VIN_L: u8 = 0x24;
const REG_5VINOUT_L: u8 = 0x26;

const LDO_EN: u8 = 1 << 2;
const PM1_GPIO2: u8 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerSource {
    FiveVoltIn,
    FiveVoltInOut,
    Battery,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatteryStatus {
    pub millivolts: u16,
    pub percent: u8,
    pub power_source: PowerSource,
}

pub fn init_sticks3_power(i2c: &mut I2cDevice) -> Result<(), EspError> {
    bit_on(i2c, REG_PWR_CFG, LDO_EN)?;
    i2c.write(&[REG_I2C_CFG, 0x00])?;
    bit_off(i2c, REG_GPIO_FUNC0, PM1_GPIO2)?;
    bit_on(i2c, REG_GPIO_MODE, PM1_GPIO2)?;
    bit_off(i2c, REG_GPIO_DRV, PM1_GPIO2)?;
    bit_on(i2c, REG_GPIO_OUT, PM1_GPIO2)?;
    FreeRtos::delay_ms(50);
    Ok(())
}

pub fn read_battery_status(i2c: &mut I2cDevice) -> Result<BatteryStatus, EspError> {
    let millivolts = read_u16(i2c, REG_VBAT_L)?;
    let source = read_power_source(i2c)?;
    Ok(BatteryStatus {
        millivolts,
        percent: battery_percent(millivolts),
        power_source: source,
    })
}

#[allow(dead_code)]
pub fn read_input_mv(i2c: &mut I2cDevice) -> Result<(u16, u16), EspError> {
    Ok((read_u16(i2c, REG_VIN_L)?, read_u16(i2c, REG_5VINOUT_L)?))
}

fn bit_on(i2c: &mut I2cDevice, register: u8, mask: u8) -> Result<(), EspError> {
    update_reg(i2c, register, |value| value | mask)
}

fn bit_off(i2c: &mut I2cDevice, register: u8, mask: u8) -> Result<(), EspError> {
    update_reg(i2c, register, |value| value & !mask)
}

fn update_reg(
    i2c: &mut I2cDevice,
    register: u8,
    update: impl FnOnce(u8) -> u8,
) -> Result<(), EspError> {
    let mut value = [0u8; 1];
    i2c.write_read(&[register], &mut value)?;
    i2c.write(&[register, update(value[0])])
}

fn read_u16(i2c: &mut I2cDevice, register: u8) -> Result<u16, EspError> {
    let mut value = [0u8; 2];
    i2c.write_read(&[register], &mut value)?;
    Ok(u16::from_le_bytes(value))
}

fn read_power_source(i2c: &mut I2cDevice) -> Result<PowerSource, EspError> {
    let mut value = [0u8; 1];
    i2c.write_read(&[REG_PWR_SRC], &mut value)?;
    Ok(match value[0] & 0x07 {
        0 => PowerSource::FiveVoltIn,
        1 => PowerSource::FiveVoltInOut,
        2 => PowerSource::Battery,
        _ => PowerSource::Unknown,
    })
}

fn battery_percent(millivolts: u16) -> u8 {
    const TABLE: &[(u16, u8)] = &[
        (3300, 0),
        (3600, 10),
        (3700, 20),
        (3750, 30),
        (3790, 40),
        (3830, 50),
        (3870, 60),
        (3920, 70),
        (3970, 80),
        (4050, 90),
        (4150, 100),
    ];

    if millivolts <= TABLE[0].0 {
        return 0;
    }

    for pair in TABLE.windows(2) {
        let (low_mv, low_pct) = pair[0];
        let (high_mv, high_pct) = pair[1];
        if millivolts <= high_mv {
            let span_mv = (high_mv - low_mv) as u32;
            let span_pct = (high_pct - low_pct) as u32;
            let offset_mv = (millivolts - low_mv) as u32;
            return (low_pct as u32 + offset_mv * span_pct / span_mv) as u8;
        }
    }

    100
}

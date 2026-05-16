use esp_idf_hal::i2s::config::{
    ClockSource, Config, DataBitWidth, MclkMultiple, SlotBitWidth, SlotMode, StdClkConfig,
    StdConfig, StdGpioConfig, StdSlotConfig, StdSlotMask,
};

pub fn mic_i2s_config(sample_rate: u32) -> StdConfig {
    let channel = Config::default().dma_buffer_count(8).frames_per_buffer(128);
    let clock = StdClkConfig::new(sample_rate, ClockSource::Pll160M, MclkMultiple::M128);
    let slot = StdSlotConfig::philips_slot_default(DataBitWidth::Bits16, SlotMode::Mono)
        .slot_bit_width(SlotBitWidth::Bits16)
        .slot_mode_mask(SlotMode::Mono, StdSlotMask::Right)
        .ws_width(16)
        .bit_shift(true)
        .left_align(true)
        .big_endian(false)
        .bit_order_lsb(false);

    StdConfig::new(channel, clock, slot, StdGpioConfig::default())
}

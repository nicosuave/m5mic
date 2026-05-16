use anyhow::{Context, Result};
use esp_idf_hal::{
    delay::FreeRtos,
    gpio::{AnyInputPin, Output, OutputPin, PinDriver},
    ledc::LedcDriver,
    spi::{config, SpiAnyPins, SpiDeviceDriver, SpiDriver, SpiDriverConfig},
    units::FromValueType,
};

const WIDTH: i32 = 135;
const HEIGHT: i32 = 240;
const X_OFFSET: u16 = 52;
const Y_OFFSET: u16 = 40;
const HEADER_HEIGHT: i32 = 31;
const RECORD_CX: i32 = WIDTH / 2;
const HOME_RECORD_CY: i32 = 82;
const ACTIVE_RECORD_CY: i32 = 78;

const BLACK: u16 = rgb565(3, 6, 14);
const PANEL: u16 = rgb565(11, 17, 29);
const PANEL_3: u16 = rgb565(28, 39, 62);
const TEXT: u16 = rgb565(238, 242, 255);
const MUTED: u16 = rgb565(126, 143, 166);
const GREEN: u16 = rgb565(44, 214, 125);
const CYAN: u16 = rgb565(64, 196, 255);
const AMBER: u16 = rgb565(252, 176, 69);
const RED: u16 = rgb565(239, 46, 70);
const RED_SOFT: u16 = rgb565(150, 27, 49);
const RED_DARK: u16 = rgb565(66, 13, 27);
const LINE: u16 = rgb565(36, 50, 78);
const BACKLIGHT_FULL_PERCENT: u32 = 100;
const BACKLIGHT_DIM_PERCENT: u32 = 18;

const fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    (((r as u16) & 0xf8) << 8) | (((g as u16) & 0xfc) << 3) | ((b as u16) >> 3)
}

pub struct StickDisplay<'d> {
    spi: SpiDeviceDriver<'d, SpiDriver<'d>>,
    dc: PinDriver<'d, Output>,
    reset: PinDriver<'d, Output>,
    backlight: LedcDriver<'d>,
    battery: BatteryView,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatteryView {
    pub percent: Option<u8>,
    pub external_power: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordModeView {
    Latched,
    PushToTalk,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportView {
    Wifi,
    Bluetooth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Brightness {
    Full,
    Dim,
    Off,
}

impl BatteryView {
    pub const fn unknown() -> Self {
        Self {
            percent: None,
            external_power: false,
        }
    }
}

impl<'d> StickDisplay<'d> {
    #[allow(clippy::too_many_arguments)]
    pub fn new<SPI, SCLK, MOSI, CS, DC, RST>(
        spi: SPI,
        sclk: SCLK,
        mosi: MOSI,
        cs: CS,
        dc: DC,
        reset: RST,
        backlight: LedcDriver<'d>,
    ) -> Result<Self>
    where
        SPI: SpiAnyPins + 'd,
        SCLK: OutputPin + 'd,
        MOSI: OutputPin + 'd,
        CS: OutputPin + 'd,
        DC: OutputPin + 'd,
        RST: OutputPin + 'd,
    {
        let config = config::Config::new()
            .baudrate(40.MHz().into())
            .write_only(true);
        let spi = SpiDeviceDriver::new_single(
            spi,
            sclk,
            mosi,
            Option::<AnyInputPin<'d>>::None,
            Some(cs),
            &SpiDriverConfig::new(),
            &config,
        )
        .context("create LCD SPI")?;

        let mut display = Self {
            spi,
            dc: PinDriver::output(dc).context("create LCD DC")?,
            reset: PinDriver::output(reset).context("create LCD reset")?,
            backlight,
            battery: BatteryView::unknown(),
        };
        display.init().context("init LCD")?;
        Ok(display)
    }

    pub fn set_battery(&mut self, battery: BatteryView) {
        self.battery = battery;
    }

    pub fn update_battery(&mut self, battery: BatteryView) -> Result<()> {
        self.battery = battery;
        self.fill_rect(82, 0, 53, HEADER_HEIGHT, PANEL)?;
        self.draw_battery()
    }

    pub fn external_power(&self) -> bool {
        self.battery.external_power
    }

    pub fn set_brightness(&mut self, brightness: Brightness) -> Result<()> {
        match brightness {
            Brightness::Full => self.set_backlight_percent(BACKLIGHT_FULL_PERCENT),
            Brightness::Dim => self.set_backlight_percent(BACKLIGHT_DIM_PERCENT),
            Brightness::Off => self.set_backlight_percent(0),
        }
    }

    pub fn show_wifi_connecting(&mut self) -> Result<()> {
        self.base_wifi_with_net(AMBER, false)?;
        self.draw_centered("JOINING", 56, 2, AMBER)?;
        self.draw_centered("WIFI", 88, 3, TEXT)?;
        self.draw_progress(132, AMBER, 18)?;
        self.draw_centered("PLEASE WAIT", 176, 1, MUTED)?;
        self.draw_centered("NO SETUP AP", 221, 1, MUTED)
    }

    pub fn show_ready(&mut self) -> Result<()> {
        self.base_wifi(false)?;
        self.draw_record_target(false)?;
        self.draw_centered("TAP", 139, 3, TEXT)?;
        self.draw_centered("START", 168, 2, RED)?;
        self.draw_centered("HOLD A TALK", 219, 1, MUTED)
    }

    pub fn show_usb_ready(&mut self) -> Result<()> {
        self.base_usb(false)?;
        self.draw_record_target(false)?;
        self.draw_centered("USB MIC", 143, 2, TEXT)?;
        self.draw_level_meter(0)?;
        self.draw_centered("B WIFI", 223, 1, MUTED)
    }

    pub fn show_bluetooth_ready(&mut self) -> Result<()> {
        self.base_bluetooth(false)?;
        self.draw_record_target(false)?;
        self.draw_centered("TAP", 139, 3, TEXT)?;
        self.draw_centered("START", 168, 2, RED)?;
        self.draw_centered("BLUETOOTH", 219, 1, MUTED)
    }

    pub fn show_finding_receiver(&mut self, transport: TransportView) -> Result<()> {
        self.show_finding_receiver_phase(transport, 0)
    }

    pub fn show_finding_receiver_phase(
        &mut self,
        transport: TransportView,
        phase: usize,
    ) -> Result<()> {
        self.base_transport(transport, false)?;
        self.draw_centered("FINDING", 56, 2, AMBER)?;
        self.draw_centered("RECEIVER", 88, 2, TEXT)?;
        self.draw_progress(132, AMBER, progress_phase(phase))?;
        self.draw_centered("NETWORK", 176, 2, MUTED)?;
        self.draw_centered("WAIT", 221, 1, MUTED)
    }

    pub fn show_setup_portal(&mut self, ssid: &str) -> Result<()> {
        self.base_setup(false)?;
        self.draw_centered("SETUP", 55, 2, CYAN)?;
        self.draw_centered("JOIN WIFI", 91, 2, TEXT)?;
        self.draw_divider(124, CYAN)?;
        self.draw_centered(ssid, 151, 1, AMBER)?;
        self.draw_centered("192.168.71.1", 221, 1, MUTED)
    }

    pub fn show_setup_saved(&mut self) -> Result<()> {
        self.base_wifi_with_net(GREEN, false)?;
        self.draw_centered("OK", 58, 3, GREEN)?;
        self.draw_divider(105, GREEN)?;
        self.draw_centered("WIFI", 139, 2, TEXT)?;
        self.draw_centered("SAVED", 166, 2, TEXT)?;
        self.draw_centered("REBOOTING", 221, 1, MUTED)
    }

    pub fn show_setup_rebooting(&mut self) -> Result<()> {
        self.base_setup(false)?;
        self.draw_centered("OK", 58, 3, CYAN)?;
        self.draw_divider(105, CYAN)?;
        self.draw_centered("MIC", 139, 2, TEXT)?;
        self.draw_centered("MODE", 166, 2, TEXT)?;
        self.draw_centered("REBOOTING", 221, 1, MUTED)
    }

    pub fn show_recording(
        &mut self,
        transport: TransportView,
        elapsed_secs: u64,
        mode: RecordModeView,
        live_meters: bool,
    ) -> Result<()> {
        self.base_transport(transport, true)?;
        self.draw_record_target(true)?;
        self.draw_elapsed(elapsed_secs)?;
        if live_meters {
            self.draw_level_meter(0)?;
            self.draw_buffer_meter(0, 1)?;
        } else {
            self.fill_rect(9, 161, 117, 56, BLACK)?;
        }
        match (mode, live_meters) {
            (RecordModeView::Latched, true) => self.draw_centered("STOP", 223, 2, MUTED),
            (RecordModeView::PushToTalk, true) => self.draw_centered("RELEASE", 223, 2, MUTED),
            (RecordModeView::Latched, false) => self.draw_centered("A STOP  B OFF", 224, 1, MUTED),
            (RecordModeView::PushToTalk, false) => {
                self.draw_centered("RELEASE B OFF", 224, 1, MUTED)
            }
        }
    }

    pub fn update_recording_time(&mut self, elapsed_secs: u64) -> Result<()> {
        self.fill_rect(0, 120, WIDTH, 34, BLACK)?;
        self.draw_elapsed(elapsed_secs)
    }

    pub fn update_level(&mut self, level_percent: u8) -> Result<()> {
        self.draw_level_meter(level_percent)
    }

    pub fn update_buffer(&mut self, queued_frames: usize, max_frames: usize) -> Result<()> {
        self.draw_buffer_meter(queued_frames, max_frames)
    }

    pub fn show_error(&mut self, line1: &str, line2: &str) -> Result<()> {
        let wifi_fail = line1.starts_with("WIFI");
        if wifi_fail {
            self.base_wifi_with_net(AMBER, false)?;
        } else {
            self.base_error(false)?;
        }
        self.draw_centered("!", 58, 3, RED)?;
        self.draw_divider(105, RED)?;

        if wifi_fail {
            self.draw_centered("WIFI", 139, 2, TEXT)?;
            self.draw_centered("FAIL", 166, 2, TEXT)?;
            self.draw_centered(line2, 221, 1, MUTED)
        } else {
            self.draw_centered(line1, 139, 2, TEXT)?;
            self.draw_centered("ERROR", 166, 2, TEXT)?;
            self.draw_centered("PRESS AGAIN", 221, 1, MUTED)
        }
    }

    fn init(&mut self) -> Result<()> {
        self.set_backlight_percent(0)?;
        self.reset.set_high().context("reset high")?;
        FreeRtos::delay_ms(20);
        self.reset.set_low().context("reset low")?;
        FreeRtos::delay_ms(20);
        self.reset.set_high().context("reset release")?;
        FreeRtos::delay_ms(120);

        self.command(0x01, &[])?; // Software reset
        FreeRtos::delay_ms(150);
        self.command(0x11, &[])?; // Sleep out
        FreeRtos::delay_ms(120);
        self.command(0x3a, &[0x55])?; // RGB565
        self.command(0x36, &[0x00])?; // Natural portrait memory order
        self.command(0x21, &[])?; // M5GFX uses inverted color on this panel
        self.clear(BLACK)?;
        self.command(0x29, &[])?; // Display on
        FreeRtos::delay_ms(20);
        self.set_brightness(Brightness::Full)
    }

    fn set_backlight_percent(&mut self, percent: u32) -> Result<()> {
        let duty = self.backlight.get_max_duty() * percent.min(100) / 100;
        self.backlight.set_duty(duty).context("set LCD backlight")
    }

    fn base_transport(&mut self, transport: TransportView, recording: bool) -> Result<()> {
        match transport {
            TransportView::Wifi => self.base_wifi(recording),
            TransportView::Bluetooth => self.base_bluetooth(recording),
        }
    }

    fn base_wifi(&mut self, recording: bool) -> Result<()> {
        self.base_wifi_with_net(GREEN, recording)
    }

    fn base_wifi_with_net(&mut self, net_color: u16, recording: bool) -> Result<()> {
        self.clear(BLACK)?;
        self.fill_rect(0, 0, WIDTH, HEADER_HEIGHT, PANEL)?;
        self.fill_rect(0, HEADER_HEIGHT, WIDTH, 1, LINE)?;
        self.draw_text("WIFI", 7, 8, 2, TEXT)?;
        self.draw_network_bars(64, net_color)?;
        self.draw_battery()?;
        if recording {
            self.fill_rect(0, HEADER_HEIGHT + 1, WIDTH, 2, RED_DARK)?;
        }
        Ok(())
    }

    fn base_usb(&mut self, recording: bool) -> Result<()> {
        self.clear(BLACK)?;
        self.fill_rect(0, 0, WIDTH, HEADER_HEIGHT, PANEL)?;
        self.fill_rect(0, HEADER_HEIGHT, WIDTH, 1, LINE)?;
        self.draw_text("USB", 8, 8, 2, TEXT)?;
        self.draw_usb_mark(56, CYAN)?;
        self.draw_battery()?;
        if recording {
            self.fill_rect(0, HEADER_HEIGHT + 1, WIDTH, 2, RED_DARK)?;
        }
        Ok(())
    }

    fn base_bluetooth(&mut self, recording: bool) -> Result<()> {
        self.clear(BLACK)?;
        self.fill_rect(0, 0, WIDTH, HEADER_HEIGHT, PANEL)?;
        self.fill_rect(0, HEADER_HEIGHT, WIDTH, 1, LINE)?;
        self.draw_text("BT", 8, 5, 3, TEXT)?;
        self.draw_battery()?;
        if recording {
            self.fill_rect(0, HEADER_HEIGHT + 1, WIDTH, 2, RED_DARK)?;
        }
        Ok(())
    }

    fn base_setup(&mut self, recording: bool) -> Result<()> {
        self.clear(BLACK)?;
        self.fill_rect(0, 0, WIDTH, HEADER_HEIGHT, PANEL)?;
        self.fill_rect(0, HEADER_HEIGHT, WIDTH, 1, LINE)?;
        self.draw_text("SETUP", 7, 8, 2, TEXT)?;
        self.draw_battery()?;
        if recording {
            self.fill_rect(0, HEADER_HEIGHT + 1, WIDTH, 2, RED_DARK)?;
        }
        Ok(())
    }

    fn base_error(&mut self, recording: bool) -> Result<()> {
        self.clear(BLACK)?;
        self.fill_rect(0, 0, WIDTH, HEADER_HEIGHT, PANEL)?;
        self.fill_rect(0, HEADER_HEIGHT, WIDTH, 1, LINE)?;
        self.draw_text("ERR", 8, 8, 2, RED)?;
        self.draw_battery()?;
        if recording {
            self.fill_rect(0, HEADER_HEIGHT + 1, WIDTH, 2, RED_DARK)?;
        }
        Ok(())
    }

    fn draw_battery(&mut self) -> Result<()> {
        let color = match self.battery.percent {
            Some(percent) if percent <= 15 && !self.battery.external_power => RED,
            Some(percent) if percent <= 35 && !self.battery.external_power => AMBER,
            Some(_) if self.battery.external_power => GREEN,
            Some(_) => TEXT,
            None => MUTED,
        };

        if let Some(percent) = self.battery.percent {
            return self.draw_battery_with_percent(percent, color);
        }

        let label = if self.battery.external_power {
            "USB"
        } else {
            "--"
        };

        let x = WIDTH - self.text_width(label, 2) - 7;
        self.draw_text(label, x, 8, 2, color)
    }

    fn draw_battery_with_percent(&mut self, percent: u8, color: u16) -> Result<()> {
        let mut text = [b' '; 4];
        if percent >= 100 {
            text = *b"100%";
        } else if percent >= 10 {
            text[0] = b'0' + percent / 10;
            text[1] = b'0' + percent % 10;
            text[2] = b'%';
        } else {
            text[0] = b'0' + percent;
            text[1] = b'%';
        }
        let label = core::str::from_utf8(&text).unwrap().trim_end();
        let x = WIDTH - self.text_width(label, 2) - 7;
        self.draw_text(label, x, 8, 2, color)
    }

    fn draw_network_bars(&mut self, x: i32, color: u16) -> Result<()> {
        self.fill_rect(x, 18, 3, 4, color)?;
        self.fill_rect(x + 6, 14, 3, 8, color)?;
        self.fill_rect(x + 12, 10, 3, 12, color)
    }

    fn draw_usb_mark(&mut self, x: i32, color: u16) -> Result<()> {
        self.fill_rect(x, 18, 18, 3, color)?;
        self.fill_rect(x + 7, 11, 4, 10, color)?;
        self.fill_rect(x + 14, 13, 5, 5, color)
    }

    fn draw_progress(&mut self, y: i32, color: u16, phase: i32) -> Result<()> {
        let phase = phase.clamp(0, 63);
        self.fill_rect(18, y, 99, 5, PANEL_3)?;
        self.fill_rect(18 + phase, y, 36, 5, color)
    }

    fn draw_divider(&mut self, y: i32, color: u16) -> Result<()> {
        self.fill_rect(18, y, 99, 1, color)
    }

    fn draw_record_target(&mut self, recording: bool) -> Result<()> {
        let cy = if recording {
            ACTIVE_RECORD_CY
        } else {
            HOME_RECORD_CY
        };
        let outer_radius = if recording { 43 } else { 48 };
        let outer_inner = outer_radius - 8;
        let middle_radius = outer_radius - 15;
        let middle_inner = outer_radius - 22;
        let center_radius = outer_radius - 27;
        let outer = if recording { RED } else { RED_DARK };
        let middle = if recording { RED_SOFT } else { RED };

        self.draw_ring(RECORD_CX, cy, outer_radius, outer_inner, outer, BLACK)?;
        self.draw_ring(RECORD_CX, cy, middle_radius, middle_inner, middle, BLACK)?;
        self.fill_circle(RECORD_CX, cy, center_radius, RED)
    }

    fn draw_level_meter(&mut self, level_percent: u8) -> Result<()> {
        const BARS: i32 = 12;
        let active = ((level_percent.min(100) as i32 * BARS) + 99) / 100;
        let bottom = 197;

        self.fill_rect(9, 161, 117, 40, BLACK)?;
        self.draw_text("LEVEL", 12, 164, 1, MUTED)?;
        for index in 0..BARS {
            let height = 6 + index * 2;
            let x = 13 + index * 9;
            let color = if index < active {
                match index {
                    0..=7 => GREEN,
                    8..=9 => AMBER,
                    _ => RED,
                }
            } else {
                PANEL_3
            };
            self.fill_rect(x, bottom - height, 6, height, color)?;
        }
        Ok(())
    }

    fn draw_buffer_meter(&mut self, queued_frames: usize, max_frames: usize) -> Result<()> {
        let max_frames = max_frames.max(1);
        let queued_frames = queued_frames.min(max_frames);
        let fill = (queued_frames as i32 * 72) / max_frames as i32;
        let color = if queued_frames >= max_frames {
            AMBER
        } else if queued_frames == 0 {
            PANEL_3
        } else {
            CYAN
        };

        self.fill_rect(9, 204, 117, 13, BLACK)?;
        self.draw_text("BUF", 12, 206, 1, MUTED)?;
        self.fill_round_rect(43, 207, 72, 6, 3, PANEL_3)?;
        if fill > 0 {
            self.fill_round_rect(43, 207, fill, 6, 3, color)?;
        }
        Ok(())
    }

    fn draw_elapsed(&mut self, elapsed_secs: u64) -> Result<()> {
        let minutes = (elapsed_secs / 60).min(99);
        let seconds = elapsed_secs % 60;
        let mut text = [b'0'; 5];
        text[0] = b'0' + (minutes / 10) as u8;
        text[1] = b'0' + (minutes % 10) as u8;
        text[2] = b':';
        text[3] = b'0' + (seconds / 10) as u8;
        text[4] = b'0' + (seconds % 10) as u8;
        let text = core::str::from_utf8(&text).unwrap();
        self.draw_centered(text, 123, 4, TEXT)
    }

    fn clear(&mut self, color: u16) -> Result<()> {
        self.fill_rect(0, 0, WIDTH, HEIGHT, color)
    }

    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16) -> Result<()> {
        let x0 = x.clamp(0, WIDTH);
        let y0 = y.clamp(0, HEIGHT);
        let x1 = (x + w).clamp(0, WIDTH);
        let y1 = (y + h).clamp(0, HEIGHT);
        if x1 <= x0 || y1 <= y0 {
            return Ok(());
        }

        let width = (x1 - x0) as usize;
        self.set_window(x0 as u16, y0 as u16, x1 as u16 - 1, y1 as u16 - 1)?;

        let [hi, lo] = color.to_be_bytes();
        let mut line = [0u8; (WIDTH as usize) * 2];
        for pixel in line[..width * 2].chunks_exact_mut(2) {
            pixel[0] = hi;
            pixel[1] = lo;
        }

        self.dc.set_high().context("LCD data mode")?;
        for _ in y0..y1 {
            self.spi
                .write(&line[..width * 2])
                .context("write LCD row")?;
        }
        Ok(())
    }

    fn fill_circle(&mut self, cx: i32, cy: i32, radius: i32, color: u16) -> Result<()> {
        let r2 = radius * radius;
        for dy in -radius..=radius {
            let mut dx = 0;
            while (dx + 1) * (dx + 1) + dy * dy <= r2 {
                dx += 1;
            }
            self.fill_rect(cx - dx, cy + dy, dx * 2 + 1, 1, color)?;
        }
        Ok(())
    }

    fn draw_ring(
        &mut self,
        cx: i32,
        cy: i32,
        outer_radius: i32,
        inner_radius: i32,
        color: u16,
        inner_color: u16,
    ) -> Result<()> {
        self.fill_circle(cx, cy, outer_radius, color)?;
        self.fill_circle(cx, cy, inner_radius, inner_color)
    }

    fn fill_round_rect(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: i32,
        color: u16,
    ) -> Result<()> {
        let radius = radius.min(w / 2).min(h / 2).max(0);
        if radius == 0 {
            return self.fill_rect(x, y, w, h, color);
        }

        self.fill_rect(x + radius, y, w - radius * 2, h, color)?;
        self.fill_rect(x, y + radius, w, h - radius * 2, color)?;
        self.fill_circle(x + radius, y + radius, radius, color)?;
        self.fill_circle(x + w - radius - 1, y + radius, radius, color)?;
        self.fill_circle(x + radius, y + h - radius - 1, radius, color)?;
        self.fill_circle(x + w - radius - 1, y + h - radius - 1, radius, color)
    }

    fn draw_centered(&mut self, text: &str, y: i32, scale: i32, color: u16) -> Result<()> {
        let x = (WIDTH - self.text_width(text, scale)) / 2;
        self.draw_text(text, x, y, scale, color)
    }

    fn draw_text(&mut self, text: &str, x: i32, y: i32, scale: i32, color: u16) -> Result<()> {
        let mut cursor = x;
        for ch in text.chars() {
            self.draw_char(ch, cursor, y, scale, color)?;
            cursor += 6 * scale;
        }
        Ok(())
    }

    fn text_width(&self, text: &str, scale: i32) -> i32 {
        text.chars().count() as i32 * 6 * scale - scale
    }

    fn draw_char(&mut self, ch: char, x: i32, y: i32, scale: i32, color: u16) -> Result<()> {
        let glyph = glyph(ch);
        for row in 0..7 {
            let mut run_start = None;
            for col in 0..5 {
                let filled = glyph[col] & (1 << row) != 0;
                match (filled, run_start) {
                    (true, None) => run_start = Some(col),
                    (false, Some(start)) => {
                        self.fill_rect(
                            x + start as i32 * scale,
                            y + row * scale,
                            (col - start) as i32 * scale,
                            scale,
                            color,
                        )?;
                        run_start = None;
                    }
                    _ => {}
                }
            }
            if let Some(start) = run_start {
                self.fill_rect(
                    x + start as i32 * scale,
                    y + row * scale,
                    (5 - start) as i32 * scale,
                    scale,
                    color,
                )?;
            }
        }
        Ok(())
    }

    fn set_window(&mut self, x0: u16, y0: u16, x1: u16, y1: u16) -> Result<()> {
        let x0 = x0 + X_OFFSET;
        let x1 = x1 + X_OFFSET;
        let y0 = y0 + Y_OFFSET;
        let y1 = y1 + Y_OFFSET;
        self.command(
            0x2a,
            &[(x0 >> 8) as u8, x0 as u8, (x1 >> 8) as u8, x1 as u8],
        )?;
        self.command(
            0x2b,
            &[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8],
        )?;
        self.command(0x2c, &[])
    }

    fn command(&mut self, command: u8, data: &[u8]) -> Result<()> {
        self.dc.set_low().context("LCD command mode")?;
        self.spi.write(&[command]).context("write LCD command")?;
        if !data.is_empty() {
            self.dc.set_high().context("LCD data mode")?;
            self.spi.write(data).context("write LCD data")?;
        }
        Ok(())
    }
}

fn progress_phase(step: usize) -> i32 {
    ((step % 8) as i32) * 9
}

fn glyph(ch: char) -> [u8; 5] {
    match ch.to_ascii_uppercase() {
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00],
        '!' => [0x00, 0x00, 0x5f, 0x00, 0x00],
        '.' => [0x00, 0x60, 0x60, 0x00, 0x00],
        ':' => [0x00, 0x36, 0x36, 0x00, 0x00],
        '%' => [0x23, 0x13, 0x08, 0x64, 0x62],
        '-' => [0x08, 0x08, 0x08, 0x08, 0x08],
        '0' => [0x3e, 0x51, 0x49, 0x45, 0x3e],
        '1' => [0x00, 0x42, 0x7f, 0x40, 0x00],
        '2' => [0x42, 0x61, 0x51, 0x49, 0x46],
        '3' => [0x21, 0x41, 0x45, 0x4b, 0x31],
        '4' => [0x18, 0x14, 0x12, 0x7f, 0x10],
        '5' => [0x27, 0x45, 0x45, 0x45, 0x39],
        '6' => [0x3c, 0x4a, 0x49, 0x49, 0x30],
        '7' => [0x01, 0x71, 0x09, 0x05, 0x03],
        '8' => [0x36, 0x49, 0x49, 0x49, 0x36],
        '9' => [0x06, 0x49, 0x49, 0x29, 0x1e],
        'A' => [0x7e, 0x11, 0x11, 0x11, 0x7e],
        'B' => [0x7f, 0x49, 0x49, 0x49, 0x36],
        'C' => [0x3e, 0x41, 0x41, 0x41, 0x22],
        'D' => [0x7f, 0x41, 0x41, 0x22, 0x1c],
        'E' => [0x7f, 0x49, 0x49, 0x49, 0x41],
        'F' => [0x7f, 0x09, 0x09, 0x09, 0x01],
        'G' => [0x3e, 0x41, 0x49, 0x49, 0x7a],
        'H' => [0x7f, 0x08, 0x08, 0x08, 0x7f],
        'I' => [0x00, 0x41, 0x7f, 0x41, 0x00],
        'J' => [0x20, 0x40, 0x41, 0x3f, 0x01],
        'K' => [0x7f, 0x08, 0x14, 0x22, 0x41],
        'L' => [0x7f, 0x40, 0x40, 0x40, 0x40],
        'M' => [0x7f, 0x02, 0x0c, 0x02, 0x7f],
        'N' => [0x7f, 0x04, 0x08, 0x10, 0x7f],
        'O' => [0x3e, 0x41, 0x41, 0x41, 0x3e],
        'P' => [0x7f, 0x09, 0x09, 0x09, 0x06],
        'Q' => [0x3e, 0x41, 0x51, 0x21, 0x5e],
        'R' => [0x7f, 0x09, 0x19, 0x29, 0x46],
        'S' => [0x46, 0x49, 0x49, 0x49, 0x31],
        'T' => [0x01, 0x01, 0x7f, 0x01, 0x01],
        'U' => [0x3f, 0x40, 0x40, 0x40, 0x3f],
        'V' => [0x1f, 0x20, 0x40, 0x20, 0x1f],
        'W' => [0x3f, 0x40, 0x38, 0x40, 0x3f],
        'X' => [0x63, 0x14, 0x08, 0x14, 0x63],
        'Y' => [0x07, 0x08, 0x70, 0x08, 0x07],
        'Z' => [0x61, 0x51, 0x49, 0x45, 0x43],
        _ => [0x7f, 0x41, 0x5d, 0x41, 0x7f],
    }
}

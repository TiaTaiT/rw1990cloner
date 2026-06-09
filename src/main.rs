#![no_std]
#![no_main]

use panic_probe as _;
use defmt_rtt as _;

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Flex, Input, Pull, Speed};
use embassy_time::{Delay, Timer};
use embedded_hal::delay::DelayNs;

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Rgb888,
    prelude::*,
    text::Text,
};

// Mock Display Wrapper for UI logging
pub struct MockDisplay;

impl DrawTarget for MockDisplay {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, _pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        Ok(())
    }
}

impl OriginDimensions for MockDisplay {
    fn size(&self) -> Size {
        Size::new(800, 480)
    }
}

fn draw_ui_text(lcd: &mut MockDisplay, text: &str, point: Point, style: MonoTextStyle<Rgb888>) {
    info!("[UI DISPLAY] At x: {}, y: {} -> {}", point.x, point.y, text);
    let _ = Text::new(text, point, style).draw(lcd);
}

fn calculate_crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        let mut b = byte;
        for _ in 0..8 {
            let mix = (crc ^ b) & 0x01;
            crc >>= 1;
            if mix != 0 {
                crc ^= 0x8C;
            }
            b >>= 1;
        }
    }
    crc
}

pub struct Rw1990<'d> {
    pin: Flex<'d>,
}

impl<'d> Rw1990<'d> {
    pub fn new(pin: Flex<'d>) -> Self {
        let mut dev = Self { pin };
        dev.pin.set_as_input_output(Speed::VeryHigh);
        dev.pin.set_high();
        dev
    }

    pub fn reset(&mut self, delay: &mut Delay) -> bool {
        self.pin.set_low();
        delay.delay_us(480);
        self.pin.set_high();
        delay.delay_us(70);
        
        let presence = self.pin.is_low();
        delay.delay_us(410);
        
        presence
    }

    pub fn write_bit(&mut self, bit: bool, delay: &mut Delay) {
        if bit {
            self.pin.set_low();
            delay.delay_us(6);
            self.pin.set_high();
            delay.delay_us(64);
        } else {
            self.pin.set_low();
            delay.delay_us(60);
            self.pin.set_high();
            delay.delay_us(10);
        }
    }

    pub fn read_bit(&mut self, delay: &mut Delay) -> bool {
        self.pin.set_low();
        delay.delay_us(6);
        self.pin.set_high();
        delay.delay_us(9);
        let bit = self.pin.is_high();
        delay.delay_us(55);
        bit
    }

    pub fn write_byte(&mut self, byte: u8, delay: &mut Delay) {
        for i in 0..8 {
            self.write_bit((byte >> i) & 1 != 0, delay);
        }
    }

    pub fn read_byte(&mut self, delay: &mut Delay) -> u8 {
        let mut byte = 0;
        for i in 0..8 {
            if self.read_bit(delay) {
                byte |= 1 << i;
            }
        }
        byte
    }

    async fn write_rw1990_bit(&mut self, bit: bool, delay: &mut Delay) {
        if bit {
            self.pin.set_low();
            delay.delay_us(60);
            self.pin.set_high();
        } else {
            self.pin.set_low();
            delay.delay_us(6);
            self.pin.set_high();
        }
        Timer::after_millis(10).await;
    }

    pub async fn write_clone_id(&mut self, new_id: &[u8; 8], delay: &mut Delay) -> bool {
        if !self.reset(delay) { return false; }
        self.write_byte(0xD1, delay);
        self.write_rw1990_bit(false, delay).await;

        if !self.reset(delay) { return false; }
        self.write_byte(0xD5, delay);
        for byte in new_id {
            for bit_idx in 0..8 {
                let bit = (byte >> bit_idx) & 1 != 0;
                self.write_rw1990_bit(bit, delay).await;
            }
        }

        if !self.reset(delay) { return false; }
        self.write_byte(0xD1, delay);
        self.write_rw1990_bit(true, delay).await;
        
        self.reset(delay);
        true
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // 1. Keep debug SWD clocks alive during sleep
    unsafe {
        core::ptr::write_volatile(0xE0042004 as *mut u32, 0x07);
    }

    // 2. Busy-wait at startup to keep the CPU 100% awake for ~3 seconds.
    // This allows probe-rs to safely locate the RTT control block in SRAM 
    // before the Embassy executor puts the CPU into low-power sleep.
    for _ in 0..3 {
        cortex_m::asm::delay(80_000_000); // 80M cycles is ~0.5s - 1.0s depending on clock source
    }

    // 3. System initialization
    let p = embassy_stm32::init(Default::default());
    let mut delay = Delay;

    info!("System Online. Booting cloner software...");

    let mut lcd = MockDisplay;
    let style = MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE);

    draw_ui_text(&mut lcd, "iButton Cloner", Point::new(20, 40), style);
    draw_ui_text(&mut lcd, "Status: IDLE", Point::new(20, 80), style);

    let one_wire_pin = Flex::new(p.PB14);
    let mut cloner = Rw1990::new(one_wire_pin);
    let user_button = Input::new(p.PA0, Pull::Down);

    let mut saved_key = [0u8; 8];
    let mut has_saved_key = false;

    loop {
        if user_button.is_high() {
            if !has_saved_key {
                draw_ui_text(&mut lcd, "Error: No key in memory to write!", Point::new(20, 100), style);
                Timer::after_secs(2).await;
                continue;
            }

            draw_ui_text(&mut lcd, "MODE: WRITING...", Point::new(20, 100), style);
            draw_ui_text(&mut lcd, "Touch BLANK key to write", Point::new(20, 130), style);

            let mut timeout = 0;
            loop {
                if cloner.reset(&mut delay) {
                    draw_ui_text(&mut lcd, "Programming key...", Point::new(20, 160), style);
                    
                    if cloner.write_clone_id(&saved_key, &mut delay).await {
                        if cloner.reset(&mut delay) {
                            cloner.write_byte(0x33, &mut delay);
                            let mut read_back = [0u8; 8];
                            for byte in read_back.iter_mut() {
                                *byte = cloner.read_byte(&mut delay);
                            }
                            
                            if read_back == saved_key {
                                draw_ui_text(&mut lcd, "SUCCESSfully programmed!", Point::new(20, 190), style);
                            } else {
                                draw_ui_text(&mut lcd, "FAILED! Verification mismatched.", Point::new(20, 190), style);
                            }
                        }
                    } else {
                        draw_ui_text(&mut lcd, "FAILED! No response.", Point::new(20, 190), style);
                    }
                    break;
                }
                
                Timer::after_millis(100).await;
                timeout += 1;
                if timeout > 50 {
                    draw_ui_text(&mut lcd, "Write Timeout!", Point::new(20, 160), style);
                    break;
                }
            }
            Timer::after_secs(3).await;
        } else {
            draw_ui_text(&mut lcd, "MODE: SCANNING (Touch Key)", Point::new(20, 100), style);

            if cloner.reset(&mut delay) {
                cloner.write_byte(0x33, &mut delay);
                
                let mut buf = [0u8; 8];
                for byte in buf.iter_mut() {
                    *byte = cloner.read_byte(&mut delay);
                }

                let calc_crc = calculate_crc8(&buf[0..7]);
                if buf[7] == calc_crc && buf[0] != 0 && buf[0] != 0xFF {
                    saved_key = buf;
                    has_saved_key = true;

                    let mut text_buf = [0u8; 32];
                    let key_str = format_key(&mut text_buf, &saved_key);

                    draw_ui_text(&mut lcd, "Key Detected!", Point::new(20, 140), style);
                    draw_ui_text(&mut lcd, key_str, Point::new(20, 170), style);
                    draw_ui_text(&mut lcd, "Press blue button to clone.", Point::new(20, 200), style);
                    
                    Timer::after_secs(2).await;
                } else {
                    draw_ui_text(&mut lcd, "Read error: Bad CRC or noise", Point::new(20, 140), style);
                    Timer::after_millis(500).await;
                }
            }
        }

        Timer::after_millis(200).await;
    }
}

fn format_key<'a>(buf: &'a mut [u8], key: &[u8; 8]) -> &'a str {
    let mut index = 0;
    for (i, &byte) in key.iter().enumerate() {
        let high = byte >> 4;
        let low = byte & 0x0F;
        buf[index] = if high < 10 { b'0' + high } else { b'A' + (high - 10) };
        buf[index + 1] = if low < 10 { b'0' + low } else { b'A' + (low - 10) };
        if i < 7 {
            buf[index + 2] = b':';
            index += 3;
        } else {
            index += 2;
        }
    }
    core::str::from_utf8(&buf[..index]).unwrap_or("FORMAT_ERR")
}
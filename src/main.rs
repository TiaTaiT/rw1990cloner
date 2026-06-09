#![no_std]
#![no_main]

use panic_probe as _;
use defmt_rtt as _;

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Flex, Input, Output, Pull, Speed, Level};
use embassy_time::Timer;

// -----------------------------------------------------------------------------
// Cycle-Accurate Microsecond Delay Helper
// Default Boot Clock = 8 MHz. This means exactly 8 CPU cycles per microsecond.
// -----------------------------------------------------------------------------
#[inline(always)]
fn delay_us(us: u32) {
    cortex_m::asm::delay(us * 8);
}

// -----------------------------------------------------------------------------
// Dallas 1-Wire CRC-8 Calculator
// -----------------------------------------------------------------------------
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

// -----------------------------------------------------------------------------
// High-Precision RW1990 / TM2004 / TM01 Driver
// -----------------------------------------------------------------------------
pub struct Rw1990<'d> {
    pin: Flex<'d>,
}

impl<'d> Rw1990<'d> {
    pub fn new(pin: Flex<'d>) -> Self {
        let mut dev = Self { pin };
        // Disable internal pull-ups because we have a strong external 4.7k resistor to 5V
        dev.pin.set_as_input_output(Speed::VeryHigh);
        dev.pin.set_high();
        dev
    }

    pub fn reset(&mut self) -> bool {
        self.pin.set_low();
        delay_us(480);
        self.pin.set_high();
        delay_us(70);
        
        let presence = self.pin.is_low();
        delay_us(410);
        
        presence
    }

    pub fn write_bit(&mut self, bit: bool) {
        if bit {
            self.pin.set_low();
            delay_us(6);
            self.pin.set_high();
            delay_us(64);
        } else {
            self.pin.set_low();
            delay_us(60);
            self.pin.set_high();
            delay_us(10);
        }
    }

    pub fn read_bit(&mut self) -> bool {
        self.pin.set_low();
        delay_us(6);
        self.pin.set_high();
        delay_us(9);
        let bit = self.pin.is_high();
        delay_us(55);
        bit
    }

    pub fn write_byte(&mut self, byte: u8) {
        for i in 0..8 {
            self.write_bit((byte >> i) & 1 != 0);
        }
    }

    pub fn read_byte(&mut self) -> u8 {
        let mut byte = 0;
        for i in 0..8 {
            if self.read_bit() {
                byte |= 1 << i;
            }
        }
        byte
    }

    // High-power write bit operations for burning EEPROM cells
    async fn write_rw1990_bit(&mut self, bit: bool) {
        if bit {
            // Write 1: Short pulse (15us)
            self.pin.set_low();
            delay_us(15);
            self.pin.set_high();
        } else {
            // Write 0: Long pulse (60us)
            self.pin.set_low();
            delay_us(60);
            self.pin.set_high();
        }
        
        // ---------------------------------------------------------------------
        // Active Push-Pull Power Delivery
        // ---------------------------------------------------------------------
        // Actively drive the pin HIGH (push-pull) instead of letting it float 
        // high. This supplies the raw current (~15mA) required by the key's 
        // internal programming charge pump.
        self.pin.set_as_output(Speed::VeryHigh);
        self.pin.set_high();
        
        // Use a cycle-accurate blocking delay to prevent any async sleep/wake jitter
        delay_us(10_000); // 10ms
        
        // Restore open-drain/input mode for the next bit-slots
        self.pin.set_as_input_output(Speed::VeryHigh);
        self.pin.set_high();
        delay_us(10); // Quick recovery window
    }

    pub async fn write_clone_id(&mut self, new_id: &[u8; 8]) -> bool {
        // Attempt 0: RW1990.1 (V1) - Requires bitwise inverted data stream
        // Attempt 1: RW1990.2 (V2) - Requires standard data stream
        // Attempt 2: TM01 / TM01C  - Unlock command 0xC1, write command 0xC5
        // Attempt 3: TM2004 / RW2004 - Continuous block write using command 0x3C
        for attempt in 0..4 {
            match attempt {
                0 => {
                    info!("Trying RW1990.1 (V1) protocol (with inverted bits)...");
                    // 1. Enable Write Mode (Unlock): Command 0xD1, then LONG pulse
                    if !self.reset() { continue; }
                    self.write_byte(0xD1);
                    self.write_rw1990_bit(true).await; // Long pulse + active pull-up

                    // 2. Transmit ROM ID payload (0xD5 + inverted 64-bit ID)
                    if !self.reset() { continue; }
                    self.write_byte(0xD5);
                    for &byte in new_id {
                        let inverted_byte = !byte; // RW1990.1 expects inverted bytes
                        for bit_idx in 0..8 {
                            let bit = (inverted_byte >> bit_idx) & 1 != 0;
                            self.write_rw1990_bit(bit).await;
                        }
                    }

                    // 3. Disable Write Mode (Lock)
                    if !self.reset() { continue; }
                    self.write_byte(0xD1);
                    self.write_rw1990_bit(false).await; // SHORT pulse + active pull-up
                }
                1 => {
                    info!("Trying RW1990.2 (V2) protocol...");
                    // 1. Enable Write Mode (Unlock): Command 0x1D, then SHORT pulse
                    if !self.reset() { continue; }
                    self.write_byte(0x1D);
                    self.write_rw1990_bit(true).await; // SHORT pulse + active pull-up

                    // 2. Transmit ROM ID payload (0xD5 + standard 64-bit ID)
                    if !self.reset() { continue; }
                    self.write_byte(0xD5);
                    for byte in new_id {
                        for bit_idx in 0..8 {
                            let bit = (byte >> bit_idx) & 1 != 0;
                            self.write_rw1990_bit(bit).await;
                        }
                    }

                    // 3. Disable Write Mode (Lock)
                    if !self.reset() { continue; }
                    self.write_byte(0x1D);
                    self.write_rw1990_bit(false).await; // LONG pulse + active pull-up
                }
                2 => {
                    info!("Trying TM01 / TM01C protocol...");
                    // 1. Enable Write Mode (Unlock): Command 0xC1, then SHORT pulse
                    if !self.reset() { continue; }
                    self.write_byte(0xC1);
                    self.write_rw1990_bit(true).await; // SHORT pulse + active pull-up

                    // 2. Transmit ROM ID payload (0xD5 + standard 64-bit ID)
                    if !self.reset() { continue; }
                    self.write_byte(0xC5); // TM01 write command is 0xC5
                    for byte in new_id {
                        for bit_idx in 0..8 {
                            let bit = (byte >> bit_idx) & 1 != 0;
                            self.write_rw1990_bit(bit).await;
                        }
                    }
                }
                _ => {
                    info!("Trying TM2004 / RW2004 protocol (Continuous stream)...");
                    if self.reset() {
                        // 1. Send Write Memory command
                        self.write_byte(0x3C);
                        // 2. Send 16-bit starting address: 0x0000 (Low, then High)
                        self.write_byte(0x00);
                        self.write_byte(0x00);

                        // 3. Write all 8 bytes continuously in a single block (no resets)
                        for i in 0..8 {
                            self.write_byte(new_id[i]);

                            // Read back the confirmation echo byte returned by TM2004
                            let _echo = self.read_byte();

                            // Required 600us delay before the programming pulse
                            delay_us(600);

                            // Send standard 1-Wire logical '1' to trigger programming
                            self.write_bit(true);

                            // Programming impulse: actively drive the pin HIGH (push-pull) for 50ms
                            self.pin.set_as_output(Speed::VeryHigh);
                            self.pin.set_high();
                            delay_us(50_000); // 50ms precise blocking delay

                            // Restore pin to open-drain mode
                            self.pin.set_as_input_output(Speed::VeryHigh);
                            self.pin.set_high();
                        }
                    }
                }
            }

            // Verification Read-Back after the attempt
            if self.reset() {
                self.write_byte(0x33);
                let mut read_back = [0u8; 8];
                for byte in read_back.iter_mut() {
                    *byte = self.read_byte();
                }

                if &read_back == new_id {
                    let protocol_name = match attempt {
                        0 => "RW1990.1 (V1)",
                        1 => "RW1990.2 (V2)",
                        2 => "TM01 / TM01C",
                        _ => "TM2004 / RW2004",
                    };
                    info!("SUCCESS! Programmed successfully using {} protocol.", protocol_name);
                    return true;
                }
            }
        }

        false
    }
}

// -----------------------------------------------------------------------------
// Main Application Loop
// -----------------------------------------------------------------------------
#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // Write 0x07 to DBGMCU_CR register on STM32F0 (located at 0x40015804)
    // to keep the SWD hardware clocks running during sleep yields.
    unsafe {
        core::ptr::write_volatile(0x40015804 as *mut u32, 0x07);
    }

    // Busy-wait startup loop: keeps the 8MHz clock awake for ~2.5 seconds
    // (5 * 4M cycles = 20M cycles, which is 2.5 seconds on an 8 MHz clock).
    // Allows probe-rs to safely bind to the RTT structure in SRAM.
    for _ in 0..5 {
        cortex_m::asm::delay(4_000_000);
    }

    // Standard clock configuration (defaults safely to 8 MHz)
    let p = embassy_stm32::init(Default::default());

    info!("STM32F0DISCOVERY Cloner Initialized (8 MHz).");

    // Configure onboard user LEDs
    // LD3 (Green) -> PC9
    // LD4 (Blue)  -> PC8
    let mut ld3 = Output::new(p.PC9, Level::Low, Speed::Low);
    let mut ld4 = Output::new(p.PC8, Level::Low, Speed::Low);

    // Configure onboard blue User button (B1) -> PA0
    let user_button = Input::new(p.PA0, Pull::Down);

    // Setup PB14 as the default 1-Wire pin (routed to side headers)
    let one_wire_pin = Flex::new(p.PB9);
    let mut cloner = Rw1990::new(one_wire_pin);

    let mut saved_key = [0u8; 8];
    let mut has_saved_key = false;

    loop {
        if user_button.is_high() {
            // -----------------------------------------------------------------
            // WRITE MODE
            // -----------------------------------------------------------------
            if !has_saved_key {
                warn!("Cannot write: No key saved in memory yet.");
                for _ in 0..2 {
                    ld3.set_high();
                    Timer::after_millis(150).await;
                    ld3.set_low();
                    Timer::after_millis(150).await;
                }
                continue;
            }

            // SAFETY STEP: Wait for the user to lift the original key off the reader
            if cloner.reset() {
                info!("Please REMOVE the original key from the reader...");
                while cloner.reset() {
                    Timer::after_millis(100).await;
                }
                info!("Original key removed!");
            }

            info!("MODE: WRITING... Now touch the BLANK key to clone.");

            let mut timeout = 0;
            loop {
                if cloner.reset() {
                    // Give the user a brief 200ms window to make solid mechanical contact
                    Timer::after_millis(200).await;
                    
                    if cloner.reset() {
                        info!("Writing ID to target...");
                        if cloner.write_clone_id(&saved_key).await {
                            // Success path: Turn on LD4 (Blue) for 2 seconds
                            ld4.set_high();
                            Timer::after_secs(2).await;
                            ld4.set_low();
                        } else {
                            // Error path: Flash LD4 (Blue) rapidly
                            error!("FAILED! Key refused all supported cloning protocols.");
                            for _ in 0..5 {
                                ld4.set_high();
                                Timer::after_millis(100).await;
                                ld4.set_low();
                                Timer::after_millis(100).await;
                            }
                        }
                        break;
                    }
                }
                
                Timer::after_millis(100).await;
                timeout += 1;
                if timeout > 100 { // 10-second write timeout
                    warn!("Write Timeout! No target key detected.");
                    break;
                }
            }
            Timer::after_secs(1).await;
        } else {
            // -----------------------------------------------------------------
            // READ MODE
            // -----------------------------------------------------------------
            if cloner.reset() {
                cloner.write_byte(0x33);
                
                let mut buf = [0u8; 8];
                for byte in buf.iter_mut() {
                    *byte = cloner.read_byte();
                }

                let calc_crc = calculate_crc8(&buf[0..7]);
                if buf[7] == calc_crc && buf[0] != 0 && buf[0] != 0xFF {
                    saved_key = buf;
                    has_saved_key = true;

                    info!("SUCCESS! Key Read: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", 
                        saved_key[0], saved_key[1], saved_key[2], saved_key[3],
                        saved_key[4], saved_key[5], saved_key[6], saved_key[7]
                    );

                    // Turn on LD3 (Green) for 2 seconds to indicate successful read
                    ld3.set_high();
                    Timer::after_secs(2).await;
                    ld3.set_low();
                } else {
                    warn!("Bad CRC or line noise.");
                    Timer::after_millis(500).await;
                }
            }
        }

        Timer::after_millis(200).await;
    }
}
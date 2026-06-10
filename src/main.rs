#![no_std]
#![no_main]

use panic_probe as _;
use defmt_rtt as _;

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Flex, Input, Output, Pull, Speed, Level};
use embassy_time::{Delay, Timer};
use embedded_hal::delay::DelayNs;

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
// RW1990 / DS1990 Driver
// -----------------------------------------------------------------------------
pub struct Rw1990<'d> {
    pin: Flex<'d>,
}

impl<'d> Rw1990<'d> {
    pub fn new(pin: Flex<'d>) -> Self {
        let mut dev = Self { pin };
        // Change from 'set_as_input_output_pull' to standard 'set_as_input_output'
        // This ensures the internal pull-ups are disabled and we rely purely on the 5V external resistor.
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
            delay.delay_us(4);  // Short pulse for '1'
            self.pin.set_high(); // Release line
            
            // --- ACTIVE PULL-UP ---
            // Briefly switch to Push-Pull output to instantly blast power into the clone
            self.pin.set_as_output(Speed::VeryHigh);
            self.pin.set_high();
            delay.delay_us(3);
            // Switch back to Open-Drain
            self.pin.set_as_input_output(Speed::VeryHigh); 
            // ----------------------
            
            delay.delay_us(67); 
        } else {
            self.pin.set_low();
            delay.delay_us(60); // Long pulse for '0' drains the clone's power
            self.pin.set_high(); // Release line
            
            // --- ACTIVE PULL-UP ---
            // Instantly recharge the clone's dead capacitor
            self.pin.set_as_output(Speed::VeryHigh);
            self.pin.set_high();
            delay.delay_us(3);
            self.pin.set_as_input_output(Speed::VeryHigh); 
            // ----------------------
            
            delay.delay_us(37); 
        }
    }

    pub fn read_bit(&mut self, delay: &mut Delay) -> bool {
        self.pin.set_low();
        delay.delay_us(2);   // Shortest possible pulse to initiate read
        self.pin.set_high(); // Release the open-drain line
        
        delay.delay_us(11);  // Wait precisely for Flipper Zero standard sample time
        let bit = self.pin.is_high(); // Sample the line
        
        // INCREASED FROM 55us TO 60us!
        // Gives the clone extra time to release the line and recharge between bits.
        delay.delay_us(60);  
        
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

    // Inverted write timings for burning RW1990 pages
    async fn write_rw1990_bit(&mut self, bit: bool, delay: &mut Delay) {
        if bit {
            // Long pulse (60us)
            self.pin.set_low();
            delay.delay_us(60);
            self.pin.set_high();
        } else {
            // Short pulse (15us)
            // Increased from 6us to 15us to ensure the weak internal pull-up has time to recover.
            self.pin.set_low();
            delay.delay_us(15);
            self.pin.set_high();
        }
        Timer::after_millis(10).await; // Pause for EEPROM cell programming
    }

    /// Protocol for TM2004 / RW2004 keys
    pub async fn write_tm2004_id(&mut self, new_id: &[u8; 8], delay: &mut Delay) -> bool {
        // 1. TM2004 requires a standard Read ROM (0x33) to transition to extended mode
        if !self.reset(delay) { return false; }
        self.write_byte(0x33, delay);
        for _ in 0..8 {
            self.read_byte(delay);
        }
        
        // 2. Command 0x3C is Write ROM for TM2004
        if !self.reset(delay) { return false; }
        self.write_byte(0x3C, delay);
        
        for byte in new_id {
            // TM2004 uses STANDARD 1-Wire write timings (not inverted!)
            for i in 0..8 {
                let bit = (byte >> i) & 1 != 0;
                
                // Write standard bit, but we add an EEPROM delay afterward
                if bit {
                    self.pin.set_low(); delay.delay_us(6); self.pin.set_high(); delay.delay_us(64);
                } else {
                    self.pin.set_low(); delay.delay_us(60); self.pin.set_high(); delay.delay_us(10);
                }
                
                Timer::after_millis(10).await; // 10ms for EEPROM burn
            }
        }
        
        self.reset(delay);
        true
    }

    /// Protocol for RW1990.2 keys
    pub async fn write_rw1990_2_id(&mut self, new_id: &[u8; 8], delay: &mut Delay) -> bool {
        // 1. Enable Write Mode: Send Command 0x1D, then a LONG pulse (true)
        if !self.reset(delay) { return false; }
        self.write_byte(0x1D, delay);
        self.write_rw1990_bit(true, delay).await;

        // 2. Transmit ROM ID payload (0xD5 + 64-bit ID)
        if !self.reset(delay) { return false; }
        self.write_byte(0xD5, delay);
        for byte in new_id {
            for bit_idx in 0..8 {
                let bit = (byte >> bit_idx) & 1 != 0;
                self.write_rw1990_bit(bit, delay).await;
            }
        }

        // 3. Disable Write Mode (Lock): Send Command 0x1D, then a SHORT pulse (false)
        if !self.reset(delay) { return false; }
        self.write_byte(0x1D, delay);
        self.write_rw1990_bit(false, delay).await;
        
        self.reset(delay);
        true
    }
    
    // Helper to read and verify ID against what we just wrote
    pub fn verify_id(&mut self, target_id: &[u8; 8], delay: &mut Delay) -> bool {
        if self.reset(delay) {
            self.write_byte(0x33, delay);
            let mut read_back = [0u8; 8];
            for byte in read_back.iter_mut() {
                *byte = self.read_byte(delay);
            }
            return &read_back == target_id;
        }
        false
    }

    pub async fn write_clone_id(&mut self, new_id: &[u8; 8], delay: &mut Delay) -> bool {
        // 1. Enable Write Mode: Send Command 0xD1, then a LONG pulse (true)
        if !self.reset(delay) { return false; }
        self.write_byte(0xD1, delay);
        self.write_rw1990_bit(true, delay).await; // FIXED: Changed from false to true

        // 2. Transmit ROM ID payload (0xD5 + 64-bit ID)
        if !self.reset(delay) { return false; }
        self.write_byte(0xD5, delay);
        for byte in new_id {
            for bit_idx in 0..8 {
                let bit = (byte >> bit_idx) & 1 != 0;
                self.write_rw1990_bit(bit, delay).await;
            }
        }

        // 3. Disable Write Mode (Lock): Send Command 0xD1, then a SHORT pulse (false)
        if !self.reset(delay) { return false; }
        self.write_byte(0xD1, delay);
        self.write_rw1990_bit(false, delay).await; // FIXED: Changed from true to false
        
        self.reset(delay);
        true
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

    // Busy-wait startup loop: keeps the 48MHz Cortex-M0 awake for ~2.5 seconds
    // to allow probe-rs to safely bind to the RTT structure in SRAM.
    for _ in 0..5 {
        cortex_m::asm::delay(24_000_000);
    }

    let p = embassy_stm32::init(Default::default());
    let mut delay = Delay;

    info!("STM32F0DISCOVERY Cloner Initialized.");

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
                // Flash the green LED (LD3) twice to signal error
                for _ in 0..2 {
                    ld3.set_high();
                    Timer::after_millis(150).await;
                    ld3.set_low();
                    Timer::after_millis(150).await;
                }
                continue;
            }

            info!("MODE: WRITING... Touch blank key to clone.");

            let mut timeout = 0;
            loop {
                if cloner.reset(&mut delay) {
                    info!("Attempting to write...");
                    let mut success = false;
    
                    // Attempt 1: Standard RW1990.1
                    if cloner.write_clone_id(&saved_key, &mut delay).await {
                        if cloner.verify_id(&saved_key, &mut delay) {
                            info!("SUCCESS using RW1990.1 Protocol.");
                            success = true;
                        }
                    }

                    // Attempt 2: TM2004 / RW2004
                    if !success && cloner.write_tm2004_id(&saved_key, &mut delay).await {
                        if cloner.verify_id(&saved_key, &mut delay) {
                            info!("SUCCESS using TM2004 Protocol.");
                            success = true;
                        }
                    }

                    // Attempt 3: RW1990.2
                    if !success && cloner.write_rw1990_2_id(&saved_key, &mut delay).await {
                        if cloner.verify_id(&saved_key, &mut delay) {
                            info!("SUCCESS using RW1990.2 Protocol.");
                            success = true;
                        }
                    }

                    if success {
                        // Blink LED Blue for success
                        ld4.set_high();
                        Timer::after_secs(2).await;
                        ld4.set_low();
                    } else {
                        error!("FAILED! Key rejected all known write protocols.");
                        for _ in 0..5 {
                            ld4.set_high(); Timer::after_millis(100).await;
                            ld4.set_low(); Timer::after_millis(100).await;
                        }
                    }
                    break;
                }
                
                Timer::after_millis(100).await;
                timeout += 1;
                if timeout > 50 {
                    warn!("Write Timeout! No key detected.");
                    break;
                }
            }
            Timer::after_secs(1).await;
        } else {
            // -----------------------------------------------------------------
            // READ MODE
            // -----------------------------------------------------------------
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

                    info!("SUCCESS! Key Read: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", 
                        saved_key[0], saved_key[1], saved_key[2], saved_key[3],
                        saved_key[4], saved_key[5], saved_key[6], saved_key[7]
                    );

                    // Turn on LD3 (Green) for 2 seconds to indicate successful read
                    ld3.set_high();
                    Timer::after_secs(2).await;
                    ld3.set_low();
                } else {
                    warn!("Bad CRC or line noise. Read: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", 
                        buf[0], buf[1], buf[2], buf[3],
                        buf[4], buf[5], buf[6], buf[7]
                    );
                    Timer::after_millis(500).await;
                }
            }
        }

        Timer::after_millis(200).await;
    }
}
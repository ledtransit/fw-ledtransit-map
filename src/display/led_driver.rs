use defmt::error;
use esp_hal::{
    Blocking,
    gpio::Level,
    ram,
    rmt::{Channel, PulseCode, Tx},
};
use rgb::{Grb, RGB8};

pub struct LedDriver<'ch> {
    channel: Option<Channel<'ch, Blocking, Tx>>,
    pulses: Pulses,
}

pub struct Pulses {
    pulse_zero: PulseCode,
    pulse_one: PulseCode,
    pulse_reset: PulseCode,
}

// LED parameters as per datasheet:
// - 0 bit: 300 ns high, 900 ns low
// - 1 bit: 900 ns high, 300 ns low
// - 0/1 cycle: 1.2 us, < 20 us
// - reset: > 200 us low
// - color: GRB MSB

pub fn ws2812_pulses(clock_mhz: u32) -> Pulses {
    Pulses {
        pulse_zero: PulseCode::new(
            Level::High,
            ((300 * clock_mhz) / 1000) as u16,
            Level::Low,
            ((900 * clock_mhz) / 1000) as u16,
        ),
        pulse_one: PulseCode::new(
            Level::High,
            ((900 * clock_mhz) / 1000) as u16,
            Level::Low,
            ((300 * clock_mhz) / 1000) as u16,
        ),
        pulse_reset: PulseCode::new(
            Level::High,
            (60 * clock_mhz) as u16,
            Level::Low,
            (200 * clock_mhz) as u16,
        ),
    }
}

impl<'ch> LedDriver<'ch> {
    pub fn new(channel: Channel<'ch, Blocking, Tx>, pulses: Pulses) -> Self {
        Self {
            channel: Some(channel),
            pulses,
        }
    }

    #[inline(always)]
    fn encode_color(&self, color: RGB8) -> [PulseCode; 3 * 8 + 1] {
        let mut rmt_buffer = [PulseCode::end_marker(); 3 * 8 + 1];
        let mut rmt_iter = rmt_buffer.iter_mut();
        let grb: Grb<u8> = color.into();
        let bytes = [grb.g, grb.r, grb.b];
        for byte in bytes {
            for pos in [128, 64, 32, 16, 8, 4, 2, 1] {
                *rmt_iter.next().unwrap() = if byte & pos != 0 {
                    self.pulses.pulse_one
                } else {
                    self.pulses.pulse_zero
                };
            }
        }
        rmt_buffer
    }

    /// Writes the given colors to the LED strip.
    /// Timing: Only call within critical section.
    #[ram]
    pub fn write(&mut self, mut iter: impl Iterator<Item = RGB8>) {
        // Transmit a dummy high pulse to warm up RMT/GPIO and reset the LEDs
        let channel = self.channel.take().unwrap();
        let rmt_buffer = [self.pulses.pulse_reset, PulseCode::end_marker()];
        self.channel = match channel.transmit(&rmt_buffer).unwrap().wait() {
            Ok(chan) => Some(chan),
            Err((e, chan)) => {
                error!("Failed to transmit to LEDs: {:?}", e);
                Some(chan)
            }
        };

        // Transmit all colors one by one
        // Note: Within critical section, driver placed in RAM and RMT primed, the blocking transmit loop is fast enough to meet the <20 us code cycle time.
        // Any slower and the LEDs have a chance to interpret the low time between code cycles as a reset which would cause a glitch in the LED strip.
        // That way we don't have to maintain a large buffer of all pulse codes in RAM (e.g. 4 bytes per pulse code * 8 bit per channel * 3 channels * 1000 LEDs = 96 kB)
        let mut next_buf = Some(self.encode_color(iter.next().unwrap()));
        while let Some(rmt_buf) = next_buf {
            let channel = self.channel.take().unwrap();
            let transaction = channel.transmit(&rmt_buf).unwrap();
            next_buf = iter.next().map(|color| self.encode_color(color));
            self.channel = match transaction.wait() {
                Ok(chan) => Some(chan),
                Err((e, chan)) => {
                    error!("Failed to transmit to LEDs: {:?}", e);
                    Some(chan)
                }
            };
        }
    }
}

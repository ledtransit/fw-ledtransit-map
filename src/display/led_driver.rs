// WS2812 LEDs, driven by SPI: each data bit is 4 SPI bits at 3.2 MHz
// (312.5 ns each), high for 1 (0 bit) or 3 (1 bit) of them. DMA sends the whole
// encoded frame on its own, so neither interrupts nor a debugger halting the
// CPU (e.g. for RTT logging) break the timing.
use defmt::error;
use esp_hal::{
    Async,
    gpio::AnyPin,
    peripherals::{DMA_CH0, SPI2},
    ram,
    spi::master::{Config, Spi, SpiDma},
    time::Rate,
};
use rgb::{Grb, RGB8};

const SPI_FREQUENCY: Rate = Rate::from_khz(3200);
// Low before the data, so the LEDs start at the first one: > 200 us
const RESET_LEN: usize = 80;
const BYTES_PER_LED: usize = 3 * 4;
// SPI bytes for two data bits, MSB first
const BIT_PAIRS: [u8; 4] = [0b1000_1000, 0b1000_1110, 0b1110_1000, 0b1110_1110];
// The 4 SPI bytes of each data byte (big-endian). In RAM, not flash: read for
// each channel of each LED every frame
#[ram]
static BYTE_SPI_WORDS: [u32; 256] = byte_spi_words();

const fn byte_spi_words() -> [u32; 256] {
    let mut words = [0; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut i = 0;
        while i < 4 {
            words[byte] = (words[byte] << 8) | BIT_PAIRS[(byte >> (6 - 2 * i)) & 0b11] as u32;
            i += 1;
        }
        byte += 1;
    }
    words
}

pub const fn frame_len(led_count: usize) -> usize {
    RESET_LEN + led_count * BYTES_PER_LED
}

pub struct LedDriver {
    spi: SpiDma<'static, Async>,
    // In RAM (DMA can't read flash), zero-initialized: the reset stays low
    frame: &'static mut [u8],
    frame_len: usize,
}

impl LedDriver {
    pub fn new(
        spi: SPI2<'static>,
        dma_channel: DMA_CH0<'static>,
        pin: AnyPin<'static>,
        frame: &'static mut [u8],
    ) -> Self {
        let spi = Spi::new(spi, Config::default().with_frequency(SPI_FREQUENCY))
            .expect("Failed to initialize SPI")
            .with_mosi(pin)
            .with_dma(dma_channel)
            .into_async();
        Self {
            spi,
            frame,
            frame_len: RESET_LEN,
        }
    }

    /// Encodes the colors into the frame, for the next [`Self::transmit`].
    pub fn encode(&mut self, colors: impl Iterator<Item = RGB8>) {
        let led_frames = self.frame[RESET_LEN..].chunks_exact_mut(BYTES_PER_LED);
        let mut led_count = 0;
        for (color, led_frame) in colors.zip(led_frames) {
            encode_led(led_frame, color);
            led_count += 1;
        }
        self.frame_len = frame_len(led_count);
    }

    /// Encodes the colors from the LED at the index on (the frame then ends
    /// after them), each channel's value mapped through the table first. The
    /// mapped values, summed up per channel.
    pub fn encode_mapped(
        &mut self,
        first_led: usize,
        colors: &[RGB8],
        channel_map: &[u8; 256],
    ) -> [u32; 3] {
        let start = RESET_LEN + first_led * BYTES_PER_LED;
        let end = start + colors.len() * BYTES_PER_LED;
        self.frame_len = end;
        encode_mapped(&mut self.frame[start..end], colors, channel_map)
    }

    /// Sends the encoded frame. The LEDs show it once the line stays low after it.
    pub async fn transmit(&mut self) {
        if let Err(e) = self.spi.write_async(&self.frame[..self.frame_len]).await {
            error!("Failed to transmit to LEDs: {:?}", e);
        }
    }
}

fn encode_led(led_frame: &mut [u8], color: RGB8) {
    let grb: Grb<u8> = color.into();
    let channels = [grb.g, grb.r, grb.b];
    for (byte, out) in channels.into_iter().zip(led_frame.chunks_exact_mut(4)) {
        out.copy_from_slice(&BYTE_SPI_WORDS[usize::from(byte)].to_be_bytes());
    }
}

// Runs for each LED every frame: in RAM (not run from flash through the cache),
// as one loop
#[ram]
fn encode_mapped(frame: &mut [u8], colors: &[RGB8], channel_map: &[u8; 256]) -> [u32; 3] {
    let mut sums = [0u32; 3];
    for (color, led_frame) in colors.iter().zip(frame.chunks_exact_mut(BYTES_PER_LED)) {
        let mapped = RGB8::new(
            channel_map[usize::from(color.r)],
            channel_map[usize::from(color.g)],
            channel_map[usize::from(color.b)],
        );
        sums[0] += u32::from(mapped.r);
        sums[1] += u32::from(mapped.g);
        sums[2] += u32::from(mapped.b);
        encode_led(led_frame, mapped);
    }
    sums
}

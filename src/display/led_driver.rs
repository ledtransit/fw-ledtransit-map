// WS2812 LEDs, driven by SPI: each data bit is 4 SPI bits at 3.2 MHz
// (312.5 ns each), high for 1 (0 bit) or 3 (1 bit) of them. DMA sends the whole
// encoded frame on its own, so neither interrupts nor a debugger halting the
// CPU (e.g. for RTT logging) break the timing.
use defmt::error;
use esp_hal::{
    Async,
    gpio::AnyPin,
    peripherals::{DMA_CH0, SPI2},
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
            let grb: Grb<u8> = color.into();
            let channels = [grb.g, grb.r, grb.b];
            for (byte, out) in channels.into_iter().zip(led_frame.chunks_exact_mut(4)) {
                for (i, out) in out.iter_mut().enumerate() {
                    *out = BIT_PAIRS[usize::from((byte >> (6 - 2 * i)) & 0b11)];
                }
            }
            led_count += 1;
        }
        self.frame_len = frame_len(led_count);
    }

    /// Sends the encoded frame. The LEDs show it once the line stays low after it.
    pub async fn transmit(&mut self) {
        if let Err(e) = self.spi.write_async(&self.frame[..self.frame_len]).await {
            error!("Failed to transmit to LEDs: {:?}", e);
        }
    }
}

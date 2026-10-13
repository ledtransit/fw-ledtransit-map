// The chip's internal temperature sensor. esp-hal has no driver for it on the
// ESP32-C3: set up as ESP-IDF's (temperature_sensor), measuring 20..100 °C
// (±2 °C), with the factory calibration from eFuse
use esp_hal::{
    efuse,
    peripherals::{APB_SARADC, LPWR, SYSTEM},
};

// The range's DAC setting, and its offset in the conversion (ESP-IDF's
// temperature_sensor_attributes)
const TSENS_DAC: u8 = 7;
const TSENS_DAC_OFFSET: f32 = -1.0;
// The raw value to °C (ESP-IDF's TEMPERATURE_SENSOR_LL_*_FACTOR)
const ADC_FACTOR: f32 = 0.4386;
const DAC_FACTOR: f32 = 27.88;
const OFFSET_FACTOR: f32 = 20.52;
// The DAC's analog register: I2C_SARADC_TSENS_DAC, bits 3..0
const I2C_SAR_ADC: u8 = 0x69;
const I2C_SAR_ADC_HOST_ID: u8 = 0;
const I2C_SARADC_TSENS_DAC_REG: u8 = 6;

unsafe extern "C" {
    // In ROM (esp_rom_regi2c_write_mask)
    fn rom_i2c_writeReg_Mask(block: u8, host_id: u8, reg_add: u8, msb: u8, lsb: u8, data: u8);
}

/// Powers the sensor up: it measures continuously from then on.
pub fn init() {
    // The WiFi driver also writes analog registers (and the clock enables)
    critical_section::with(|_| {
        let system = SYSTEM::regs();
        system
            .perip_clk_en1()
            .modify(|_, w| w.tsens_clk_en().set_bit());
        system
            .perip_rst_en1()
            .modify(|_, w| w.tsens_rst().clear_bit());
        // The analog I2C to the SAR ADC, for the DAC (also on for the TRNG)
        LPWR::regs()
            .ana_conf()
            .modify(|_, w| w.sar_i2c_pu().set_bit());
        // SAFETY: A ROM function, writing the DAC's bits only
        unsafe {
            rom_i2c_writeReg_Mask(
                I2C_SAR_ADC,
                I2C_SAR_ADC_HOST_ID,
                I2C_SARADC_TSENS_DAC_REG,
                3,
                0,
                TSENS_DAC,
            );
        }
        let saradc = APB_SARADC::regs();
        // Clocked from the crystal
        saradc.tsens_ctrl2().modify(|_, w| w.clk_sel().set_bit());
        saradc.tsens_ctrl().modify(|_, w| w.pu().set_bit());
    });
}

/// The chip's temperature now.
pub fn read_celsius() -> f32 {
    let raw = APB_SARADC::regs().tsens_ctrl().read().out().bits();
    ADC_FACTOR * f32::from(raw)
        - DAC_FACTOR * TSENS_DAC_OFFSET
        - OFFSET_FACTOR
        - calibration_celsius()
}

// The factory calibration (ESP-IDF's esp_efuse_rtc_calib_get_tsens_val): in
// 0.1 °C, the 9th bit the sign. Only with the calibration version 1
fn calibration_celsius() -> f32 {
    if efuse::read_field_le::<u8>(efuse::BLK_VERSION_MAJOR) != 1 {
        return 0.0;
    }
    let calibration = efuse::read_field_le::<u16>(efuse::TEMP_CALIB);
    let magnitude = f32::from(calibration as u8) / 10.0;
    if calibration & 0x100 != 0 {
        -magnitude
    } else {
        magnitude
    }
}

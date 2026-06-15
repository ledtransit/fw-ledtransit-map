use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::gpio::{AnyPin, Input, InputConfig, Pull};

static BUTTON_SIGNAL: Signal<CriticalSectionRawMutex, ButtonPress> = Signal::new();
static BUTTON_STATE: Mutex<CriticalSectionRawMutex, ButtonsPressedState> =
    Mutex::new(ButtonsPressedState {
        up: false,
        down: false,
        select: false,
    });

pub enum ButtonPress {
    Short(Button),
    Long(Button),
    CombinedLong(ButtonsPressedState), // More than one button held for long press
}

#[derive(defmt::Format, Clone, Copy)]
pub enum Button {
    Up,
    Middle,
    Down,
}

#[derive(defmt::Format, Clone, Copy, Default)]
pub struct ButtonsPressedState {
    pub up: bool,
    pub down: bool,
    pub select: bool,
}

impl ButtonsPressedState {
    fn set_pressed(&mut self, button: Button, pressed: bool) {
        match button {
            Button::Up => self.up = pressed,
            Button::Middle => self.select = pressed,
            Button::Down => self.down = pressed,
        }
    }

    fn is_pressed(&self, button: Button) -> bool {
        match button {
            Button::Up => self.up,
            Button::Middle => self.select,
            Button::Down => self.down,
        }
    }

    fn is_other_pressed(&self, exclude: Button) -> bool {
        match exclude {
            Button::Up => self.down || self.select,
            Button::Middle => self.up || self.down,
            Button::Down => self.up || self.select,
        }
    }

    fn clear(&mut self) {
        *self = Default::default();
    }
}

const LONG_PRESS_DURATION_MS: u64 = 2000;
const DEBOUNCE_DURATION_MS: u64 = 10;

#[embassy_executor::task(pool_size = 3)]
async fn button_task(pin: AnyPin<'static>, button: Button) {
    // Configure input with pull-up (active low)
    let mut input = Input::new(pin, InputConfig::default().with_pull(Pull::Up));

    loop {
        input.wait_for_falling_edge().await; // Begin press
        BUTTON_STATE.lock().await.set_pressed(button, true);

        if input.is_high() {
            BUTTON_STATE.lock().await.set_pressed(button, false);
            continue; // Ignore if button was released immediately (bounce)
        }

        // Wait for button release or long press timeout
        match with_timeout(
            Duration::from_millis(LONG_PRESS_DURATION_MS),
            input.wait_for_rising_edge(),
        )
        .await
        {
            Ok(_) => {
                // Button released - emit short press
                BUTTON_SIGNAL.signal(ButtonPress::Short(button));
            }
            Err(_) => {
                // Timeout - long press
                if !BUTTON_STATE.lock().await.is_pressed(button) {
                    continue; // Long press absorbed by other button's combined long press event
                }

                // If another button is also pressed, emit combined long press event
                if BUTTON_STATE.lock().await.is_other_pressed(button) {
                    BUTTON_SIGNAL.signal(ButtonPress::CombinedLong(*BUTTON_STATE.lock().await));
                    BUTTON_STATE.lock().await.clear(); // Clear state to prevent multiple events
                } else {
                    // Otherwise, emit regular long press event
                    BUTTON_SIGNAL.signal(ButtonPress::Long(button));
                }
            }
        };

        // Debounce delay
        Timer::after(Duration::from_millis(DEBOUNCE_DURATION_MS)).await;
        BUTTON_STATE.lock().await.set_pressed(button, false);
    }
}

pub fn spawn(spawner: Spawner, pin: AnyPin<'static>, button: Button) {
    spawner.spawn(button_task(pin, button).unwrap());
}

pub async fn wait_for_button_press() -> ButtonPress {
    BUTTON_SIGNAL.wait().await
}

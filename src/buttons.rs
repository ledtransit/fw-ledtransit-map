use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::gpio::{AnyPin, Input, InputConfig, Pull};

const LONG_PRESS_DURATION: Duration = Duration::from_millis(2000);
const DEBOUNCE_DURATION: Duration = Duration::from_millis(10);

static BUTTON_SIGNAL: Signal<CriticalSectionRawMutex, ButtonPress> = Signal::new();
static BUTTON_STATE: Mutex<CriticalSectionRawMutex, ButtonsPressedState> =
    Mutex::new(ButtonsPressedState {
        up: false,
        middle: false,
        down: false,
    });

pub enum ButtonPress {
    Short(Button),
    Long(Button),
    CombinedLong(ButtonsPressedState), // More than one button held for a long press
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
    pub middle: bool,
    pub down: bool,
}

impl ButtonsPressedState {
    fn set_pressed(&mut self, button: Button, pressed: bool) {
        match button {
            Button::Up => self.up = pressed,
            Button::Middle => self.middle = pressed,
            Button::Down => self.down = pressed,
        }
    }

    fn is_pressed(&self, button: Button) -> bool {
        match button {
            Button::Up => self.up,
            Button::Middle => self.middle,
            Button::Down => self.down,
        }
    }

    fn is_other_pressed(&self, exclude: Button) -> bool {
        match exclude {
            Button::Up => self.down || self.middle,
            Button::Middle => self.up || self.down,
            Button::Down => self.up || self.middle,
        }
    }

    fn clear(&mut self) {
        *self = Default::default();
    }
}

pub fn spawn(spawner: Spawner, pin: AnyPin<'static>, button: Button) {
    spawner.spawn(button_task(pin, button).unwrap());
}

pub async fn wait_for_button_press() -> ButtonPress {
    BUTTON_SIGNAL.wait().await
}

#[embassy_executor::task(pool_size = 3)]
async fn button_task(pin: AnyPin<'static>, button: Button) {
    // Active low
    let mut input = Input::new(pin, InputConfig::default().with_pull(Pull::Up));

    loop {
        input.wait_for_falling_edge().await;
        BUTTON_STATE.lock().await.set_pressed(button, true);

        if input.is_high() {
            // Bounce
            BUTTON_STATE.lock().await.set_pressed(button, false);
            continue;
        }

        let released = with_timeout(LONG_PRESS_DURATION, input.wait_for_rising_edge())
            .await
            .is_ok();
        if released {
            BUTTON_SIGNAL.signal(ButtonPress::Short(button));
        } else if !signal_long_press(button).await {
            continue;
        }

        Timer::after(DEBOUNCE_DURATION).await;
        BUTTON_STATE.lock().await.set_pressed(button, false);
    }
}

/// Signals a long press of the button, or a combined one with the other
/// buttons held. False if another button's combined long press absorbed it.
async fn signal_long_press(button: Button) -> bool {
    if !BUTTON_STATE.lock().await.is_pressed(button) {
        return false;
    }

    if BUTTON_STATE.lock().await.is_other_pressed(button) {
        BUTTON_SIGNAL.signal(ButtonPress::CombinedLong(*BUTTON_STATE.lock().await));
        // Only one event for all buttons held
        BUTTON_STATE.lock().await.clear();
    } else {
        BUTTON_SIGNAL.signal(ButtonPress::Long(button));
    }
    true
}

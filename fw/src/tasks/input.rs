use crate::{Button, InputEvent, INPUT_EVENTS};
use embassy_time::{Instant, Timer};
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcCalScheme, AdcPin};
use esp_hal::gpio::{GpioPin, Input};
use esp_hal::peripherals::ADC1;

const POLL_MS: u64 = 40;
const CALIBRATION_ONLY: bool = false;
const RAW_LOG_ENABLED: bool = false;
const DEBOUNCE_TICKS: u8 = 2;
const REPEAT_COOLDOWN_TICKS: u8 = 12;
const NAV_BACK_MIN_MV: u16 = 2400;
const NAV_BACK_MAX_MV: u16 = 2700;
const RAW_LOG_TICKS: u8 = 25;

#[derive(Clone, Copy)]
struct Band {
    min: u16,
    max: u16,
    button: HardwareButton,
}

pub struct InputPins {
    pub power: Input<'static>,
    pub aux_pin: AdcPin<GpioPin<0>, ADC1, AdcCalCurve<ADC1>>,
    pub nav_pin: AdcPin<GpioPin<1>, ADC1, AdcCalCurve<ADC1>>,
    pub page_pin: AdcPin<GpioPin<2>, ADC1, AdcCalCurve<ADC1>>,
}

#[derive(Clone, Copy)]
struct RawSample {
    aux: u16,
    nav: u16,
    page: u16,
}

struct StableButton {
    candidate: Option<HardwareButton>,
    current: Option<HardwareButton>,
    ticks: u8,
    cooldown: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HardwareButton {
    Back,
    Confirm,
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum FrontLayout {
    BackConfirmLeftRight,
    BackConfirmRightLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum SideLayout {
    PrevNext,
    NextPrev,
}

const FRONT_LAYOUT: FrontLayout = FrontLayout::BackConfirmLeftRight;
const SIDE_LAYOUT: SideLayout = SideLayout::PrevNext;

const NAV: &[Band] = &[
    // X4 front-button ladder on GPIO1. These bands scale Adafruit's current
    // 16-bit CircuitPython X4 thresholds to the 12-bit esp-hal ADC reads.
    Band {
        min: NAV_BACK_MIN_MV,
        max: NAV_BACK_MAX_MV,
        button: HardwareButton::Back,
    },
    Band {
        min: 1800,
        max: 2150,
        button: HardwareButton::Confirm,
    },
    Band {
        min: 1000,
        max: 1250,
        button: HardwareButton::Left,
    },
    Band {
        min: 0,
        max: 100,
        button: HardwareButton::Right,
    },
];

const PAGE: &[Band] = &[
    // X4 side-button ladder on GPIO2, scaled from Adafruit's thresholds.
    Band {
        min: 1500,
        max: 1800,
        button: HardwareButton::Up,
    },
    Band {
        min: 0,
        max: 100,
        button: HardwareButton::Down,
    },
];

#[embassy_executor::task]
pub async fn run(mut adc: Adc<'static, ADC1>, mut pins: InputPins) {
    esp_println::println!("input: started");

    let mut last_power = false;
    let mut power_ticks = 0u8;
    let mut nav_stable = StableButton::new();
    let mut page_stable = StableButton::new();
    let mut raw_log_ticks = 0u8;

    loop {
        Timer::after_millis(POLL_MS).await;

        let sample = RawSample {
            aux: read_adc(&mut adc, &mut pins.aux_pin).await,
            nav: read_adc(&mut adc, &mut pins.nav_pin).await,
            page: read_adc(&mut adc, &mut pins.page_pin).await,
        };
        if RAW_LOG_ENABLED {
            raw_log_ticks = raw_log_ticks.wrapping_add(1);
            if raw_log_ticks >= RAW_LOG_TICKS {
                raw_log_ticks = 0;
                esp_println::println!(
                    "input raw: gpio0={} gpio1={} gpio2={}",
                    sample.aux,
                    sample.nav,
                    sample.page,
                );
            }
        }

        if CALIBRATION_ONLY {
            continue;
        }

        let power_pressed = debounce_active_low(pins.power.is_low(), &mut power_ticks);
        if power_pressed && !last_power {
            emit(Some(Button::Power), sample);
            log_input(Some(Button::Power), sample);
        }
        last_power = power_pressed;

        let nav = nav_stable.update(classify(sample.nav, NAV));
        if let Some(nav) = nav {
            let StableEvent::Changed(hardware) = nav;
            let button = map_hardware(hardware);
            emit(Some(button), sample);
            log_input(Some(button), sample);
        }

        let page = page_stable.update(classify(sample.page, PAGE));
        if let Some(page) = page {
            let StableEvent::Changed(hardware) = page;
            let button = map_hardware(hardware);
            emit(Some(button), sample);
            log_input(Some(button), sample);
        }
    }
}

fn emit(button: Option<Button>, sample: RawSample) {
    let event = InputEvent::Sample {
        button,
        aux_raw: sample.aux,
        nav_raw: sample.nav,
        page_raw: sample.page,
        battery_mv: battery_mv(sample.aux),
        battery_percent: battery_percent(sample.aux),
    };
    if INPUT_EVENTS.try_send(event).is_ok() {
        return;
    }
    let _ = INPUT_EVENTS.try_receive();
    if INPUT_EVENTS.try_send(event).is_err() {
        esp_println::println!("input: event queue full");
    }
}

fn battery_mv(aux_mv: u16) -> u16 {
    aux_mv.saturating_mul(2)
}

fn battery_percent(aux_mv: u16) -> u8 {
    let mv = battery_mv(aux_mv).clamp(3300, 4200);
    (((mv - 3300) as u32 * 100) / 900) as u8
}

fn log_input(button: Option<Button>, sample: RawSample) {
    esp_println::println!(
        "input: {:?} gpio0={} gpio1={} gpio2={} t={}",
        button,
        sample.aux,
        sample.nav,
        sample.page,
        Instant::now().as_millis(),
    );
}

enum StableEvent {
    Changed(HardwareButton),
}

impl StableButton {
    const fn new() -> Self {
        Self {
            candidate: None,
            current: None,
            ticks: 0,
            cooldown: 0,
        }
    }

    fn update(&mut self, next: Option<HardwareButton>) -> Option<StableEvent> {
        self.cooldown = self.cooldown.saturating_sub(1);
        if next == self.candidate {
            self.ticks = self.ticks.saturating_add(1);
        } else {
            self.candidate = next;
            self.ticks = 1;
        }

        if self.ticks < DEBOUNCE_TICKS {
            return None;
        }

        if self.candidate != self.current {
            self.current = self.candidate;
            if let Some(current) = self.current {
                self.cooldown = REPEAT_COOLDOWN_TICKS;
                return Some(StableEvent::Changed(current));
            }
            self.cooldown = 0;
            return None;
        }

        let Some(current) = self.current else {
            return None;
        };
        if self.cooldown > 0 || !is_repeatable(current) {
            return None;
        }
        self.cooldown = REPEAT_COOLDOWN_TICKS;
        Some(StableEvent::Changed(current))
    }
}

fn is_repeatable(button: HardwareButton) -> bool {
    matches!(
        button,
        HardwareButton::Left | HardwareButton::Right | HardwareButton::Up | HardwareButton::Down
    )
}

fn debounce_active_low(raw_pressed: bool, ticks: &mut u8) -> bool {
    if raw_pressed {
        *ticks = ticks.saturating_add(1).min(DEBOUNCE_TICKS);
    } else {
        *ticks = ticks.saturating_sub(1);
    }
    *ticks == DEBOUNCE_TICKS
}

fn classify(value: u16, table: &[Band]) -> Option<HardwareButton> {
    for band in table {
        if value >= band.min && value <= band.max {
            return Some(band.button);
        }
    }
    None
}

fn map_hardware(button: HardwareButton) -> Button {
    match button {
        HardwareButton::Back => Button::Back,
        HardwareButton::Confirm => Button::Confirm,
        HardwareButton::Left => match FRONT_LAYOUT {
            FrontLayout::BackConfirmLeftRight => Button::Previous,
            FrontLayout::BackConfirmRightLeft => Button::Next,
        },
        HardwareButton::Right => match FRONT_LAYOUT {
            FrontLayout::BackConfirmLeftRight => Button::Next,
            FrontLayout::BackConfirmRightLeft => Button::Previous,
        },
        HardwareButton::Up => match SIDE_LAYOUT {
            SideLayout::PrevNext => Button::Previous,
            SideLayout::NextPrev => Button::Next,
        },
        HardwareButton::Down => match SIDE_LAYOUT {
            SideLayout::PrevNext => Button::Next,
            SideLayout::NextPrev => Button::Previous,
        },
    }
}

async fn read_adc<P, CS>(adc: &mut Adc<'static, ADC1>, pin: &mut AdcPin<P, ADC1, CS>) -> u16
where
    P: esp_hal::analog::adc::AdcChannel,
    CS: AdcCalScheme<ADC1>,
{
    loop {
        match adc.read_oneshot(pin) {
            Ok(value) => return value,
            Err(nb::Error::WouldBlock) => Timer::after_micros(50).await,
            Err(_) => return 0,
        }
    }
}

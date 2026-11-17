//! Battery & Sensors screen: battery health from the AppleSmartBattery
//! registry entry, plus SMC temperatures and fans. Refreshes every 5 s.
//!
//! Port of `BatteryView`, `BatteryViewModel` and `TemperatureAndFansViewModel`.
//! The screen is read-only: nothing here changes system state.
//!
//! Windows shows the battery only: Windows has no supported API for CPU/GPU
//! temperatures or fan speeds, so those sections are not built there rather
//! than filled with unverifiable numbers.

#[cfg(target_os = "macos")]
use crate::engine::present::{self, temperature_rows, thousands};
#[cfg(windows)]
use crate::engine::{present, read_battery, BatteryInfo};
#[cfg(target_os = "macos")]
use crate::engine::{
    read_battery, read_sensors, BatteryInfo, Condition, Fan, Level, SensorSnapshot, Temperature,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Disableable as _;
#[cfg(target_os = "macos")]
use gpui_kit::component::Sizable as _;
use gpui_kit::*;
use nera_ui::nav::{self, Screen};
use nera_ui::*;
use std::time::Duration;

/// How often the readings are refreshed.
const REFRESH: Duration = Duration::from_secs(5);

struct Reading {
    battery: Option<BatteryInfo>,
    #[cfg(target_os = "macos")]
    sensors: SensorSnapshot,
}

pub struct BatteryScreen {
    reading: Option<Reading>,
    busy: bool,
    /// A user-requested read is in flight (disables the Refresh button; the
    /// 5 s background reads don't, so the button doesn't flicker).
    manual: bool,
    _read_task: Option<Task<()>>,
    _tick_task: Option<Task<()>>,
    _shown: Subscription,
}

impl BatteryScreen {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Screens stay alive after the first visit: read only while shown,
        // and catch up at once when the user comes back.
        let tick = refresh_every(cx, REFRESH, |this, cx| {
            if nav::current(cx) == Screen::Battery {
                this.refresh(cx, false);
            }
        });
        let shown = cx.observe_global::<nav::Nav>(|this, cx| {
            if nav::current(cx) == Screen::Battery {
                this.refresh(cx, false);
            }
        });
        let mut this = Self {
            reading: None,
            busy: false,
            manual: false,
            _read_task: None,
            _tick_task: Some(tick),
            _shown: shown,
        };
        this.refresh(cx, false);
        this
    }

    /// Re-read everything off the UI thread; ignored while a read is running.
    fn refresh(&mut self, cx: &mut Context<Self>, manual: bool) {
        if self.busy {
            // Never start a second read; a click during a background read just
            // waits for it and shows the busy state.
            self.manual |= manual;
            return;
        }
        self.busy = true;
        self.manual = manual;
        self._read_task = Some(run_blocking(
            cx,
            || Reading {
                battery: read_battery(),
                #[cfg(target_os = "macos")]
                sensors: read_sensors(),
            },
            |this, reading, _cx| {
                this.reading = Some(reading);
                this.busy = false;
                this.manual = false;
            },
        ));
    }
}

#[cfg(target_os = "macos")]
fn health_tone(b: &BatteryInfo) -> Tone {
    match b.condition {
        Some((Condition::Normal, _)) => Tone::Success,
        Some((Condition::ServiceRecommended, _)) => Tone::Warning,
        None => Tone::Neutral,
    }
}

fn charge_tone(b: &BatteryInfo) -> Tone {
    if b.is_charging {
        Tone::Accent
    } else if b.percent <= 10.0 {
        Tone::Danger
    } else if b.percent <= 20.0 {
        Tone::Warning
    } else {
        Tone::Success
    }
}

#[cfg(target_os = "macos")]
fn level_tone(level: Level) -> Tone {
    match level {
        Level::Normal => Tone::Success,
        Level::Warm => Tone::Warning,
        Level::Hot => Tone::Danger,
    }
}

fn charge_icon(b: &BatteryInfo) -> &'static str {
    if b.is_charging {
        "battery-charging"
    } else if b.percent <= 10.0 {
        "battery-warning"
    } else if b.percent <= 30.0 {
        "battery-low"
    } else if b.percent <= 70.0 {
        "battery-medium"
    } else {
        "battery-full"
    }
}

#[cfg(target_os = "macos")]
fn hero(b: &BatteryInfo) -> impl IntoElement {
    let cycles_tile = {
        let tile = StatTile::new("Cycle count", thousands(b.cycle_count as u64))
            .icon("refresh-cw")
            .caption(present::cycles_caption(b));
        match present::cycles_fraction(b) {
            Some(used) => tile.tone(Tone::for_usage(used)).meter(used.min(1.0)),
            None => tile,
        }
    };
    let mut health = StatTile::new(present::health_title(b), present::health_value(b))
        .icon("heart-pulse")
        .tone(health_tone(b))
        .caption(present::health_caption(b));
    if let Some(f) = present::health_fraction(b) {
        health = health.meter(f);
    }
    StatGrid::new(4)
        .child(
            StatTile::new("Charge", present::charge_value(b))
                .icon(charge_icon(b))
                .tone(charge_tone(b))
                .meter(b.percent / 100.0)
                .caption(present::charge_caption(b)),
        )
        .child(health)
        .child(cycles_tile)
        .child(
            StatTile::new("Condition", present::condition_value(b))
                .icon("shield")
                .tone(health_tone(b))
                .caption(present::condition_caption(b)),
        )
}

#[cfg(target_os = "macos")]
fn details(b: &BatteryInfo, sensors: &SensorSnapshot, cx: &App) -> impl IntoElement {
    let mut card = Card::new();
    for (label, value) in present::battery_lines(b, sensors) {
        card = card.child(kv_row(label, value, cx));
    }
    card
}

/// Windows: charge, health (full charge vs design capacity, as in
/// `powercfg /batteryreport`) and cycle count. No condition tile: Windows
/// makes no service judgement, so neither does Nera.
#[cfg(windows)]
fn hero(b: &BatteryInfo) -> impl IntoElement {
    let mut charge = StatTile::new("Charge", present::charge_value(b))
        .icon(if b.percent_known {
            charge_icon(b)
        } else {
            "battery"
        })
        .caption(present::charge_caption(b));
    if b.percent_known {
        charge = charge.tone(charge_tone(b)).meter(b.percent / 100.0);
    }
    let mut health = StatTile::new(present::health_title(b), present::health_value(b))
        .icon("heart-pulse")
        .caption(present::health_caption(b));
    if let Some(f) = present::health_fraction(b) {
        health = health.meter(f);
    }
    StatGrid::new(3).child(charge).child(health).child(
        StatTile::new("Cycle count", present::cycles_value(b))
            .icon("refresh-cw")
            .caption(present::cycles_caption(b)),
    )
}

#[cfg(windows)]
fn details(b: &BatteryInfo, cx: &App) -> impl IntoElement {
    let mut card = Card::new();
    for (label, value) in present::battery_lines(b) {
        card = card.child(kv_row(label, value, cx));
    }
    card
}

#[cfg(target_os = "macos")]
fn temperature_row(t: &Temperature) -> Row {
    // Unverified groups get no colour and no badge: see `SensorKind::alert_level`.
    let level = t.kind.alert_level(t.value());
    let tone = level.map(level_tone).unwrap_or(Tone::Neutral);
    let mut row = Row::new(format!("temp-{:?}", t.kind), t.kind.label())
        .icon(t.kind.icon())
        .icon_tone(tone)
        .subtitle(present::temperature_subtitle(t))
        .value(present::celsius(t.value()))
        .value_tone(tone);
    match level {
        Some(Level::Warm) => row = row.badge(Badge::new("Warm").tone(Tone::Warning)),
        Some(Level::Hot) => row = row.badge(Badge::new("Hot").tone(Tone::Danger)),
        Some(Level::Normal) => {}
        None => row = row.badge(Badge::new("Unverified").tone(Tone::Neutral)),
    }
    row
}

#[cfg(target_os = "macos")]
fn fan_row(f: &Fan) -> Row {
    let fraction = present::fan_fraction(f);
    let (value, subtitle) = present::fan_lines(f);
    let row = Row::new(format!("fan-{}", f.index), format!("Fan {}", f.index))
        .icon("fan")
        .subtitle(subtitle)
        .value(value);
    // Without a known maximum speed a meter would be a made-up fraction.
    if f.max > 0.0 {
        row.badge(div().w_40().child(Meter::new(fraction).tone(Tone::Accent)))
    } else {
        row
    }
}

#[cfg(target_os = "macos")]
impl Render for BatteryScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = Page::new("battery-page", Screen::Battery.title())
            .subtitle("Battery health, temperatures and fans · updates every 5 seconds")
            .action(
                Button::new("refresh")
                    .primary()
                    .label("Refresh")
                    .disabled(self.manual)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx, true))),
            );

        let Some(reading) = &self.reading else {
            return page.child(Section::untitled().child(ProgressPanel::new(
                "battery-loading",
                "Reading battery and sensors",
            )));
        };

        let sensors = &reading.sensors;
        let mut page = page;

        if !sensors.smc_available {
            page = page.child(Section::untitled().child(
                Banner::new(Tone::Info, "Sensor data is unavailable").message(
                    "Nera couldn't connect to the system management controller on this Mac.",
                ),
            ));
        }

        // Battery.
        match &reading.battery {
            Some(b) => {
                page = page
                    .child(Section::untitled().child(hero(b)))
                    .child(Section::new("Details").child(details(b, sensors, cx)));
            }
            None => {
                page = page.child(Section::new("Battery").child(
                    EmptyState::new("battery", "No battery").message(
                        "This Mac doesn't have a battery. Temperatures and fans are shown below.",
                    ),
                ));
            }
        }

        // Explain loud fans and high temperatures instead of leaving them unexplained.
        if present::fans_at_full_speed(sensors) {
            page = page.child(
                Section::untitled().child(
                    Banner::new(Tone::Warning, "Fans are running at full speed")
                        .message(
                            "Your Mac is cooling itself because something keeps the CPU or GPU busy, so high temperatures are expected. Check Processes to see what is using them.",
                        )
                        .action(
                            Button::new("show-processes")
                                .small()
                                .outline()
                                .label("Show processes")
                                .on_click(|_, _, cx| nera_ui::nav::navigate(Screen::Processes, cx)),
                        ),
                ),
            );
        }

        // Temperatures.
        let rows = temperature_rows(sensors);
        let mut temps = Section::new("Temperatures");
        if rows.is_empty() {
            temps = temps.child(
                EmptyState::new("thermometer", "No temperature sensors").message(if sensors.smc_available {
                    "This Mac doesn't expose any readable temperature sensors."
                } else {
                    "The system management controller couldn't be opened, so sensor data isn't available."
                }),
            );
        } else {
            temps = temps.description("CPU, GPU and memory show the average of this chip's per-core sensors (the same SMC keys monitoring apps such as Stats read); other groups show their hottest sensor. Battery and storage were cross-checked against macOS's own sensors; Apple does not document the other keys.").child(
                ListCard::new()
                    .header(list_header("temp", "Sensor", Some("Temperature"), None))
                    .children(rows.iter().map(temperature_row)),
            );
        }
        page = page.child(temps);

        // Fans.
        let mut fans = Section::new("Fans");
        if sensors.fans.is_empty() {
            fans = fans.child(EmptyState::new("fan", "No fans").message(if sensors.smc_available {
                "This Mac has no fans, so it cools passively."
            } else {
                "The system management controller couldn't be opened, so fan data isn't available."
            }));
        } else {
            fans = fans.child(
                ListCard::new()
                    .header(list_header("fan", "Fan", Some("Speed"), None))
                    .children(sensors.fans.iter().map(fan_row)),
            );
        }
        page = page.child(fans);

        page
    }
}

#[cfg(windows)]
impl Render for BatteryScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = Page::new("battery-page", Screen::Battery.title())
            .subtitle("Battery charge and health · updates every 5 seconds")
            .action(
                Button::new("refresh")
                    .primary()
                    .label("Refresh")
                    .disabled(self.manual)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx, true))),
            );

        let Some(reading) = &self.reading else {
            return page.child(
                Section::untitled().child(ProgressPanel::new("battery-loading", "Reading battery")),
            );
        };

        let page = match &reading.battery {
            Some(b) => page
                .child(Section::untitled().child(hero(b)))
                .child(Section::new("Details").child(details(b, cx))),
            None => page.child(
                Section::new("Battery").child(
                    EmptyState::new("battery", "No battery")
                        .message("Windows doesn't report a battery on this PC."),
                ),
            ),
        };

        // The screen is "Battery & Sensors" on both platforms: say why the
        // sensor part is missing instead of leaving it out silently.
        page.child(
            Section::new("Temperatures and fans").child(
                EmptyState::new("thermometer", "Not available on Windows").message(
                    "Windows has no supported way to read CPU, GPU or fan sensors, so Nera doesn't show numbers it can't verify.",
                ),
            ),
        )
    }
}

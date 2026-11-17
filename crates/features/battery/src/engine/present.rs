//! Text and aggregation helpers shared by the screen (pure, unit-tested).

use super::battery::{BatteryInfo, Health, Source};
use super::sensors::{Fan, SensorKind, SensorSnapshot, Temperature};

/// "1h 23m", "47m". `None`/0 minutes yields `None`.
pub fn duration_minutes(minutes: u32) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Time-remaining line for the battery ("1h 5m until full" / "47m remaining").
pub fn time_remaining(b: &BatteryInfo) -> Option<String> {
    if b.fully_charged && b.external_connected {
        return None;
    }
    match (b.minutes_remaining, b.is_charging, b.external_connected) {
        (Some(m), true, _) => Some(format!("{} until full", duration_minutes(m))),
        (Some(m), false, false) => Some(format!("{} remaining", duration_minutes(m))),
        (None, false, false) => Some("Calculating…".to_string()),
        (None, true, _) => Some("Calculating…".to_string()),
        _ => None,
    }
}

/// Integer with thousands separators: 5615 -> "5,615".
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn celsius(c: f64) -> String {
    format!("{c:.1} °C")
}

pub fn volts(mv: u32) -> String {
    format!("{:.2} V", mv as f64 / 1000.0)
}

pub fn milliamps(ma: i32) -> String {
    let sign = if ma < 0 { "-" } else { "" };
    format!("{sign}{} mA", thousands(ma.unsigned_abs() as u64))
}

pub fn mah(v: u32) -> String {
    format!("{} mAh", thousands(v as u64))
}

/// Temperature rows for display (SMC groups only; the registry's own
/// `Temperature` key is absent on current macOS and its unit could not be
/// verified, so it is not used).
pub fn temperature_rows(sensors: &SensorSnapshot) -> Vec<Temperature> {
    sensors.temperatures.clone()
}

/// Battery temperature for the details list: the same hottest-sensor value the
/// "Battery" row shows.
pub fn battery_temperature(sensors: &SensorSnapshot) -> Option<f64> {
    sensors
        .temperatures
        .iter()
        .find(|t| t.kind == SensorKind::Battery)
        .map(|t| t.max)
}

/// True when at least one fan runs at 90% or more of its maximum speed:
/// macOS is cooling hard, usually because something keeps the CPU or GPU busy.
pub fn fans_at_full_speed(sensors: &SensorSnapshot) -> bool {
    sensors
        .fans
        .iter()
        .any(|f| f.max > 0.0 && f.rpm >= 0.9 * f.max)
}

/// Row subtitle of a temperature group.
pub fn temperature_subtitle(t: &Temperature) -> String {
    let what = match t.kind {
        SensorKind::Cpu => "performance cores",
        SensorKind::CpuEfficiency => "efficiency cores",
        SensorKind::Gpu => "GPU sensors",
        _ => "sensors",
    };
    if t.count <= 1 {
        return "1 sensor".to_string();
    }
    if t.kind.shows_average() {
        format!("Average of {} {what} · hottest {}", t.count, celsius(t.max))
    } else {
        format!("Hottest of {} {what}", t.count)
    }
}

/// (value, subtitle) for a fan row. A fan at 0 RPM is normal on Apple Silicon
/// when the Mac is cool, so it reads "Idle", not an error.
pub fn fan_lines(f: &Fan) -> (String, String) {
    let value = if f.rpm < 1.0 {
        "Idle".to_string()
    } else {
        format!("{} RPM", thousands(f.rpm.round() as u64))
    };
    // A range is shown only when the SMC really reported a maximum.
    let mut parts: Vec<String> = Vec::new();
    if f.max > 0.0 && f.max.is_finite() && f.min.is_finite() && f.min <= f.max {
        parts.push(format!(
            "Range {}–{} RPM",
            thousands(f.min.round() as u64),
            thousands(f.max.round() as u64)
        ));
    }
    if let Some(t) = f.target.filter(|t| t.is_finite() && *t > 0.0) {
        parts.push(format!("target {} RPM", thousands(t.round() as u64)));
    }
    (value, parts.join(" · "))
}

/// Fraction of the fan's maximum speed, clamped to 0–1 (a fan can read a
/// little above its nominal maximum).
pub fn fan_fraction(f: &Fan) -> f64 {
    if f.max > 0.0 {
        (f.rpm / f.max).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Charge tile value ("35%").
pub fn charge_value(b: &BatteryInfo) -> String {
    format!("{:.0}%", b.percent)
}

pub fn charge_caption(b: &BatteryInfo) -> String {
    match time_remaining(b) {
        Some(t) => format!("{} · {t}", b.state_label()),
        None => b.state_label().to_string(),
    }
}

/// Title of the health tile.
pub fn health_title(b: &BatteryInfo) -> &'static str {
    match b.health {
        Some(Health {
            source: Source::Estimated,
            ..
        }) => "Health (estimated)",
        _ => "Health",
    }
}

pub fn health_value(b: &BatteryInfo) -> String {
    match b.health {
        Some(h) => format!("{:.0}%", h.percent),
        None => "Unknown".to_string(),
    }
}

pub fn health_caption(b: &BatteryInfo) -> String {
    match (b.health, b.nominal_mah.or(b.full_mah), b.design_mah) {
        (
            Some(Health {
                source: Source::Apple,
                ..
            }),
            _,
            _,
        ) => "Maximum capacity, as in System Settings".to_string(),
        (
            Some(Health {
                source: Source::Estimated,
                ..
            }),
            Some(m),
            Some(d),
        ) => {
            format!("{} of {} design, not reported by macOS", mah(m), mah(d))
        }
        (Some(_), _, _) => "Not reported by macOS".to_string(),
        (None, _, _) => "Capacity not reported".to_string(),
    }
}

/// Fraction (0-1) for the health meter, when known.
pub fn health_fraction(b: &BatteryInfo) -> Option<f64> {
    b.health.map(|h| h.percent / 100.0)
}

/// Cycle count versus the rated cycles (`DesignCycleCount9C`; Apple rates every
/// current MacBook Pro for 1,000 cycles).
pub fn cycles_fraction(b: &BatteryInfo) -> Option<f64> {
    b.design_cycle_count
        .map(|d| b.cycle_count as f64 / d as f64)
}

pub fn cycles_caption(b: &BatteryInfo) -> String {
    match b.design_cycle_count {
        Some(d) => format!("Rated for {}", thousands(d as u64)),
        None => "Charge cycles completed".to_string(),
    }
}

pub fn condition_value(b: &BatteryInfo) -> String {
    b.condition
        .map(|(c, _)| c.label().to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

pub fn condition_caption(b: &BatteryInfo) -> &'static str {
    match b.condition {
        Some((_, Source::Apple)) => "Reported by macOS",
        Some((_, Source::Estimated)) => "Estimated: macOS recommends service below 80%",
        None => "Not reported",
    }
}

/// The "Details" list: (label, value), in display order.
pub fn battery_lines(b: &BatteryInfo, sensors: &SensorSnapshot) -> Vec<(&'static str, String)> {
    let mut v: Vec<(&'static str, String)> = vec![
        ("Status", b.state_label().to_string()),
        (
            "Power source",
            if b.external_connected {
                "Power adapter"
            } else {
                "Battery"
            }
            .to_string(),
        ),
    ];
    if let Some(t) = time_remaining(b) {
        // The row label already says "to full"/"remaining": show just the duration.
        let value = b.minutes_remaining.map(duration_minutes).unwrap_or(t);
        v.push((
            if b.is_charging {
                "Time to full"
            } else {
                "Time remaining"
            },
            value,
        ));
    }
    if let Some(x) = b.current_mah {
        v.push(("Remaining charge", mah(x)));
    }
    if let Some(x) = b.full_mah {
        v.push(("Full charge capacity", mah(x)));
    }
    if let Some(x) = b.nominal_mah {
        v.push(("Nominal capacity", mah(x)));
    }
    if let Some(x) = b.design_mah {
        v.push(("Design capacity", mah(x)));
    }
    if let Some(t) = battery_temperature(sensors) {
        v.push(("Temperature", celsius(t)));
    }
    if let Some(x) = b.voltage_mv {
        v.push(("Voltage", volts(x)));
    }
    if let Some(a) = b.amperage_ma {
        v.push(("Battery current", milliamps(a)));
    }
    if let Some(w) = b.power_w() {
        if w <= -0.05 {
            v.push(("Discharging at", format!("{:.1} W", w.abs())));
        } else if w >= 0.05 {
            v.push(("Charging at", format!("{w:.1} W")));
        }
    }
    if let Some(a) = &b.adapter {
        let text = match (a.watts, a.name.as_deref()) {
            (Some(w), Some(n)) => format!("{w} W · {n}"),
            (Some(w), None) => format!("{w} W"),
            (None, Some(n)) => n.to_string(),
            (None, None) => String::new(),
        };
        if !text.is_empty() {
            v.push(("Adapter", text));
        }
    }
    if let Some(n) = &b.device_name {
        v.push(("Gauge", n.clone()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::battery::{parse, Dict, Value};

    fn battery(extra: &[(&str, Value)]) -> BatteryInfo {
        let mut d = Dict::new();
        d.insert("CurrentCapacity".into(), Value::Int(50));
        d.insert("MaxCapacity".into(), Value::Int(100));
        d.insert("CycleCount".into(), Value::Int(1));
        for (k, v) in extra {
            d.insert(k.to_string(), v.clone());
        }
        parse(&d).unwrap()
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(duration_minutes(47), "47m");
        assert_eq!(duration_minutes(125), "2h 5m");
        assert_eq!(thousands(5), "5");
        assert_eq!(thousands(5615), "5,615");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(milliamps(-3438), "-3,438 mA");
        assert_eq!(milliamps(1200), "1,200 mA");
        assert_eq!(volts(11076), "11.08 V");
        assert_eq!(celsius(37.54), "37.5 °C");
    }

    #[test]
    fn time_remaining_text() {
        let on_battery = battery(&[("AvgTimeToEmpty", Value::Int(90))]);
        assert_eq!(
            time_remaining(&on_battery).as_deref(),
            Some("1h 30m remaining")
        );
        let charging = battery(&[
            ("IsCharging", Value::Bool(true)),
            ("ExternalConnected", Value::Bool(true)),
            ("AvgTimeToFull", Value::Int(20)),
        ]);
        assert_eq!(time_remaining(&charging).as_deref(), Some("20m until full"));
        let full = battery(&[
            ("FullyCharged", Value::Bool(true)),
            ("ExternalConnected", Value::Bool(true)),
        ]);
        assert_eq!(time_remaining(&full), None);
        let unknown = battery(&[("AvgTimeToEmpty", Value::Int(65535))]);
        assert_eq!(time_remaining(&unknown).as_deref(), Some("Calculating…"));
    }

    #[test]
    fn battery_temperature_matches_row() {
        let with_smc = SensorSnapshot {
            temperatures: vec![Temperature {
                kind: SensorKind::Battery,
                max: 38.0,
                avg: 37.0,
                count: 3,
            }],
            ..Default::default()
        };
        assert_eq!(battery_temperature(&with_smc), Some(38.0));
        assert_eq!(temperature_rows(&with_smc)[0].max, 38.0);
        assert_eq!(battery_temperature(&SensorSnapshot::default()), None);
        // A registry Temperature key is never displayed.
        let b = battery(&[("Temperature", Value::Int(30315))]);
        assert!(!battery_lines(&b, &SensorSnapshot::default())
            .iter()
            .any(|(l, _)| *l == "Temperature"));
    }

    #[test]
    fn fan_text() {
        let fan = |rpm: f64, target: Option<f64>| Fan {
            index: 1,
            rpm,
            min: 1200.0,
            max: 5779.0,
            target,
        };
        let (v, s) = fan_lines(&fan(5828.4, Some(5779.0)));
        assert_eq!(v, "5,828 RPM");
        assert_eq!(s, "Range 1,200–5,779 RPM · target 5,779 RPM");
        // Stopped fan (normal on Apple Silicon when cool): "Idle", no target.
        let (v, s) = fan_lines(&fan(0.0, Some(0.0)));
        assert_eq!(v, "Idle");
        assert_eq!(s, "Range 1,200–5,779 RPM");
        // Unreported range (max 0) is hidden, not shown as "0-0".
        let unknown = Fan {
            index: 1,
            rpm: 3000.0,
            min: 0.0,
            max: 0.0,
            target: None,
        };
        assert_eq!(
            fan_lines(&unknown),
            ("3,000 RPM".to_string(), String::new())
        );
        // Reading slightly above the nominal maximum clamps the meter at 1.
        assert_eq!(fan_fraction(&fan(5828.4, None)), 1.0);
        assert_eq!(fan_fraction(&fan(0.0, None)), 0.0);
        assert!((fan_fraction(&fan(2889.5, None)) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn detects_fans_at_full_speed() {
        let snap = |rpm| SensorSnapshot {
            fans: vec![Fan {
                index: 1,
                rpm,
                min: 1200.0,
                max: 5779.0,
                target: None,
            }],
            ..Default::default()
        };
        assert!(fans_at_full_speed(&snap(5827.0)));
        assert!(!fans_at_full_speed(&snap(2400.0)));
        assert!(!fans_at_full_speed(&snap(0.0)));
    }

    #[test]
    fn temperature_subtitles() {
        let t = |kind, count| Temperature {
            kind,
            max: 50.0,
            avg: 40.0,
            count,
        };
        assert_eq!(
            temperature_subtitle(&t(SensorKind::Battery, 3)),
            "Hottest of 3 sensors"
        );
        assert_eq!(temperature_subtitle(&t(SensorKind::Storage, 1)), "1 sensor");
        assert_eq!(
            temperature_subtitle(&t(SensorKind::Cpu, 8)),
            "Average of 8 performance cores · hottest 50.0 °C"
        );
        assert_eq!(t(SensorKind::Cpu, 8).value(), 40.0);
        assert_eq!(t(SensorKind::Battery, 3).value(), 50.0);
    }

    #[test]
    fn health_text_apple_vs_estimate() {
        use crate::engine::battery::{with_apple_health, Condition};
        use crate::engine::health::AppleHealth;
        let mut nested = Dict::new();
        nested.insert("DesignCapacity".into(), Value::Int(6075));
        nested.insert("NominalChargeCapacity".into(), Value::Int(4680));
        let b = battery(&[("BatteryData", Value::Dict(nested))]);
        assert_eq!(health_title(&b), "Health (estimated)");
        assert_eq!(health_value(&b), "77%");
        assert_eq!(
            health_caption(&b),
            "4,680 mAh of 6,075 mAh design, not reported by macOS"
        );
        let apple = AppleHealth {
            max_capacity_percent: Some(79),
            condition: Some(Condition::ServiceRecommended),
        };
        let b = with_apple_health(b, Some(apple));
        assert_eq!(
            (health_title(&b).to_string(), health_value(&b)),
            ("Health".to_string(), "79%".to_string())
        );
        assert_eq!(condition_value(&b), "Service recommended");
        assert_eq!(condition_caption(&b), "Reported by macOS");
    }

    #[test]
    fn time_row_shows_duration_only() {
        let none = SensorSnapshot::default();
        let get = |b: &BatteryInfo, l: &str| {
            battery_lines(b, &none)
                .into_iter()
                .find(|(x, _)| *x == l)
                .map(|(_, v)| v)
        };
        let on_battery = battery(&[("AvgTimeToEmpty", Value::Int(90))]);
        assert_eq!(
            get(&on_battery, "Time remaining").as_deref(),
            Some("1h 30m")
        );
        let charging = battery(&[
            ("IsCharging", Value::Bool(true)),
            ("ExternalConnected", Value::Bool(true)),
            ("AvgTimeToFull", Value::Int(101)),
        ]);
        assert_eq!(get(&charging, "Time to full").as_deref(), Some("1h 41m"));
        let unknown = battery(&[("AvgTimeToEmpty", Value::Int(65535))]);
        assert_eq!(
            get(&unknown, "Time remaining").as_deref(),
            Some("Calculating…")
        );
    }

    #[test]
    fn power_rows() {
        let none = SensorSnapshot::default();
        let get = |b: &BatteryInfo, l: &str| {
            battery_lines(b, &none)
                .into_iter()
                .find(|(x, _)| *x == l)
                .map(|(_, v)| v)
        };
        let b = battery(&[
            ("Voltage", Value::Int(11931)),
            ("InstantAmperage", Value::Int(2968)),
        ]);
        assert_eq!(get(&b, "Charging at").as_deref(), Some("35.4 W"));
        assert_eq!(get(&b, "Battery current").as_deref(), Some("2,968 mA"));
        let b = battery(&[
            ("Voltage", Value::Int(11060)),
            ("InstantAmperage", Value::Int(-4026)),
        ]);
        assert_eq!(get(&b, "Discharging at").as_deref(), Some("44.5 W"));
        // Idle on AC: no "Charging at 0.0 W".
        let b = battery(&[
            ("Voltage", Value::Int(12000)),
            ("InstantAmperage", Value::Int(0)),
        ]);
        assert_eq!(get(&b, "Charging at"), None);
        assert_eq!(get(&b, "Discharging at"), None);
    }
}

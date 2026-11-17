//! Text for the Windows battery screen (pure, unit-tested). Labels follow
//! Windows' own wording: `powercfg /batteryreport` ("Design capacity", "Full
//! charge capacity", "Cycle count", "Chemistry") and Settings > System >
//! Power & battery. Windows makes no service judgement, so there is no
//! "Condition" here.

use super::battery::{BatteryInfo, Unit};

/// "1h 23m", "47m".
pub fn duration_minutes(minutes: u32) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Integer with thousands separators: 93110 -> "93,110".
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

/// A capacity as the driver reports it: mWh, or a bare number when the
/// battery uses relative units.
pub fn capacity(b: &BatteryInfo, v: u32) -> String {
    if b.relative() {
        thousands(v as u64)
    } else {
        format!("{} mWh", thousands(v as u64))
    }
}

/// Time line for the battery. Windows estimates the time left only while
/// running on battery and has no time-to-full, so plugged in there is none;
/// on battery an unknown estimate is said as such.
pub fn time_remaining(b: &BatteryInfo) -> Option<String> {
    if b.external_connected || b.is_charging {
        return None;
    }
    Some(match b.minutes_remaining {
        Some(m) => format!("{} remaining", duration_minutes(m)),
        None => "Time remaining unknown".to_string(),
    })
}

/// Charge tile value ("87%").
pub fn charge_value(b: &BatteryInfo) -> String {
    if b.percent_known {
        format!("{:.0}%", b.percent)
    } else {
        "Unknown".to_string()
    }
}

pub fn charge_caption(b: &BatteryInfo) -> String {
    match time_remaining(b) {
        Some(t) => format!("{} · {t}", b.state_label()),
        None => b.state_label().to_string(),
    }
}

pub fn health_title(_b: &BatteryInfo) -> &'static str {
    "Health"
}

pub fn health_value(b: &BatteryInfo) -> String {
    match b.health_percent() {
        Some(p) => format!("{p:.0}%"),
        None => "Unknown".to_string(),
    }
}

pub fn health_caption(b: &BatteryInfo) -> String {
    match (b.full_charge(), b.design()) {
        (Some(f), Some(d)) => format!(
            "Full charge capacity {} of {} design capacity",
            capacity(b, f),
            capacity(b, d)
        ),
        (_, None) => "Design capacity not reported".to_string(),
        (None, _) => "Full charge capacity not reported".to_string(),
    }
}

/// Fraction (0-1) for the health meter, when known.
pub fn health_fraction(b: &BatteryInfo) -> Option<f64> {
    b.health_percent().map(|p| (p / 100.0).clamp(0.0, 1.0))
}

fn unit_cycles(u: &Unit) -> String {
    match u.cycle_count {
        Some(c) => thousands(c as u64),
        None => "Not reported".to_string(),
    }
}

/// Cycle count tile value. A driver cycle count of 0 means the firmware
/// doesn't report it, so it reads "Not reported", never "0".
pub fn cycles_value(b: &BatteryInfo) -> String {
    match b.units.as_slice() {
        [] => "Not reported".to_string(),
        [u] => unit_cycles(u),
        many if many.iter().all(|u| u.cycle_count.is_none()) => "Not reported".to_string(),
        many => many.iter().map(unit_cycles).collect::<Vec<_>>().join(" · "),
    }
}

pub fn cycles_caption(b: &BatteryInfo) -> String {
    let reported = b.units.iter().any(|u| u.cycle_count.is_some());
    match (b.units.len(), reported) {
        (_, false) => "The battery firmware doesn't report it".to_string(),
        (1, true) => "Charge cycles completed".to_string(),
        (n, true) => format!("Per battery, {n} batteries"),
    }
}

/// Friendly name of a driver chemistry code, with the code as
/// `powercfg /batteryreport` prints it.
pub fn chemistry(code: &str) -> String {
    let name = match code.to_ascii_uppercase().as_str() {
        "LION" | "LI-I" | "LIION" => Some("Lithium-ion"),
        "LIP" | "LIPO" => Some("Lithium polymer"),
        "PBAC" => Some("Lead acid"),
        "NICD" => Some("Nickel cadmium"),
        "NIMH" => Some("Nickel metal hydride"),
        "NIZN" => Some("Nickel zinc"),
        "RAM" => Some("Rechargeable alkaline"),
        _ => None,
    };
    match name {
        Some(n) => format!("{n} ({code})"),
        None => code.to_string(),
    }
}

/// Distinct non-empty values across batteries, joined.
fn joined(b: &BatteryInfo, f: impl Fn(&Unit) -> Option<String>) -> Option<String> {
    let mut values: Vec<String> = Vec::new();
    for v in b.units.iter().filter_map(f) {
        if !values.contains(&v) {
            values.push(v);
        }
    }
    (!values.is_empty()).then(|| values.join(", "))
}

/// The "Details" list: (label, value), in display order.
pub fn battery_lines(b: &BatteryInfo) -> Vec<(&'static str, String)> {
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
    if time_remaining(b).is_some() {
        v.push((
            "Time remaining",
            b.minutes_remaining
                .map(duration_minutes)
                .unwrap_or_else(|| "Unknown".to_string()),
        ));
    }
    v.push((
        "Battery saver",
        if b.battery_saver { "On" } else { "Off" }.to_string(),
    ));
    if let Some(x) = b.remaining() {
        v.push(("Remaining capacity", capacity(b, x)));
    }
    if let Some(x) = b.full_charge() {
        v.push(("Full charge capacity", capacity(b, x)));
    }
    if let Some(x) = b.design() {
        v.push(("Design capacity", capacity(b, x)));
    }
    if let Some(t) = b.temperature_c() {
        v.push(("Temperature", celsius(t)));
    }
    if let Some(x) = b.voltage_mv() {
        v.push(("Voltage", volts(x)));
    }
    // The driver's rate is in mW; relative-unit batteries report no watts.
    if let Some(mw) = b.rate_mw().filter(|_| !b.relative()) {
        let w = mw as f64 / 1000.0;
        if w <= -0.05 {
            v.push(("Discharging at", format!("{:.1} W", w.abs())));
        } else if w >= 0.05 {
            v.push(("Charging at", format!("{w:.1} W")));
        }
    }
    if b.units.len() > 1 {
        v.push(("Batteries", b.units.len().to_string()));
    }
    if let Some(c) = joined(b, |u| u.chemistry.as_deref().map(chemistry)) {
        v.push(("Chemistry", c));
    }
    if let Some(m) = joined(b, |u| u.manufacturer.clone()) {
        v.push(("Manufacturer", m));
    }
    if let Some(n) = joined(b, |u| u.name.clone()) {
        v.push(("Name", n));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::battery::{combine, PowerStatus};

    fn unit(design: u32, full: u32, remaining: u32, cycles: Option<u32>) -> Unit {
        Unit {
            design: Some(design),
            full_charge: Some(full),
            remaining: Some(remaining),
            cycle_count: cycles,
            chemistry: Some("LiP".into()),
            voltage_mv: Some(13_127),
            rate_mw: Some(0),
            name: Some("00NY493".into()),
            manufacturer: Some("SMP".into()),
            ..Default::default()
        }
    }

    fn power(ac: bool, percent: u8, seconds: Option<u32>) -> PowerStatus {
        PowerStatus {
            ac_online: Some(ac),
            percent: Some(percent),
            seconds_remaining: seconds,
            ..Default::default()
        }
    }

    fn get(b: &BatteryInfo, label: &str) -> Option<String> {
        battery_lines(b)
            .into_iter()
            .find(|(l, _)| *l == label)
            .map(|(_, v)| v)
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(duration_minutes(47), "47m");
        assert_eq!(duration_minutes(125), "2h 5m");
        assert_eq!(thousands(93_110), "93,110");
        assert_eq!(volts(13_127), "13.13 V");
        assert_eq!(celsius(29.96), "30.0 °C");
    }

    #[test]
    fn full_on_ac() {
        let b = combine(
            power(true, 100, None),
            vec![unit(99_480, 93_110, 93_110, Some(46))],
        )
        .unwrap();
        assert_eq!(charge_value(&b), "100%");
        assert_eq!(charge_caption(&b), "Fully charged");
        assert_eq!(health_value(&b), "94%");
        assert_eq!(
            health_caption(&b),
            "Full charge capacity 93,110 mWh of 99,480 mWh design capacity"
        );
        assert_eq!(
            (cycles_value(&b).as_str(), cycles_caption(&b).as_str()),
            ("46", "Charge cycles completed")
        );
        assert_eq!(get(&b, "Time remaining"), None);
        assert_eq!(
            get(&b, "Full charge capacity").as_deref(),
            Some("93,110 mWh")
        );
        assert_eq!(get(&b, "Design capacity").as_deref(), Some("99,480 mWh"));
        assert_eq!(get(&b, "Remaining capacity").as_deref(), Some("93,110 mWh"));
        assert_eq!(
            get(&b, "Chemistry").as_deref(),
            Some("Lithium polymer (LiP)")
        );
        assert_eq!(get(&b, "Name").as_deref(), Some("00NY493"));
        assert_eq!(get(&b, "Manufacturer").as_deref(), Some("SMP"));
        // Idle on AC: no "Charging at 0.0 W"; no battery temperature reported.
        assert_eq!(get(&b, "Charging at"), None);
        assert_eq!(get(&b, "Temperature"), None);
        assert_eq!(get(&b, "Batteries"), None);
    }

    #[test]
    fn on_battery_time_known_and_unknown() {
        let mut u = unit(50_000, 45_000, 22_500, Some(120));
        u.rate_mw = Some(-9_800);
        let b = combine(power(false, 50, Some(5_400)), vec![u.clone()]).unwrap();
        assert_eq!(charge_caption(&b), "On battery · 1h 30m remaining");
        assert_eq!(get(&b, "Time remaining").as_deref(), Some("1h 30m"));
        assert_eq!(get(&b, "Discharging at").as_deref(), Some("9.8 W"));
        let b = combine(power(false, 50, None), vec![u]).unwrap();
        assert_eq!(charge_caption(&b), "On battery · Time remaining unknown");
        assert_eq!(get(&b, "Time remaining").as_deref(), Some("Unknown"));
    }

    #[test]
    fn charging_shows_no_time_to_full() {
        let mut u = unit(50_000, 45_000, 22_500, Some(120));
        u.rate_mw = Some(31_300);
        let mut p = power(true, 50, None);
        p.charging = true;
        let b = combine(p, vec![u]).unwrap();
        assert_eq!(charge_caption(&b), "Charging");
        assert_eq!(get(&b, "Charging at").as_deref(), Some("31.3 W"));
    }

    #[test]
    fn unreported_cycles_and_capacities() {
        let mut u = unit(50_000, 45_000, 22_500, None);
        let b = combine(power(true, 90, None), vec![u.clone()]).unwrap();
        assert_eq!(cycles_value(&b), "Not reported");
        assert_eq!(cycles_caption(&b), "The battery firmware doesn't report it");
        u.design = None;
        let b = combine(power(true, 90, None), vec![u]).unwrap();
        assert_eq!(health_value(&b), "Unknown");
        assert_eq!(health_caption(&b), "Design capacity not reported");
        assert_eq!(health_fraction(&b), None);
        assert_eq!(get(&b, "Design capacity"), None);
    }

    #[test]
    fn health_above_design_is_not_capped() {
        let b = combine(
            power(true, 100, None),
            vec![unit(50_000, 51_000, 51_000, Some(3))],
        )
        .unwrap();
        assert_eq!(health_value(&b), "102%");
        assert_eq!(health_fraction(&b), Some(1.0));
    }

    #[test]
    fn relative_capacities_have_no_unit() {
        let mut u = unit(100, 95, 50, Some(3));
        u.relative = true;
        u.rate_mw = Some(-5_000);
        let b = combine(power(false, 50, None), vec![u]).unwrap();
        assert_eq!(
            health_caption(&b),
            "Full charge capacity 95 of 100 design capacity"
        );
        assert_eq!(get(&b, "Discharging at"), None);
    }

    #[test]
    fn two_batteries() {
        let mut second = unit(48_000, 40_000, 20_000, None);
        second.name = Some("45N1727".into());
        let b = combine(
            power(true, 60, None),
            vec![unit(24_000, 22_000, 11_000, Some(300)), second],
        )
        .unwrap();
        assert_eq!(cycles_value(&b), "300 · Not reported");
        assert_eq!(cycles_caption(&b), "Per battery, 2 batteries");
        assert_eq!(get(&b, "Batteries").as_deref(), Some("2"));
        assert_eq!(get(&b, "Name").as_deref(), Some("00NY493, 45N1727"));
        assert_eq!(get(&b, "Manufacturer").as_deref(), Some("SMP"));
        assert_eq!(get(&b, "Voltage"), None);
    }

    #[test]
    fn unknown_percent() {
        let mut u = unit(50_000, 45_000, 0, Some(1));
        u.remaining = None;
        let p = PowerStatus {
            ac_online: Some(false),
            ..Default::default()
        };
        let b = combine(p, vec![u]).unwrap();
        assert_eq!(charge_value(&b), "Unknown");
    }

    #[test]
    fn battery_saver_row() {
        let mut p = power(false, 30, None);
        p.battery_saver = true;
        let b = combine(p, vec![unit(1, 1, 1, None)]).unwrap();
        assert_eq!(get(&b, "Battery saver").as_deref(), Some("On"));
    }

    #[test]
    fn chemistry_names() {
        assert_eq!(chemistry("LION"), "Lithium-ion (LION)");
        assert_eq!(chemistry("LiP"), "Lithium polymer (LiP)");
        assert_eq!(chemistry("XYZ"), "XYZ");
    }
}

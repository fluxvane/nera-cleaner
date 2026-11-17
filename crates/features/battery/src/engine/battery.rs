//! AppleSmartBattery registry reader (port of BatteryService/BatteryInfo).
//!
//! The registry entry is read through IOKit; if that fails the same data is
//! taken from `ioreg -rn AppleSmartBattery -a` (a plist). Both paths produce a
//! [`Dict`], which [`parse`] turns into a [`BatteryInfo`].

use core_foundation::array::CFArray;
use core_foundation::array::CFArrayRef;
use core_foundation::base::CFTypeRef;
use core_foundation::base::{CFGetTypeID, CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::dictionary::CFDictionaryRef;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use io_kit_sys::{
    kIOMasterPortDefault, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingService, IOServiceMatching,
};
use std::collections::HashMap;
use std::ffi::c_void;

/// Loosely typed registry value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Real(f64),
    Bool(bool),
    Str(String),
    Data(Vec<u8>),
    Dict(Dict),
    Array(Vec<Value>),
}

pub type Dict = HashMap<String, Value>;

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Real(r) => Some(*r as i64),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Int(i) => Some(*i != 0),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }
}

fn int(d: &Dict, key: &str) -> Option<i64> {
    d.get(key).and_then(Value::as_i64)
}
fn flag(d: &Dict, key: &str) -> Option<bool> {
    d.get(key).and_then(Value::as_bool)
}

/// Battery condition as macOS words it (System Settings > Battery >
/// Battery Health). Apple only distinguishes these two; the 80 % boundary is
/// the one powerd logs ("Nominal Capacity percentage(79) is less than the
/// threshold(80)").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    Normal,
    ServiceRecommended,
}

/// Maximum capacity (percent of design) below which macOS recommends service.
pub const SERVICE_THRESHOLD_PERCENT: f64 = 80.0;

impl Condition {
    pub fn from_percent(percent: f64) -> Condition {
        if percent >= SERVICE_THRESHOLD_PERCENT {
            Condition::Normal
        } else {
            Condition::ServiceRecommended
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Condition::Normal => "Normal",
            Condition::ServiceRecommended => "Service recommended",
        }
    }
}

/// Where a health figure comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Reported by macOS itself (the number System Settings shows).
    Apple,
    /// Computed here from registry capacities; may differ from macOS by a few points.
    Estimated,
}

/// Maximum capacity as a percentage of design capacity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Health {
    pub percent: f64,
    pub source: Source,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Adapter {
    pub watts: Option<u32>,
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BatteryInfo {
    /// Charge, 0–100.
    pub percent: f64,
    pub is_charging: bool,
    pub fully_charged: bool,
    pub external_connected: bool,
    /// Minutes until full (charging) or empty (discharging).
    pub minutes_remaining: Option<u32>,
    pub cycle_count: u32,
    pub design_cycle_count: Option<u32>,
    pub design_mah: Option<u32>,
    /// `NominalChargeCapacity` (the gauge's aged capacity at rated conditions).
    pub nominal_mah: Option<u32>,
    /// `FullChargeCapacity` (what a full charge holds right now). Charge
    /// percent and `current_mah` are relative to this.
    pub full_mah: Option<u32>,
    /// `RemainingCapacity`.
    pub current_mah: Option<u32>,
    /// Maximum capacity, if known at all.
    pub health: Option<Health>,
    pub condition: Option<(Condition, Source)>,
    pub voltage_mv: Option<u32>,
    /// Negative while discharging.
    pub amperage_ma: Option<i32>,
    pub adapter: Option<Adapter>,
    pub device_name: Option<String>,
}

impl BatteryInfo {
    /// Instantaneous battery power in watts (negative while discharging).
    pub fn power_w(&self) -> Option<f64> {
        Some(self.voltage_mv? as f64 * self.amperage_ma? as f64 / 1_000_000.0)
    }

    pub fn state_label(&self) -> &'static str {
        if self.fully_charged && self.external_connected {
            "Fully charged"
        } else if self.is_charging {
            "Charging"
        } else if self.external_connected {
            "Plugged in, not charging"
        } else {
            "On battery"
        }
    }
}

/// `65535` means "unknown" in the gas gauge.
fn minutes(d: &Dict, key: &str) -> Option<u32> {
    int(d, key)
        .filter(|&m| m > 0 && m < 65535)
        .map(|m| m as u32)
}

fn positive(v: Option<i64>) -> Option<u32> {
    v.filter(|&x| x > 0 && x <= u32::MAX as i64)
        .map(|x| x as u32)
}

/// Turn the AppleSmartBattery property dictionary into [`BatteryInfo`].
/// `None` when the Mac reports no installed battery.
pub fn parse(d: &Dict) -> Option<BatteryInfo> {
    if flag(d, "BatteryInstalled") == Some(false) {
        return None;
    }
    let nested = d.get("BatteryData").and_then(Value::as_dict);
    let nint = |k: &str| nested.and_then(|n| int(n, k));

    let design = positive(int(d, "DesignCapacity").or_else(|| nint("DesignCapacity")));
    // Modern macOS keeps mAh in `BatteryData` and only percentages at top level.
    let looks_percent = |v: Option<i64>| match (v, design) {
        (Some(v), Some(des)) if des > 500 && v <= 100 => None,
        (v, _) => v,
    };
    let nominal_mah = positive(looks_percent(nint("NominalChargeCapacity")));
    let full_mah = positive(looks_percent(
        int(d, "AppleRawMaxCapacity")
            .or_else(|| nint("FullChargeCapacity"))
            .or_else(|| int(d, "MaxCapacity")),
    ));
    let current_mah = positive(looks_percent(
        int(d, "AppleRawCurrentCapacity")
            .or_else(|| nint("RemainingCapacity"))
            .or_else(|| int(d, "CurrentCapacity")),
    ));

    let cycle_count = int(d, "CycleCount").filter(|&c| c >= 0).unwrap_or(0) as u32;
    let voltage_mv = positive(int(d, "Voltage").or_else(|| int(d, "AppleRawBatteryVoltage")));

    // Charge percentage: top-level values are a percentage on current macOS.
    let percent = match (int(d, "CurrentCapacity"), int(d, "MaxCapacity")) {
        (Some(cur), Some(100)) => cur as f64,
        (Some(cur), Some(max)) if max > 0 => cur as f64 / max as f64 * 100.0,
        _ => match (current_mah, full_mah) {
            (Some(c), Some(m)) => c as f64 / m as f64 * 100.0,
            _ => 0.0,
        },
    }
    .clamp(0.0, 100.0);

    // Nothing usable at all: a desktop Mac.
    if nominal_mah.is_none()
        && full_mah.is_none()
        && current_mah.is_none()
        && cycle_count == 0
        && voltage_mv.is_none()
        && int(d, "CurrentCapacity").is_none()
    {
        return None;
    }

    // Estimate only; `with_apple_health` replaces it with macOS's own figure.
    let health = match (nominal_mah.or(full_mah), design) {
        (Some(m), Some(des)) => Some(Health {
            percent: (m as f64 / des as f64 * 100.0).clamp(0.0, 100.0),
            source: Source::Estimated,
        }),
        _ => None,
    };

    let external_connected = flag(d, "ExternalConnected").unwrap_or(false);
    let is_charging = flag(d, "IsCharging").unwrap_or(false);
    let fully_charged = flag(d, "FullyCharged").unwrap_or(false);

    let minutes_remaining = if is_charging {
        minutes(d, "AvgTimeToFull")
    } else if external_connected {
        None
    } else {
        minutes(d, "AvgTimeToEmpty").or_else(|| minutes(d, "TimeRemaining"))
    };

    let amperage_ma = int(d, "InstantAmperage")
        .or_else(|| int(d, "Amperage"))
        .and_then(|a| i32::try_from(a).ok());

    let adapter = d
        .get("AdapterDetails")
        .and_then(Value::as_dict)
        .and_then(|a| {
            let watts = positive(int(a, "Watts"));
            // Only a real marketing name; "Description" is a generic type such as "pd charger".
            let name = a
                .get("Name")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if watts.is_none() && name.is_none() {
                None
            } else {
                Some(Adapter { watts, name })
            }
        });

    Some(BatteryInfo {
        percent,
        is_charging,
        fully_charged,
        external_connected,
        minutes_remaining,
        cycle_count,
        design_cycle_count: positive(int(d, "DesignCycleCount9C")),
        design_mah: design,
        nominal_mah,
        full_mah,
        current_mah,
        condition: health.map(|h| (Condition::from_percent(h.percent), Source::Estimated)),
        health,
        voltage_mv,
        amperage_ma,
        adapter: if external_connected { adapter } else { None },
        device_name: d
            .get("DeviceName")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

// ---- IOKit path ----------------------------------------------------------

fn cf_to_value(v: &CFType) -> Option<Value> {
    unsafe {
        let ty = CFGetTypeID(v.as_CFTypeRef());
        if ty == CFBoolean::type_id() {
            let b = v.downcast::<CFBoolean>()?;
            Some(Value::Bool(b.into()))
        } else if ty == CFNumber::type_id() {
            let n = v.downcast::<CFNumber>()?;
            n.to_i64()
                .map(Value::Int)
                .or_else(|| n.to_f64().map(Value::Real))
        } else if ty == CFString::type_id() {
            Some(Value::Str(v.downcast::<CFString>()?.to_string()))
        } else if ty == CFData::type_id() {
            Some(Value::Data(v.downcast::<CFData>()?.bytes().to_vec()))
        } else if ty == CFDictionary::<*const c_void, *const c_void>::type_id() {
            let d = v.downcast::<CFDictionary>()?;
            Some(Value::Dict(cf_dict(&d)))
        } else if ty == CFArray::<*const c_void>::type_id() {
            let a = v.downcast::<CFArray>()?;
            Some(Value::Array(
                a.iter()
                    .filter_map(|i| cf_to_value(&CFType::wrap_under_get_rule(*i as _)))
                    .collect(),
            ))
        } else {
            None
        }
    }
}

fn cf_dict(d: &CFDictionary) -> Dict {
    let (keys, values) = d.get_keys_and_values();
    let mut out = Dict::new();
    for (k, v) in keys.into_iter().zip(values) {
        unsafe {
            if k.is_null() || v.is_null() {
                continue;
            }
            let key = CFString::wrap_under_get_rule(k as _).to_string();
            let val = CFType::wrap_under_get_rule(v as _);
            if let Some(val) = cf_to_value(&val) {
                out.insert(key, val);
            }
        }
    }
    out
}

/// Read the AppleSmartBattery properties through IOKit.
pub fn registry_via_iokit() -> Option<Dict> {
    unsafe {
        let matching = IOServiceMatching(c"AppleSmartBattery".as_ptr());
        if matching.is_null() {
            return None;
        }
        let service = IOServiceGetMatchingService(kIOMasterPortDefault, matching as _);
        if service == 0 {
            return None;
        }
        let mut props = std::ptr::null_mut();
        let kr = IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0);
        IOObjectRelease(service);
        if kr != 0 || props.is_null() {
            return None;
        }
        let dict: CFDictionary = CFDictionary::wrap_under_create_rule(props as _);
        Some(cf_dict(&dict))
    }
}

/// Whether the Mac has an `AppleSmartBattery` service at all.
fn has_battery_service() -> bool {
    // SAFETY: `IOServiceGetMatchingService` consumes `matching`; the service
    // it returns is released here.
    unsafe {
        let matching = IOServiceMatching(c"AppleSmartBattery".as_ptr());
        if matching.is_null() {
            return false;
        }
        let service = IOServiceGetMatchingService(kIOMasterPortDefault, matching as _);
        if service == 0 {
            return false;
        }
        IOObjectRelease(service);
        true
    }
}

// ---- ioreg fallback ------------------------------------------------------

fn plist_to_value(v: &plist::Value) -> Option<Value> {
    Some(match v {
        plist::Value::Boolean(b) => Value::Bool(*b),
        plist::Value::Integer(i) => Value::Int(
            i.as_signed()
                .or_else(|| i.as_unsigned().map(|u| u as i64))?,
        ),
        plist::Value::Real(r) => Value::Real(*r),
        plist::Value::String(s) => Value::Str(s.clone()),
        plist::Value::Data(d) => Value::Data(d.clone()),
        plist::Value::Dictionary(d) => Value::Dict(plist_dict(d)),
        plist::Value::Array(a) => Value::Array(a.iter().filter_map(plist_to_value).collect()),
        _ => return None,
    })
}

fn plist_dict(d: &plist::Dictionary) -> Dict {
    d.iter()
        .filter_map(|(k, v)| plist_to_value(v).map(|v| (k.clone(), v)))
        .collect()
}

/// Parse the output of `ioreg -rn AppleSmartBattery -a`.
pub fn registry_from_ioreg_plist(xml: &[u8]) -> Option<Dict> {
    let root = plist::Value::from_reader(std::io::Cursor::new(xml)).ok()?;
    let first = root.as_array()?.first()?.as_dictionary()?;
    Some(plist_dict(first))
}

fn registry_via_ioreg() -> Option<Dict> {
    let out = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rn", "AppleSmartBattery", "-a"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    registry_from_ioreg_plist(&out.stdout)
}

// ---- IOPowerSources (what `pmset -g batt` and the menu bar read) ------------

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFArrayRef;
    fn IOPSGetPowerSourceDescription(blob: CFTypeRef, ps: CFTypeRef) -> CFDictionaryRef;
}

/// Description of the internal battery from IOPowerSources, the same record
/// `pmset -g batt` prints (keys such as "Time to Full Charge", "Is Charged").
pub fn power_source_via_iops() -> Option<Dict> {
    unsafe {
        let blob = IOPSCopyPowerSourcesInfo();
        if blob.is_null() {
            return None;
        }
        let blob = CFType::wrap_under_create_rule(blob);
        let list = IOPSCopyPowerSourcesList(blob.as_CFTypeRef());
        if list.is_null() {
            return None;
        }
        let list: CFArray<*const c_void> = CFArray::wrap_under_create_rule(list);
        for item in list.iter() {
            let desc = IOPSGetPowerSourceDescription(blob.as_CFTypeRef(), *item as CFTypeRef);
            if desc.is_null() {
                continue;
            }
            let desc: CFDictionary = CFDictionary::wrap_under_get_rule(desc);
            let d = cf_dict(&desc);
            if d.get("Type").and_then(Value::as_str) == Some("InternalBattery") {
                return Some(d);
            }
        }
        None
    }
}

/// Let macOS's own power-source record decide the time estimate and the
/// "charged" state, so both match `pmset -g batt` exactly (including the
/// discharging case, where the registry has two similar keys). `-1`/`0` mean
/// "still calculating".
pub fn apply_power_source(info: &mut BatteryInfo, ps: &Dict) {
    let key = if info.is_charging {
        Some("Time to Full Charge")
    } else if !info.external_connected {
        Some("Time to Empty")
    } else {
        None
    };
    if let Some(key) = key {
        if ps.contains_key(key) {
            info.minutes_remaining = int(ps, key)
                .filter(|&m| m > 0 && m < 65535)
                .map(|m| m as u32);
        }
    }
    if flag(ps, "Is Charged") == Some(true) {
        info.fully_charged = true;
    }
}

/// Current battery state, or `None` on a Mac without a battery.
pub fn read_battery() -> Option<BatteryInfo> {
    let dict = match registry_via_iokit() {
        Some(dict) => dict,
        // No battery service at all (a desktop Mac): `ioreg` has nothing to
        // add, and would be started again on every refresh.
        None if !has_battery_service() => return None,
        None => registry_via_ioreg()?,
    };
    let mut info = parse(&dict)?;
    if let Some(ps) = power_source_via_iops() {
        apply_power_source(&mut info, &ps);
    }
    Some(with_apple_health(info, super::health::apple_health()))
}

/// Replace the estimated health/condition with macOS's own when available.
pub fn with_apple_health(
    mut info: BatteryInfo,
    apple: Option<super::health::AppleHealth>,
) -> BatteryInfo {
    if let Some(a) = apple {
        if let Some(p) = a.max_capacity_percent {
            info.health = Some(Health {
                percent: p as f64,
                source: Source::Apple,
            });
        }
        if let Some(c) = a.condition {
            info.condition = Some((c, Source::Apple));
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(pairs: Vec<(&str, Value)>) -> Dict {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }
    fn i(v: i64) -> Value {
        Value::Int(v)
    }

    fn apple_silicon() -> Dict {
        d(vec![
            ("CurrentCapacity", i(53)),
            ("MaxCapacity", i(100)),
            ("CycleCount", i(811)),
            ("DesignCycleCount9C", i(1000)),
            ("Voltage", i(11060)),
            ("IsCharging", Value::Bool(false)),
            ("ExternalConnected", Value::Bool(false)),
            ("AvgTimeToEmpty", i(47)),
            ("AvgTimeToFull", i(65535)),
            ("InstantAmperage", i(-4026)),
            ("DeviceName", Value::Str("bq40z651".into())),
            (
                "BatteryData",
                Value::Dict(d(vec![
                    ("FullChargeCapacity", i(4515)),
                    ("NominalChargeCapacity", i(4665)),
                    ("RemainingCapacity", i(2294)),
                    ("DesignCapacity", i(6075)),
                ])),
            ),
        ])
    }

    #[test]
    fn parses_apple_silicon_registry() {
        let b = parse(&apple_silicon()).unwrap();
        assert_eq!(b.percent, 53.0);
        assert_eq!(b.cycle_count, 811);
        assert_eq!(b.design_mah, Some(6075));
        assert_eq!(b.nominal_mah, Some(4665));
        assert_eq!(b.full_mah, Some(4515));
        assert_eq!(b.current_mah, Some(2294));
        let h = b.health.unwrap();
        assert_eq!(h.source, Source::Estimated);
        assert!((h.percent - 76.79).abs() < 0.01);
        assert_eq!(
            b.condition,
            Some((Condition::ServiceRecommended, Source::Estimated))
        );
        assert_eq!(b.minutes_remaining, Some(47));
        assert_eq!(b.state_label(), "On battery");
        assert_eq!(b.amperage_ma, Some(-4026));
        assert!(b.power_w().unwrap() < 0.0);
        assert!(b.adapter.is_none());
    }

    #[test]
    fn percent_values_are_not_mah() {
        // Top-level capacities are percentages; they must not drive health.
        let mut m = d(vec![
            ("CurrentCapacity", i(80)),
            ("MaxCapacity", i(100)),
            ("DesignCapacity", i(5000)),
            ("CycleCount", i(10)),
        ]);
        let b = parse(&m).unwrap();
        assert_eq!(b.full_mah, None);
        assert_eq!(b.health, None);
        m.insert("AppleRawMaxCapacity".into(), i(4500));
        assert_eq!(parse(&m).unwrap().health.unwrap().percent, 90.0);
    }

    #[test]
    fn charging_adapter_and_time() {
        let mut m = apple_silicon();
        m.insert("IsCharging".into(), Value::Bool(true));
        m.insert("ExternalConnected".into(), Value::Bool(true));
        m.insert("AvgTimeToFull".into(), i(95));
        m.insert(
            "AdapterDetails".into(),
            Value::Dict(d(vec![
                ("Watts", i(67)),
                ("Name", Value::Str("67W USB-C".into())),
            ])),
        );
        let b = parse(&m).unwrap();
        assert_eq!(b.state_label(), "Charging");
        assert_eq!(b.minutes_remaining, Some(95));
        assert_eq!(
            b.adapter,
            Some(Adapter {
                watts: Some(67),
                name: Some("67W USB-C".into())
            })
        );
    }

    #[test]
    fn unknown_time_is_none() {
        let mut m = apple_silicon();
        m.insert("AvgTimeToEmpty".into(), i(65535));
        assert_eq!(parse(&m).unwrap().minutes_remaining, None);
    }

    #[test]
    fn desktop_has_no_battery() {
        assert!(parse(&Dict::new()).is_none());
        assert!(parse(&d(vec![("BatteryInstalled", Value::Bool(false))])).is_none());
    }

    #[test]
    fn condition_thresholds() {
        assert_eq!(Condition::from_percent(95.0), Condition::Normal);
        assert_eq!(Condition::from_percent(80.0), Condition::Normal);
        assert_eq!(Condition::from_percent(79.0), Condition::ServiceRecommended);
        assert_eq!(Condition::from_percent(10.0), Condition::ServiceRecommended);
    }

    #[test]
    fn apple_health_overrides_estimate() {
        use super::super::health::AppleHealth;
        let b = parse(&apple_silicon()).unwrap();
        let apple = AppleHealth {
            max_capacity_percent: Some(79),
            condition: Some(Condition::ServiceRecommended),
        };
        let b = with_apple_health(b, Some(apple));
        assert_eq!(
            b.health,
            Some(Health {
                percent: 79.0,
                source: Source::Apple
            })
        );
        assert_eq!(
            b.condition,
            Some((Condition::ServiceRecommended, Source::Apple))
        );
    }

    #[test]
    fn adapter_ignores_generic_description() {
        let mut m = apple_silicon();
        m.insert("ExternalConnected".into(), Value::Bool(true));
        m.insert(
            "AdapterDetails".into(),
            Value::Dict(d(vec![
                ("Watts", i(100)),
                ("Description", Value::Str("pd charger".into())),
            ])),
        );
        assert_eq!(
            parse(&m).unwrap().adapter,
            Some(Adapter {
                watts: Some(100),
                name: None
            })
        );
    }

    #[test]
    fn registry_capacities_from_reference_mac() {
        // ioreg capture: FullChargeCapacity 4530, Nominal 4680, Design 6075, Remaining 1541, 35 %.
        let mut m = d(vec![
            ("CurrentCapacity", i(35)),
            ("MaxCapacity", i(100)),
            ("CycleCount", i(812)),
            ("Voltage", i(11931)),
        ]);
        m.insert(
            "BatteryData".into(),
            Value::Dict(d(vec![
                ("FullChargeCapacity", i(4530)),
                ("NominalChargeCapacity", i(4680)),
                ("RemainingCapacity", i(1541)),
                ("DesignCapacity", i(6075)),
            ])),
        );
        let b = parse(&m).unwrap();
        assert_eq!(
            (b.full_mah, b.nominal_mah, b.current_mah, b.design_mah),
            (Some(4530), Some(4680), Some(1541), Some(6075))
        );
        assert_eq!(b.percent, 35.0);
    }

    #[test]
    fn parses_ioreg_plist() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><array><dict>
<key>CurrentCapacity</key><integer>90</integer>
<key>MaxCapacity</key><integer>100</integer>
<key>CycleCount</key><integer>12</integer>
<key>InstantAmperage</key><integer>18446744073709547590</integer>
<key>ExternalConnected</key><true/>
<key>BatteryData</key><dict><key>DesignCapacity</key><integer>6000</integer><key>NominalChargeCapacity</key><integer>5700</integer></dict>
</dict></array></plist>"#;
        let b = parse(&registry_from_ioreg_plist(xml).unwrap()).unwrap();
        assert_eq!(b.percent, 90.0);
        assert_eq!(b.amperage_ma, Some(-4026));
        assert_eq!(b.health.unwrap().percent, 95.0);
        assert_eq!(b.state_label(), "Plugged in, not charging");
    }

    #[test]
    fn power_source_overrides_time_and_charged() {
        let mut m = apple_silicon();
        m.insert("IsCharging".into(), Value::Bool(true));
        m.insert("ExternalConnected".into(), Value::Bool(true));
        m.insert("AvgTimeToFull".into(), i(101));
        let mut b = parse(&m).unwrap();
        assert_eq!(b.minutes_remaining, Some(101));
        // IOPS (pmset) value wins over the registry's.
        apply_power_source(
            &mut b,
            &d(vec![
                ("Time to Full Charge", i(94)),
                ("Time to Empty", i(0)),
            ]),
        );
        assert_eq!(b.minutes_remaining, Some(94));
        // "Calculating" (-1) is unknown, never a number.
        apply_power_source(&mut b, &d(vec![("Time to Full Charge", i(-1))]));
        assert_eq!(b.minutes_remaining, None);
        // On battery the empty-time key is used.
        let mut b = parse(&apple_silicon()).unwrap();
        apply_power_source(
            &mut b,
            &d(vec![
                ("Time to Empty", i(52)),
                ("Time to Full Charge", i(7)),
            ]),
        );
        assert_eq!(b.minutes_remaining, Some(52));
        // Without the key the registry value is kept.
        let mut b = parse(&apple_silicon()).unwrap();
        apply_power_source(&mut b, &Dict::new());
        assert_eq!(b.minutes_remaining, Some(47));
        // "Is Charged" turns a plugged-in, idle battery into "Fully charged".
        let mut m = apple_silicon();
        m.insert("ExternalConnected".into(), Value::Bool(true));
        let mut b = parse(&m).unwrap();
        assert_eq!(b.state_label(), "Plugged in, not charging");
        apply_power_source(&mut b, &d(vec![("Is Charged", Value::Bool(true))]));
        assert_eq!(b.state_label(), "Fully charged");
    }

    #[test]
    fn real_power_source_is_readable() {
        // Read-only probe; a desktop or CI machine may have no battery.
        if let Some(ps) = power_source_via_iops() {
            assert_eq!(
                ps.get("Type").and_then(Value::as_str),
                Some("InternalBattery")
            );
        }
    }

    #[test]
    fn real_read_does_not_panic() {
        // Read-only probe of this machine; any outcome is acceptable.
        let _ = read_battery();
    }
}

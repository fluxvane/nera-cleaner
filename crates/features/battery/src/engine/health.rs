//! Apple's own battery-health figures.
//!
//! On current macOS the registry no longer exposes `AppleRawMaxCapacity` and
//! macOS's "Maximum Capacity" (System Settings > Battery > Battery Health) is
//! not `NominalChargeCapacity / DesignCapacity`: on the reference M1 Max the
//! registry gives 4680 / 6075 = 77 % while System Settings, `system_profiler`
//! and powerd ("Nominal Capacity percentage(79)") all say 79 %. powerd stores
//! the figure and refreshes it rarely, so it cannot be recomputed from live
//! registry values. The only unprivileged way to read the number macOS shows is
//! `system_profiler SPPowerDataType -json`, which takes about 0.2 s; the result
//! is cached for a minute.

use super::battery::Condition;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppleHealth {
    /// "Maximum Capacity", whole percent.
    pub max_capacity_percent: Option<u32>,
    pub condition: Option<Condition>,
}

const CACHE_FOR: Duration = Duration::from_secs(60);

static CACHE: Mutex<Option<(Instant, Option<AppleHealth>)>> = Mutex::new(None);

/// Cached read of `system_profiler`; `None` when it is unavailable.
pub fn apple_health() -> Option<AppleHealth> {
    let mut cache = CACHE.lock().ok()?;
    if let Some((at, value)) = *cache {
        if at.elapsed() < CACHE_FOR {
            return value;
        }
    }
    // A failed run (timeout, transient error) must not flip the tile from
    // macOS's figure to an estimate: keep the last good value.
    let value = run_system_profiler().or_else(|| (*cache).and_then(|(_, v)| v));
    *cache = Some((Instant::now(), value));
    value
}

fn run_system_profiler() -> Option<AppleHealth> {
    let out = std::process::Command::new("/usr/sbin/system_profiler")
        .args(["-json", "SPPowerDataType"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_system_profiler(&out.stdout)
}

/// `"79%"` -> 79.
fn percent(s: &str) -> Option<u32> {
    s.trim()
        .trim_end_matches('%')
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|p| *p <= 100)
}

/// macOS words: "Normal" is fine; "Check Battery" (which System Settings shows
/// as "Service Recommended") and the older "Service Battery", "Replace Soon",
/// "Replace Now" all mean service is recommended.
fn condition(s: &str) -> Option<Condition> {
    match s.trim() {
        "" => None,
        "Normal" | "Good" => Some(Condition::Normal),
        _ => Some(Condition::ServiceRecommended),
    }
}

/// Parse `system_profiler -json SPPowerDataType`.
pub fn parse_system_profiler(json: &[u8]) -> Option<AppleHealth> {
    let root: serde_json::Value = serde_json::from_slice(json).ok()?;
    let info = root
        .get("SPPowerDataType")?
        .as_array()?
        .iter()
        .find_map(|e| e.get("sppower_battery_health_info"))?;
    let max_capacity_percent = info
        .get("sppower_battery_health_maximum_capacity")
        .and_then(|v| v.as_str())
        .and_then(percent);
    let condition = info
        .get("sppower_battery_health")
        .and_then(|v| v.as_str())
        .and_then(condition);
    if max_capacity_percent.is_none() && condition.is_none() {
        return None;
    }
    Some(AppleHealth {
        max_capacity_percent,
        condition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"SPPowerDataType":[{"_name":"spbattery_information",
      "sppower_battery_health_info":{"sppower_battery_cycle_count":812,
      "sppower_battery_health":"Check Battery","sppower_battery_health_maximum_capacity":"79%"}},
      {"_name":"sppower_information"}]}"#;

    #[test]
    fn parses_reference_output() {
        let h = parse_system_profiler(SAMPLE.as_bytes()).unwrap();
        assert_eq!(h.max_capacity_percent, Some(79));
        assert_eq!(h.condition, Some(Condition::ServiceRecommended));
    }

    #[test]
    fn normal_and_missing() {
        let j = r#"{"SPPowerDataType":[{"sppower_battery_health_info":{"sppower_battery_health":"Normal","sppower_battery_health_maximum_capacity":"100%"}}]}"#;
        let h = parse_system_profiler(j.as_bytes()).unwrap();
        assert_eq!(
            (h.max_capacity_percent, h.condition),
            (Some(100), Some(Condition::Normal))
        );
        assert_eq!(
            parse_system_profiler(br#"{"SPPowerDataType":[{"_name":"x"}]}"#),
            None
        );
        assert_eq!(parse_system_profiler(b"not json"), None);
    }
}

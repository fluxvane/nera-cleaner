//! Temperatures and fans from the SMC (port of TemperatureService/FanService).
//!
//! Apple Silicon and Intel Macs name their sensors differently, so SMC `T*`
//! keys are grouped by prefix into a handful of meaningful rows. Nothing is
//! estimated: a Mac without readable sensors yields an empty snapshot.

use super::smc::Smc;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SensorKind {
    Cpu,
    CpuEfficiency,
    Gpu,
    Memory,
    Battery,
    Storage,
    Airflow,
    Chipset,
    Display,
    Skin,
    Wireless,
}

impl SensorKind {
    pub fn label(self) -> &'static str {
        match self {
            SensorKind::Cpu => "CPU",
            SensorKind::CpuEfficiency => "CPU efficiency cores",
            SensorKind::Gpu => "GPU",
            SensorKind::Memory => "Memory",
            SensorKind::Battery => "Battery",
            SensorKind::Storage => "Storage",
            SensorKind::Airflow => "Airflow",
            SensorKind::Chipset => "Chipset",
            SensorKind::Display => "Display",
            SensorKind::Skin => "Palm rest",
            SensorKind::Wireless => "Wireless",
        }
    }

    /// Groups of per-core sensors show their average (as Stats and iStat
    /// Menus do for "CPU" and "GPU"), with the hottest core alongside; other
    /// groups show their hottest sensor.
    pub fn shows_average(self) -> bool {
        matches!(
            self,
            SensorKind::Cpu | SensorKind::CpuEfficiency | SensorKind::Gpu | SensorKind::Memory
        )
    }

    pub fn icon(self) -> &'static str {
        match self {
            SensorKind::Cpu | SensorKind::CpuEfficiency | SensorKind::Chipset => "cpu",
            SensorKind::Battery => "battery",
            SensorKind::Airflow => "fan",
            SensorKind::Storage => "hard-drive",
            SensorKind::Memory => "memory-stick",
            SensorKind::Display => "monitor",
            SensorKind::Wireless => "wifi",
            _ => "thermometer",
        }
    }

    /// True when the group was cross-checked against an independent sensor
    /// (IOHID "gas gauge battery" and "NAND CH0 temp" on an M1 Max). The other
    /// groups come from SMC keys whose meaning is only known from community
    /// reverse engineering, so the screen labels them as unverified.
    pub fn verified(self) -> bool {
        matches!(self, SensorKind::Battery | SensorKind::Storage)
    }

    /// (warning, danger) thresholds in °C. Only the groups that were
    /// cross-checked have thresholds; see [`SensorKind::alert_level`].
    /// Battery: 45 °C is the usual Li-ion limit for charging and 60 °C for
    /// discharging (JEITA-style); Storage: NAND throttles in the 70s.
    pub fn thresholds(self) -> (f64, f64) {
        match self {
            SensorKind::Battery => (45.0, 60.0),
            SensorKind::Storage => (70.0, 85.0),
            _ => (f64::INFINITY, f64::INFINITY),
        }
    }

    pub fn level(self, celsius: f64) -> Level {
        let (warn, danger) = self.thresholds();
        if celsius >= danger {
            Level::Hot
        } else if celsius >= warn {
            Level::Warm
        } else {
            Level::Normal
        }
    }

    /// Level to show as a badge/colour, or `None` when no judgement is made.
    /// Unverified SMC groups never get "Warm"/"Hot": we cannot be sure what
    /// they measure, and the limits are not documented by Apple, so a red
    /// badge would present a guess as fact.
    pub fn alert_level(self, celsius: f64) -> Option<Level> {
        self.verified().then(|| self.level(celsius))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Normal,
    Warm,
    Hot,
}

/// Per-core SMC keys of each Apple Silicon generation, as mapped by the
/// open-source monitor Stats (exelban/stats, Modules/Sensors/values.swift),
/// whose readings match iStat Menus and the fan controller's behaviour.
/// Apple Silicon exposes hundreds of other `T*` keys (hotspot estimates,
/// power-rail and calibration slots); those are not core temperatures and
/// used to inflate "CPU" by 5–10 °C when the hottest of all of them was shown.
struct ChipKeys {
    performance: &'static [&'static str],
    efficiency: &'static [&'static str],
    gpu: &'static [&'static str],
    memory: &'static [&'static str],
}

const M1: ChipKeys = ChipKeys {
    performance: &[
        "Tp01", "Tp05", "Tp0D", "Tp0H", "Tp0L", "Tp0P", "Tp0X", "Tp0b",
    ],
    efficiency: &["Tp09", "Tp0T"],
    gpu: &["Tg05", "Tg0D", "Tg0L", "Tg0T"],
    memory: &["Tm02", "Tm06", "Tm08", "Tm09"],
};
const M2: ChipKeys = ChipKeys {
    performance: &[
        "Tp01", "Tp05", "Tp09", "Tp0D", "Tp0X", "Tp0b", "Tp0f", "Tp0j",
    ],
    efficiency: &["Tp1h", "Tp1t", "Tp1p", "Tp1l"],
    gpu: &["Tg0f", "Tg0j"],
    memory: &[],
};
const M3: ChipKeys = ChipKeys {
    performance: &[
        "Tf04", "Tf09", "Tf0A", "Tf0B", "Tf0D", "Tf0E", "Tf44", "Tf49", "Tf4A", "Tf4B", "Tf4D",
        "Tf4E",
    ],
    efficiency: &["Te05", "Te0L", "Te0P", "Te0S"],
    gpu: &[
        "Tf14", "Tf18", "Tf19", "Tf1A", "Tf24", "Tf28", "Tf29", "Tf2A",
    ],
    memory: &[],
};
const M4: ChipKeys = ChipKeys {
    performance: &[
        "Tp01", "Tp05", "Tp09", "Tp0D", "Tp0V", "Tp0Y", "Tp0b", "Tp0e",
    ],
    efficiency: &["Te05", "Te0S", "Te09", "Te0H"],
    gpu: &[
        "Tg0G", "Tg0H", "Tg1U", "Tg1k", "Tg0K", "Tg0L", "Tg0d", "Tg0e", "Tg0j", "Tg0k",
    ],
    memory: &["Tm0p", "Tm1p", "Tm2p"],
};
const M5: ChipKeys = ChipKeys {
    performance: &[
        "Tp00", "Tp04", "Tp08", "Tp0C", "Tp0G", "Tp0K", "Tp0O", "Tp0R", "Tp0U", "Tp0X", "Tp0a",
        "Tp0d", "Tp0g", "Tp0j", "Tp0m", "Tp0p", "Tp0u", "Tp0y",
    ],
    efficiency: &[],
    gpu: &[
        "Tg0U", "Tg0X", "Tg0d", "Tg0g", "Tg0j", "Tg1Y", "Tg1c", "Tg1g",
    ],
    memory: &[],
};
const M6: ChipKeys = ChipKeys {
    performance: &[
        "Tp0j", "Tp0g", "Tp09", "Tp07", "Tp0m", "Tp0L", "Tp0I", "Tp0d", "Tp0E", "Tp0G", "Tp0b",
        "Tp05",
    ],
    efficiency: &["Te07", "Te08", "Te09", "Te0c", "Te0h", "Te0i"],
    gpu: &[
        "Tg1e", "Tg0d", "Tg0c", "Tg0b", "Tg1c", "Tg1d", "Tg07", "Tg1f", "Tg09", "Tg0a", "Tg1g",
        "Tg1h",
    ],
    memory: &[],
};

fn chip_keys(generation: u8) -> Option<&'static ChipKeys> {
    match generation {
        1 => Some(&M1),
        2 => Some(&M2),
        3 => Some(&M3),
        4 => Some(&M4),
        5 => Some(&M5),
        6 => Some(&M6),
        _ => None,
    }
}

/// Which Mac the sensors belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    Intel,
    /// Apple Silicon generation (1 for M1/M1 Pro/M1 Max/M1 Ultra, ...).
    /// `None` for a chip newer than the key tables: its CPU/GPU keys are
    /// unknown, so they are not shown rather than guessed.
    AppleSilicon(Option<u8>),
}

/// Parse `machdep.cpu.brand_string` ("Apple M1 Max", "Intel(R) Core(TM) ...").
pub fn chip_from_brand(brand: &str) -> Chip {
    let Some(rest) = brand.trim().strip_prefix("Apple M") else {
        return if brand.contains("Intel") {
            Chip::Intel
        } else {
            Chip::AppleSilicon(None)
        };
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    Chip::AppleSilicon(digits.parse().ok())
}

/// This Mac's chip.
pub fn current_chip() -> Chip {
    if !cfg!(target_arch = "aarch64") {
        return Chip::Intel;
    }
    let mut buf = [0u8; 128];
    let mut len = buf.len();
    let name = c"machdep.cpu.brand_string";
    // SAFETY: valid C string, buffer and length pointer.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Chip::AppleSilicon(None);
    }
    chip_from_brand(String::from_utf8_lossy(&buf[..len]).trim_end_matches('\0'))
}

/// Group an SMC temperature key into a sensor kind for `chip`. Only keys whose
/// meaning is well established are accepted; unknown keys are ignored rather
/// than guessed.
pub fn classify(key: &str, chip: Chip) -> Option<SensorKind> {
    let b = key.as_bytes();
    if b.len() != 4 || b[0] != b'T' {
        return None;
    }
    let digit = |i: usize| b[i].is_ascii_digit();
    match chip {
        Chip::AppleSilicon(generation) => {
            if let Some(keys) = generation.and_then(chip_keys) {
                if keys.performance.contains(&key) {
                    return Some(SensorKind::Cpu);
                }
                if keys.efficiency.contains(&key) {
                    return Some(SensorKind::CpuEfficiency);
                }
                if keys.gpu.contains(&key) {
                    return Some(SensorKind::Gpu);
                }
                if keys.memory.contains(&key) {
                    return Some(SensorKind::Memory);
                }
            }
            // TG* are the battery gas gauge's own sensors here, not a GPU.
        }
        Chip::Intel => match (b[1], key) {
            (b'C', "TCXC" | "TCXc" | "TCAD") => return Some(SensorKind::Cpu),
            (b'C', _) if digit(2) => return Some(SensorKind::Cpu),
            (b'G', _) => return Some(SensorKind::Gpu),
            (b'M', _) if digit(2) => return Some(SensorKind::Memory),
            (b'N', _) if digit(2) => return Some(SensorKind::Chipset),
            (b'P', "TPCD") => return Some(SensorKind::Chipset),
            _ => {}
        },
    }
    match (b[1], key) {
        (b'B', _) if digit(2) && b[3] == b'T' => Some(SensorKind::Battery),
        (b'H', _) if digit(2) => Some(SensorKind::Storage),
        (b'a', "TaLP" | "TaRP" | "TaLF" | "TaRF") => Some(SensorKind::Airflow),
        (b'A', _) if digit(2) => Some(SensorKind::Airflow),
        (b'L', _) if digit(2) => Some(SensorKind::Display),
        (b's', "Ts0P" | "Ts1P") => Some(SensorKind::Skin),
        (b'W', _) if digit(2) => Some(SensorKind::Wireless),
        _ => None,
    }
}

/// Plausible die/board temperature; filters out 0 / sentinel readings.
pub fn plausible(celsius: f64) -> bool {
    celsius.is_finite() && celsius > 1.0 && celsius < 130.0
}

#[derive(Clone, Debug, PartialEq)]
pub struct Temperature {
    pub kind: SensorKind,
    /// Hottest sensor in the group.
    pub max: f64,
    pub avg: f64,
    pub count: usize,
}

impl Temperature {
    /// The figure the row shows: the average for per-core groups, else the hottest.
    pub fn value(&self) -> f64 {
        if self.kind.shows_average() {
            self.avg
        } else {
            self.max
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fan {
    /// 1-based index; stable id.
    pub index: u8,
    pub rpm: f64,
    pub min: f64,
    pub max: f64,
    pub target: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SensorSnapshot {
    pub temperatures: Vec<Temperature>,
    pub fans: Vec<Fan>,
    /// False when the SMC could not be opened at all.
    pub smc_available: bool,
}

/// Group `(key, celsius)` readings into per-kind rows.
pub fn group_temperatures(readings: &[(String, f64)], chip: Chip) -> Vec<Temperature> {
    let mut groups: BTreeMap<SensorKind, Vec<f64>> = BTreeMap::new();
    for (key, c) in readings {
        if let (Some(kind), true) = (classify(key, chip), plausible(*c)) {
            groups.entry(kind).or_default().push(*c);
        }
    }
    groups
        .into_iter()
        .map(|(kind, v)| {
            let max = v.iter().cloned().fold(f64::MIN, f64::max);
            let avg = v.iter().sum::<f64>() / v.len() as f64;
            Temperature {
                kind,
                max,
                avg,
                count: v.len(),
            }
        })
        .collect()
}

pub fn read_sensors() -> SensorSnapshot {
    let Some(smc) = Smc::open() else {
        return SensorSnapshot::default();
    };
    let chip = current_chip();
    // Listing the key names takes one call per SMC key (a few thousand); the
    // set never changes while the Mac runs, so it is listed once.
    static TEMPERATURE_KEYS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let keys = match TEMPERATURE_KEYS.get() {
        Some(keys) => keys.clone(),
        None => {
            let keys: Vec<String> = smc
                .keys_with_prefix('T')
                .into_iter()
                .filter(|k| classify(k, chip).is_some())
                .collect();
            // An empty list may be a failed read: try again next time.
            if !keys.is_empty() {
                let _ = TEMPERATURE_KEYS.set(keys.clone());
            }
            keys
        }
    };
    let readings: Vec<(String, f64)> = keys
        .into_iter()
        .filter_map(|k| smc.read_number(&k).map(|v| (k, v)))
        .collect();
    let temperatures = group_temperatures(&readings, chip);

    // FNum is authoritative; if it cannot be read, probe for fans instead of
    // claiming the Mac has none.
    let count = match smc.read_number("FNum") {
        Some(n) if n.is_finite() && n >= 0.0 => n.min(8.0) as u8,
        _ => (0..8u8)
            .take_while(|i| smc.read(&format!("F{i}Ac")).is_some())
            .count() as u8,
    };
    let fans = (0..count)
        .filter_map(|i| {
            let rpm = smc
                .read_number(&format!("F{i}Ac"))
                .filter(|r| r.is_finite() && (0.0..30_000.0).contains(r))?;
            let min = smc.read_number(&format!("F{i}Mn")).unwrap_or(0.0);
            let max = smc.read_number(&format!("F{i}Mx")).unwrap_or(0.0);
            let target = smc.read_number(&format!("F{i}Tg"));
            Some(Fan {
                index: i + 1,
                rpm,
                min,
                max,
                target,
            })
        })
        .collect();

    SensorSnapshot {
        temperatures,
        fans,
        smc_available: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M1_MAX: Chip = Chip::AppleSilicon(Some(1));

    #[test]
    fn reads_the_chip_generation() {
        assert_eq!(chip_from_brand("Apple M1 Max"), M1_MAX);
        assert_eq!(chip_from_brand("Apple M4 Pro"), Chip::AppleSilicon(Some(4)));
        assert_eq!(chip_from_brand("Apple M10"), Chip::AppleSilicon(Some(10)));
        assert_eq!(
            chip_from_brand("Intel(R) Core(TM) i9-9980HK CPU @ 2.40GHz"),
            Chip::Intel
        );
    }

    #[test]
    fn classifies_m1_core_keys_only() {
        assert_eq!(classify("Tp01", M1_MAX), Some(SensorKind::Cpu));
        // On M1 the efficiency cores are Tp09/Tp0T, not Te*.
        assert_eq!(classify("Tp09", M1_MAX), Some(SensorKind::CpuEfficiency));
        assert_eq!(classify("Te00", M1_MAX), None);
        assert_eq!(classify("Tg05", M1_MAX), Some(SensorKind::Gpu));
        assert_eq!(classify("Tm02", M1_MAX), Some(SensorKind::Memory));
        // Hotspot/estimate keys that inflated "CPU" to 111 °C.
        for k in [
            "Tp0Q", "Tp02", "Tp06", "TCMz", "TCMb", "TC10", "Tm0E", "TG0B", "TMVR", "TCHP",
        ] {
            assert_eq!(classify(k, M1_MAX), None, "{k}");
        }
        assert_eq!(classify("TB1T", M1_MAX), Some(SensorKind::Battery));
        assert_eq!(classify("TH0a", M1_MAX), Some(SensorKind::Storage));
        assert_eq!(classify("TaLP", M1_MAX), Some(SensorKind::Airflow));
        assert_eq!(classify("FNum", M1_MAX), None);
    }

    #[test]
    fn unknown_generation_hides_core_groups() {
        let future = Chip::AppleSilicon(Some(42));
        assert_eq!(classify("Tp01", future), None);
        assert_eq!(classify("TB1T", future), Some(SensorKind::Battery));
    }

    #[test]
    fn intel_keys() {
        assert_eq!(classify("TC0P", Chip::Intel), Some(SensorKind::Cpu));
        assert_eq!(classify("TG0D", Chip::Intel), Some(SensorKind::Gpu));
        assert_eq!(classify("Tp0P", Chip::Intel), None);
    }

    #[test]
    fn per_core_groups_show_the_average() {
        let readings: Vec<(String, f64)> = [
            ("Tp01", 100.0),
            ("Tp05", 104.0),
            ("Tp0Q", 111.6),
            ("TB1T", 31.0),
            ("TB2T", 33.0),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
        let g = group_temperatures(&readings, M1_MAX);
        let cpu = g.iter().find(|t| t.kind == SensorKind::Cpu).unwrap();
        assert_eq!((cpu.count, cpu.max, cpu.value()), (2, 104.0, 102.0));
        let bat = g.iter().find(|t| t.kind == SensorKind::Battery).unwrap();
        assert_eq!(bat.value(), 33.0);
    }

    #[test]
    fn verification_and_alert_levels() {
        assert!(SensorKind::Battery.verified() && SensorKind::Storage.verified());
        assert!(!SensorKind::Cpu.verified() && !SensorKind::Airflow.verified());
        // Unverified groups never get a Warm/Hot judgement.
        for k in [
            SensorKind::Cpu,
            SensorKind::Gpu,
            SensorKind::Airflow,
            SensorKind::Wireless,
            SensorKind::Skin,
        ] {
            assert_eq!(k.alert_level(120.0), None, "{k:?}");
        }
        assert_eq!(SensorKind::Battery.alert_level(36.8), Some(Level::Normal));
        assert_eq!(SensorKind::Battery.alert_level(46.0), Some(Level::Warm));
        assert_eq!(SensorKind::Battery.alert_level(61.0), Some(Level::Hot));
        assert_eq!(SensorKind::Storage.alert_level(45.0), Some(Level::Normal));
        assert_eq!(SensorKind::Storage.alert_level(86.0), Some(Level::Hot));
    }

    #[test]
    fn real_read_does_not_panic() {
        // Read-only probe of this machine; any outcome is acceptable.
        let _ = read_sensors();
    }
}

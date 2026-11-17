//! Windows battery reader. Two sources, both of them what Windows itself shows:
//!
//! - `GetSystemPowerStatus`: AC line, charge percentage, estimated time
//!   remaining and battery saver, the values of the taskbar flyout and
//!   Settings > System > Power & battery. Windows often reports the time as
//!   unknown (always while plugged in); that is kept as unknown.
//! - The battery class driver, through `IOCTL_BATTERY_QUERY_*` on every
//!   `GUID_DEVCLASS_BATTERY` device interface: design and full charge
//!   capacity, cycle count, chemistry, voltage and rate, the figures
//!   `powercfg /batteryreport` lists. Capacities are in mWh.
//!
//! Windows has no documented API for CPU/GPU temperatures or fan speeds, so
//! nothing here reads them. The battery's own temperature is used only when
//! its driver answers `BatteryTemperature`.

use windows_sys::Win32::System::Power::{
    BATTERY_CAPACITY_RELATIVE, BATTERY_CHARGING, BATTERY_INFORMATION, BATTERY_IS_SHORT_TERM,
    BATTERY_POWER_ON_LINE, BATTERY_STATUS, BATTERY_SYSTEM_BATTERY, BATTERY_UNKNOWN_CAPACITY,
    BATTERY_UNKNOWN_RATE, BATTERY_UNKNOWN_VOLTAGE, SYSTEM_POWER_STATUS,
};

/// `GetSystemPowerStatus`, decoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PowerStatus {
    /// `None` when Windows reports the AC line status as unknown.
    pub ac_online: Option<bool>,
    /// Charge, 0–100; `None` when unknown.
    pub percent: Option<u8>,
    pub charging: bool,
    /// `BatteryFlag` says the system has no battery.
    pub no_battery: bool,
    /// Battery life left in seconds; `None` while Windows has no estimate
    /// (always the case on AC power).
    pub seconds_remaining: Option<u32>,
    pub battery_saver: bool,
}

impl PowerStatus {
    pub fn decode(s: &SYSTEM_POWER_STATUS) -> PowerStatus {
        // BatteryFlag: 1 high, 2 low, 4 critical, 8 charging, 128 no system
        // battery, 255 unknown.
        let flag = (s.BatteryFlag != 255).then_some(s.BatteryFlag);
        PowerStatus {
            ac_online: match s.ACLineStatus {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            },
            percent: (s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent),
            charging: flag.is_some_and(|f| f & 8 != 0),
            no_battery: flag.is_some_and(|f| f & 128 != 0),
            seconds_remaining: (s.BatteryLifeTime != u32::MAX).then_some(s.BatteryLifeTime),
            battery_saver: s.SystemStatusFlag == 1,
        }
    }
}

/// One battery as its class driver reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Unit {
    /// The driver reports capacities as relative, unitless values
    /// (`BATTERY_CAPACITY_RELATIVE`) instead of mWh.
    pub relative: bool,
    pub design: Option<u32>,
    pub full_charge: Option<u32>,
    pub remaining: Option<u32>,
    /// `None` when the firmware does not report it (the driver returns 0).
    pub cycle_count: Option<u32>,
    /// Chemistry code as the driver gives it ("LION", "LiP", ...).
    pub chemistry: Option<String>,
    pub voltage_mv: Option<u32>,
    /// mW; negative while discharging.
    pub rate_mw: Option<i32>,
    /// `BATTERY_STATUS.PowerState` flags.
    pub power_state: u32,
    pub temperature_c: Option<f64>,
    pub name: Option<String>,
    pub manufacturer: Option<String>,
}

fn known(v: u32) -> Option<u32> {
    (v != 0 && v != BATTERY_UNKNOWN_CAPACITY).then_some(v)
}

/// `BATTERY_INFORMATION.Chemistry`: up to four ASCII characters.
pub fn chemistry_code(raw: [u8; 4]) -> Option<String> {
    let s: String = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as char)
        .collect();
    let s = s.trim();
    (!s.is_empty() && s.chars().all(|c| c.is_ascii_graphic())).then(|| s.to_string())
}

/// NUL-terminated UTF-16 from the driver (device and manufacturer names).
pub fn wide_string(raw: &[u16]) -> Option<String> {
    let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    let s = String::from_utf16_lossy(&raw[..len]);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// `BatteryTemperature` is in tenths of a kelvin; 0 or an implausible value
/// means the driver doesn't really measure it.
pub fn decikelvin_to_celsius(v: u32) -> Option<f64> {
    let c = v as f64 / 10.0 - 273.15;
    (v != 0 && (-40.0..=100.0).contains(&c)).then_some(c)
}

/// Decode one battery. `None` for devices that are not system batteries
/// (a UPS reports `BATTERY_IS_SHORT_TERM`).
pub fn decode_unit(info: &BATTERY_INFORMATION, status: Option<&BATTERY_STATUS>) -> Option<Unit> {
    if info.Capabilities & BATTERY_SYSTEM_BATTERY == 0
        || info.Capabilities & BATTERY_IS_SHORT_TERM != 0
    {
        return None;
    }
    Some(Unit {
        relative: info.Capabilities & BATTERY_CAPACITY_RELATIVE != 0,
        design: known(info.DesignedCapacity),
        full_charge: known(info.FullChargedCapacity),
        remaining: status
            .and_then(|s| (s.Capacity != BATTERY_UNKNOWN_CAPACITY).then_some(s.Capacity)),
        cycle_count: (info.CycleCount != 0).then_some(info.CycleCount),
        chemistry: chemistry_code(info.Chemistry),
        voltage_mv: status.and_then(|s| {
            (s.Voltage != 0 && s.Voltage != BATTERY_UNKNOWN_VOLTAGE).then_some(s.Voltage)
        }),
        rate_mw: status.and_then(|s| (s.Rate != BATTERY_UNKNOWN_RATE as i32).then_some(s.Rate)),
        power_state: status.map(|s| s.PowerState).unwrap_or(0),
        temperature_c: None,
        name: None,
        manufacturer: None,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct BatteryInfo {
    /// Charge, 0–100, as the taskbar shows it. Only meaningful when
    /// `percent_known`.
    pub percent: f64,
    pub percent_known: bool,
    pub is_charging: bool,
    pub fully_charged: bool,
    pub external_connected: bool,
    /// Windows' estimate of the battery time left. Windows estimates only
    /// while discharging and has no time-to-full.
    pub minutes_remaining: Option<u32>,
    pub battery_saver: bool,
    /// Every system battery (most laptops have one).
    pub units: Vec<Unit>,
}

impl BatteryInfo {
    /// Sum over all batteries, or `None` if any of them doesn't report it.
    fn total(&self, f: impl Fn(&Unit) -> Option<u32>) -> Option<u32> {
        if self.units.is_empty() {
            return None;
        }
        let mut sum: u64 = 0;
        for u in &self.units {
            sum += f(u)? as u64;
        }
        u32::try_from(sum).ok()
    }

    /// Capacities are relative values, not mWh.
    pub fn relative(&self) -> bool {
        self.units.iter().any(|u| u.relative)
    }

    pub fn design(&self) -> Option<u32> {
        self.total(|u| u.design)
    }

    pub fn full_charge(&self) -> Option<u32> {
        self.total(|u| u.full_charge)
    }

    pub fn remaining(&self) -> Option<u32> {
        self.total(|u| u.remaining)
    }

    /// Full charge capacity as a percentage of design capacity, the two
    /// figures `powercfg /batteryreport` lists. Not clamped: a new battery
    /// can hold a little more than its design capacity.
    pub fn health_percent(&self) -> Option<f64> {
        Some(self.full_charge()? as f64 / self.design()? as f64 * 100.0)
    }

    /// Net charge/discharge rate in mW (negative while discharging).
    pub fn rate_mw(&self) -> Option<i32> {
        if self.units.is_empty() {
            return None;
        }
        let mut sum: i64 = 0;
        for u in &self.units {
            sum += u.rate_mw? as i64;
        }
        i32::try_from(sum).ok()
    }

    /// Voltage, only for a single battery (several batteries have no single
    /// voltage).
    pub fn voltage_mv(&self) -> Option<u32> {
        match self.units.as_slice() {
            [u] => u.voltage_mv,
            _ => None,
        }
    }

    /// Hottest battery temperature, when a driver reports one.
    pub fn temperature_c(&self) -> Option<f64> {
        self.units
            .iter()
            .filter_map(|u| u.temperature_c)
            .reduce(f64::max)
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

/// Merge the power status and the per-battery data. `None` when Windows
/// reports no battery at all.
pub fn combine(power: PowerStatus, units: Vec<Unit>) -> Option<BatteryInfo> {
    if units.is_empty() && (power.no_battery || power.percent.is_none()) {
        return None;
    }
    let mut info = BatteryInfo {
        percent: 0.0,
        percent_known: false,
        is_charging: power.charging || units.iter().any(|u| u.power_state & BATTERY_CHARGING != 0),
        fully_charged: false,
        external_connected: power.ac_online.unwrap_or_else(|| {
            units
                .iter()
                .any(|u| u.power_state & BATTERY_POWER_ON_LINE != 0)
        }),
        minutes_remaining: None,
        battery_saver: power.battery_saver,
        units,
    };
    // Windows' own percentage; if it is unknown, the same ratio the
    // composite battery uses.
    let percent = power.percent.map(f64::from).or_else(|| {
        let (r, f) = (info.remaining()?, info.full_charge()?);
        Some((r as f64 / f as f64 * 100.0).clamp(0.0, 100.0))
    });
    if let Some(p) = percent {
        info.percent = p;
        info.percent_known = true;
    }
    info.fully_charged =
        info.external_connected && !info.is_charging && percent.is_some_and(|p| p >= 100.0);
    if !info.external_connected && !info.is_charging {
        info.minutes_remaining = power.seconds_remaining.map(|s| s / 60);
    }
    Some(info)
}

/// Current battery state, or `None` on a PC without a battery.
pub fn read_battery() -> Option<BatteryInfo> {
    combine(sys::power_status().unwrap_or_default(), sys::units())
}

/// Raw readings for the dump example.
pub fn read_raw() -> (Option<PowerStatus>, Vec<Unit>) {
    (sys::power_status(), sys::units())
}

mod sys {
    //! The Win32 calls; everything they return is decoded by the pure
    //! functions above.

    use super::{decikelvin_to_celsius, decode_unit, wide_string, PowerStatus, Unit};
    use std::mem::{offset_of, size_of};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
        GUID_DEVCLASS_BATTERY, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Power::{
        BatteryDeviceName, BatteryInformation, BatteryManufactureName, BatteryTemperature,
        GetSystemPowerStatus, BATTERY_INFORMATION, BATTERY_QUERY_INFORMATION,
        BATTERY_QUERY_INFORMATION_LEVEL, BATTERY_STATUS, BATTERY_TAG_INVALID, BATTERY_WAIT_STATUS,
        IOCTL_BATTERY_QUERY_INFORMATION, IOCTL_BATTERY_QUERY_STATUS, IOCTL_BATTERY_QUERY_TAG,
        SYSTEM_POWER_STATUS,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    /// Batteries beyond this are not looked for (laptops have one or two).
    const MAX_BATTERIES: u32 = 16;

    pub fn power_status() -> Option<PowerStatus> {
        let mut s = SYSTEM_POWER_STATUS::default();
        // SAFETY: `s` is a valid out pointer for the duration of the call.
        let ok = unsafe { GetSystemPowerStatus(&mut s) } != 0;
        ok.then(|| PowerStatus::decode(&s))
    }

    struct DevInfo(HDEVINFO);

    impl Drop for DevInfo {
        fn drop(&mut self) {
            // SAFETY: the set came from SetupDiGetClassDevsW and is freed once.
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateFileW and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Device paths (NUL-terminated UTF-16) of every present battery interface.
    fn battery_paths() -> Vec<Vec<u16>> {
        // SAFETY: valid GUID pointer; the other arguments may be null.
        let set = unsafe {
            SetupDiGetClassDevsW(
                &GUID_DEVCLASS_BATTERY,
                null(),
                null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        };
        if set == INVALID_HANDLE_VALUE as HDEVINFO {
            return Vec::new();
        }
        let set = DevInfo(set);
        let mut paths = Vec::new();
        for index in 0..MAX_BATTERIES {
            let mut iface = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            // SAFETY: valid set, GUID and out struct with cbSize set.
            let found = unsafe {
                SetupDiEnumDeviceInterfaces(
                    set.0,
                    null(),
                    &GUID_DEVCLASS_BATTERY,
                    index,
                    &mut iface,
                )
            } != 0;
            if !found {
                break; // ERROR_NO_MORE_ITEMS
            }
            let mut needed = 0u32;
            // SAFETY: size query; a null buffer with size 0 is allowed.
            unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    set.0,
                    &iface,
                    null_mut(),
                    0,
                    &mut needed,
                    null_mut(),
                )
            };
            let needed = needed as usize;
            if needed <= size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
                continue;
            }
            // u64 storage keeps the struct suitably aligned.
            let mut buf = vec![0u64; needed.div_ceil(8)];
            let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            // SAFETY: `buf` holds at least `needed` bytes, aligned for the struct;
            // cbSize is the fixed struct size, as the API requires.
            let ok = unsafe {
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                SetupDiGetDeviceInterfaceDetailW(
                    set.0,
                    &iface,
                    detail,
                    needed as u32,
                    null_mut(),
                    null_mut(),
                )
            } != 0;
            if !ok {
                continue;
            }
            let offset = offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
            let chars = (needed - offset) / 2;
            // SAFETY: DevicePath starts at `offset` inside `buf` and the API
            // filled `needed` bytes; u16 alignment holds (offset is even).
            let path = unsafe {
                std::slice::from_raw_parts(
                    buf.as_ptr().cast::<u8>().add(offset).cast::<u16>(),
                    chars,
                )
            };
            let len = path.iter().position(|&c| c == 0).unwrap_or(chars);
            let mut p = path[..len].to_vec();
            p.push(0);
            paths.push(p);
        }
        paths
    }

    fn open(path: &[u16]) -> Option<Handle> {
        // The battery IOCTLs need read access only.
        // SAFETY: `path` is NUL-terminated; other pointers may be null.
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        (h != INVALID_HANDLE_VALUE && !h.is_null()).then_some(Handle(h))
    }

    /// Send `code` with `input` and read the reply into `out`; returns the
    /// number of bytes the driver wrote.
    fn ioctl<I>(h: &Handle, code: u32, input: &I, out: &mut [u8]) -> Option<usize> {
        let mut returned = 0u32;
        // SAFETY: input and output point to live buffers of the given sizes;
        // synchronous call (no OVERLAPPED).
        let ok = unsafe {
            DeviceIoControl(
                h.0,
                code,
                (input as *const I).cast(),
                size_of::<I>() as u32,
                out.as_mut_ptr().cast(),
                out.len() as u32,
                &mut returned,
                null_mut(),
            )
        } != 0;
        ok.then_some(returned as usize)
    }

    /// `ioctl` into a plain-old-data reply struct; `None` unless the driver
    /// filled all of it.
    fn ioctl_struct<I, O: Copy + Default>(h: &Handle, code: u32, input: &I) -> Option<O> {
        let mut out = O::default();
        // SAFETY: `O` is a repr(C) windows-sys POD struct (or an integer); viewing
        // it as bytes for the driver to fill is sound.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut((&mut out as *mut O).cast::<u8>(), size_of::<O>())
        };
        let n = ioctl(h, code, input, bytes)?;
        (n >= size_of::<O>()).then_some(out)
    }

    fn query(tag: u32, level: BATTERY_QUERY_INFORMATION_LEVEL) -> BATTERY_QUERY_INFORMATION {
        BATTERY_QUERY_INFORMATION {
            BatteryTag: tag,
            InformationLevel: level,
            AtRate: 0,
        }
    }

    fn query_string(
        h: &Handle,
        tag: u32,
        level: BATTERY_QUERY_INFORMATION_LEVEL,
    ) -> Option<String> {
        let mut buf = [0u16; 256];
        // SAFETY: a u16 array viewed as bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), size_of::<[u16; 256]>())
        };
        let n = ioctl(
            h,
            IOCTL_BATTERY_QUERY_INFORMATION,
            &query(tag, level),
            bytes,
        )?;
        wide_string(&buf[..(n / 2).min(buf.len())])
    }

    fn read_unit(path: &[u16]) -> Option<Unit> {
        let h = open(path)?;
        // Wait 0 ms: answer now, or fail if the slot is empty.
        let tag: u32 = ioctl_struct(&h, IOCTL_BATTERY_QUERY_TAG, &0u32)?;
        if tag == BATTERY_TAG_INVALID {
            return None;
        }
        let info: BATTERY_INFORMATION = ioctl_struct(
            &h,
            IOCTL_BATTERY_QUERY_INFORMATION,
            &query(tag, BatteryInformation),
        )?;
        let wait = BATTERY_WAIT_STATUS {
            BatteryTag: tag,
            ..Default::default()
        };
        let status: Option<BATTERY_STATUS> = ioctl_struct(&h, IOCTL_BATTERY_QUERY_STATUS, &wait);
        let mut unit = decode_unit(&info, status.as_ref())?;
        // Most ACPI batteries don't implement this; then it stays None.
        unit.temperature_c = ioctl_struct::<_, u32>(
            &h,
            IOCTL_BATTERY_QUERY_INFORMATION,
            &query(tag, BatteryTemperature),
        )
        .and_then(decikelvin_to_celsius);
        unit.name = query_string(&h, tag, BatteryDeviceName);
        unit.manufacturer = query_string(&h, tag, BatteryManufactureName);
        Some(unit)
    }

    pub fn units() -> Vec<Unit> {
        battery_paths()
            .iter()
            .filter_map(|p| read_unit(p))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Power::{BATTERY_DISCHARGING, BATTERY_UNKNOWN_TIME};

    fn status(ac: u8, flag: u8, percent: u8, life: u32) -> PowerStatus {
        PowerStatus::decode(&SYSTEM_POWER_STATUS {
            ACLineStatus: ac,
            BatteryFlag: flag,
            BatteryLifePercent: percent,
            SystemStatusFlag: 0,
            BatteryLifeTime: life,
            BatteryFullLifeTime: u32::MAX,
        })
    }

    fn info(design: u32, full: u32, cycles: u32) -> BATTERY_INFORMATION {
        BATTERY_INFORMATION {
            Capabilities: BATTERY_SYSTEM_BATTERY,
            Technology: 1,
            Reserved: [0; 3],
            Chemistry: *b"LiP\0",
            DesignedCapacity: design,
            FullChargedCapacity: full,
            DefaultAlert1: 0,
            DefaultAlert2: 0,
            CriticalBias: 0,
            CycleCount: cycles,
        }
    }

    fn bstatus(state: u32, capacity: u32, voltage: u32, rate: i32) -> BATTERY_STATUS {
        BATTERY_STATUS {
            PowerState: state,
            Capacity: capacity,
            Voltage: voltage,
            Rate: rate,
        }
    }

    /// The development machine's ThinkPad battery as WMI (root\wmi) reports
    /// it: full charge 93,110 mWh, remaining 93,110 mWh, 13,127 mV, 46 cycles
    /// (design capacity illustrative).
    fn thinkpad() -> Unit {
        decode_unit(
            &info(99_480, 93_110, 46),
            Some(&bstatus(BATTERY_POWER_ON_LINE, 93_110, 13_127, 0)),
        )
        .unwrap()
    }

    #[test]
    fn struct_layouts_match_the_driver_abi() {
        // Sizes from batclass.h / winbase.h.
        assert_eq!(std::mem::size_of::<BATTERY_INFORMATION>(), 36);
        assert_eq!(std::mem::size_of::<BATTERY_STATUS>(), 16);
        assert_eq!(std::mem::size_of::<SYSTEM_POWER_STATUS>(), 12);
    }

    #[test]
    fn decodes_power_status() {
        let s = status(1, 8 | 1, 87, BATTERY_UNKNOWN_TIME);
        assert_eq!(
            s,
            PowerStatus {
                ac_online: Some(true),
                percent: Some(87),
                charging: true,
                no_battery: false,
                seconds_remaining: None,
                battery_saver: false,
            }
        );
        let s = status(0, 2, 15, 3_725);
        assert_eq!(
            (s.ac_online, s.seconds_remaining),
            (Some(false), Some(3_725))
        );
        // Desktop: no system battery, percent unknown.
        let s = status(1, 128, 255, u32::MAX);
        assert!(s.no_battery && s.percent.is_none());
        // Everything unknown.
        let s = status(255, 255, 255, u32::MAX);
        assert_eq!(
            (s.ac_online, s.charging, s.no_battery),
            (None, false, false)
        );
    }

    #[test]
    fn decodes_battery_information() {
        let u = thinkpad();
        assert_eq!(
            (u.design, u.full_charge, u.remaining, u.cycle_count),
            (Some(99_480), Some(93_110), Some(93_110), Some(46))
        );
        assert_eq!(u.chemistry.as_deref(), Some("LiP"));
        assert_eq!((u.voltage_mv, u.rate_mw), (Some(13_127), Some(0)));
        assert!(!u.relative);
    }

    #[test]
    fn unreported_values_stay_unknown() {
        let u = decode_unit(
            &info(0, BATTERY_UNKNOWN_CAPACITY, 0),
            Some(&bstatus(
                0,
                BATTERY_UNKNOWN_CAPACITY,
                BATTERY_UNKNOWN_VOLTAGE,
                BATTERY_UNKNOWN_RATE as i32,
            )),
        )
        .unwrap();
        // Cycle count 0 = "firmware doesn't report it", not zero cycles.
        assert_eq!(u.cycle_count, None);
        assert_eq!((u.design, u.full_charge, u.remaining), (None, None, None));
        assert_eq!((u.voltage_mv, u.rate_mw), (None, None));
        // No status at all.
        let u = decode_unit(&info(50_000, 45_000, 3), None).unwrap();
        assert_eq!((u.remaining, u.voltage_mv, u.power_state), (None, None, 0));
    }

    #[test]
    fn skips_ups_and_non_system_batteries() {
        let mut ups = info(1, 1, 0);
        ups.Capabilities |= BATTERY_IS_SHORT_TERM;
        assert!(decode_unit(&ups, None).is_none());
        let mut other = info(1, 1, 0);
        other.Capabilities = 0;
        assert!(decode_unit(&other, None).is_none());
        let mut rel = info(100, 95, 0);
        rel.Capabilities |= BATTERY_CAPACITY_RELATIVE;
        assert!(decode_unit(&rel, None).unwrap().relative);
    }

    #[test]
    fn decodes_strings_and_temperature() {
        assert_eq!(chemistry_code(*b"LION"), Some("LION".into()));
        assert_eq!(chemistry_code(*b"LiP\0"), Some("LiP".into()));
        assert_eq!(chemistry_code([0; 4]), None);
        let w: Vec<u16> = "SMP  \0garbage".encode_utf16().collect();
        assert_eq!(wide_string(&w), Some("SMP".into()));
        assert_eq!(wide_string(&[0, 65]), None);
        assert_eq!(wide_string(&[]), None);
        // 3,031 dK = 29.95 °C.
        assert!((decikelvin_to_celsius(3031).unwrap() - 29.95).abs() < 1e-9);
        assert_eq!(decikelvin_to_celsius(0), None);
        assert_eq!(decikelvin_to_celsius(u32::MAX), None);
    }

    #[test]
    fn desktop_has_no_battery() {
        assert!(combine(status(1, 128, 255, u32::MAX), vec![]).is_none());
        assert!(combine(status(255, 255, 255, u32::MAX), vec![]).is_none());
        assert!(combine(PowerStatus::default(), vec![]).is_none());
    }

    #[test]
    fn plugged_in_full_battery() {
        let b = combine(status(1, 1, 100, u32::MAX), vec![thinkpad()]).unwrap();
        assert_eq!(b.percent, 100.0);
        assert!(b.percent_known && b.fully_charged && b.external_connected);
        assert_eq!(b.state_label(), "Fully charged");
        assert_eq!(b.minutes_remaining, None);
        assert!((b.health_percent().unwrap() - 93.597).abs() < 0.001);
        assert_eq!(b.voltage_mv(), Some(13_127));
    }

    #[test]
    fn discharging_with_and_without_estimate() {
        let unit = decode_unit(
            &info(50_000, 45_000, 120),
            Some(&bstatus(BATTERY_DISCHARGING, 22_500, 11_400, -9_800)),
        )
        .unwrap();
        let b = combine(status(0, 0, 50, 5_430), vec![unit.clone()]).unwrap();
        assert_eq!(b.state_label(), "On battery");
        assert_eq!(b.minutes_remaining, Some(90));
        assert_eq!(b.rate_mw(), Some(-9_800));
        let b = combine(status(0, 0, 50, u32::MAX), vec![unit]).unwrap();
        assert_eq!(b.minutes_remaining, None);
    }

    #[test]
    fn charging_never_has_a_time() {
        let unit = decode_unit(
            &info(50_000, 45_000, 120),
            Some(&bstatus(
                BATTERY_POWER_ON_LINE | BATTERY_CHARGING,
                22_500,
                12_400,
                30_000,
            )),
        )
        .unwrap();
        // Even if a bogus life time came with it.
        let b = combine(status(1, 0, 50, 1_000), vec![unit]).unwrap();
        assert!(b.is_charging);
        assert_eq!(b.state_label(), "Charging");
        assert_eq!(b.minutes_remaining, None);
    }

    #[test]
    fn plugged_in_not_charging_below_full() {
        // E.g. a charge threshold at 80 %.
        let b = combine(status(1, 1, 80, u32::MAX), vec![thinkpad()]).unwrap();
        assert!(!b.fully_charged);
        assert_eq!(b.state_label(), "Plugged in, not charging");
    }

    #[test]
    fn unknown_percent_falls_back_to_capacities() {
        let unit = decode_unit(
            &info(50_000, 40_000, 1),
            Some(&bstatus(BATTERY_DISCHARGING, 10_000, 11_000, -5_000)),
        )
        .unwrap();
        let b = combine(status(0, 255, 255, u32::MAX), vec![unit.clone()]).unwrap();
        assert!(b.percent_known);
        assert_eq!(b.percent, 25.0);
        let mut no_caps = unit;
        no_caps.remaining = None;
        let b = combine(status(0, 255, 255, u32::MAX), vec![no_caps]).unwrap();
        assert!(!b.percent_known);
    }

    #[test]
    fn two_batteries_are_summed() {
        let a = decode_unit(
            &info(24_000, 22_000, 300),
            Some(&bstatus(BATTERY_DISCHARGING, 11_000, 12_000, -4_000)),
        )
        .unwrap();
        let b2 = decode_unit(
            &info(48_000, 40_000, 0),
            Some(&bstatus(BATTERY_DISCHARGING, 20_000, 12_100, -2_000)),
        )
        .unwrap();
        let b = combine(status(0, 0, 50, u32::MAX), vec![a, b2]).unwrap();
        assert_eq!(
            (b.design(), b.full_charge(), b.remaining()),
            (Some(72_000), Some(62_000), Some(31_000))
        );
        assert_eq!(b.rate_mw(), Some(-6_000));
        assert_eq!(b.voltage_mv(), None);
    }

    #[test]
    fn real_read_does_not_panic() {
        // Read-only probe of this machine; any outcome is acceptable.
        let _ = read_battery();
        let (power, units) = read_raw();
        if let Some(b) = combine(power.unwrap_or_default(), units) {
            assert!((0.0..=100.0).contains(&b.percent));
        }
    }
}

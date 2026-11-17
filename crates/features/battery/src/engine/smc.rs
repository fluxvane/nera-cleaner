//! Minimal read-only client for the AppleSMC user client (IOKit).
//!
//! The Swift app stubbed this out; here it is implemented for real. Only the
//! "read key / key by index / key info" commands are ever sent. Nothing is
//! written to the SMC.

use io_kit_sys::types::{io_connect_t, io_service_t};
use io_kit_sys::{
    kIOMasterPortDefault, IOConnectCallStructMethod, IOServiceClose, IOServiceGetMatchingService,
    IOServiceMatching, IOServiceOpen,
};
use mach2::kern_return::KERN_SUCCESS;
use mach2::traps::mach_task_self;
use std::ffi::c_void;

const KERNEL_INDEX_SMC: u32 = 2;
const CMD_READ_BYTES: u8 = 5;
const CMD_READ_INDEX: u8 = 8;
const CMD_READ_KEYINFO: u8 = 9;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Vers {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PLimit {
    version: u16,
    length: u16,
    cpu: u32,
    gpu: u32,
    mem: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct KeyInfo {
    data_size: u32,
    data_type: u32,
    attributes: u8,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct KeyData {
    key: u32,
    vers: Vers,
    plimit: PLimit,
    info: KeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

/// A raw SMC value: four-char type code plus payload.
#[derive(Clone, Debug, PartialEq)]
pub struct Raw {
    pub ty: [u8; 4],
    pub bytes: Vec<u8>,
}

pub fn key_to_u32(key: &str) -> Option<u32> {
    let b = key.as_bytes();
    if b.len() != 4 {
        return None;
    }
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

pub fn u32_to_key(v: u32) -> String {
    v.to_be_bytes()
        .iter()
        .map(|&c| if c.is_ascii_graphic() { c as char } else { '?' })
        .collect()
}

/// Multi-byte integers in SMC payloads use the CPU's byte order on Apple
/// Silicon (verified on an M1 Max: `B0DC` design capacity = bytes `bb 17` =
/// 0x17bb = 6075 mAh, `B0NC` nominal capacity = `48 12` = 0x1248 = 4680 mAh,
/// both matching the AppleSmartBattery registry) and big-endian on Intel.
/// Floats (`flt `) are little-endian on Apple Silicon. A handful of
/// bookkeeping keys such as `#KEY` stay big-endian; read those with
/// [`be_u32`].
const NATIVE_LE: bool = cfg!(target_arch = "aarch64");

fn u16_of(b: &[u8]) -> u16 {
    let a = [b[0], b[1]];
    if NATIVE_LE {
        u16::from_le_bytes(a)
    } else {
        u16::from_be_bytes(a)
    }
}

fn u32_of(b: &[u8]) -> u32 {
    let a = [b[0], b[1], b[2], b[3]];
    if NATIVE_LE {
        u32::from_le_bytes(a)
    } else {
        u32::from_be_bytes(a)
    }
}

/// Big-endian `u32` (used by `#KEY`, which is big-endian on every Mac).
pub fn be_u32(raw: &Raw) -> Option<u32> {
    let b = &raw.bytes;
    (b.len() >= 4).then(|| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Decode an SMC payload into a number, by type code. `None` for types we do
/// not understand or payloads that are too short.
pub fn decode(raw: &Raw) -> Option<f64> {
    let b = &raw.bytes;
    match &raw.ty {
        // IEEE float: little-endian on Apple Silicon, big-endian on Intel.
        b"flt " if b.len() >= 4 => {
            let a = [b[0], b[1], b[2], b[3]];
            let v = if NATIVE_LE {
                f32::from_le_bytes(a)
            } else {
                f32::from_be_bytes(a)
            };
            Some(v as f64).filter(|v| v.is_finite())
        }
        // Fixed-point types only occur on Intel Macs (big-endian).
        b"sp78" if b.len() >= 2 => Some(i16::from_be_bytes([b[0], b[1]]) as f64 / 256.0),
        b"sp87" if b.len() >= 2 => Some(i16::from_be_bytes([b[0], b[1]]) as f64 / 128.0),
        b"fpe2" if b.len() >= 2 => Some(u16::from_be_bytes([b[0], b[1]]) as f64 / 4.0),
        b"fp88" if b.len() >= 2 => Some(u16::from_be_bytes([b[0], b[1]]) as f64 / 256.0),
        b"ui8 " if !b.is_empty() => Some(b[0] as f64),
        b"si8 " if !b.is_empty() => Some(b[0] as i8 as f64),
        b"ui16" if b.len() >= 2 => Some(u16_of(b) as f64),
        b"si16" if b.len() >= 2 => Some(u16_of(b) as i16 as f64),
        b"ui32" if b.len() >= 4 => Some(u32_of(b) as f64),
        b"si32" if b.len() >= 4 => Some(u32_of(b) as i32 as f64),
        _ => None,
    }
}

/// An open connection to the SMC. Closed on drop.
pub struct Smc {
    conn: io_connect_t,
}

impl Smc {
    /// `None` when the Mac has no AppleSMC or the user client cannot be opened.
    pub fn open() -> Option<Smc> {
        unsafe {
            let matching = IOServiceMatching(c"AppleSMC".as_ptr());
            if matching.is_null() {
                return None;
            }
            let service: io_service_t =
                IOServiceGetMatchingService(kIOMasterPortDefault, matching as _);
            if service == 0 {
                return None;
            }
            let mut conn: io_connect_t = 0;
            let kr = IOServiceOpen(service, mach_task_self(), 0, &mut conn);
            io_kit_sys::IOObjectRelease(service);
            if kr != KERN_SUCCESS || conn == 0 {
                return None;
            }
            Some(Smc { conn })
        }
    }

    fn call(&self, input: &KeyData) -> Option<KeyData> {
        let mut out = KeyData::default();
        let mut out_size = std::mem::size_of::<KeyData>();
        let kr = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                input as *const KeyData as *const c_void,
                std::mem::size_of::<KeyData>(),
                &mut out as *mut KeyData as *mut c_void,
                &mut out_size,
            )
        };
        if kr == KERN_SUCCESS && out.result == 0 {
            Some(out)
        } else {
            None
        }
    }

    /// Read one key's raw value.
    pub fn read(&self, key: &str) -> Option<Raw> {
        let k = key_to_u32(key)?;
        self.read_u32(k)
    }

    fn read_u32(&self, key: u32) -> Option<Raw> {
        let info = self.call(&KeyData {
            key,
            data8: CMD_READ_KEYINFO,
            ..Default::default()
        })?;
        let size = info.info.data_size.min(32);
        if size == 0 {
            return None;
        }
        let val = self.call(&KeyData {
            key,
            info: KeyInfo {
                data_size: size,
                ..info.info
            },
            data8: CMD_READ_BYTES,
            ..Default::default()
        })?;
        Some(Raw {
            ty: info.info.data_type.to_be_bytes(),
            bytes: val.bytes[..size as usize].to_vec(),
        })
    }

    pub fn read_number(&self, key: &str) -> Option<f64> {
        decode(&self.read(key)?)
    }

    /// Total number of keys the SMC exposes.
    pub fn key_count(&self) -> u32 {
        self.read("#KEY").and_then(|r| be_u32(&r)).unwrap_or(0)
    }

    /// Four-char key name at `index`.
    pub fn key_at(&self, index: u32) -> Option<String> {
        let out = self.call(&KeyData {
            data8: CMD_READ_INDEX,
            data32: index,
            ..Default::default()
        })?;
        Some(u32_to_key(out.key))
    }

    /// All keys whose name starts with `prefix`.
    pub fn keys_with_prefix(&self, prefix: char) -> Vec<String> {
        (0..self.key_count())
            .filter_map(|i| self.key_at(i))
            .filter(|k| k.starts_with(prefix))
            .collect()
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(ty: &[u8; 4], bytes: &[u8]) -> Raw {
        Raw {
            ty: *ty,
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn key_roundtrip() {
        assert_eq!(u32_to_key(key_to_u32("TC0P").unwrap()), "TC0P");
        assert_eq!(key_to_u32("abc"), None);
    }

    #[test]
    fn apple_silicon_captures() {
        if !NATIVE_LE {
            return;
        }
        // Captured from an M1 Max (MacBookPro18,4).
        assert_eq!(decode(&raw(b"ui16", &[0xbb, 0x17])), Some(6075.0)); // B0DC design capacity
        assert_eq!(decode(&raw(b"ui16", &[0x48, 0x12])), Some(4680.0)); // B0NC nominal capacity
        assert_eq!(
            decode(&raw(b"flt ", &[0x98, 0x23, 0xb6, 0x45])).map(|v| v.round()),
            Some(5828.0)
        ); // F0Ac
        assert_eq!(be_u32(&raw(b"ui32", &[0, 0, 0x08, 0xc9])), Some(2249)); // #KEY
    }

    #[test]
    fn struct_matches_smc_abi() {
        assert_eq!(std::mem::size_of::<KeyData>(), 80);
    }

    #[test]
    fn decodes_types() {
        // Native byte order for floats and multi-byte integers.
        let (f, u16b, u32b): (Vec<u8>, Vec<u8>, Vec<u8>) = if NATIVE_LE {
            (
                42.5f32.to_le_bytes().to_vec(),
                6075u16.to_le_bytes().to_vec(),
                3000u32.to_le_bytes().to_vec(),
            )
        } else {
            (
                42.5f32.to_be_bytes().to_vec(),
                6075u16.to_be_bytes().to_vec(),
                3000u32.to_be_bytes().to_vec(),
            )
        };
        assert_eq!(decode(&raw(b"flt ", &f)), Some(42.5));
        assert_eq!(decode(&raw(b"ui16", &u16b)), Some(6075.0));
        assert_eq!(decode(&raw(b"ui32", &u32b)), Some(3000.0));
        assert_eq!(decode(&raw(b"sp78", &[0x2a, 0x80])), Some(42.5));
        assert_eq!(decode(&raw(b"fpe2", &[0x1f, 0x40])), Some(2000.0));
        assert_eq!(decode(&raw(b"ui8 ", &[2])), Some(2.0));
        assert_eq!(decode(&raw(b"flt ", &f32::NAN.to_ne_bytes())), None);
        assert_eq!(decode(&raw(b"ch8*", &[1, 2])), None);
        assert_eq!(decode(&raw(b"flt ", &[1])), None);
    }
}

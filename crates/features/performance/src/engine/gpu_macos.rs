//! GPU load from IOKit: every `IOAccelerator` service publishes a
//! `PerformanceStatistics` dictionary (the same source Activity Monitor's GPU
//! History uses). Works on Apple Silicon and Intel/AMD GPUs without admin
//! rights. Values are instantaneous percentages maintained by the driver.
//! Re-exported by `gpu`.

use super::gpu::GpuStats;
use std::collections::HashMap;
use std::ffi::{c_void, CString};

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::base::{kCFAllocatorDefault, CFGetTypeID, CFRelease, CFTypeRef};
use core_foundation_sys::data::{CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID};
use core_foundation_sys::dictionary::{
    CFDictionaryGetCount, CFDictionaryGetKeysAndValues, CFDictionaryGetTypeID,
    CFDictionaryGetValue, CFDictionaryRef, CFMutableDictionaryRef,
};
use core_foundation_sys::number::{
    kCFNumberSInt64Type, CFNumberGetTypeID, CFNumberGetValue, CFNumberRef,
};
use core_foundation_sys::string::{CFStringGetTypeID, CFStringRef};
use io_kit_sys::types::io_iterator_t;
use io_kit_sys::{
    kIOMasterPortDefault, IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingServices, IOServiceMatching,
};

/// Read every GPU's current load. Empty when IOKit exposes no accelerator
/// (e.g. inside a VM).
pub fn read_gpus() -> Vec<GpuStats> {
    let mut out = Vec::new();
    let class = CString::new("IOAccelerator").expect("static C string");
    // SAFETY: IOServiceMatching returns a +1 dictionary that
    // IOServiceGetMatchingServices consumes; the iterator and each service are
    // released below; property dictionaries follow the CF create rule.
    unsafe {
        let matching = IOServiceMatching(class.as_ptr());
        if matching.is_null() {
            return out;
        }
        let mut iter: io_iterator_t = 0;
        if IOServiceGetMatchingServices(
            kIOMasterPortDefault,
            matching as CFDictionaryRef,
            &mut iter,
        ) != 0
        {
            return out;
        }
        loop {
            let service = IOIteratorNext(iter);
            if service == 0 {
                break;
            }
            let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
            let kr = IORegistryEntryCreateCFProperties(service, &mut props, kCFAllocatorDefault, 0);
            if kr == 0 && !props.is_null() {
                let dict = props as CFDictionaryRef;
                let name = dict_string(dict, "model").unwrap_or_else(|| "Graphics".to_string());
                if let Some(stats) = dict_get(dict, "PerformanceStatistics")
                    .filter(|v| CFGetTypeID(*v) == CFDictionaryGetTypeID())
                    .map(|v| flatten_numbers(v as CFDictionaryRef))
                {
                    if let Some(gpu) = GpuStats::from_statistics(name, &stats) {
                        out.push(gpu);
                    }
                }
                CFRelease(props as CFTypeRef);
            }
            IOObjectRelease(service);
        }
        IOObjectRelease(iter);
    }
    out
}

/// Value for a string key (borrowed; valid while `dict` lives).
unsafe fn dict_get(dict: CFDictionaryRef, key: &str) -> Option<CFTypeRef> {
    let key = CFString::new(key);
    let value = CFDictionaryGetValue(dict, key.as_concrete_TypeRef() as *const c_void);
    (!value.is_null()).then_some(value as CFTypeRef)
}

/// `model` is a CFString on most Macs and NUL-terminated CFData on some.
unsafe fn dict_string(dict: CFDictionaryRef, key: &str) -> Option<String> {
    let value = dict_get(dict, key)?;
    let type_id = CFGetTypeID(value);
    if type_id == CFStringGetTypeID() {
        let s = CFString::wrap_under_get_rule(value as CFStringRef).to_string();
        return (!s.is_empty()).then_some(s);
    }
    if type_id == CFDataGetTypeID() {
        let data = value as core_foundation_sys::data::CFDataRef;
        let bytes =
            std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize);
        let s = String::from_utf8_lossy(bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        return (!s.is_empty()).then_some(s);
    }
    None
}

/// Every numeric entry of a dictionary with string keys.
unsafe fn flatten_numbers(dict: CFDictionaryRef) -> HashMap<String, i64> {
    let count = CFDictionaryGetCount(dict) as usize;
    let mut keys: Vec<*const c_void> = vec![std::ptr::null(); count];
    let mut values: Vec<*const c_void> = vec![std::ptr::null(); count];
    CFDictionaryGetKeysAndValues(dict, keys.as_mut_ptr(), values.as_mut_ptr());
    let mut out = HashMap::with_capacity(count);
    for (k, v) in keys.into_iter().zip(values) {
        if k.is_null()
            || v.is_null()
            || CFGetTypeID(k) != CFStringGetTypeID()
            || CFGetTypeID(v) != CFNumberGetTypeID()
        {
            continue;
        }
        let key = CFString::wrap_under_get_rule(k as CFStringRef).to_string();
        let mut n: i64 = 0;
        if CFNumberGetValue(
            v as CFNumberRef,
            kCFNumberSInt64Type,
            (&mut n as *mut i64).cast(),
        ) {
            out.insert(key, n);
        }
    }
    out
}

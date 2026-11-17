//! Dev-only: prints exactly what the Battery screen would show, using the same
//! engine functions and formatting helpers. `cargo run -p nera-battery --example dump`
//! Pass `raw` to also list every SMC `T*`/`F*` key with its type and raw bytes
//! (macOS), or the decoded power status and battery driver data (Windows).
#[cfg(target_os = "macos")]
use nera_battery::engine::present;
#[cfg(target_os = "macos")]
use nera_battery::engine::smc::{self, Smc};
#[cfg(target_os = "macos")]
use nera_battery::engine::{read_battery, read_sensors};

#[cfg(target_os = "macos")]
fn main() {
    let raw = std::env::args().any(|a| a == "raw");
    if raw {
        match Smc::open() {
            None => println!("SMC: cannot open"),
            Some(s) => {
                println!("SMC keys: {}", s.key_count());
                for i in 0..s.key_count() {
                    let Some(k) = s.key_at(i) else { continue };
                    if !(k.starts_with('T') || k.starts_with('F')) {
                        continue;
                    }
                    match s.read(&k) {
                        Some(r) => println!(
                            "RAW {k} ty={:?} bytes={:02x?} => {:?}",
                            String::from_utf8_lossy(&r.ty),
                            r.bytes,
                            smc::decode(&r)
                        ),
                        None => println!("RAW {k} <unreadable>"),
                    }
                }
            }
        }
    }
    let battery = read_battery();
    let sensors = read_sensors();
    match &battery {
        None => println!("battery: none"),
        Some(b) => {
            println!(
                "TILE Charge: {} | {}",
                present::charge_value(b),
                present::charge_caption(b)
            );
            println!(
                "TILE {}: {} | {}",
                present::health_title(b),
                present::health_value(b),
                present::health_caption(b)
            );
            println!(
                "TILE Cycle count: {} | {}",
                present::thousands(b.cycle_count as u64),
                present::cycles_caption(b)
            );
            println!(
                "TILE Condition: {} | {}",
                present::condition_value(b),
                present::condition_caption(b)
            );
            for (label, value) in present::battery_lines(b, &sensors) {
                println!("BAT {label}: {value}");
            }
        }
    }
    println!("smc_available: {}", sensors.smc_available);
    for t in present::temperature_rows(&sensors) {
        println!(
            "TEMP {}: {} ({})",
            t.kind.label(),
            present::celsius(t.value()),
            present::temperature_subtitle(&t)
        );
    }
    for f in &sensors.fans {
        let (value, sub) = present::fan_lines(f);
        println!("FAN {}: {value} | {sub}", f.index);
    }
}

#[cfg(windows)]
fn main() {
    use nera_battery::engine::battery::read_raw;
    use nera_battery::engine::{present, read_battery};

    if std::env::args().any(|a| a == "raw") {
        let (power, units) = read_raw();
        println!("RAW power status: {power:?}");
        for (i, u) in units.iter().enumerate() {
            println!("RAW battery {}: {u:?}", i + 1);
        }
    }
    match read_battery() {
        None => println!("battery: none (screen shows \"No battery\")"),
        Some(b) => {
            println!(
                "TILE Charge: {} | {}",
                present::charge_value(&b),
                present::charge_caption(&b)
            );
            println!(
                "TILE {}: {} | {}",
                present::health_title(&b),
                present::health_value(&b),
                present::health_caption(&b)
            );
            println!(
                "TILE Cycle count: {} | {}",
                present::cycles_value(&b),
                present::cycles_caption(&b)
            );
            for (label, value) in present::battery_lines(&b) {
                println!("BAT {label}: {value}");
            }
        }
    }
    println!("Temperatures and fans: not available on Windows (section explains why)");
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("dump_battery: this feature is available on macOS and Windows only");
}

//! RAM-aware model verdicts — the "will this actually run smoothly on your
//! machine?" logic behind the model picker warnings.

use serde::Serialize;
use std::sync::Mutex;
use sysinfo::System;

pub struct RamInfo {
    pub total: u64,
    pub available: u64,
}

pub fn ram_info() -> RamInfo {
    static SYS: Mutex<Option<System>> = Mutex::new(None);
    let mut guard = SYS.lock().expect("sysinfo lock");
    let sys = guard.get_or_insert_with(System::new);
    sys.refresh_memory();
    RamInfo {
        total: sys.total_memory(),
        available: sys.available_memory(),
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum RamVerdict {
    Ok,
    Caution { message: String },
    NotRecommended { message: String },
}

const GIB: u64 = 1024 * 1024 * 1024;

pub fn verdict(est_ram_bytes: u64) -> RamVerdict {
    let ram = ram_info();
    let gb = |b: u64| b as f64 / GIB as f64;

    if ram.available.saturating_sub(est_ram_bytes) < 2 * GIB {
        return RamVerdict::NotRecommended {
            message: format!(
                "Needs ~{:.1} GB but only {:.1} GB is free. Your whole system would slow down — a smaller model will feel much better on this machine.",
                gb(est_ram_bytes),
                gb(ram.available),
            ),
        };
    }
    if est_ram_bytes > ram.total / 4 || est_ram_bytes > ram.available / 2 {
        return RamVerdict::Caution {
            message: format!(
                "Uses ~{:.1} GB of your {:.0} GB RAM. Fine on its own, but may feel heavy with many apps open.",
                gb(est_ram_bytes),
                gb(ram.total),
            ),
        };
    }
    RamVerdict::Ok
}

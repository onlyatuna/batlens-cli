use std::collections::BTreeMap;
use std::fmt;

/// Energy unit wrapper (milliwatt-hours mWh -> formatted as "X.X Wh")
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mwh(pub i64);

impl fmt::Display for Mwh {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1} Wh", self.0 as f64 / 1000.0)
    }
}

/// Power unit wrapper (milliwatts mW -> formatted as "X.X W" or signed "+X.X W")
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mw(pub i64);

impl fmt::Display for Mw {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1} W", self.0 as f64 / 1000.0)
    }
}

impl Mw {
    #[inline]
    pub fn signed(self) -> String {
        format!("{:+.1} W", self.0 as f64 / 1000.0)
    }
}

/// Voltage unit wrapper (millivolts mV -> formatted as "X.XX V")
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mv(pub u32);

impl fmt::Display for Mv {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2} V", self.0 as f64 / 1000.0)
    }
}

/// Floating-point power wrapper (watts W -> formatted as "X.X W")
#[derive(Copy, Clone, Debug, Default, PartialEq, PartialOrd)]
pub struct Watts(pub f64);

impl fmt::Display for Watts {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1} W", self.0)
    }
}

impl Watts {
    #[inline]
    pub fn signed(self) -> String {
        format!("{:+.1} W", self.0)
    }
}

/// Duration unit wrapper (hours h -> formatted as "X.X h")
#[derive(Copy, Clone, Debug, Default, PartialEq, PartialOrd)]
pub struct Hours(pub f64);

impl fmt::Display for Hours {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1} h", self.0)
    }
}

/// Windows 100 ns timer ticks per hour.
pub const TICKS_PER_HOUR: f64 = 36_000_000_000.0;

#[derive(Debug)]
pub enum BatlensError {
    DeviceOpen { code: u32 },
    DeviceIoctl { ioctl: &'static str, code: u32 },
    SetupApi { api: &'static str, code: u32 },
    NoActiveBattery { diagnostics: Vec<String> },
    PowercfgNotFound,
    PowercfgFailed { status: Option<i32>, stderr: String },
    Io(std::io::Error),
    XmlParse(quick_xml::Error),
    InvalidReport(String),
}

pub fn win32_error_hint(code: u32) -> &'static str {
    match code {
        2 => " (Device or file not found)",
        5 => " (Access denied. Try running as Administrator)",
        31 => " (ACPI driver error)",
        32 => " (Sharing violation, locked by another process)",
        259 => " (No more items)",
        1168 => " (Element not found)",
        _ => "",
    }
}

impl fmt::Display for BatlensError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceOpen { code } => {
                let hint = win32_error_hint(*code);
                write!(
                    f,
                    "CreateFileW failed to open battery device (Win32 error {code}{hint})"
                )
            }
            Self::DeviceIoctl { ioctl, code } => {
                let hint = win32_error_hint(*code);
                write!(f, "{ioctl} failed (Win32 error {code}{hint})")
            }
            Self::SetupApi { api, code } => {
                let hint = win32_error_hint(*code);
                write!(f, "{api} failed (Win32 error {code}{hint})")
            }
            Self::NoActiveBattery { diagnostics } => {
                if diagnostics.is_empty() {
                    write!(f, "No active battery detected via ACPI/IOCTL")
                } else {
                    write!(
                        f,
                        "No active battery detected via ACPI/IOCTL. Diagnostic details:\n{}",
                        diagnostics.join("\n")
                    )
                }
            }
            Self::PowercfgNotFound => write!(
                f,
                "powercfg not found; report and chart need it. 'batlens status' still works."
            ),
            Self::PowercfgFailed { status: _, stderr } => {
                let s = stderr.trim();
                if s.is_empty() {
                    write!(
                        f,
                        "powercfg failed with no error output. 'batlens status' still works."
                    )
                } else {
                    write!(
                        f,
                        "powercfg failed: {s} (Note: 'batlens status' uses direct IOCTL and does not require powercfg)"
                    )
                }
            }
            Self::Io(err) => write!(
                f,
                "standard I/O error (file access, lock conflict, sharing violation): {err}"
            ),
            Self::XmlParse(err) => write!(f, "XML report parsing error: {err}"),
            Self::InvalidReport(msg) => {
                write!(f, "Incomplete or invalid battery report XML: {msg}")
            }
        }
    }
}

impl std::error::Error for BatlensError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::XmlParse(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BatlensError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<quick_xml::Error> for BatlensError {
    fn from(err: quick_xml::Error) -> Self {
        Self::XmlParse(err)
    }
}

#[derive(Clone, Debug, Default)]
pub struct BatteryUnit {
    pub device: String,
    pub manufacturer: String,
    pub chemistry: String,
    pub serial_number: String,
    pub percent: Option<u32>,
    pub voltage_mv: u32,
    pub remaining_mwh: i64,
    pub full_mwh: i64,
    pub design_mwh: i64,
    pub rate_mw: i64,
    pub low_alert_mwh: u32,
    pub crit_alert_mwh: u32,
    pub cycle_count: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Sample {
    pub batteries: Vec<BatteryUnit>,
    pub percent: Option<u32>,
    pub ac: bool,
    pub remaining_mwh: i64,
    pub full_mwh: i64,
    pub design_mwh: i64,
    pub rate_mw: i64,
    pub dis_w: f64,
    pub gross_dis_w: f64,
    pub gross_chg_w: f64,
    pub low_alert_mwh: u32,
    pub crit_alert_mwh: u32,
}

/// A cache-derived design capacity is accepted only if `full / 1.2 <= design <= full * max_ratio`
/// (`max_ratio` given x10). A lone battery gets a wider upper bound. `full == 0` never passes.
fn is_plausible_design(design: i64, full: i64, max_ratio_x10: i64) -> bool {
    design > 0 && full > 0 && design <= full * max_ratio_x10 / 10 && design * 12 / 10 >= full
}
const MULTI_MAX_X10: i64 = 18;
const SINGLE_MAX_X10: i64 = 30;

impl Sample {
    pub fn apply_design_capacity_cache(&mut self, cache: &DesignCapacityCache) {
        if self.batteries.is_empty() {
            return;
        }
        let mut total_design = 0i64;
        let mut total_full = 0i64;
        for b in &mut self.batteries {
            if b.design_mwh == 0
                && let Some(cached_val) = cache.lookup(b)
            {
                b.design_mwh = cached_val;
            }
            total_design += b.design_mwh;
            total_full += b.full_mwh;
        }

        // Module count recorded in the cache.
        let recorded_module_count = if cache.module_count > 0 {
            cache.module_count
        } else {
            cache
                .modules
                .keys()
                .filter(|k| k.starts_with("bat"))
                .count()
        };

        // The cache recorded more modules than are connected, so its total includes hardware
        // that is not present. Do not derive from it.
        let multi_battery_mismatch = recorded_module_count > self.batteries.len();
        let mut all_missing_total_ok = false;

        if !multi_battery_mismatch && total_design < cache.total && cache.total > 0 {
            if self.batteries.len() == 1 && self.batteries[0].design_mwh == 0 {
                if is_plausible_design(cache.total, self.batteries[0].full_mwh, SINGLE_MAX_X10) {
                    self.batteries[0].design_mwh = cache.total;
                }
            } else if self.batteries.len() > 1 {
                let missing_count = self.batteries.iter().filter(|b| b.design_mwh == 0).count();
                if missing_count == 1 {
                    let known: i64 = self.batteries.iter().map(|b| b.design_mwh).sum();
                    if known < cache.total
                        && let Some(missing_b) =
                            self.batteries.iter_mut().find(|b| b.design_mwh == 0)
                    {
                        let derived = cache.total - known;
                        let plausible =
                            is_plausible_design(derived, missing_b.full_mwh, MULTI_MAX_X10);
                        if plausible {
                            missing_b.design_mwh = derived;
                        }
                    }
                } else if missing_count == self.batteries.len() {
                    all_missing_total_ok = total_full > 0
                        && is_plausible_design(cache.total, total_full, MULTI_MAX_X10);
                }
            }
        }
        // A partial sum would inflate health, so the total stays N/A while any module is
        // unresolved. If every module was missing and the cached total is plausible, use it.
        self.design_mwh = if self.batteries.iter().all(|b| b.design_mwh > 0) {
            self.batteries.iter().map(|b| b.design_mwh).sum()
        } else if all_missing_total_ok {
            cache.total
        } else {
            0
        };
    }
}

#[derive(Clone, Debug, Default)]
pub struct ReportBattery {
    pub id: String,
    pub manufacturer: Option<String>,
    pub serial_number: Option<String>,
    pub chemistry: Option<String>,
    pub design_capacity: Option<i64>,
    pub full_charge: Option<i64>,
    pub cycle_count: Option<u32>,
}

pub struct HistPoint {
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    pub full: u64,
    pub design: u64,
    pub cycles: Option<u32>,
}

pub struct Seg {
    pub t0: Option<chrono::DateTime<chrono::Utc>>,
    pub t1: Option<chrono::DateTime<chrono::Utc>>,
    pub start_mwh: i64,
    pub end_mwh: i64,
}

pub struct Usage {
    pub t: Option<chrono::DateTime<chrono::Utc>>,
    pub dur_ticks: u64,
    pub ac: bool,
    pub entry_type: String,
    pub discharge_mwh: u64,
}

#[derive(Default)]
pub struct Report {
    pub system_product: Option<String>,
    pub system_mfg: Option<String>,
    pub bios_version: Option<String>,
    pub bios_date: Option<String>,
    pub connected_standby: bool,
    pub batteries: Vec<ReportBattery>,
    pub design_capacity: Option<i64>,
    pub full_charge: Option<i64>,
    pub cycle_count: Option<u32>,
    pub est_active_runtime: Option<String>,
    pub drains: Vec<Seg>,
    pub history: Vec<HistPoint>,
    pub usage: Vec<Usage>,
}

#[derive(Default, Clone, Debug)]
pub struct DesignCapacityCache {
    pub total: i64,
    pub module_count: usize,
    pub modules: BTreeMap<String, i64>,
}
pub fn is_valid_serial(sn: &str) -> bool {
    let trimmed = sn.trim();
    if trimmed.len() < 2 {
        return false;
    }
    let lower = trimmed.to_lowercase();
    if matches!(
        lower.as_str(),
        "none" | "unknown" | "n/a" | "null" | "default" | "not specified"
    ) {
        return false;
    }
    if trimmed.chars().all(|c| c == '0' || c == '-' || c == ' ') {
        return false;
    }
    true
}

impl DesignCapacityCache {
    pub fn lookup(&self, unit: &BatteryUnit) -> Option<i64> {
        // Without a live full-charge value (relative units, unknown capacity) health would
        // be 0 / design, and the cache cannot be validated.
        if unit.full_mwh <= 0 {
            return None;
        }
        let is_stale_or_mismatched = |dc: i64| -> bool {
            // Full charge more than 120% of the cached design capacity: cache is stale.
            unit.full_mwh > 0 && unit.full_mwh > dc * 12 / 10
        };

        if is_valid_serial(&unit.serial_number)
            && let Some(&dc) = self.modules.get(&unit.serial_number.to_lowercase())
            && dc > 0
            && !is_stale_or_mismatched(dc)
        {
            return Some(dc);
        }
        if !unit.device.is_empty()
            && unit.device != "Unknown"
            && let Some(&dc) = self.modules.get(&unit.device.to_lowercase())
            && dc > 0
            && !is_stale_or_mismatched(dc)
        {
            return Some(dc);
        }
        None
    }
}

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_ITEMS, GENERIC_READ, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetConsoleOutputCP, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler, SetConsoleMode, SetConsoleOutputCP,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::types::*;

pub const BATTERY_UNKNOWN_CAPACITY: u32 = 0xFFFF_FFFF;
pub const BATTERY_UNKNOWN_RATE: i32 = -2147483648; // 0x80000000 as i32

// GUID_DEVINTERFACE_BATTERY: {72631e54-78a4-11d0-bcf7-00aa00b7b32a}
pub const GUID_DEVINTERFACE_BATTERY: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0x72631e54,
    data2: 0x78a4,
    data3: 0x11d0,
    data4: [0xbc, 0xf7, 0x00, 0xaa, 0x00, 0xb7, 0xb3, 0x2a],
};

pub const IOCTL_BATTERY_QUERY_TAG: u32 = 0x00294040;
pub const IOCTL_BATTERY_QUERY_INFORMATION: u32 = 0x00294044;
pub const IOCTL_BATTERY_QUERY_STATUS: u32 = 0x0029404c;

pub const BATTERY_QUERY_DEVICE_NAME: u32 = 4;
pub const BATTERY_QUERY_MANUFACTURE_NAME: u32 = 6;
pub const BATTERY_QUERY_SERIAL_NUMBER: u32 = 8;
pub const BATTERY_CAPACITY_RELATIVE: u32 = 0x4000_0000;
pub const SP_DEVICE_INTERFACE_DETAIL_DATA_W_CBSIZE: u32 = if cfg!(target_pointer_width = "64") {
    8
} else {
    6
};

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BatteryQueryInformation {
    pub battery_tag: u32,
    pub information_level: u32,
    pub at_rate: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BatteryInformation {
    pub capabilities: u32,
    pub technology: u8,
    pub reserved: [u8; 3],
    pub chemistry: [u8; 4],
    pub designed_capacity: u32,
    pub full_charged_capacity: u32,
    pub default_alert1: u32,
    pub default_alert2: u32,
    pub critical_bias: u32,
    pub cycle_count: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BatteryWaitStatus {
    pub battery_tag: u32,
    pub timeout: u32,
    pub power_state: u32,
    pub low_capacity: u32,
    pub high_capacity: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BatteryStatus {
    pub power_state: u32,
    pub capacity: u32,
    pub voltage: u32,
    pub rate: i32,
}

pub struct HandleGuard(pub HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

pub struct DevListGuard(pub HDEVINFO);
impl Drop for DevListGuard {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE as isize && self.0 != 0 {
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }
}

static RESTORE_CP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static RESTORE_MODE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CONSOLE_RESTORED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn restore_console_state() {
    // Only one caller (Ctrl handler or ConsoleGuard::drop) restores the console.
    if CONSOLE_RESTORED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let mode_packed = RESTORE_MODE.swap(0, std::sync::atomic::Ordering::SeqCst);
    if mode_packed != 0 {
        let mode = mode_packed as u32;
        let h_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if h_out != INVALID_HANDLE_VALUE && !h_out.is_null() {
            unsafe { SetConsoleMode(h_out, mode) };
        }
    }
    let cp = RESTORE_CP.swap(0, std::sync::atomic::Ordering::SeqCst);
    if cp != 0 && cp != 65001 {
        unsafe { SetConsoleOutputCP(cp) };
    }
}

unsafe extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> windows_sys::core::BOOL {
    restore_console_state();
    0
}
pub struct ConsoleGuard {
    pub original_cp: u32,
    pub original_mode: Option<(HANDLE, u32)>,
}

impl ConsoleGuard {
    pub fn init() -> Self {
        CONSOLE_RESTORED.store(false, std::sync::atomic::Ordering::SeqCst);
        let original_cp = unsafe { GetConsoleOutputCP() };
        if original_cp != 0 && original_cp != 65001 {
            unsafe { SetConsoleOutputCP(65001) };
            RESTORE_CP.store(original_cp, std::sync::atomic::Ordering::SeqCst);
        }

        let mut original_mode = None;
        let h_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if h_out != INVALID_HANDLE_VALUE && !h_out.is_null() {
            let mut mode = 0u32;
            if unsafe { GetConsoleMode(h_out, &mut mode) } != 0 {
                let new_mode = mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                if unsafe { SetConsoleMode(h_out, new_mode) } != 0 {
                    original_mode = Some((h_out, mode));
                    RESTORE_MODE.store(
                        mode as u64 | 0x1_0000_0000,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                } else {
                    colored::control::set_override(false);
                }
            } else {
                colored::control::set_override(false);
            }
        } else {
            colored::control::set_override(false);
        }

        unsafe {
            SetConsoleCtrlHandler(Some(console_ctrl_handler), 1);
        }

        Self {
            original_cp,
            original_mode,
        }
    }
}

impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        unsafe {
            SetConsoleCtrlHandler(Some(console_ctrl_handler), 0);
        }
        restore_console_state();
    }
}

pub fn chart_width_from_cols(cols: usize) -> usize {
    let available = cols.saturating_sub(9);
    let min_bound = 30.min(available);
    available.clamp(min_bound, 300)
}

pub fn terminal_columns() -> usize {
    let conout: [u16; 8] = [67, 79, 78, 79, 85, 84, 36, 0]; // "CONOUT$\0"
    let handle = unsafe {
        CreateFileW(
            conout.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            0 as _,
        )
    };
    if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
        let _guard = HandleGuard(handle);
        let mut csbi: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetConsoleScreenBufferInfo(handle, &mut csbi) != 0 };
        if ok {
            let cols = (csbi.srWindow.Right - csbi.srWindow.Left + 1).max(0) as usize;
            if cols > 0 {
                return cols;
            }
        }
    }

    if let Ok(cols_str) = std::env::var("COLUMNS")
        && let Ok(cols) = cols_str.trim().parse::<usize>()
        && cols > 0
    {
        return cols;
    }

    80
}

pub fn default_chart_width() -> usize {
    chart_width_from_cols(terminal_columns())
}

pub fn terminal_width() -> usize {
    terminal_columns()
}

#[derive(Debug)]
pub struct EnumeratedDevices {
    pub paths: Vec<Vec<u16>>,
    pub errors: Vec<String>,
}

pub fn enumerate_battery_device_paths() -> Result<EnumeratedDevices, BatlensError> {
    let hdev = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_BATTERY,
            std::ptr::null(),
            0 as _,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if hdev == INVALID_HANDLE_VALUE as isize || hdev == 0 {
        return Err(BatlensError::SetupApi {
            api: "SetupDiGetClassDevsW",
            code: unsafe { GetLastError() },
        });
    }
    let _dev_guard = DevListGuard(hdev);

    let mut did: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
    did.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

    let mut paths = Vec::new();
    let mut errors = Vec::new();
    let mut consecutive_errors = 0usize;

    for idx in 0.. {
        if unsafe {
            SetupDiEnumDeviceInterfaces(
                hdev,
                std::ptr::null_mut(),
                &GUID_DEVINTERFACE_BATTERY,
                idx,
                &mut did,
            )
        } == 0
        {
            let err = unsafe { GetLastError() };
            if err == ERROR_NO_MORE_ITEMS {
                break;
            }
            errors.push(format!(
                "SetupDiEnumDeviceInterfaces failed at device index {idx} (error {err})"
            ));
            consecutive_errors += 1;
            if consecutive_errors >= 8 {
                break;
            }
            continue;
        }
        consecutive_errors = 0;

        let mut req_size = 0u32;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                hdev,
                &did,
                std::ptr::null_mut(),
                0,
                &mut req_size,
                std::ptr::null_mut(),
            );
        }
        let path_offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        if (req_size as usize) < path_offset + 2 {
            errors.push(format!(
                "Device #{idx}: SetupDiGetDeviceInterfaceDetailW returned invalid size ({req_size} < {})",
                path_offset + 2
            ));
            continue;
        }
        let u64_count = (req_size as usize).div_ceil(8);
        let mut detail_buf = vec![0u64; u64_count.max(1) + 1];
        let detail_ptr = detail_buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        unsafe { (*detail_ptr).cbSize = SP_DEVICE_INTERFACE_DETAIL_DATA_W_CBSIZE };

        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                hdev,
                &did,
                detail_buf.as_mut_ptr() as *mut _,
                req_size,
                &mut req_size,
                std::ptr::null_mut(),
            )
        } == 0
        {
            errors.push(format!(
                "Device #{idx}: SetupDiGetDeviceInterfaceDetailW failed (error {})",
                unsafe { GetLastError() }
            ));
            continue;
        }

        let path_ptr = unsafe { (detail_buf.as_ptr() as *const u8).add(path_offset) as *const u16 };
        let path_bytes = (req_size as usize).saturating_sub(path_offset);
        let path_chars = (path_bytes & !1) / 2;
        let raw_parts = unsafe { std::slice::from_raw_parts(path_ptr, path_chars) };
        let Some(null_pos) = raw_parts.iter().position(|&c| c == 0) else {
            errors.push(format!(
                "Device #{idx}: DevicePath contained missing null-terminator in returned buffer"
            ));
            continue;
        };
        let path_slice = &raw_parts[..null_pos];
        let mut path_vec: Vec<u16> = path_slice.to_vec();
        path_vec.push(0);
        paths.push(path_vec);
    }

    Ok(EnumeratedDevices { paths, errors })
}

pub fn parse_battery_name_utf16(buf: &[u16], bytes_ret: u32) -> String {
    let even_bytes = bytes_ret as usize & !1;
    let chars_len = (even_bytes / std::mem::size_of::<u16>()).min(buf.len());
    let mut valid_chars = &buf[..chars_len];
    if let Some(pos) = valid_chars.iter().position(|&c| c == 0) {
        valid_chars = &valid_chars[..pos];
    }
    String::from_utf16_lossy(valid_chars).trim().to_string()
}

pub fn query_battery_unit(path: &[u16]) -> Result<(BatteryUnit, bool), BatlensError> {
    let h_file = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            0 as _,
        )
    };
    if h_file == INVALID_HANDLE_VALUE || h_file.is_null() {
        return Err(BatlensError::DeviceOpen {
            code: unsafe { GetLastError() },
        });
    }
    let _guard = HandleGuard(h_file);

    let timeout = 0u32;
    let mut batt_tag = 0u32;
    let mut bytes_ret = 0u32;
    if unsafe {
        DeviceIoControl(
            h_file,
            IOCTL_BATTERY_QUERY_TAG,
            &timeout as *const _ as *const _,
            std::mem::size_of::<u32>() as u32,
            &mut batt_tag as *mut _ as *mut _,
            std::mem::size_of::<u32>() as u32,
            &mut bytes_ret,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(BatlensError::DeviceIoctl {
            ioctl: "IOCTL_BATTERY_QUERY_TAG",
            code: unsafe { GetLastError() },
        });
    }
    if batt_tag == 0 {
        return Err(BatlensError::DeviceIoctl {
            ioctl: "IOCTL_BATTERY_QUERY_TAG (tag 0, battery not recognized)",
            code: 0,
        });
    }

    let wait_status = BatteryWaitStatus {
        battery_tag: batt_tag,
        timeout: 0,
        power_state: 0,
        low_capacity: 0,
        high_capacity: 0,
    };
    let mut status: BatteryStatus = unsafe { std::mem::zeroed() };
    if unsafe {
        DeviceIoControl(
            h_file,
            IOCTL_BATTERY_QUERY_STATUS,
            &wait_status as *const _ as *const _,
            std::mem::size_of::<BatteryWaitStatus>() as u32,
            &mut status as *mut _ as *mut _,
            std::mem::size_of::<BatteryStatus>() as u32,
            &mut bytes_ret,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(BatlensError::DeviceIoctl {
            ioctl: "IOCTL_BATTERY_QUERY_STATUS",
            code: unsafe { GetLastError() },
        });
    }

    let query_info = BatteryQueryInformation {
        battery_tag: batt_tag,
        information_level: 0,
        at_rate: 0,
    };
    let mut info: BatteryInformation = unsafe { std::mem::zeroed() };
    if unsafe {
        DeviceIoControl(
            h_file,
            IOCTL_BATTERY_QUERY_INFORMATION,
            &query_info as *const _ as *const _,
            std::mem::size_of::<BatteryQueryInformation>() as u32,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<BatteryInformation>() as u32,
            &mut bytes_ret,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(BatlensError::DeviceIoctl {
            ioctl: "IOCTL_BATTERY_QUERY_INFORMATION (information_level=0)",
            code: unsafe { GetLastError() },
        });
    }

    let query_name = BatteryQueryInformation {
        battery_tag: batt_tag,
        information_level: BATTERY_QUERY_DEVICE_NAME,
        at_rate: 0,
    };
    let mut name_buf = [0u16; 128];
    let mut dev_name = String::new();
    if unsafe {
        DeviceIoControl(
            h_file,
            IOCTL_BATTERY_QUERY_INFORMATION,
            &query_name as *const _ as *const _,
            std::mem::size_of::<BatteryQueryInformation>() as u32,
            name_buf.as_mut_ptr() as *mut _,
            (name_buf.len() * std::mem::size_of::<u16>()) as u32,
            &mut bytes_ret,
            std::ptr::null_mut(),
        )
    } != 0
    {
        dev_name = parse_battery_name_utf16(&name_buf, bytes_ret);
    }

    let query_mfg = BatteryQueryInformation {
        battery_tag: batt_tag,
        information_level: BATTERY_QUERY_MANUFACTURE_NAME,
        at_rate: 0,
    };
    let mut mfg_buf = [0u16; 128];
    let mut mfg_name = String::new();
    if unsafe {
        DeviceIoControl(
            h_file,
            IOCTL_BATTERY_QUERY_INFORMATION,
            &query_mfg as *const _ as *const _,
            std::mem::size_of::<BatteryQueryInformation>() as u32,
            mfg_buf.as_mut_ptr() as *mut _,
            (mfg_buf.len() * std::mem::size_of::<u16>()) as u32,
            &mut bytes_ret,
            std::ptr::null_mut(),
        )
    } != 0
    {
        mfg_name = parse_battery_name_utf16(&mfg_buf, bytes_ret);
    }

    let query_serial = BatteryQueryInformation {
        battery_tag: batt_tag,
        information_level: BATTERY_QUERY_SERIAL_NUMBER,
        at_rate: 0,
    };
    let mut serial_buf = [0u16; 128];
    let mut serial_num = String::new();
    if unsafe {
        DeviceIoControl(
            h_file,
            IOCTL_BATTERY_QUERY_INFORMATION,
            &query_serial as *const _ as *const _,
            std::mem::size_of::<BatteryQueryInformation>() as u32,
            serial_buf.as_mut_ptr() as *mut _,
            (serial_buf.len() * std::mem::size_of::<u16>()) as u32,
            &mut bytes_ret,
            std::ptr::null_mut(),
        )
    } != 0
    {
        serial_num = parse_battery_name_utf16(&serial_buf, bytes_ret);
    }

    let (unit, ac) = parse_battery_unit_data(&status, &info, dev_name, mfg_name, serial_num);
    Ok((unit, ac))
}

pub fn parse_battery_unit_data(
    status: &BatteryStatus,
    info: &BatteryInformation,
    dev_name: String,
    mfg_name: String,
    serial_num: String,
) -> (BatteryUnit, bool) {
    let ac = (status.power_state & 0x01) != 0;
    let full_mwh = if info.full_charged_capacity == BATTERY_UNKNOWN_CAPACITY {
        0
    } else {
        info.full_charged_capacity as i64
    };
    let remaining_mwh = if status.capacity == BATTERY_UNKNOWN_CAPACITY {
        0
    } else {
        status.capacity as i64
    };
    let is_relative = (info.capabilities & BATTERY_CAPACITY_RELATIVE) != 0;
    let percent = if full_mwh > 0 {
        ((100.0 * remaining_mwh as f64 / full_mwh as f64).round() as u32).min(100)
    } else if is_relative {
        // Full capacity unknown: relative values are conventionally on a 0-100 scale.
        (remaining_mwh as u32).min(100)
    } else {
        0
    };
    let design_mwh = if is_relative || info.designed_capacity == BATTERY_UNKNOWN_CAPACITY {
        0
    } else {
        info.designed_capacity as i64
    };
    let full_mwh = if is_relative { 0 } else { full_mwh };
    let remaining_mwh = if is_relative { 0 } else { remaining_mwh };
    let rate_mw = if is_relative || status.rate == BATTERY_UNKNOWN_RATE || status.rate == i32::MIN {
        0
    } else {
        status.rate as i64
    };

    let chem_bytes = match info.chemistry.iter().position(|&b| b == 0) {
        Some(pos) => &info.chemistry[..pos],
        None => &info.chemistry[..],
    };
    let chemistry = std::str::from_utf8(chem_bytes)
        .unwrap_or("????")
        .trim()
        .to_string();
    let chemistry = if chemistry.is_empty() {
        "????".to_string()
    } else {
        chemistry
    };

    let low_alert_mwh = if info.default_alert1 == BATTERY_UNKNOWN_CAPACITY {
        0
    } else {
        info.default_alert1
    };
    let crit_alert_mwh = if info.default_alert2 == BATTERY_UNKNOWN_CAPACITY {
        0
    } else {
        info.default_alert2
    };

    let cycle_count = if info.cycle_count == u32::MAX {
        None
    } else {
        Some(info.cycle_count)
    };
    let voltage_mv = if status.voltage == BATTERY_UNKNOWN_CAPACITY {
        0
    } else {
        status.voltage
    };

    let dev_name = if dev_name.is_empty() {
        "Unknown".to_string()
    } else {
        dev_name
    };
    let mfg_name = if mfg_name.is_empty() {
        "Unknown".to_string()
    } else {
        mfg_name
    };

    (
        BatteryUnit {
            device: dev_name,
            manufacturer: mfg_name,
            chemistry,
            serial_number: serial_num,
            percent,
            voltage_mv,
            remaining_mwh,
            full_mwh,
            design_mwh,
            rate_mw,
            low_alert_mwh,
            crit_alert_mwh,
            cycle_count,
        },
        ac,
    )
}

pub fn aggregate_alerts(batteries: &[BatteryUnit]) -> (u32, u32) {
    let low = batteries.iter().map(|b| b.low_alert_mwh).sum();
    let crit = batteries.iter().map(|b| b.crit_alert_mwh).sum();
    (low, crit)
}

pub fn build_sample(batteries: Vec<BatteryUnit>, any_ac: bool) -> Sample {
    let mut total_remaining = 0i64;
    let mut total_full = 0i64;
    let mut total_design = 0i64;
    let mut total_rate = 0i64;
    let mut gross_dis_w = 0.0f64;
    let mut gross_chg_w = 0.0f64;

    for b in &batteries {
        total_remaining += b.remaining_mwh;
        total_full += b.full_mwh;
        total_design += b.design_mwh;
        total_rate += b.rate_mw;
        if b.rate_mw < 0 {
            gross_dis_w += -b.rate_mw as f64 / 1000.0;
        } else if b.rate_mw > 0 {
            gross_chg_w += b.rate_mw as f64 / 1000.0;
        }
    }

    // A module without full-charge capacity makes mWh totals partial: report them as unknown.
    let complete = batteries.iter().all(|b| b.full_mwh > 0);
    if !complete {
        total_remaining = 0;
        total_full = 0;
    }
    let percent = if total_full > 0 {
        ((100.0 * total_remaining as f64 / total_full as f64).round() as u32).min(100)
    } else if batteries.len() == 1 {
        batteries[0].percent
    } else {
        0
    };
    let dis_w = if total_rate < 0 {
        -total_rate as f64 / 1000.0
    } else {
        0.0
    };

    let (low_alert_mwh, crit_alert_mwh) = aggregate_alerts(&batteries);

    Sample {
        batteries,
        percent,
        ac: any_ac,
        remaining_mwh: total_remaining,
        full_mwh: total_full,
        design_mwh: total_design,
        rate_mw: total_rate,
        dis_w,
        gross_dis_w,
        gross_chg_w,
        low_alert_mwh,
        crit_alert_mwh,
    }
}

pub fn sample_with<FEnum, FQuery>(enum_fn: FEnum, query_fn: FQuery) -> Result<Sample, BatlensError>
where
    FEnum: FnOnce() -> Result<EnumeratedDevices, BatlensError>,
    FQuery: Fn(&[u16]) -> Result<(BatteryUnit, bool), BatlensError>,
{
    let enum_res = enum_fn()?;
    let mut errors = enum_res.errors;
    let mut batteries = Vec::new();
    let mut any_ac = false;

    for (idx, path) in enum_res.paths.iter().enumerate() {
        match query_fn(path) {
            Ok((unit, ac)) => {
                if ac {
                    any_ac = true;
                }
                batteries.push(unit);
            }
            Err(e) => {
                errors.push(format!("Device #{idx}: {e}"));
            }
        }
    }

    if batteries.is_empty() {
        return Err(BatlensError::NoActiveBattery {
            diagnostics: errors,
        });
    }

    Ok(build_sample(batteries, any_ac))
}

pub fn sample() -> Result<Sample, BatlensError> {
    let mut s = sample_with(enumerate_battery_device_paths, query_battery_unit)?;
    s.apply_design_capacity_cache(&crate::xml::read_design_capacity_cache());
    Ok(s)
}

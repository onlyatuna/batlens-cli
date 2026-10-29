use chrono::{DateTime, Utc};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::types::*;

pub struct TmpCleaner {
    pub path: PathBuf,
    pub armed: bool,
}
impl TmpCleaner {
    pub fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for TmpCleaner {
    fn drop(&mut self) {
        if self.armed && self.path.exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn get_powercfg_path() -> PathBuf {
    let sys_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .unwrap_or_else(|| r"C:\Windows".into());
    let sys_dir = PathBuf::from(&sys_root);
    let p_sysnative = sys_dir.join("Sysnative").join("powercfg.exe");
    if p_sysnative.exists() {
        return p_sysnative;
    }
    let p_system32 = sys_dir.join("System32").join("powercfg.exe");
    if p_system32.exists() {
        return p_system32;
    }
    PathBuf::from("powercfg.exe")
}

pub fn execute_powercfg(out: &Path) -> Result<Option<String>, BatlensError> {
    let powercfg_exe = get_powercfg_path();
    let tmp_out = out.with_extension(format!("tmp.{}.xml", std::process::id()));
    let mut cleaner = TmpCleaner::new(tmp_out.clone());

    let mut child = Command::new(&powercfg_exe)
        .arg("/batteryreport")
        .arg("/xml")
        .arg("/output")
        .arg(&tmp_out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| BatlensError::PowercfgNotFound)?;

    // Drain stderr on a thread so powercfg does not block on a full 4 KB pipe. The channel
    // lets the timeout path give up without joining a hung thread.
    let (tx, rx) = std::sync::mpsc::channel();
    let stderr_pipe = child.stderr.take();
    let spawned = std::thread::Builder::new().spawn(move || {
        let mut buf = String::new();
        if let Some(mut pipe) = stderr_pipe {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });
    if let Err(e) = spawned {
        let _ = child.kill();
        let _ = child.wait();
        return Err(BatlensError::Io(e));
    }

    let start = std::time::Instant::now();
    // Measured 5-7 s on a healthy laptop; leave headroom for AV scans and slow disks.
    let timeout = std::time::Duration::from_secs(30);
    let mut status = None;

    while start.elapsed() < timeout {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = rx.recv_timeout(std::time::Duration::from_millis(100));
                return Err(BatlensError::Io(e));
            }
        }
    }

    let status = match status {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = rx.recv_timeout(std::time::Duration::from_millis(150));
            return Err(BatlensError::PowercfgFailed {
                status: None,
                stderr: format!(
                    "powercfg /batteryreport timed out after {} seconds",
                    timeout.as_secs()
                ),
            });
        }
    };

    let stderr = rx
        .recv_timeout(std::time::Duration::from_millis(500))
        .unwrap_or_default();
    if !status.success() {
        return Err(BatlensError::PowercfgFailed {
            status: status.code(),
            stderr,
        });
    }

    for attempt in 0..6 {
        if fs::rename(&tmp_out, out).is_ok() {
            cleaner.disarm();
            return Ok(None);
        }
        if attempt < 5 {
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }

    let direct_xml = read_xml_string(&tmp_out)?;
    // `cleaner` deletes tmp_out when dropped on return.
    Ok(Some(direct_xml))
}

pub struct TagFrame {
    pub name: Vec<u8>,
    pub text: String,
}

pub fn cleanup_stale_cache_files(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && (name.contains(".tmp.") || name.starts_with("tmp."))
                && let Ok(meta) = entry.metadata()
                && let Ok(mtime) = meta.modified()
                && let Ok(age) = now.duration_since(mtime)
                && age.as_secs() > 1800
            {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

pub fn get_cache_dir() -> PathBuf {
    let dir = if let Some(appdata) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(appdata).join("batlens")
    } else {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
        let safe_user: String = user
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        let safe_user = if safe_user.is_empty() {
            "default"
        } else {
            &safe_user
        };
        std::env::temp_dir().join(format!("batlens_{safe_user}"))
    };
    let _ = fs::create_dir_all(&dir);
    cleanup_stale_cache_files(&dir);
    dir
}

pub fn get_report_cache_path() -> PathBuf {
    get_cache_dir().join("battery_report.xml")
}

pub fn design_capacity_file() -> PathBuf {
    get_cache_dir().join(".design_capacity")
}

pub fn parse_design_capacity_cache(content: &str) -> DesignCapacityCache {
    let mut cache = DesignCapacityCache::default();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim().to_lowercase();
            if let Ok(val) = v.trim().parse::<i64>() {
                if key == "__total__" || key == "total" {
                    cache.total = val;
                } else if key == "__modules__" || key == "modules" {
                    cache.module_count = val as usize;
                } else {
                    cache.modules.insert(key, val);
                }
            }
        } else if let Ok(val) = trimmed.parse::<i64>()
            && cache.total == 0
        {
            cache.total = val;
        }
    }
    cache
}

pub fn format_design_capacity_cache(cache: &DesignCapacityCache) -> String {
    let mut out = String::from("# batlens design capacity cache (per-module & total)\n");
    if cache.total > 0 {
        out.push_str(&format!("__total__={}\n", cache.total));
    }
    if cache.module_count > 0 {
        out.push_str(&format!("__modules__={}\n", cache.module_count));
    }
    for (k, v) in &cache.modules {
        out.push_str(&format!("{k}={v}\n"));
    }
    out
}

pub fn read_design_capacity_cache() -> DesignCapacityCache {
    let target = design_capacity_file();
    if let Ok(txt) = fs::read_to_string(&target) {
        return parse_design_capacity_cache(&txt);
    }
    let old_base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let old_file = old_base.join(".design_capacity");
    if old_file.exists()
        && let Ok(txt) = fs::read_to_string(&old_file)
    {
        let cache = parse_design_capacity_cache(&txt);
        let _ = fs::copy(&old_file, &target);
        return cache;
    }
    DesignCapacityCache::default()
}

pub fn update_design_capacity_cache_from_report(r: &Report) {
    let mut cache = read_design_capacity_cache();
    let mut total_from_modules = 0i64;
    cache.module_count = r.batteries.len();
    for (idx, b) in r.batteries.iter().enumerate() {
        if let Some(dc) = b.design_capacity
            && dc > 0
        {
            total_from_modules += dc;
            let id_str = if b.id.is_empty() {
                format!("bat{idx}")
            } else {
                b.id.to_lowercase()
            };
            cache.modules.insert(id_str, dc);
            if let Some(sn) = b.serial_number.as_deref().filter(|s| is_valid_serial(s)) {
                cache.modules.insert(sn.to_lowercase(), dc);
            }
        }
    }

    if let Some(tot) = r.design_capacity
        && tot > 0
    {
        cache.total = tot;
    } else if total_from_modules > 0 {
        cache.total = total_from_modules;
    }

    if cache.total > 0 || !cache.modules.is_empty() {
        let content = format_design_capacity_cache(&cache);
        let target = design_capacity_file();
        let _ = atomic_write_cache_file(&target, &content);
    }
}

pub fn atomic_write_cache_file(target: &Path, content: &str) -> std::io::Result<()> {
    let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, content)?;
    let mut cleaner = TmpCleaner::new(tmp.clone());
    let mut last_err = None;
    for attempt in 0..6 {
        match fs::rename(&tmp, target) {
            Ok(()) => {
                cleaner.disarm();
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < 5 {
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("Failed to atomically rename cache file")))
}

pub fn lookup_module_design_capacity(
    cache: &DesignCapacityCache,
    unit: &BatteryUnit,
) -> Option<i64> {
    cache.lookup(unit)
}

pub fn read_cached_report() -> Option<Report> {
    let path = get_report_cache_path();
    if !path.exists() {
        return None;
    }
    let xml = read_xml_string(&path).ok()?;
    parse_report(&xml).ok()
}

pub fn read_xml_string(path: &Path) -> Result<String, BatlensError> {
    let mut last_err = None;
    for attempt in 0..5 {
        match fs::read(path) {
            Ok(bytes) => {
                if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
                    let u16_vec: Vec<u16> = bytes[2..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&c| u16::from_le_bytes(c))
                        .collect();
                    return Ok(String::from_utf16_lossy(&u16_vec));
                }
                if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
                    let u16_vec: Vec<u16> = bytes[2..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&c| u16::from_be_bytes(c))
                        .collect();
                    return Ok(String::from_utf16_lossy(&u16_vec));
                }
                let slice = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                    &bytes[3..]
                } else {
                    &bytes[..]
                };
                return match std::str::from_utf8(slice) {
                    Ok(s) => Ok(s.to_string()),
                    Err(_) => {
                        if slice.len() >= 4 && slice[1] == 0 && slice[3] == 0 {
                            let u16_vec: Vec<u16> = slice
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|&c| u16::from_le_bytes(c))
                                .collect();
                            Ok(String::from_utf16_lossy(&u16_vec))
                        } else {
                            Ok(String::from_utf8_lossy(slice).into_owned())
                        }
                    }
                };
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
            }
        }
    }
    Err(last_err.map(BatlensError::Io).unwrap_or_else(|| {
        BatlensError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "failed to read XML report file",
        ))
    }))
}

pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let ndt = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ").ok()?;
    Some(ndt.and_utc())
}

pub fn iso_hours(s: &str) -> Option<f64> {
    let b = s.strip_prefix('P')?;
    let (dur, time) = b.split_once('T').unwrap_or((b, ""));
    fn num(part: &str) -> Option<f64> {
        part.parse().ok()
    }
    let mut total = 0.0f64;
    let mut n = String::new();
    for c in dur.chars() {
        match c {
            'Y' => {
                total += num(&n)? * 8760.0;
                n.clear();
            }
            'M' => {
                total += num(&n)? * 730.5;
                n.clear();
            }
            'W' => {
                total += num(&n)? * 168.0;
                n.clear();
            }
            'D' => {
                total += num(&n)? * 24.0;
                n.clear();
            }
            c if c.is_ascii_digit() || c == '.' => n.push(c),
            _ => return None,
        }
    }
    if !n.is_empty() {
        return None;
    }
    for c in time.chars() {
        match c {
            'H' => {
                total += num(&n)?;
                n.clear();
            }
            'M' => {
                total += num(&n)? / 60.0;
                n.clear();
            }
            'S' => {
                total += num(&n)? / 3600.0;
                n.clear();
            }
            c if c.is_ascii_digit() || c == '.' => n.push(c),
            _ => return None,
        }
    }
    if !n.is_empty() {
        return None;
    }
    Some(total)
}

fn parse_entry(e: &quick_xml::events::BytesStart, tag_stack: &[TagFrame], r: &mut Report) {
    let in_section = |sec: &[u8]| tag_stack.iter().any(|t| t.name == *sec);
    match e.local_name().as_ref() {
        b"UsageEntry" if in_section(b"RecentUsage") => {
            let mut t = None;
            let mut dur_ticks = 0u64;
            let mut ac = false;
            let mut entry_type = String::new();
            let mut discharge_mwh = 0u64;
            for a in e.attributes().flatten() {
                if let Ok(v) = a.unescape_value() {
                    match a.key.local_name().as_ref() {
                        b"Timestamp" => t = parse_ts(&v),
                        b"Duration" => dur_ticks = v.parse().unwrap_or(0),
                        b"Ac" => ac = v.as_ref() == "1",
                        b"EntryType" => entry_type = v.into_owned(),
                        b"Discharge" => discharge_mwh = v.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            r.usage.push(Usage {
                t,
                dur_ticks,
                ac,
                entry_type,
                discharge_mwh,
            });
        }
        b"HistoryEntry" if in_section(b"History") => {
            let mut date = None;
            let mut full = 0u64;
            let mut design = 0u64;
            let mut cyc = None;
            for a in e.attributes().flatten() {
                if let Ok(v) = a.unescape_value() {
                    match a.key.local_name().as_ref() {
                        b"StartDate" => date = parse_ts(&v),
                        b"FullChargeCapacity" => full = v.parse().unwrap_or(0),
                        b"DesignCapacity" => design = v.parse().unwrap_or(0),
                        b"CycleCount" => {
                            cyc = v.parse::<u32>().ok().filter(|&c| c > 0 && c != u32::MAX);
                        }
                        _ => {}
                    }
                }
            }
            if r.cycle_count.is_none() && cyc.is_some() {
                r.cycle_count = cyc;
            }
            r.history.push(HistPoint {
                date,
                full,
                design,
                cycles: cyc,
            });
        }
        b"Drain" if in_section(b"EnergyDrains") => {
            let mut t0 = None;
            let mut t1 = None;
            let mut start_mwh = 0i64;
            let mut end_mwh = 0i64;
            for a in e.attributes().flatten() {
                if let Ok(v) = a.unescape_value() {
                    match a.key.local_name().as_ref() {
                        b"StartTimestamp" => t0 = parse_ts(&v),
                        b"EndTimestamp" => t1 = parse_ts(&v),
                        b"StartChargeCapacity" => start_mwh = v.parse().unwrap_or(0),
                        b"EndChargeCapacity" => end_mwh = v.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            r.drains.push(Seg {
                t0,
                t1,
                start_mwh,
                end_mwh,
            });
        }
        _ => {}
    }
}

fn handle_xml_text(
    parent: Option<&[u8]>,
    in_estimates: bool,
    tag: &[u8],
    val: &str,
    r: &mut Report,
) {
    if parent == Some(b"SystemInformation") {
        match tag {
            b"SystemProductName" => r.system_product = Some(val.to_string()),
            b"SystemManufacturer" => r.system_mfg = Some(val.to_string()),
            b"BIOSVersion" => r.bios_version = Some(val.to_string()),
            b"BIOSDate" => r.bios_date = Some(val.to_string()),
            b"ConnectedStandby" => r.connected_standby = val == "1",
            _ => {}
        }
    } else if parent == Some(b"Battery") {
        match tag {
            b"Id" => {
                if let Some(b) = r.batteries.last_mut() {
                    b.id = val.to_string();
                }
            }
            b"Manufacturer" => {
                if let Some(b) = r.batteries.last_mut() {
                    b.manufacturer = Some(val.to_string());
                }
            }
            b"SerialNumber" => {
                if let Some(b) = r.batteries.last_mut() {
                    b.serial_number = Some(val.to_string());
                }
            }
            b"Chemistry" => {
                if let Some(b) = r.batteries.last_mut() {
                    b.chemistry = Some(val.to_string());
                }
            }
            b"CycleCount" => {
                let cyc = val.parse::<u32>().ok().filter(|&c| c > 0 && c != u32::MAX);
                if let Some(b) = r.batteries.last_mut() {
                    b.cycle_count = cyc;
                }
            }
            b"DesignCapacity" => {
                if let Some(b) = r.batteries.last_mut() {
                    b.design_capacity = val.parse().ok();
                }
            }
            b"FullChargeCapacity" => {
                if let Some(b) = r.batteries.last_mut() {
                    b.full_charge = val.parse().ok();
                }
            }
            _ => {}
        }
    } else if in_estimates {
        match (parent, tag) {
            (Some(b"DesignCapacity"), b"Capacity") => {
                r.design_capacity = val.parse().ok();
            }
            (Some(b"FullChargeCapacity"), b"Capacity") => {
                r.full_charge = val.parse().ok();
            }
            (Some(b"FullChargeCapacity"), b"ActiveRuntime") => {
                r.est_active_runtime = Some(val.to_string());
            }
            _ => {}
        }
    }
}

pub fn parse_report(xml: &str) -> Result<Report, BatlensError> {
    let mut r = Report::default();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut tag_stack: Vec<TagFrame> = Vec::with_capacity(16);

    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) => {
                parse_entry(&e, &tag_stack, &mut r);
                let name = e.local_name().as_ref().to_vec();
                if name == b"Battery" {
                    r.batteries.push(ReportBattery::default());
                }
                tag_stack.push(TagFrame {
                    name,
                    text: String::new(),
                });
            }
            Event::Empty(e) => {
                parse_entry(&e, &tag_stack, &mut r);
                if e.local_name().as_ref() == b"Battery" {
                    r.batteries.push(ReportBattery::default());
                }
            }
            Event::Text(t) => {
                if let Ok(v) = t.unescape()
                    && let Some(top) = tag_stack.last_mut()
                {
                    top.text.push_str(&v);
                }
            }
            Event::CData(c) => {
                if let Ok(v) = std::str::from_utf8(c.as_ref())
                    && let Some(top) = tag_stack.last_mut()
                {
                    top.text.push_str(v);
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                if let Some(pos) = tag_stack.iter().rposition(|t| t.name == name.as_ref()) {
                    tag_stack.truncate(pos + 1);
                    let frame = tag_stack.pop().unwrap();
                    let val = frame.text.trim();
                    if !val.is_empty() {
                        let parent = tag_stack.last().map(|f| f.name.as_slice());
                        let in_estimates = tag_stack.iter().any(|t| t.name == b"RuntimeEstimates");
                        handle_xml_text(parent, in_estimates, frame.name.as_slice(), val, &mut r);
                    }
                }
            }
            _ => {}
        }
    }

    if !tag_stack.is_empty() {
        return Err(BatlensError::InvalidReport(format!(
            "truncated XML: unclosed tags remain ({})",
            tag_stack
                .iter()
                .filter_map(|t| std::str::from_utf8(&t.name).ok())
                .collect::<Vec<_>>()
                .join("/")
        )));
    }

    // Sum of the <Batteries> capacities takes precedence over <RuntimeEstimates>.
    let module_dc_sum: i64 = r.batteries.iter().filter_map(|b| b.design_capacity).sum();
    let module_fc_sum: i64 = r.batteries.iter().filter_map(|b| b.full_charge).sum();
    if module_dc_sum > 0 {
        r.design_capacity = Some(module_dc_sum);
    }
    if module_fc_sum > 0 {
        r.full_charge = Some(module_fc_sum);
    }
    if r.cycle_count.is_none() && r.batteries.len() == 1 {
        r.cycle_count = r.batteries[0].cycle_count;
    } else if r.batteries.len() > 1 {
        r.cycle_count = None;
    }
    if r.batteries.len() == 1 && r.batteries[0].cycle_count.is_none() {
        r.batteries[0].cycle_count = r.cycle_count;
    }
    if let Some(total_dc) = r.design_capacity
        && r.batteries.len() == 1
        && r.batteries[0].design_capacity.is_none()
    {
        r.batteries[0].design_capacity = Some(total_dc);
    }

    if r.design_capacity.is_none() && r.history.is_empty() && r.usage.is_empty() {
        return Err(BatlensError::InvalidReport(
            "missing design capacity, history and usage".to_string(),
        ));
    }
    Ok(r)
}

pub fn load_or_generate_xml(out: &Path) -> Result<String, BatlensError> {
    let fresh = fs::metadata(out)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().is_ok_and(|d| d.as_secs() < 600))
        .unwrap_or(false);

    if fresh && let Ok(xml) = read_xml_string(out) {
        return Ok(xml);
    }

    match execute_powercfg(out) {
        Ok(Some(direct_xml)) => Ok(direct_xml),
        Ok(None) => read_xml_string(out),
        Err(e) => {
            if out.exists()
                && let Ok(fallback_xml) = read_xml_string(out)
            {
                return Ok(fallback_xml);
            }
            Err(e)
        }
    }
}

pub fn load_report_with<FLoad, FExec, FRead>(
    load_fn: FLoad,
    exec_fn: FExec,
    read_fn: FRead,
    out: &Path,
) -> Result<Report, BatlensError>
where
    FLoad: FnOnce(&Path) -> Result<String, BatlensError>,
    FExec: FnOnce(&Path) -> Result<Option<String>, BatlensError>,
    FRead: FnOnce(&Path) -> Result<String, BatlensError>,
{
    let xml = load_fn(out)?;
    match parse_report(&xml) {
        Ok(report) => Ok(report),
        Err(initial_err) => {
            let _ = fs::remove_file(out);
            let fresh_xml = match exec_fn(out) {
                Ok(Some(direct)) => direct,
                Ok(None) => read_fn(out)?,
                Err(_) => return Err(initial_err),
            };
            parse_report(&fresh_xml)
        }
    }
}

pub fn run_report_xml() -> Result<Report, BatlensError> {
    let out = get_report_cache_path();
    load_report_with(
        load_or_generate_xml,
        execute_powercfg,
        read_xml_string,
        &out,
    )
}

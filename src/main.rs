// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Evan Chen <evanchen940628@gmail.com>. All rights reserved.

#[cfg(not(windows))]
compile_error!("batlens is a Windows-only tool (requires Windows ACPI/IOCTL and powercfg).");

#[macro_use]
mod out;
mod console;
pub mod render;

use chrono::{Local, Utc};
use clap::{Parser, Subcommand};
use colored::Colorize;

use batlens_core::stats::{clean_history, compute_wear, find_battery_replacement};
use batlens_core::sys::sample;
use batlens_core::types::*;
use batlens_core::xml::{
    find_matching_report_battery, read_cached_report, run_report_xml,
    update_design_capacity_cache_from_report,
};

use crate::console::ConsoleGuard;
use crate::render::{charge_paint, health_paint, lab, pct_bar, title, visual_width};

#[derive(Parser)]
#[command(
    name = "batlens",
    version = concat!(
        env!("CARGO_PKG_VERSION"),
        " (c) 2026 Evan Chen <evanchen940628@gmail.com>\nLicense: MIT"
    ),
    about = "Lightweight laptop battery monitor for Windows.",
    arg_required_else_help = true,
    after_help = "Examples:\n  batlens status          live charge, AC state, capacity, health\n  batlens report          full history: per-segment drain, runtime, wear trend\n  batlens chart           health trend across the battery lifetime (braille)\n  batlens chart --drain   daily average drain power instead\n  batlens chart --ascii   block-char chart for fonts without braille glyphs\n\nHistory data comes from powercfg /batteryreport, cached for 10 minutes.\nColour is off when piped or when NO_COLOR=1 is set."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Live battery readings (charge, capacity, health, power).
    Status,
    /// Detailed diagnostic report from Windows battery history.
    Report {
        /// Number of days of recent usage to inspect (default: 14)
        #[arg(long, default_value = "14")]
        days: u32,
        /// Health % threshold for time-to-wear estimate (default: 80)
        #[arg(long, default_value = "80")]
        target: f64,
        /// Include full pre-replacement history in wear rate calculation
        #[arg(long)]
        all: bool,
    },
    /// Detailed hardware, manufacturer, and EC firmware info.
    Info,
    /// Terminal charts of battery metrics over time.
    Chart {
        /// Show daily average drain power chart instead of health
        #[arg(long)]
        drain: bool,
        /// Fall back to standard block characters instead of Unicode braille
        #[arg(long)]
        ascii: bool,
        /// Explicitly force Unicode braille dot-matrix characters (default)
        #[arg(long)]
        braille: bool,
        /// Show health only for the current (newest) battery life-cycle
        #[arg(long)]
        current: bool,
        /// Explicit chart width in terminal columns (default: fit to window)
        #[arg(long)]
        width: Option<usize>,
    },
}

/// Estimates over 36 h are shown as a bound.
fn eta_text(hours: f64) -> String {
    if hours <= 36.0 {
        Hours(hours).to_string()
    } else {
        "> 36 h".to_string()
    }
}

/// Percent text, `N/A` when unknown.
fn pct_text(p: Option<u32>) -> String {
    p.map_or("N/A".to_string(), |p| format!("{p}%"))
}

/// Capacity text; 0 means unknown (relative units, missing full-charge value).
fn mwh_or_na(v: i64) -> String {
    if v > 0 {
        Mwh(v).to_string()
    } else {
        "N/A".to_string()
    }
}

/// Time-left line for non-mixed packs; `dis_w` is the discharge power.
fn time_left_line(s: &Sample, dis_w: f64) -> Option<String> {
    if dis_w >= 0.1 {
        // Remaining capacity is unreliable when percent is unknown.
        s.percent?;
        let hours = s.remaining_mwh as f64 / 1000.0 / dis_w;
        let tail = if s.ac {
            " (discharging while on AC power)"
        } else {
            ""
        };
        return Some(format!(
            "Time left: {} at current draw{tail}",
            eta_text(hours)
        ));
    }
    if s.rate_mw < 0 {
        return Some("Time left: discharging, draw too low to estimate".into());
    }
    let pct = s.percent.unwrap_or(0);
    if s.ac && (s.rate_mw > 100 || pct >= 99) {
        // `rate_mw <= 100` only applies at `percent >= 99`.
        // Unknown percent never counts as full.
        let full = s.percent.is_some()
            && (s.remaining_mwh >= s.full_mwh || pct >= 100 || s.rate_mw <= 100);
        return Some(if full {
            "Time left: fully charged (100%)".into()
        } else {
            "Time left: charging".into()
        });
    }
    if !s.ac && s.rate_mw == 0 {
        return Some("Time left: idle, draw too low to estimate".into());
    }
    None
}

/// Prints a `Time left: ...` line with a dim label.
fn print_time_left(line: &str) {
    match line.strip_prefix("Time left: ") {
        Some(rest) => println!("{} {rest}", lab("Time left:")),
        None => println!("{line}"),
    }
}

fn print_status(s: &Sample, rep: Option<&Report>) {
    if s.batteries.len() == 1 {
        let b = &s.batteries[0];
        let dev_label = if let Some(prod) = rep.and_then(|r| r.system_product.as_deref()) {
            format!("{} ({prod}, {})", b.device, b.chemistry)
        } else {
            format!("{} ({}, {})", b.device, b.manufacturer, b.chemistry)
        };
        println!("{} {dev_label}", lab("Device   :"));
        println!(
            "{} {}  {}  ({})",
            lab("Charge   :"),
            pct_text(s.percent),
            s.percent.map_or(String::new(), |p| {
                charge_paint(&pct_bar(p as f64, 20), p as f64).to_string()
            }),
            if s.ac { "AC" } else { "battery" }
        );
        let pw = Mw(s.rate_mw).signed();
        let pw = if s.rate_mw > 0 {
            pw.green().to_string()
        } else if s.rate_mw < 0 {
            pw.yellow().to_string()
        } else {
            pw
        };
        println!("{} {pw}  (negative = discharging)", lab("Power    :"));
        if b.voltage_mv > 0 {
            println!("{} {}", lab("Voltage  :"), Mv(b.voltage_mv));
        }
        if s.design_mwh > 0 {
            println!(
                "{} {} / {} design",
                lab("Capacity :"),
                Mwh(s.full_mwh),
                Mwh(s.design_mwh)
            );
            let hv = 100.0 * s.full_mwh as f64 / s.design_mwh as f64;
            let hs = format!("{:.1}%", hv);
            println!(
                "{} {}  {}",
                lab("Health   :"),
                health_paint(&hs, hv),
                health_paint(&pct_bar(hv, 20), hv)
            );
        } else {
            println!("{} {}", lab("Capacity :"), mwh_or_na(s.full_mwh));
            println!(
                "{} N/A (run 'batlens report' to fetch design capacity)",
                lab("Health   :")
            );
        }
        if let Some(line) = time_left_line(s, s.dis_w) {
            print_time_left(&line);
        }
    } else {
        println!(
            "{}",
            title(&format!(
                "Multiple Batteries Detected ({} modules):",
                s.batteries.len()
            ))
        );
        for (i, b) in s.batteries.iter().enumerate() {
            let h = if b.design_mwh > 0 {
                format!("{:.1}%", 100.0 * b.full_mwh as f64 / b.design_mwh as f64)
            } else {
                "N/A".to_string()
            };
            let matched_rb = rep.and_then(|r| find_matching_report_battery(b, r, i));
            let cyc = match b
                .cycle_count
                .or_else(|| matched_rb.and_then(|rb| rb.cycle_count))
            {
                Some(c) => c.to_string(),
                None => "N/A".to_string(),
            };
            let state_str = if b.rate_mw > 50 {
                "charging"
            } else if b.rate_mw < -50 {
                "discharging"
            } else {
                "idle"
            };
            let pw = Mw(b.rate_mw).signed();
            println!(
                "  [{}] {} ({}, {}) - {}, {} / {} design, {}, {} (Health: {}, Cycles: {})",
                i + 1,
                b.device,
                b.manufacturer,
                b.chemistry,
                pct_text(b.percent),
                mwh_or_na(b.full_mwh),
                if b.design_mwh > 0 {
                    format!("{}", Mwh(b.design_mwh))
                } else {
                    "N/A".to_string()
                },
                state_str,
                pw,
                health_paint(
                    &h,
                    if b.design_mwh > 0 {
                        100.0 * b.full_mwh as f64 / b.design_mwh as f64
                    } else {
                        0.0
                    }
                ),
                cyc
            );
        }
        let is_mixed = s.gross_dis_w > 0.05 && s.gross_chg_w > 0.05;
        let mode_str = if is_mixed {
            "transitional / cross-charging"
        } else if s.ac {
            "AC power"
        } else {
            "battery"
        };
        let pw_str = if is_mixed {
            format!(
                "net {}, drawing {}, charging {}",
                Mw(s.rate_mw).signed(),
                Watts(s.gross_dis_w),
                Watts(s.gross_chg_w)
            )
        } else {
            Mw(s.rate_mw).signed()
        };
        println!(
            "  Total : {} | Mode: {} | Power: {} | Capacity: {} / {} design",
            pct_text(s.percent),
            mode_str,
            pw_str,
            mwh_or_na(s.full_mwh),
            mwh_or_na(s.design_mwh)
        );
        if is_mixed {
            let dis_remaining_mwh: i64 = s
                .batteries
                .iter()
                .filter(|b| b.rate_mw < 0)
                .map(|b| b.remaining_mwh)
                .sum();
            if s.gross_dis_w >= 0.1 && dis_remaining_mwh > 0 {
                let hours = dis_remaining_mwh as f64 / 1000.0 / s.gross_dis_w;
                let note = if s.ac { " (on AC power)" } else { "" };
                println!(
                    "{} {} until active pack depletes (drawing {}; net draw {}){}",
                    lab("Time left:"),
                    eta_text(hours),
                    Watts(s.gross_dis_w),
                    Mw(s.rate_mw).signed(),
                    note
                );
            }
        } else if let Some(line) = time_left_line(s, s.gross_dis_w) {
            // `rate_mw` (net) picks the state; `gross_dis_w` is the runtime basis.
            print_time_left(&line);
        }
    }
}

fn cmd_info() -> Result<(), BatlensError> {
    let s = sample()?;
    let rep = read_cached_report();

    println!("{}\n", title("System & Battery Info"));
    if let Some(r) = &rep {
        if let Some(prod) = &r.system_product {
            println!("  {} {prod}", lab("Product Model  :"));
        }
        if let Some(mfg) = &r.system_mfg {
            println!("  {} {mfg}", lab("Manufacturer   :"));
        }
        if let Some(bios) = &r.bios_version {
            let date = r.bios_date.as_deref().unwrap_or("");
            if !date.is_empty() {
                println!("  {} {bios} ({date})", lab("BIOS Version   :"));
            } else {
                println!("  {} {bios}", lab("BIOS Version   :"));
            }
        }
        if r.connected_standby {
            println!(
                "  {} Modern Standby (S0ix supported)",
                lab("Standby Mode   :")
            );
        }
    }

    for (i, b) in s.batteries.iter().enumerate() {
        let matched_rep = rep
            .as_ref()
            .and_then(|r| find_matching_report_battery(b, r, i));
        let prefix = if s.batteries.len() > 1 {
            format!("  {} ", lab(&format!("[Battery {}]", i + 1)))
        } else {
            format!("  {} ", lab("Battery Module :"))
        };
        println!("{prefix}{} ({}, {})", b.device, b.manufacturer, b.chemistry);
        let sn = if !b.serial_number.is_empty() {
            Some(b.serial_number.as_str())
        } else {
            matched_rep
                .and_then(|rb| rb.serial_number.as_deref())
                .filter(|s| !s.is_empty())
        };
        if let Some(s_num) = sn {
            println!("  - {} {s_num}", lab("Serial Number:"));
        }
        if b.voltage_mv > 0 {
            println!(
                "  - {} {} ({})",
                lab("Live Voltage :"),
                Mv(b.voltage_mv),
                if s.ac { "on AC power" } else { "on battery" }
            );
        }
        if b.rate_mw != 0 {
            println!(
                "  - {} {} ({})",
                lab("Live Power   :"),
                Mw(b.rate_mw).signed(),
                if b.rate_mw > 0 {
                    "charging"
                } else {
                    "discharging"
                }
            );
        }
        let detail = |v: i64| {
            if v > 0 {
                format!("{} ({v} mWh)", Mwh(v))
            } else {
                "N/A".to_string()
            }
        };
        println!("  - {} {}", lab("Design Cap   :"), detail(b.design_mwh));
        // Remaining is meaningless without a full-charge value (relative units).
        let remaining = if b.percent.is_some() {
            detail(b.remaining_mwh)
        } else {
            "N/A".to_string()
        };
        println!("  - {} {remaining}", lab("Remaining    :"));
        println!("  - {} {}", lab("Full Charge  :"), detail(b.full_mwh));
        if b.design_mwh > 0 {
            let hv = 100.0 * b.full_mwh as f64 / b.design_mwh as f64;
            let hs = format!("{:.1}%", hv);
            println!("  - {} {}", lab("Health       :"), health_paint(&hs, hv));
        } else {
            println!("  - {} N/A", lab("Health       :"));
        }
        let cycle = b.cycle_count.or_else(|| {
            matched_rep.and_then(|rb| rb.cycle_count).or_else(|| {
                if rep.as_ref().is_some_and(|r| r.batteries.len() <= 1) {
                    rep.as_ref().and_then(|r| r.cycle_count)
                } else {
                    None
                }
            })
        });
        let cycle_str = match cycle {
            Some(0) | Some(u32::MAX) => "0 (not reported by EC firmware)".to_string(),
            Some(c) => c.to_string(),
            None => "Unknown".to_string(),
        };
        println!("  - {} {cycle_str}", lab("Cycle Count  :"));
        if b.low_alert_mwh > 0 || b.crit_alert_mwh > 0 {
            println!(
                "  - {} Low {}, Critical {}",
                lab("Alert Levels :"),
                Mwh(b.low_alert_mwh as i64),
                Mwh(b.crit_alert_mwh as i64)
            );
        }
    }
    if s.low_alert_mwh > 0 || s.crit_alert_mwh > 0 {
        if s.batteries.len() > 1 {
            println!(
                "  Combined Alerts: Low at {}, Critical at {} (sum of {} modules)",
                Mwh(s.low_alert_mwh as i64),
                Mwh(s.crit_alert_mwh as i64),
                s.batteries.len()
            );
        } else {
            println!(
                "  Alert Threshold: Low at {}, Critical at {}",
                Mwh(s.low_alert_mwh as i64),
                Mwh(s.crit_alert_mwh as i64)
            );
        }
    }
    Ok(())
}

fn cmd_report(days: u32, target: f64, all: bool) -> Result<(), BatlensError> {
    let r = run_report_xml()?;
    update_design_capacity_cache_from_report(&r);
    let pts = clean_history(&r);
    let now_h = match (r.design_capacity, r.full_charge) {
        (Some(d), Some(f)) if d > 0 => Some(100.0 * f as f64 / d as f64),
        _ => None,
    };

    println!(
        "{}\n",
        title("Battery report  (Windows battery report, cached 10 min)")
    );

    let pad = |cell: &dyn std::fmt::Display, plain: &str, w: usize| {
        format!(
            "{cell}{}",
            " ".repeat(w.saturating_sub(visual_width(plain)))
        )
    };
    let row = |label: &str, cell: &dyn std::fmt::Display, plain: &str, tail: &str| {
        let l = pad(&lab(label), label, 16);
        let c = pad(cell, plain, 14);
        println!("  {l}{c}{tail}");
    };

    if pts.len() >= 2 {
        let (t0, t1) = (pts[0].0, pts[pts.len() - 1].0);
        row(
            "History",
            &format!("{} segments", pts.len()),
            &format!("{} segments", pts.len()),
            &format!(
                "{} -> {}",
                t0.with_timezone(&Local).format("%Y-%m-%d"),
                t1.with_timezone(&Local).format("%Y-%m-%d")
            ),
        );
    }
    if r.batteries.len() > 1 {
        let b_strs: Vec<String> = r
            .batteries
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let id = if b.id.is_empty() {
                    format!("BAT{i}")
                } else {
                    b.id.clone()
                };
                let cap = b
                    .design_capacity
                    .map(|c| format!("{}", Mwh(c)))
                    .unwrap_or_else(|| "N/A".to_string());
                let cyc = b
                    .cycle_count
                    .map(|c| format!("{c} cyc"))
                    .unwrap_or_else(|| "N/A".to_string());
                format!("{id} ({cap}, {cyc})")
            })
            .collect();
        let tail = b_strs.join(", ");
        row(
            "Modules",
            &format!("{} installed", r.batteries.len()),
            &format!("{} installed", r.batteries.len()),
            &tail,
        );
    }

    let repl = if all {
        None
    } else {
        find_battery_replacement(&pts)
    };
    let (t_base, h_base, replaced) = match repl {
        Some(idx) => (pts[idx].0, pts[idx].1, true),
        None if !pts.is_empty() => (pts[0].0, pts[0].1, false),
        _ => (Utc::now(), 100.0, false),
    };

    if let Some(now) = now_h {
        let v = format!("{now:.1}%");
        let tail = if replaced {
            format!(
                "now, initial {} on {} (new battery)",
                health_paint(&format!("{h_base:.1}%"), h_base),
                t_base.with_timezone(&Local).format("%Y-%m-%d")
            )
        } else {
            format!(
                "now, earliest {} on {}",
                health_paint(&format!("{h_base:.1}%"), h_base),
                t_base.with_timezone(&Local).format("%Y-%m-%d")
            )
        };
        row("Health", &health_paint(&v, now), &v, &tail);
        println!(
            "  {}{}",
            " ".repeat(16),
            health_paint(&pct_bar(now, 20), now)
        );

        let t_now = Utc::now();
        let days_span = (t_now - t_base).num_seconds() as f64 / 86400.0;
        let wear = compute_wear(now, h_base, days_span, target);
        let vc = if wear.rate_str == "N/A" {
            "N/A".dimmed()
        } else if wear.is_decay {
            let pm = (h_base - now) / days_span * 30.0;
            if pm > 1.0 {
                wear.rate_str.as_str().red()
            } else if pm > 0.5 {
                wear.rate_str.as_str().yellow()
            } else {
                wear.rate_str.as_str().green()
            }
        } else if wear.is_recalibration {
            wear.rate_str.as_str().cyan()
        } else {
            wear.rate_str.as_str().green()
        };
        row("Wear", &vc, &wear.rate_str, &wear.detail_str);
    }

    let segs: Vec<(f64, f64)> = r
        .drains
        .iter()
        .filter(|s| s.t0.is_some() && s.t1.is_some())
        .map(|s| {
            let h = (s.t1.unwrap() - s.t0.unwrap()).num_seconds() as f64 / 3600.0;
            (s.start_mwh as f64 - s.end_mwh as f64, h)
        })
        .filter(|(d, h)| *d > 0.0 && *h > 0.0)
        .collect();
    if !segs.is_empty() {
        let dm: f64 = segs.iter().map(|(s, _)| s).sum();
        let dh: f64 = segs.iter().map(|(_, h)| h).sum();
        let avg = dm / dh / 1000.0;
        let v = format!("{}", Watts(avg));
        let tail = match r.full_charge {
            Some(fc) if fc > 0 && avg > 0.0 => {
                format!(
                    "avg over {} segments -> est. runtime {}",
                    segs.len(),
                    Hours((fc as f64 / 1000.0) / avg)
                )
            }
            _ => format!("avg over {} segments", segs.len()),
        };
        row("Discharge", &v, &v, &tail);
    }

    let cutoff = Utc::now() - chrono::TimeDelta::days(days as i64);
    let recent: Vec<&Usage> = r
        .usage
        .iter()
        .filter(|u| u.t.is_some_and(|t| t >= cutoff))
        .collect();
    if !recent.is_empty() {
        let drains = r
            .drains
            .iter()
            .filter(|s| s.t0.is_some_and(|t| t >= cutoff))
            .count();
        let mut dc_ticks = 0u64;
        let mut all_ticks = 0u64;
        let mut dc_count = 0usize;
        let mut active_ticks = 0u64;
        let mut active_mwh = 0u64;
        let mut cs_ticks = 0u64;
        let mut cs_mwh = 0u64;

        for u in &recent {
            all_ticks += u.dur_ticks;
            if !u.ac {
                dc_ticks += u.dur_ticks;
                dc_count += 1;
                match u.entry_type.as_str() {
                    "Active" => {
                        active_ticks += u.dur_ticks;
                        active_mwh += u.discharge_mwh;
                    }
                    "ConnectedStandby" => {
                        cs_ticks += u.dur_ticks;
                        cs_mwh += u.discharge_mwh;
                    }
                    _ => {}
                }
            }
        }

        let dc_h = dc_ticks as f64 / TICKS_PER_HOUR;
        let v = format!("{}", Hours(dc_h));
        let pct = if all_ticks > 0 {
            100.0 * dc_ticks as f64 / all_ticks as f64
        } else {
            0.0
        };
        row(
            &format!("Last {days} days"),
            &v,
            &v,
            &format!("on battery, {pct:.0}% uptime, {dc_count} DC / {drains} drain segments"),
        );

        let active_h = active_ticks as f64 / TICKS_PER_HOUR;
        let cs_h = cs_ticks as f64 / TICKS_PER_HOUR;
        if active_h > 0.05 {
            let w = (active_mwh as f64 / 1000.0) / active_h;
            let v = format!("{}", Watts(w));
            row(
                "Active drain",
                &v,
                &v,
                &format!(
                    "screen-on work: {} ({} in {days} days)",
                    Hours(active_h),
                    Mwh(active_mwh as i64)
                ),
            );
        }
        if cs_h > 0.05 {
            let w = (cs_mwh as f64 / 1000.0) / cs_h;
            let v = format!("{}", Watts(w));
            row(
                "Sleep drain",
                &v,
                &v,
                &format!(
                    "modern standby: {} ({} in {days} days)",
                    Hours(cs_h),
                    Mwh(cs_mwh as i64)
                ),
            );
        }
    }
    if let Some(h) = r
        .est_active_runtime
        .as_deref()
        .and_then(batlens_core::xml::iso_hours)
    {
        let v = format!("{}", Hours(h));
        row("Windows", &v, &v, "full-charge runtime estimate (system)");
    }
    Ok(())
}

fn run() -> Result<(), BatlensError> {
    let cli = Cli::parse();
    match cli.command {
        Command::Status => {
            let rep = read_cached_report();
            let s = sample()?;
            print_status(&s, rep.as_ref());
            Ok(())
        }
        Command::Report { days, target, all } => cmd_report(days, target, all),
        Command::Info => cmd_info(),
        Command::Chart {
            drain,
            ascii,
            braille,
            current,
            width,
        } => crate::render::cmd_chart(drain, ascii, braille, current, width),
    }
}

fn main() -> std::process::ExitCode {
    let _cp_guard = ConsoleGuard::init();
    let result = run();
    let flushed = out::flush();
    if let Err(e) = result {
        eprintln!("batlens: {e}");
        return std::process::ExitCode::FAILURE;
    }
    if let Err(e) = flushed {
        eprintln!("batlens: failed to write output: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::*;
    use crate::render::*;
    use batlens_core::sys::*;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleOutputCP, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    #[test]
    fn test_visual_width_ansi_and_zero_width() {
        // Pure ASCII
        assert_eq!(visual_width("85.1%"), 5);
        assert_eq!(visual_width("12.9 W"), 6);
        // ANSI colored strings must ignore escape codes
        assert_eq!(visual_width("\x1b[32m85.1%\x1b[0m"), 5);
        assert_eq!(visual_width("\x1b[1;31m1.01 %/month\x1b[0m"), 12);
        // CJK full-width characters (width 2 each)
        assert_eq!(visual_width("電池"), 4);
        // Zero-width characters (ZWJ, variation selector, zero-width space)
        assert_eq!(visual_width("A\u{200D}B"), 2); // ZWJ between A and B
        assert_eq!(visual_width("8\u{FE0F}"), 2); // digit 8 with VS16 (emoji presentation)
        assert_eq!(visual_width("e\u{0301}"), 1); // combining acute accent
        assert_eq!(visual_width("test\u{200B}"), 4); // zero-width space
    }

    #[test]
    fn test_chart_width_from_cols_coordination() {
        // Standard wide terminals (cols >= 39)
        assert_eq!(chart_width_from_cols(80), 71);
        assert_eq!(chart_width_from_cols(39), 30);
        assert_eq!(chart_width_from_cols(400), 300);

        // Narrow terminals (under 39 columns).
        assert_eq!(chart_width_from_cols(20), 11); // 20 columns minus 9-column Y-axis margin
        assert_eq!(chart_width_from_cols(35), 26); // 35 - 9 = 26

        // Very narrow terminals: never exceeds the available columns.
        assert_eq!(chart_width_from_cols(12), 3); // 12 - 9 = 3 columns max
        assert_eq!(chart_width_from_cols(5), 0); // 5 <= 9 -> 0 columns available
    }

    #[test]
    fn test_console_cp_guard_restoration() {
        unsafe {
            let initial = GetConsoleOutputCP();
            let h_out = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut initial_mode = 0u32;
            let has_console = h_out != INVALID_HANDLE_VALUE
                && !h_out.is_null()
                && GetConsoleMode(h_out, &mut initial_mode) != 0;

            if initial != 0 {
                {
                    let _guard = ConsoleGuard::init();
                    assert_eq!(GetConsoleOutputCP(), 65001);
                }
                assert_eq!(
                    GetConsoleOutputCP(),
                    initial,
                    "ConsoleGuard must restore initial code page on drop"
                );
                if has_console {
                    let mut restored_mode = 0u32;
                    if GetConsoleMode(h_out, &mut restored_mode) != 0 {
                        assert_eq!(
                            restored_mode, initial_mode,
                            "ConsoleGuard must restore initial console mode on drop"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_resample_time_unsorted_resilience() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::TimeDelta::hours(1);
        let t2 = t0 + chrono::TimeDelta::hours(2);
        let unsorted = vec![(t2, 80.0), (t0, 100.0), (t1, 90.0)];
        let res = resample_time(&unsorted, 3);
        assert_eq!(res.len(), 3);
        assert!((res[0] - 100.0).abs() < 1e-3);
        assert!((res[1] - 90.0).abs() < 1e-3);
        assert!((res[2] - 80.0).abs() < 1e-3);
    }

    #[test]
    fn test_resample_time_subsecond_precision() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::TimeDelta::milliseconds(500);
        let t2 = t0 + chrono::TimeDelta::milliseconds(1000);
        let pts = vec![(t0, 100.0), (t1, 90.0), (t2, 80.0)];
        let res = resample_time(&pts, 3);
        assert_eq!(res.len(), 3);
        assert!((res[0] - 100.0).abs() < 1e-3);
        assert!((res[1] - 90.0).abs() < 1e-3);
        assert!((res[2] - 80.0).abs() < 1e-3);
    }

    #[test]
    fn test_braille_rows_minimum_value_not_blank() {
        let vals = vec![50.0, 100.0];
        let rows = braille_rows(&vals, 50.0, 100.0, 1, 2);
        // The minimum value (50.0 == vmin) still draws a column.
        let full_text: String = rows.into_iter().collect();
        assert!(
            full_text.chars().any(|c| c != ' '),
            "Minimum value must render at least baseline dot, not whitespace"
        );
    }

    #[test]
    fn test_visual_width_advanced_escape_sequences_and_zwj() {
        // ZWJ family sequence emoji: must measure as 2 columns, not 6
        assert_eq!(
            visual_width("\x1b[32m\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\x1b[0m"),
            2
        );
    }

    #[test]
    fn test_unknown_remaining_is_not_zero_percent() {
        let status = BatteryStatus {
            power_state: 0,
            capacity: BATTERY_UNKNOWN_CAPACITY,
            voltage: 12000,
            rate: -15000,
        };
        let info = BatteryInformation {
            capabilities: 0,
            technology: 1,
            reserved: [0; 3],
            chemistry: *b"LION",
            designed_capacity: 52000,
            full_charged_capacity: 50000,
            default_alert1: 0,
            default_alert2: 0,
            critical_bias: 0,
            cycle_count: 0,
        };
        let (unit, _) = parse_battery_unit_data(&status, &info, "B".into(), "M".into(), "S".into());
        assert_eq!(unit.percent, None);
        let s = build_sample(vec![unit], false);
        assert_eq!(s.percent, None);
        assert_eq!(time_left_line(&s, s.dis_w), None);
    }

    #[test]
    fn test_unknown_totals_render_na() {
        let unknown = |p| BatteryUnit {
            percent: p,
            ..Default::default()
        };
        let s = build_sample(vec![unknown(Some(50)), unknown(None)], false);
        assert_eq!(s.percent, None);
        assert_eq!(pct_text(s.percent), "N/A");
        assert_eq!(mwh_or_na(s.full_mwh), "N/A");
        assert_eq!(pct_text(Some(7)), "7%");
        assert_eq!(mwh_or_na(52440), "52.4 Wh");
    }

    #[test]
    fn test_unknown_percent_never_reports_fully_charged() {
        let s = Sample {
            ac: true,
            rate_mw: 50,
            ..Default::default()
        };
        assert!(time_left_line(&s, 0.0).is_none());
        let s = Sample {
            ac: true,
            rate_mw: 5000,
            ..Default::default()
        };
        assert_eq!(
            time_left_line(&s, 0.0).as_deref(),
            Some("Time left: charging")
        );
    }

    fn tl_sample(ac: bool, rate_mw: i64, percent: u32, rem: i64) -> Sample {
        Sample {
            ac,
            rate_mw,
            percent: Some(percent),
            remaining_mwh: rem,
            full_mwh: 50000,
            ..Default::default()
        }
    }

    #[test]
    fn test_eta_text_boundary_at_36_hours() {
        assert_ne!(eta_text(35.9), "> 36 h");
        assert_ne!(eta_text(36.0), "> 36 h");
        assert_eq!(eta_text(36.1), "> 36 h");
    }

    #[test]
    fn test_time_left_line_states() {
        let line =
            |ac, rate, dis_w, pct, rem| time_left_line(&tl_sample(ac, rate, pct, rem), dis_w);
        let l = line(false, -10000, 10.0, 50, 25000).unwrap();
        assert!(l.contains("at current draw") && !l.contains("> 36"));
        assert!(
            line(false, -500, 0.5, 50, 25000)
                .unwrap()
                .contains("> 36 h")
        );
        assert!(
            line(true, -10000, 10.0, 50, 25000)
                .unwrap()
                .contains("on AC")
        );
        assert_eq!(
            line(true, 20000, 0.0, 59, 30000).unwrap(),
            "Time left: charging"
        );
        assert!(
            line(true, 20000, 0.0, 100, 50000)
                .unwrap()
                .contains("fully charged")
        );
        assert!(
            line(true, 50, 0.0, 99, 49500)
                .unwrap()
                .contains("fully charged")
        );
        assert_eq!(
            line(true, 150, 0.0, 99, 49500).unwrap(),
            "Time left: charging"
        );
        assert!(line(false, 0, 0.0, 50, 25000).unwrap().contains("idle"));
        assert!(line(true, 0, 0.0, 50, 25000).is_none());
    }

    #[test]
    fn test_pct_bar_fill_and_clamping() {
        assert_eq!(pct_bar(0.0, 4), "░░░░");
        assert_eq!(pct_bar(50.0, 4), "██░░");
        assert_eq!(pct_bar(100.0, 4), "████");
        assert_eq!(pct_bar(250.0, 4), "████");
        assert_eq!(pct_bar(-5.0, 4), "░░░░");
        assert_eq!(pct_bar(85.1, 20).chars().count(), 20);
    }
}

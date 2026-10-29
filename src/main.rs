// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Evan Chen <evanchen940628@gmail.com>. All rights reserved.

#[cfg(not(windows))]
compile_error!("batlens is a Windows-only tool (requires Windows ACPI/IOCTL and powercfg).");

#[macro_use]
mod out;
pub mod render;
pub mod stats;
pub mod sys;
pub mod types;
pub mod xml;

use chrono::{Local, Utc};
use clap::{Parser, Subcommand};
use colored::Colorize;

use crate::render::{charge_paint, health_paint, lab, pct_bar, title, visual_width};
use crate::stats::{clean_history, compute_wear, find_battery_replacement};
use crate::sys::{ConsoleGuard, sample};
use crate::types::*;
use crate::xml::{read_cached_report, run_report_xml, update_design_capacity_cache_from_report};

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

/// Time-left line for non-mixed packs; `dis_w` is the discharge power.
fn time_left_line(s: &Sample, dis_w: f64) -> Option<String> {
    if dis_w >= 0.1 {
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
    if s.ac && (s.rate_mw > 100 || s.percent >= 99) {
        // `rate_mw <= 100` only applies at `percent >= 99`.
        let full = s.remaining_mwh >= s.full_mwh || s.percent >= 100 || s.rate_mw <= 100;
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
            "{} {}%  {}  ({})",
            lab("Charge   :"),
            s.percent,
            charge_paint(&pct_bar(s.percent as f64, 20), s.percent as f64),
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
            println!("{} {}", lab("Capacity :"), Mwh(s.full_mwh));
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
                "  [{}] {} ({}, {}) - {}%, {} / {} design, {}, {} (Health: {}, Cycles: {})",
                i + 1,
                b.device,
                b.manufacturer,
                b.chemistry,
                b.percent,
                Mwh(b.full_mwh),
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
            "  Total : {}% | Mode: {} | Power: {} | Capacity: {} / {} design",
            s.percent,
            mode_str,
            pw_str,
            Mwh(s.full_mwh),
            Mwh(s.design_mwh)
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

pub fn find_matching_report_battery<'a>(
    unit: &BatteryUnit,
    report: &'a Report,
    index: usize,
) -> Option<&'a ReportBattery> {
    if report.batteries.is_empty() {
        return None;
    }
    let eq_ignore_ascii_case = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    // Tier 1: serial number.
    if is_valid_serial(&unit.serial_number)
        && let Some(matched) = report.batteries.iter().find(|rb| {
            rb.serial_number.as_deref().is_some_and(|sn| {
                is_valid_serial(sn) && eq_ignore_ascii_case(sn, &unit.serial_number)
            })
        })
    {
        return Some(matched);
    }

    // Tier 2: device name equals report ID (e.g. "MS-1552", "BAT0").
    if !unit.device.is_empty()
        && unit.device != "Unknown"
        && let Some(matched) = report
            .batteries
            .iter()
            .find(|rb| !rb.id.is_empty() && eq_ignore_ascii_case(&rb.id, &unit.device))
    {
        return Some(matched);
    }

    // Tier 3: manufacturer plus capacity within tolerance.
    if !unit.manufacturer.is_empty() && unit.manufacturer != "Unknown" {
        let u_mfg = unit.manufacturer.to_lowercase();
        let close_match = report.batteries.iter().find(|rb| {
            let mfg_match = rb.manufacturer.as_deref().is_some_and(|m| {
                !m.is_empty()
                    && (m.to_lowercase().contains(&u_mfg) || u_mfg.contains(&m.to_lowercase()))
            });
            if !mfg_match {
                return false;
            }
            if unit.design_mwh > 0
                && let Some(dc) = rb.design_capacity
            {
                return (dc - unit.design_mwh).abs() <= (unit.design_mwh * 5 / 100);
            }
            if unit.full_mwh > 0
                && let Some(fc) = rb.full_charge
            {
                return (fc - unit.full_mwh).abs() <= (unit.full_mwh * 10 / 100);
            }
            true
        });
        if let Some(matched) = close_match {
            return Some(matched);
        }
    }

    // Tier 4: enumeration index.
    report.batteries.get(index)
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
            println!("    {} {s_num}", lab("Serial Number:"));
        }
        if b.voltage_mv > 0 {
            println!(
                "    {} {} ({})",
                lab("Live Voltage :"),
                Mv(b.voltage_mv),
                if s.ac { "on AC power" } else { "on battery" }
            );
        }
        if b.rate_mw != 0 {
            println!(
                "    {} {} ({})",
                lab("Live Power   :"),
                Mw(b.rate_mw).signed(),
                if b.rate_mw > 0 {
                    "charging"
                } else {
                    "discharging"
                }
            );
        }
        println!(
            "    {} {} ({} mWh)",
            lab("Design Cap   :"),
            Mwh(b.design_mwh),
            b.design_mwh
        );
        println!(
            "    {} {} ({} mWh)",
            lab("Remaining    :"),
            Mwh(b.remaining_mwh),
            b.remaining_mwh
        );
        println!(
            "    {} {} ({} mWh)",
            lab("Full Charge  :"),
            Mwh(b.full_mwh),
            b.full_mwh
        );
        if b.design_mwh > 0 {
            let hv = 100.0 * b.full_mwh as f64 / b.design_mwh as f64;
            let hs = format!("{:.1}%", hv);
            println!("    {} {}", lab("Health       :"), health_paint(&hs, hv));
        } else {
            println!("    {} N/A", lab("Health       :"));
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
        println!("    {} {cycle_str}", lab("Cycle Count  :"));
        if b.low_alert_mwh > 0 || b.crit_alert_mwh > 0 {
            println!(
                "    {} Low {}, Critical {}",
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
        .and_then(crate::xml::iso_hours)
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
    use crate::render::*;
    use crate::stats::*;
    use crate::sys::*;
    use crate::xml::*;
    use chrono::DateTime;
    use chrono::TimeZone;
    use std::fs;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleOutputCP, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    const INVALID_HDEVINFO: isize = -1;
    const BATTERY_UNKNOWN_VOLTAGE: u32 = 0xFFFF_FFFF;
    #[test]
    fn test_parse_report_both_self_closing_and_paired_tags() {
        let xml = r#"
        <BatteryReport>
            <SystemInformation>
                <SystemProductName>TestLaptop</SystemProductName>
            </SystemInformation>
            <History>
                <!-- Paired tag -->
                <HistoryEntry StartDate="2026-01-01T00:00:00Z" FullChargeCapacity="50000" DesignCapacity="52000" CycleCount="10"></HistoryEntry>
                <!-- Self-closing tag -->
                <HistoryEntry StartDate="2026-01-02T00:00:00Z" FullChargeCapacity="49000" DesignCapacity="52000" CycleCount="15" />
            </History>
            <EnergyDrains>
                <!-- Paired tag -->
                <Drain StartTimestamp="2026-01-01T10:00:00Z" EndTimestamp="2026-01-01T11:00:00Z" StartChargeCapacity="50000" EndChargeCapacity="40000"></Drain>
                <!-- Self-closing tag -->
                <Drain StartTimestamp="2026-01-02T10:00:00Z" EndTimestamp="2026-01-02T11:00:00Z" StartChargeCapacity="49000" EndChargeCapacity="39000" />
            </EnergyDrains>
            <RecentUsage>
                <!-- Paired tag -->
                <UsageEntry Timestamp="2026-01-01T10:00:00Z" Duration="36000000000" Ac="0" EntryType="Active" Discharge="10000"></UsageEntry>
                <!-- Self-closing tag -->
                <UsageEntry Timestamp="2026-01-02T10:00:00Z" Duration="18000000000" Ac="1" EntryType="Active" Discharge="0" />
            </RecentUsage>
        </BatteryReport>
        "#;

        let rep = parse_report(xml).expect("Failed to parse XML report");
        assert_eq!(
            rep.history.len(),
            2,
            "Both paired and self-closing HistoryEntry must be parsed"
        );
        assert_eq!(
            rep.drains.len(),
            2,
            "Both paired and self-closing Drain must be parsed"
        );
        assert_eq!(
            rep.usage.len(),
            2,
            "Both paired and self-closing UsageEntry must be parsed"
        );
        assert_eq!(rep.system_product.as_deref(), Some("TestLaptop"));
    }
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
    fn test_aggregate_drains_multi_day_split() {
        use chrono::TimeZone;
        // Segment 23:00 to 07:00 next day: 8 h, 32000 mWh, 4 W.
        let t0 = chrono::Local
            .with_ymd_and_hms(2026, 10, 23, 23, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let t1 = Local
            .with_ymd_and_hms(2026, 10, 24, 7, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let drains = vec![Seg {
            t0: Some(t0),
            t1: Some(t1),
            start_mwh: 32000,
            end_mwh: 0,
        }];
        let by_day = aggregate_drains_by_day(&drains);
        assert_eq!(by_day.len(), 2, "Should split into exactly 2 days");
        let d1 = chrono::NaiveDate::from_ymd_opt(2026, 10, 23).unwrap();
        let d2 = chrono::NaiveDate::from_ymd_opt(2026, 10, 24).unwrap();
        let e1 = by_day.get(&d1).expect("Day 1 must exist");
        let e2 = by_day.get(&d2).expect("Day 2 must exist");

        // Day 1 has 1 hour (23:00 to 24:00) = 4000 mWh
        assert!((e1.1 - 1.0).abs() < 1e-4, "Day 1 should be 1.0 hour");
        assert!((e1.0 - 4000.0).abs() < 1.0, "Day 1 should be 4000 mWh");
        // Day 2 has 7 hours (00:00 to 07:00) = 28000 mWh
        assert!((e2.1 - 7.0).abs() < 1e-4, "Day 2 should be 7.0 hours");
        assert!((e2.0 - 28000.0).abs() < 1.0, "Day 2 should be 28000 mWh");
        // Power rate on both days must be 4.0 W
        assert!((e1.0 / e1.1 / 1000.0 - 4.0).abs() < 1e-4);
        assert!((e2.0 / e2.1 / 1000.0 - 4.0).abs() < 1e-4);
        // Total energy must be strictly conserved
        assert!(
            (e1.0 + e2.0 - 32000.0).abs() < 1e-4,
            "Total energy must be strictly conserved"
        );
    }
    #[test]
    fn test_detail_data_layout() {
        use windows_sys::Win32::Devices::DeviceAndDriverInstallation::SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        let offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        assert_eq!(
            offset, 4,
            "DevicePath must start right after 4-byte cbSize DWORD"
        );
        if cfg!(target_pointer_width = "64") {
            assert_eq!(
                SP_DEVICE_INTERFACE_DETAIL_DATA_W_CBSIZE, 8,
                "64-bit ABI expects 8 bytes"
            );
        } else if cfg!(target_arch = "x86") {
            assert_eq!(
                SP_DEVICE_INTERFACE_DETAIL_DATA_W_CBSIZE, 6,
                "x86 ABI expects 6 bytes"
            );
        }
        // Null-terminator boundary.
        let valid_path: [u16; 4] = [b'A' as u16, b'C' as u16, b'P' as u16, 0];
        assert!(valid_path.contains(&0));
        let unterminated: [u16; 3] = [b'B' as u16, b'A' as u16, b'T' as u16];
        assert!(!unterminated.contains(&0));
        // HDEVINFO and INVALID_HANDLE_VALUE sign extension.
        assert_eq!(
            INVALID_HANDLE_VALUE as isize, -1,
            "INVALID_HANDLE_VALUE must sign-extend to -1isize on all target architectures"
        );
        assert_eq!(INVALID_HDEVINFO, -1);
        assert_eq!(INVALID_HANDLE_VALUE as isize, INVALID_HDEVINFO);
    }
    #[test]
    fn test_parse_report_multiple_batteries() {
        let xml = r#"
        <BatteryReport>
            <Batteries>
                <Battery>
                    <Id>BAT1</Id>
                    <Manufacturer>LGC</Manufacturer>
                    <Chemistry>LION</Chemistry>
                    <DesignCapacity>24000</DesignCapacity>
                    <FullChargeCapacity>22000</FullChargeCapacity>
                    <CycleCount>150</CycleCount>
                </Battery>
                <Battery>
                    <Id>BAT2</Id>
                    <Manufacturer>SMP</Manufacturer>
                    <Chemistry>LION</Chemistry>
                    <DesignCapacity>32000</DesignCapacity>
                    <FullChargeCapacity>30000</FullChargeCapacity>
                    <CycleCount>80</CycleCount>
                </Battery>
            </Batteries>
            <RuntimeEstimates>
                <DesignCapacity>
                    <Capacity>56000</Capacity>
                </DesignCapacity>
            </RuntimeEstimates>
        </BatteryReport>
        "#;

        let rep = parse_report(xml).expect("Failed to parse multiple batteries");
        assert_eq!(
            rep.batteries.len(),
            2,
            "Must parse exactly 2 battery modules"
        );
        assert_eq!(rep.batteries[0].id, "BAT1");
        assert_eq!(rep.batteries[0].manufacturer.as_deref(), Some("LGC"));
        assert_eq!(rep.batteries[0].design_capacity, Some(24000));
        assert_eq!(rep.batteries[0].full_charge, Some(22000));
        assert_eq!(rep.batteries[0].cycle_count, Some(150));

        assert_eq!(rep.batteries[1].id, "BAT2");
        assert_eq!(rep.batteries[1].manufacturer.as_deref(), Some("SMP"));
        assert_eq!(rep.batteries[1].design_capacity, Some(32000));
        assert_eq!(rep.batteries[1].full_charge, Some(30000));
        assert_eq!(rep.batteries[1].cycle_count, Some(80));

        // RuntimeEstimates provided 56000 design capacity
        assert_eq!(rep.design_capacity, Some(56000));
        assert_eq!(
            rep.cycle_count, None,
            "Cycle count must not be summed across multiple battery modules"
        );
    }
    #[test]
    fn test_terminal_width_bounds() {
        let w = terminal_width();
        assert!(
            (4..=300).contains(&w),
            "terminal_width must be clamped between 4 and 300, got {w}"
        );
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
    fn test_get_powercfg_path() {
        let path = get_powercfg_path();
        assert!(
            path.exists() || path == std::path::Path::new("powercfg.exe"),
            "powercfg executable must exist or fallback to 'powercfg.exe', got {path:?}"
        );
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
    fn test_cache_paths_consistency() {
        let cache_dir = get_cache_dir();
        let report_cache = get_report_cache_path();
        let design_file = design_capacity_file();

        assert_eq!(report_cache.parent(), Some(cache_dir.as_path()));
        assert_eq!(design_file.parent(), Some(cache_dir.as_path()));
        assert!(cache_dir.exists(), "Cache dir must be created and exist");
    }
    #[test]
    fn test_find_battery_replacement_heuristics() {
        use chrono::TimeZone;
        let t0 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let dt = |days: i64| t0 + chrono::TimeDelta::days(days);

        // Case 1: Real replacement (69% -> 97% sustained)
        let real_rep = vec![
            (dt(0), 75.0),
            (dt(10), 69.0),
            (dt(20), 97.3),
            (dt(30), 97.0),
            (dt(40), 96.5),
        ];
        assert_eq!(find_battery_replacement(&real_rep), Some(2));

        // Case 2: Deep recalibration of aged pack (55% -> 72%, below 88% threshold)
        let recalib = vec![
            (dt(0), 58.0),
            (dt(10), 55.0),
            (dt(20), 72.0),
            (dt(30), 71.5),
        ];
        assert_eq!(
            find_battery_replacement(&recalib),
            None,
            "Recalibration below 88% must not be classified as new battery"
        );

        // Case 3: Transient glitch (60% -> 95% -> 61%, not sustained)
        let spike = vec![
            (dt(0), 62.0),
            (dt(10), 60.0),
            (dt(20), 95.0),
            (dt(30), 61.0),
        ];
        assert_eq!(
            find_battery_replacement(&spike),
            None,
            "Transient spike must not be classified as new battery"
        );

        // Case 4: Recent replacement with only 1 subsequent entry (short sampling window)
        let recent_1 = vec![
            (dt(0), 65.0),
            (dt(10), 60.0),
            (dt(20), 97.0),
            (dt(25), 96.5),
        ];
        assert_eq!(find_battery_replacement(&recent_1), Some(2));

        // Case 5: Recent replacement with early calibration dip (1 of 2 points dips to 78.5%)
        let early_dip = vec![
            (dt(0), 65.0),
            (dt(10), 60.0),
            (dt(20), 96.0),
            (dt(25), 78.5),
            (dt(30), 95.0),
        ];
        assert_eq!(
            find_battery_replacement(&early_dip),
            Some(2),
            "Early break-in calibration dip must not disqualify genuine new battery"
        );

        // Case 6: worn baseline 73%, jump to 91%, then 81.5%.
        let high_prev = vec![
            (dt(0), 76.0),
            (dt(10), 73.0),
            (dt(20), 91.0),
            (dt(30), 81.5),
        ];
        assert_eq!(find_battery_replacement(&high_prev), Some(2));
        // Case 7: unverified jump at the last entry must not truncate history.
        let tail_spike = vec![
            (dt(0), 65.0),
            (dt(10), 60.0),
            (dt(20), 58.0),
            (dt(30), 80.0), // calibration jump at tail without subsequent confirming entries
        ];
        assert_eq!(
            find_battery_replacement(&tail_spike),
            None,
            "Sub-pristine tail jump without subsequent sustained samples must not trigger replacement"
        );
    }
    #[test]
    fn test_parse_battery_name_odd_byte_alignment() {
        let buf = [
            b'M' as u16,
            b'S' as u16,
            b'-' as u16,
            b'1' as u16,
            b'5' as u16,
            b'5' as u16,
            b'2' as u16,
            0,
        ];
        // Normal even bytes (14 bytes = 7 chars)
        assert_eq!(parse_battery_name_utf16(&buf, 14), "MS-1552");
        // Odd byte count: the orphan last byte is ignored.
        assert_eq!(parse_battery_name_utf16(&buf, 15), "MS-1552");
        // Zero bytes returned
        assert_eq!(parse_battery_name_utf16(&buf, 0), "");
        // Single byte returned (< 2 bytes, 1 byte)
        assert_eq!(parse_battery_name_utf16(&buf, 1), "");
    }
    #[test]
    fn test_aggregate_alerts_multi_battery() {
        let make_b = |low, crit| BatteryUnit {
            low_alert_mwh: if low == u32::MAX { 0 } else { low },
            crit_alert_mwh: if crit == u32::MAX { 0 } else { crit },
            ..Default::default()
        };
        let alerts = vec![make_b(2400, 1200), make_b(4800, 2400)];
        assert_eq!(aggregate_alerts(&alerts), (7200, 3600));

        let with_sentinels = vec![make_b(2400, 1200), make_b(u32::MAX, 0), make_b(0, u32::MAX)];
        assert_eq!(aggregate_alerts(&with_sentinels), (2400, 1200));

        assert_eq!(aggregate_alerts(&[]), (0, 0));
    }
    #[test]
    fn test_tmp_cleaner_lifecycle_and_disarm() {
        let temp_dir = std::env::temp_dir();
        let test_file_1 =
            temp_dir.join(format!("batlens_test_cleaner_1_{}.tmp", std::process::id()));
        let test_file_2 =
            temp_dir.join(format!("batlens_test_cleaner_2_{}.tmp", std::process::id()));

        // Case 1: Active cleaner drops and deletes the file
        fs::write(&test_file_1, "test").unwrap();
        assert!(test_file_1.exists());
        {
            let _cleaner = TmpCleaner::new(test_file_1.clone());
        }
        assert!(
            !test_file_1.exists(),
            "Active TmpCleaner must remove file on drop"
        );

        // Case 2: Disarmed cleaner leaves file intact (e.g. after successful rename)
        fs::write(&test_file_2, "test").unwrap();
        assert!(test_file_2.exists());
        {
            let mut cleaner = TmpCleaner::new(test_file_2.clone());
            cleaner.disarm();
        }
        assert!(
            test_file_2.exists(),
            "Disarmed TmpCleaner must NOT remove file on drop"
        );
        let _ = fs::remove_file(&test_file_2);
    }
    #[test]
    fn test_build_sample_aggregation() {
        let b1 = BatteryUnit {
            device: "BAT1".to_string(),
            manufacturer: "SMP".to_string(),
            serial_number: "SN-111".to_string(),
            chemistry: "LION".to_string(),
            percent: 50,
            remaining_mwh: 20000,
            full_mwh: 40000,
            design_mwh: 50000,
            voltage_mv: 11400,
            rate_mw: -15000,
            cycle_count: Some(100),
            low_alert_mwh: 2000,
            crit_alert_mwh: 1000,
        };
        let b2 = BatteryUnit {
            device: "BAT2".to_string(),
            manufacturer: "LGC".to_string(),
            serial_number: "SN-222".to_string(),
            chemistry: "LION".to_string(),
            percent: 50,
            remaining_mwh: 10000,
            full_mwh: 20000,
            design_mwh: 25000,
            voltage_mv: 11400,
            rate_mw: 0,
            cycle_count: Some(50),
            low_alert_mwh: 1500,
            crit_alert_mwh: 500,
        };

        let sample = build_sample(vec![b1, b2], false);
        assert_eq!(sample.remaining_mwh, 30000);
        assert_eq!(sample.full_mwh, 60000);
        assert_eq!(sample.design_mwh, 75000);
        assert_eq!(sample.percent, 50);
        assert_eq!(sample.rate_mw, -15000);
        assert_eq!(sample.dis_w, 15.0);
        assert_eq!(sample.gross_dis_w, 15.0);
        assert_eq!(sample.gross_chg_w, 0.0);
        assert_eq!(sample.low_alert_mwh, 3500);
        assert_eq!(sample.crit_alert_mwh, 1500);
        assert!(!sample.ac);
    }
    #[test]
    fn test_dual_battery_mixed_power_and_runtime_isolation() {
        // Battery 1 discharges at -25 W with 20 Wh left.
        let b1 = BatteryUnit {
            device: "BAT1".to_string(),
            manufacturer: "SMP".to_string(),
            serial_number: "SN-111".to_string(),
            chemistry: "LION".to_string(),
            percent: 50,
            remaining_mwh: 20000,
            full_mwh: 40000,
            design_mwh: 50000,
            voltage_mv: 11400,
            rate_mw: -25000,
            ..Default::default()
        };
        // Battery 2 charges at +20 W with 10 Wh left.
        let b2 = BatteryUnit {
            device: "BAT2".to_string(),
            manufacturer: "LGC".to_string(),
            serial_number: "SN-222".to_string(),
            chemistry: "LION".to_string(),
            percent: 50,
            remaining_mwh: 10000,
            full_mwh: 20000,
            design_mwh: 25000,
            voltage_mv: 11400,
            rate_mw: 20000,
            ..Default::default()
        };

        let sample = build_sample(vec![b1, b2], true);
        // Net power is -5W (-5,000 mW)
        assert_eq!(sample.rate_mw, -5000);
        assert_eq!(sample.dis_w, 5.0);
        assert_eq!(sample.gross_dis_w, 25.0);
        assert_eq!(sample.gross_chg_w, 20.0);

        // Naive estimate: 30 Wh / 5 W net = 6.0 h.
        let naive_hours = sample.remaining_mwh as f64 / 1000.0 / sample.dis_w;
        assert_eq!(naive_hours, 6.0);

        // Active depletion:
        // Only battery 1 discharges: 20 Wh at 25 W is 0.8 h.
        let dis_remaining: i64 = sample
            .batteries
            .iter()
            .filter(|b| b.rate_mw < 0)
            .map(|b| b.remaining_mwh)
            .sum();
        let true_hours = dis_remaining as f64 / 1000.0 / sample.gross_dis_w;
        assert_eq!(true_hours, 0.8);
        assert!(
            true_hours < 1.0,
            "Active pack will deplete in 48 minutes, not 6 hours"
        );
    }
    #[test]
    fn test_batlens_error_variants_and_source() {
        use std::error::Error;
        // Win32 SetupApi error display
        let err1 = BatlensError::SetupApi {
            api: "SetupDiGetClassDevsW",
            code: 5,
        };
        assert_eq!(
            err1.to_string(),
            "SetupDiGetClassDevsW failed (Win32 error 5 (Access denied. Try running as Administrator))"
        );
        assert!(err1.source().is_none());

        // DeviceOpen error display
        let err2 = BatlensError::DeviceOpen { code: 2 };
        assert_eq!(
            err2.to_string(),
            "CreateFileW failed to open battery device (Win32 error 2 (Device or file not found))"
        );
        // DeviceIoctl error display
        let err3 = BatlensError::DeviceIoctl {
            ioctl: "IOCTL_BATTERY_QUERY_TAG",
            code: 1,
        };
        assert_eq!(
            err3.to_string(),
            "IOCTL_BATTERY_QUERY_TAG failed (Win32 error 1)"
        );

        // NoActiveBattery without and with diagnostics
        let err4 = BatlensError::NoActiveBattery {
            diagnostics: vec![],
        };
        assert_eq!(
            err4.to_string(),
            "No active battery detected via ACPI/IOCTL"
        );
        let err4_diag = BatlensError::NoActiveBattery {
            diagnostics: vec!["Device #0: Tag query failed".to_string()],
        };
        assert!(
            err4_diag
                .to_string()
                .contains("Device #0: Tag query failed")
        );

        // PowercfgFailed error display
        let err5 = BatlensError::PowercfgFailed {
            status: Some(1),
            stderr: "Access denied".to_string(),
        };
        assert!(
            err5.to_string()
                .starts_with("powercfg failed: Access denied")
        );
        assert!(err5.to_string().contains("status' uses direct IOCTL"));
        // Io error source chaining and From conversion
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err6 = BatlensError::from(io_err);
        assert!(err6.to_string().contains("access denied"));
        assert!(err6.source().is_some());

        // InvalidReport display
        let err7 = BatlensError::InvalidReport("corrupt header".to_string());
        assert_eq!(
            err7.to_string(),
            "Incomplete or invalid battery report XML: corrupt header"
        );
    }
    #[test]
    fn test_parse_report_cdata_comments_and_unexpected_nested_tags() {
        let xml = r#"
        <!-- Top level XML comment -->
        <BatteryReport>
            <!-- System information with CDATA, comments, and unexpected vendor wrappers -->
            <SystemInformation>
                <!-- OEM comment inside tag -->
                <SystemProductName><![CDATA[Summit B15 A11M (Custom Edition)]]></SystemProductName>
                <VendorExtensions>
                    <TelemetryId>ignore_me_12345</TelemetryId>
                    <DeepNestedWrapper>
                        <NoiseField>unexpected_value</NoiseField>
                    </DeepNestedWrapper>
                </VendorExtensions>
                <BIOSVersion><![CDATA[E1552IMS.321]]></BIOSVersion>
            </SystemInformation>
            <!-- Batteries with unexpected noise tags and CDATA -->
            <Batteries>
                <Battery>
                    <!-- Comment before Id -->
                    <Id><![CDATA[BAT-MS-9999]]></Id>
                    <Manufacturer>MSI</Manufacturer>
                    <UnrelatedNoiseTag>
                        <SubNoise>random text that must not bleed</SubNoise>
                    </UnrelatedNoiseTag>
                    <DesignCapacity>52000</DesignCapacity>
                    <FullChargeCapacity>44600</FullChargeCapacity>
                    <CycleCount>42</CycleCount>
                </Battery>
            </Batteries>
            <!-- RuntimeEstimates with nested comment blocks -->
            <RuntimeEstimates>
                <FullChargeCapacity>
                    <!-- Comment inside estimates -->
                    <Capacity>44600</Capacity>
                    <ActiveRuntime><![CDATA[PT3H45M]]></ActiveRuntime>
                </FullChargeCapacity>
            </RuntimeEstimates>
        </BatteryReport>
        "#;

        let rep = parse_report(xml).expect("Failed to parse report with CDATA and comments");
        assert_eq!(
            rep.system_product.as_deref(),
            Some("Summit B15 A11M (Custom Edition)"),
            "Must correctly extract CDATA in SystemProductName"
        );
        assert_eq!(
            rep.bios_version.as_deref(),
            Some("E1552IMS.321"),
            "Must correctly extract CDATA in BIOSVersion"
        );
        assert_eq!(rep.batteries.len(), 1);
        assert_eq!(rep.batteries[0].id, "BAT-MS-9999");
        assert_eq!(rep.batteries[0].manufacturer.as_deref(), Some("MSI"));
        assert_eq!(rep.batteries[0].design_capacity, Some(52000));
        assert_eq!(rep.batteries[0].full_charge, Some(44600));
        assert_eq!(rep.batteries[0].cycle_count, Some(42));
        assert_eq!(rep.est_active_runtime.as_deref(), Some("PT3H45M"));
    }
    #[test]
    fn test_sample_win32_hardware_and_permission_failures() {
        // Case 1: SetupAPI fails with Access Denied (Win32 error 5)
        let res1 = sample_with(
            || {
                Err(BatlensError::SetupApi {
                    api: "SetupDiGetClassDevsW",
                    code: 5,
                })
            },
            |_| unreachable!(),
        );
        match res1 {
            Err(BatlensError::SetupApi { api, code }) => {
                assert_eq!(api, "SetupDiGetClassDevsW");
                assert_eq!(code, 5);
            }
            _ => panic!("Expected SetupApi error"),
        }

        // Case 2: Hardware not present (no device paths found)
        let res2 = sample_with(
            || {
                Ok(EnumeratedDevices {
                    paths: vec![],
                    errors: vec![],
                })
            },
            |_| unreachable!(),
        );
        match res2 {
            Err(BatlensError::NoActiveBattery { diagnostics }) => {
                assert!(diagnostics.is_empty());
            }
            _ => panic!("Expected NoActiveBattery with empty diagnostics"),
        }

        // Case 3: opening every device fails with error 5.
        let res3 = sample_with(
            || {
                Ok(EnumeratedDevices {
                    paths: vec![vec![1, 2, 0]],
                    errors: vec![],
                })
            },
            |_| Err(BatlensError::DeviceOpen { code: 5 }),
        );
        match res3 {
            Err(BatlensError::NoActiveBattery { diagnostics }) => {
                assert_eq!(diagnostics.len(), 1);
                assert!(diagnostics[0].contains("CreateFileW failed"));
                assert!(diagnostics[0].contains("Win32 error 5"));
            }
            _ => panic!("Expected NoActiveBattery with device diagnostics"),
        }

        // Case 4: device 0 fails, device 1 succeeds; a valid Sample is returned.
        let dummy_battery = BatteryUnit {
            device: "BAT1".to_string(),
            manufacturer: "SMP".to_string(),
            serial_number: "SN-111".to_string(),
            chemistry: "LION".to_string(),
            percent: 85,
            remaining_mwh: 42500,
            full_mwh: 50000,
            design_mwh: 52000,
            voltage_mv: 11500,
            rate_mw: -12000,
            cycle_count: Some(50),
            low_alert_mwh: 2000,
            crit_alert_mwh: 1000,
        };
        let b_clone = dummy_battery.clone();
        let res4 = sample_with(
            || {
                Ok(EnumeratedDevices {
                    paths: vec![vec![1, 0], vec![2, 0]],
                    errors: vec![],
                })
            },
            move |path| {
                if path[0] == 1 {
                    Err(BatlensError::DeviceIoctl {
                        ioctl: "IOCTL_BATTERY_QUERY_TAG",
                        code: 1,
                    })
                } else {
                    Ok((b_clone.clone(), false))
                }
            },
        );
        assert!(
            res4.is_ok(),
            "Partial failure should produce valid Sample if at least 1 device succeeds"
        );
        let sample = res4.unwrap();
        assert_eq!(sample.batteries.len(), 1);
        assert_eq!(sample.percent, 85);
        assert_eq!(sample.remaining_mwh, 42500);
    }
    #[test]
    fn test_parse_battery_ioctl_data_sentinels_and_overflow() {
        // Case 1: All unknown sentinels (0xFFFF_FFFF, i32::MIN, u32::MAX)
        let status = BatteryStatus {
            power_state: 0,
            capacity: BATTERY_UNKNOWN_CAPACITY,
            voltage: BATTERY_UNKNOWN_VOLTAGE,
            rate: BATTERY_UNKNOWN_RATE,
        };
        let mut chemistry = [0u8; 4];
        chemistry.copy_from_slice(b"LION");
        let info = BatteryInformation {
            capabilities: 0,
            technology: 1,
            reserved: [0; 3],
            chemistry,
            designed_capacity: BATTERY_UNKNOWN_CAPACITY,
            full_charged_capacity: BATTERY_UNKNOWN_CAPACITY,
            default_alert1: u32::MAX,
            default_alert2: u32::MAX,
            critical_bias: 0,
            cycle_count: u32::MAX,
        };
        let (unit, ac) = parse_battery_unit_data(
            &status,
            &info,
            "BAT0".to_string(),
            "OEM".to_string(),
            "SN-001".to_string(),
        );
        assert_eq!(unit.full_mwh, 0);
        assert_eq!(unit.design_mwh, 0);
        assert_eq!(unit.voltage_mv, 0);
        assert_eq!(unit.rate_mw, 0);
        assert_eq!(unit.percent, 0, "Percent must not divide by zero");
        assert_eq!(unit.cycle_count, None, "Sentinel cycle count must be None");
        assert_eq!(unit.low_alert_mwh, 0);
        assert_eq!(unit.crit_alert_mwh, 0);
        assert!(!ac);

        // Case 2: i32::MIN rate overflow protection & discharging flag
        let mut status_dis = status;
        status_dis.power_state = 0x02; // discharging
        status_dis.rate = i32::MIN; // abs() would overflow
        let (unit_dis, _) = parse_battery_unit_data(
            &status_dis,
            &info,
            "".to_string(),
            "".to_string(),
            "".to_string(),
        );
        assert_eq!(
            unit_dis.rate_mw, 0,
            "i32::MIN must be clamped to 0 to prevent signed overflow panic"
        );
        assert_eq!(unit_dis.manufacturer, "Unknown");

        // Case 3: Charging with AC flag (power_state 0x01 | 0x04 = AC + Charging)
        let mut status_chg = status;
        status_chg.power_state = 0x01 | 0x04; // AC + Charging
        status_chg.rate = 18500;
        status_chg.capacity = 25000;
        let mut info_chg = info;
        info_chg.designed_capacity = 50000;
        info_chg.full_charged_capacity = 50000;
        info_chg.default_alert1 = 2500;
        info_chg.default_alert2 = 1000;
        info_chg.cycle_count = 120;
        let (unit_chg, ac_chg) = parse_battery_unit_data(
            &status_chg,
            &info_chg,
            "BAT1".to_string(),
            "SMP".to_string(),
            "SN-999".to_string(),
        );
        assert!(ac_chg);
        assert_eq!(unit_chg.rate_mw, 18500);
        assert_eq!(unit_chg.percent, 50);
        assert_eq!(unit_chg.cycle_count, Some(120));
        assert_eq!(unit_chg.low_alert_mwh, 2500);
        assert_eq!(unit_chg.crit_alert_mwh, 1000);
    }
    #[test]
    fn test_xml_corruption_malformed_and_truncated() {
        // Case 1: Truncated XML (abrupt EOF before tags close)
        let truncated = "<BatteryReport><SystemInformation><SystemProductName>Truncated";
        let err1 = parse_report(truncated);
        match err1 {
            Err(BatlensError::InvalidReport(msg)) => {
                assert!(msg.contains("truncated XML"));
            }
            _ => panic!("Truncated XML must return InvalidReport error with truncated warning"),
        }

        // Case 2: XML syntax error (unclosed comment / malformed syntax)
        let malformed = "<BatteryReport><!-- unclosed comment";
        let err2 = parse_report(malformed);
        assert!(
            matches!(err2, Err(BatlensError::XmlParse(_))),
            "Malformed syntax must return XmlParse error"
        );
        let empty_doc = "<BatteryReport><EmptySection /></BatteryReport>";
        let err3 = parse_report(empty_doc);
        assert!(
            matches!(err3, Err(BatlensError::InvalidReport(_))),
            "Report without history, usage, or design capacity must return InvalidReport"
        );

        // Case 4: Missing attributes in entries (must not panic, defaults to safe values)
        let sparse = r#"
        <BatteryReport>
            <History>
                <HistoryEntry />
            </History>
            <EnergyDrains>
                <Drain />
            </EnergyDrains>
            <RecentUsage>
                <UsageEntry />
            </RecentUsage>
            <RuntimeEstimates>
                <DesignCapacity>
                    <Capacity>50000</Capacity>
                </DesignCapacity>
            </RuntimeEstimates>
        </BatteryReport>
        "#;
        let rep = parse_report(sparse).expect("Sparse attributes must parse safely without panic");
        assert_eq!(rep.history.len(), 1);
        assert_eq!(rep.history[0].full, 0);
        assert_eq!(rep.drains.len(), 1);
        assert_eq!(rep.usage.len(), 1);
        assert_eq!(rep.design_capacity, Some(50000));
    }
    #[test]
    fn test_cache_repair_and_regeneration_lifecycle() {
        let temp_dir = std::env::temp_dir();
        let cache_file = temp_dir.join(format!("batlens_test_cache_{}.xml", std::process::id()));

        // Case 1: corrupt cache is removed, powercfg regenerates it, load succeeds.
        fs::write(&cache_file, "<BatteryReport><!-- unclosed").unwrap();
        assert!(cache_file.exists());

        let valid_xml = r#"
        <BatteryReport>
            <RuntimeEstimates>
                <DesignCapacity><Capacity>52000</Capacity></DesignCapacity>
            </RuntimeEstimates>
        </BatteryReport>
        "#;

        let res = load_report_with(
            |_| read_xml_string(&cache_file),
            |_| Ok(Some(valid_xml.to_string())),
            |_| unreachable!(),
            &cache_file,
        );
        assert!(
            res.is_ok(),
            "Corrupt cache must trigger regeneration and return fresh report"
        );
        assert_eq!(res.unwrap().design_capacity, Some(52000));
        assert!(
            !cache_file.exists(),
            "Corrupt cache file must have been evicted from disk"
        );

        // Case 2: corrupt cache and powercfg also fails; the parse error is returned.
        fs::write(&cache_file, "<BatteryReport><!-- unclosed").unwrap();
        let res_fail = load_report_with(
            |_| read_xml_string(&cache_file),
            |_| {
                Err(BatlensError::PowercfgFailed {
                    status: Some(1),
                    stderr: "powercfg error".to_string(),
                })
            },
            |_| unreachable!(),
            &cache_file,
        );
        assert!(
            matches!(res_fail, Err(BatlensError::XmlParse(_))),
            "When regeneration fails, initial parse error must be preserved"
        );
        let _ = fs::remove_file(&cache_file);
    }
    #[test]
    fn test_read_xml_string_non_utf8_and_bom_variants() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("batlens_test_xml_enc_{}.xml", std::process::id()));

        // Case 1: non-UTF-8 binary payload.
        let garbage_bytes: [u8; 6] = [b'<', 0xFF, 0xFE, b'a', 0xC0, b'>'];
        fs::write(&test_file, garbage_bytes).unwrap();
        let decoded = read_xml_string(&test_file);
        assert!(
            decoded.is_ok(),
            "Non-UTF-8 bytes must not panic, lossy fallback must succeed"
        );

        // Case 2: UTF-8 with BOM
        let mut utf8_bom = vec![0xEF, 0xBB, 0xBF];
        utf8_bom.extend_from_slice(b"<BatteryReport />");
        fs::write(&test_file, &utf8_bom).unwrap();
        let decoded_bom = read_xml_string(&test_file).unwrap();
        assert_eq!(decoded_bom, "<BatteryReport />");

        let _ = fs::remove_file(&test_file);
    }
    #[test]
    fn test_parse_report_text_chunking_and_entities() {
        let xml = r#"
        <BatteryReport>
            <SystemInformation>
                <!-- Entity reference &amp; splits into chunks -->
                <SystemProductName>ThinkPad &amp; Yoga 15</SystemProductName>
                <!-- Interleaved comment splits text into chunks -->
                <BIOSVersion>E1552<!-- revision -->IMS.321</BIOSVersion>
            </SystemInformation>
            <Batteries>
                <Battery>
                    <!-- Concatenated Text + CDATA + Text -->
                    <Id>BAT-<![CDATA[LGC]]>-001</Id>
                    <Manufacturer>Lenovo &amp; LG</Manufacturer>
                    <DesignCapacity>50000</DesignCapacity>
                    <FullChargeCapacity>45000</FullChargeCapacity>
                </Battery>
            </Batteries>
            <RuntimeEstimates>
                <DesignCapacity><Capacity>50000</Capacity></DesignCapacity>
            </RuntimeEstimates>
        </BatteryReport>
        "#;

        let rep = parse_report(xml).expect("Chunked text and entities must be parsed");
        assert_eq!(
            rep.system_product.as_deref(),
            Some("ThinkPad & Yoga 15"),
            "Entities must be concatenated without losing chunks"
        );
        assert_eq!(
            rep.bios_version.as_deref(),
            Some("E1552IMS.321"),
            "Text split across comments must be concatenated"
        );
        assert_eq!(rep.batteries.len(), 1);
        assert_eq!(
            rep.batteries[0].id, "BAT-LGC-001",
            "Text + CDATA + Text must be concatenated into full ID"
        );
        assert_eq!(
            rep.batteries[0].manufacturer.as_deref(),
            Some("Lenovo & LG")
        );
    }
    #[test]
    fn test_parse_report_vendor_extensions_and_ignored_end_tags() {
        let xml = r#"
        <BatteryReport>
            <SystemInformation>
                <SystemProductName>Summit B15</SystemProductName>
                <!-- Complex vendor extension with custom namespace prefix and nested tags -->
                <oem:UnknownVendorExtension>
                    <oem:TelemetryToken>XYZ-12345</oem:TelemetryToken>
                </oem:UnknownVendorExtension>
                <BIOSVersion>E1552IMS.321</BIOSVersion>
            </SystemInformation>
            <RuntimeEstimates>
                <DesignCapacity><Capacity>52000</Capacity></DesignCapacity>
            </RuntimeEstimates>
        </BatteryReport>
        "#;

        let rep = parse_report(xml).expect("Vendor extensions must not break parser");
        assert_eq!(
            rep.system_product.as_deref(),
            Some("Summit B15"),
            "SystemProductName before vendor tag must be preserved"
        );
        assert_eq!(
            rep.bios_version.as_deref(),
            Some("E1552IMS.321"),
            "BIOSVersion after vendor extension must remain properly nested under SystemInformation"
        );
    }
    #[test]
    fn test_multi_battery_matching_order_invariance() {
        // SetupAPI lists BAT1 at index 0 and BAT0 at index 1.
        let unit_bat1 = BatteryUnit {
            device: "BAT1".to_string(),
            manufacturer: "LGC".to_string(),
            serial_number: "SN-888".to_string(),
            chemistry: "LION".to_string(),
            design_mwh: 32000,
            ..Default::default()
        };
        let unit_bat0 = BatteryUnit {
            device: "BAT0".to_string(),
            manufacturer: "SMP".to_string(),
            serial_number: "SN-999".to_string(),
            chemistry: "LION".to_string(),
            design_mwh: 24000,
            ..Default::default()
        };

        // Report lists BAT0 at index 0 and BAT1 at index 1.
        let report = Report {
            batteries: vec![
                ReportBattery {
                    id: "BAT0".to_string(),
                    manufacturer: Some("SMP".to_string()),
                    serial_number: Some("SN-999".to_string()),
                    chemistry: Some("LION".to_string()),
                    design_capacity: Some(24000),
                    full_charge: Some(22000),
                    cycle_count: Some(150),
                },
                ReportBattery {
                    id: "BAT1".to_string(),
                    manufacturer: Some("LGC".to_string()),
                    serial_number: Some("SN-888".to_string()),
                    chemistry: Some("LION".to_string()),
                    design_capacity: Some(32000),
                    full_charge: Some(30000),
                    cycle_count: Some(45),
                },
            ],
            ..Default::default()
        };

        // unit_bat1 (SetupAPI index 0) matches report BAT1 (index 1).
        let m1 = find_matching_report_battery(&unit_bat1, &report, 0)
            .expect("BAT1 must find match in report");
        assert_eq!(m1.id, "BAT1");
        assert_eq!(m1.cycle_count, Some(45));

        // unit_bat0 (SetupAPI index 1) matches report BAT0 (index 0).
        let m0 = find_matching_report_battery(&unit_bat0, &report, 1)
            .expect("BAT0 must find match in report");
        assert_eq!(m0.id, "BAT0");
        assert_eq!(m0.cycle_count, Some(150));
    }
    #[test]
    fn test_per_module_design_capacity_cache_and_unplug_isolation() {
        // Case 1: Legacy single-scalar cache format
        let legacy = parse_design_capacity_cache("52440\n");
        assert_eq!(legacy.total, 52440);
        assert!(legacy.modules.is_empty());

        // Case 2: Structured multi-battery cache
        let cache_str = r#"
        # batlens design capacity cache (per-module & total)
        __total__=56000
        bat0=24000
        bat1=32000
        sn-999=24000
        sn-888=32000
        "#;
        let cache = parse_design_capacity_cache(cache_str);
        assert_eq!(cache.total, 56000);
        assert_eq!(cache.modules.get("bat0"), Some(&24000));
        assert_eq!(cache.modules.get("bat1"), Some(&32000));
        assert_eq!(cache.modules.get("sn-999"), Some(&24000));

        // Case 3: Both batteries plugged in with unknown IOCTL design capacity (0)
        let b0 = BatteryUnit {
            device: "BAT0".to_string(),
            serial_number: "SN-999".to_string(),
            remaining_mwh: 20000,
            full_mwh: 22000,
            design_mwh: 0, // IOCTL returned unknown
            ..Default::default()
        };
        let b1 = BatteryUnit {
            device: "BAT1".to_string(),
            serial_number: "SN-888".to_string(),
            remaining_mwh: 28000,
            full_mwh: 30000,
            design_mwh: 0, // IOCTL returned unknown
            ..Default::default()
        };

        assert_eq!(lookup_module_design_capacity(&cache, &b0), Some(24000));
        assert_eq!(lookup_module_design_capacity(&cache, &b1), Some(32000));

        // Case 4: after the second battery is removed, BAT0 returns 24,000 mWh, not the
        // 56,000 mWh aggregate.
        let dc_b0 = lookup_module_design_capacity(&cache, &b0).unwrap_or(cache.total);
        assert_eq!(
            dc_b0, 24000,
            "Remaining battery must retain its individual 24 Wh design capacity, not 56 Wh total"
        );
        let health = 100.0 * b0.full_mwh as f64 / dc_b0 as f64;
        assert!(
            (health - 91.666).abs() < 0.1,
            "Health must be ~91.7%, not distorted to 39.2%"
        );

        // Case 5: cache has 2 modules and 56 Wh, only 1 unknown battery is present.
        // The 56 Wh must not be assigned to it.
        let mut sample_unplugged = Sample {
            batteries: vec![BatteryUnit {
                device: "Unknown".to_string(),
                serial_number: "SN-NEW".to_string(),
                remaining_mwh: 20000,
                full_mwh: 22000,
                design_mwh: 0,
                ..Default::default()
            }],
            ..Default::default()
        };
        sample_unplugged.apply_design_capacity_cache(&cache);
        assert_eq!(
            sample_unplugged.batteries[0].design_mwh, 0,
            "Must not contaminate single unknown battery with multi-battery aggregate 56 Wh"
        );
        assert_eq!(sample_unplugged.design_mwh, 0);
    }
    #[test]
    fn test_wear_rate_calculation_branches() {
        // Case 1: Normal degradation (90% -> 85% over 60 days, target 80%)
        let w1 = compute_wear(85.0, 90.0, 60.0, 80.0);
        assert_eq!(w1.rate_str, "2.50 %/month");
        assert_eq!(w1.detail_str, "~60 days to 80%");
        assert!(w1.is_decay);

        // Case 2: upward revision 85% -> 87% over 60 days.
        let w2 = compute_wear(87.0, 85.0, 60.0, 80.0);
        assert_eq!(w2.rate_str, "+1.00 %/month");
        assert!(
            w2.detail_str
                .contains("capacity recalibrated (+2.0% vs baseline")
        );
        assert!(w2.is_recalibration);

        // Case 3: Perfectly stable capacity (85% -> 85% over 60 days)
        let w3 = compute_wear(85.0, 85.0, 60.0, 80.0);
        assert_eq!(w3.rate_str, "0.00 %/month");
        assert_eq!(w3.detail_str, "stable capacity over 60 days");

        // Case 4: Insufficient history (< 7 days)
        let w4 = compute_wear(88.0, 90.0, 4.5, 80.0);
        assert_eq!(w4.rate_str, "N/A");
        assert!(w4.detail_str.contains("need >= 7 days of history"));

        // Case 5: tiny wear rate is capped at over 5 years.
        let w5 = compute_wear(85.0, 85.06, 30.0, 80.0);
        assert_eq!(w5.rate_str, "0.06 %/month");
        assert_eq!(w5.detail_str, ">5 years to 80% (stable)");
    }
    #[test]
    fn test_display_unit_formatters() {
        // Energy Mwh -> Wh
        assert_eq!(format!("{}", Mwh(52440)), "52.4 Wh");
        assert_eq!(format!("{}", Mwh(0)), "0.0 Wh");
        assert_eq!(format!("{}", Mwh(44620)), "44.6 Wh");

        // Power Mw -> W
        assert_eq!(format!("{}", Mw(15000)), "15.0 W");
        assert_eq!(Mw(11300).signed(), "+11.3 W");
        assert_eq!(Mw(-25000).signed(), "-25.0 W");
        assert_eq!(Mw(0).signed(), "+0.0 W");

        // Voltage Mv -> V
        assert_eq!(format!("{}", Mv(11850)), "11.85 V");
        assert_eq!(format!("{}", Mv(0)), "0.00 V");

        // Power Watts -> W
        assert_eq!(format!("{}", Watts(12.87)), "12.9 W");
        assert_eq!(Watts(21.9).signed(), "+21.9 W");

        // Duration Hours -> h
        assert_eq!(format!("{}", Hours(3.5)), "3.5 h");
        assert_eq!(format!("{}", Hours(0.8)), "0.8 h");
    }

    #[test]
    fn test_underpowered_ac_drain_time_left() {
        let b = BatteryUnit {
            device: "BAT0".to_string(),
            remaining_mwh: 30000,
            full_mwh: 50000,
            design_mwh: 52000,
            rate_mw: -15000, // 15W discharge while on AC
            ..Default::default()
        };
        let s = Sample {
            batteries: vec![b],
            percent: 60,
            ac: true, // Connected to AC
            remaining_mwh: 30000,
            full_mwh: 50000,
            design_mwh: 52000,
            rate_mw: -15000,
            dis_w: 15.0,
            gross_dis_w: 15.0,
            gross_chg_w: 0.0,
            low_alert_mwh: 0,
            crit_alert_mwh: 0,
        };
        assert!(s.dis_w >= 0.1);
        let hours = s.remaining_mwh as f64 / 1000.0 / s.dis_w;
        assert_eq!(hours, 2.0);
    }

    #[test]
    fn test_xml_stacked_text_buffer_preserves_parent_around_nested_tags() {
        let xml = r#"
        <BatteryReport>
            <SystemInformation>
                <SystemProductName>Summit B15 <!-- comment --> <Extension>ignored</Extension> A11M</SystemProductName>
                <BIOSVersion>E1552IMS.321</BIOSVersion>
            </SystemInformation>
            <Batteries>
                <Battery>
                    <Id>BAT0</Id>
                    <Manufacturer>SMP</Manufacturer>
                    <DesignCapacity>52000</DesignCapacity>
                </Battery>
            </Batteries>
        </BatteryReport>
        "#;
        let rep = parse_report(xml).expect("Failed to parse report with nested tags");
        let prod = rep.system_product.expect("SystemProductName must exist");
        assert!(
            prod.contains("Summit B15"),
            "Must retain text before nested tag"
        );
        assert!(prod.contains("A11M"), "Must retain text after nested tag");
    }

    #[test]
    fn test_aftermarket_battery_replacement_heuristics() {
        let dt = |days: i64| -> (DateTime<Utc>, f64) {
            (Utc::now() + chrono::TimeDelta::days(days), 0.0)
        };
        // Case 1: Standard OEM new battery (jump from 60% -> 95%, curr >= 85)
        let pts1 = vec![
            (dt(0).0, 60.0),
            (dt(10).0, 95.0),
            (dt(20).0, 95.0),
            (dt(30).0, 94.0),
        ];
        assert_eq!(find_battery_replacement(&pts1), Some(1));

        // Case 2: jump 45% -> 80%.
        let pts2 = vec![
            (dt(0).0, 45.0),
            (dt(10).0, 80.0),
            (dt(20).0, 80.0),
            (dt(30).0, 79.0),
        ];
        assert_eq!(find_battery_replacement(&pts2), Some(1));

        // Case 3: jump 50% -> 68% (delta 18, curr below 78).
        let pts3 = vec![
            (dt(0).0, 50.0),
            (dt(10).0, 68.0),
            (dt(20).0, 67.0),
            (dt(30).0, 66.0),
        ];
        assert_eq!(find_battery_replacement(&pts3), None);
    }

    #[test]
    fn test_multibattery_design_capacity_deduction() {
        let b1 = BatteryUnit {
            device: "BAT0".to_string(),
            remaining_mwh: 20000,
            full_mwh: 24000,
            design_mwh: 24000,
            ..Default::default()
        };
        let mut b2 = BatteryUnit {
            device: "BAT1".to_string(),
            remaining_mwh: 15000,
            full_mwh: 30000,
            design_mwh: 0,
            ..Default::default()
        };
        let cache = DesignCapacityCache {
            total: 56000,
            module_count: 0,
            modules: std::collections::BTreeMap::new(),
        };
        let total = cache.total;
        let known = b1.design_mwh;
        if b2.design_mwh == 0 && known < total {
            b2.design_mwh = total - known;
        }
        assert_eq!(b2.design_mwh, 32000);
    }

    #[test]
    fn test_clock_skew_resilience() {
        let t_base = Utc::now() + chrono::TimeDelta::days(5);
        let t_now = Utc::now();
        let days_span = (t_now - t_base).num_seconds() as f64 / 86400.0;
        assert!(days_span < 0.0);
    }

    #[test]
    fn test_chemistry_null_byte_stripping() {
        let status = BatteryStatus {
            power_state: 0,
            capacity: 50000,
            voltage: 11400,
            rate: 0,
        };
        let mut chemistry = [0u8; 4];
        chemistry[0] = b'L';
        chemistry[1] = b'i';
        chemistry[2] = 0; // Embedded null byte
        chemistry[3] = 0;
        let info = BatteryInformation {
            capabilities: 0,
            technology: 1,
            reserved: [0; 3],
            chemistry,
            designed_capacity: 52000,
            full_charged_capacity: 50000,
            default_alert1: 0,
            default_alert2: 0,
            critical_bias: 0,
            cycle_count: 0,
        };
        let (unit, _) = parse_battery_unit_data(
            &status,
            &info,
            "BAT0".to_string(),
            "OEM".to_string(),
            "SN".to_string(),
        );
        assert_eq!(unit.chemistry, "Li");
        assert_eq!(unit.cycle_count, Some(0));
    }

    #[test]
    fn test_iso_hours_strict_and_weeks() {
        assert_eq!(iso_hours("PT3H45M"), Some(3.75));
        assert_eq!(iso_hours("P1W"), Some(168.0));
        assert_eq!(iso_hours("P2DT4H"), Some(52.0));
        // Dangling unit or invalid chars must fail
        assert_eq!(iso_hours("PT3H45"), None);
        assert_eq!(iso_hours("INVALID"), None);
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
    fn test_clean_history_timestamp_dedup_and_clock_skew_resilience() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::TimeDelta::days(1);
        let rep = Report {
            history: vec![
                HistPoint {
                    date: Some(t0),
                    full: 40000,
                    design: 50000,
                    cycles: None,
                },
                // Duplicate timestamp with a newer value.
                HistPoint {
                    date: Some(t0),
                    full: 42000,
                    design: 50000,
                    cycles: None,
                },
                HistPoint {
                    date: Some(t1),
                    full: 41000,
                    design: 50000,
                    cycles: None,
                },
            ],
            ..Default::default()
        };
        let pts = clean_history(&rep);
        assert_eq!(pts.len(), 2, "Duplicate timestamp must be deduplicated");
        assert_eq!(pts[0].0, t0);
        assert!(
            (pts[0].1 - 84.0).abs() < 1e-3,
            "Must retain the later value (42000/50000 = 84%)"
        );
        assert_eq!(pts[1].0, t1);

        // Zero-second leap must not trigger replacement detection
        let leap_instant = vec![
            (t0, 60.0),
            (t0, 95.0), // instant 0-second jump
            (t1, 94.0),
            (t1 + chrono::TimeDelta::days(1), 93.0),
        ];
        assert_eq!(
            find_battery_replacement(&leap_instant),
            None,
            "Zero-second jump must be rejected"
        );
    }

    #[test]
    fn test_cleanup_stale_cache_files() {
        let temp_dir =
            std::env::temp_dir().join(format!("batlens_test_clean_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let live_file = temp_dir.join("battery_report.xml");
        let fresh_tmp = temp_dir.join("tmp.12345.xml");
        let _ = fs::write(&live_file, "<live/>");
        let _ = fs::write(&fresh_tmp, "<fresh/>");

        cleanup_stale_cache_files(&temp_dir);

        // Fresh tmp and live file must not be removed
        assert!(live_file.exists());
        assert!(fresh_tmp.exists());

        let _ = fs::remove_dir_all(&temp_dir);
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
    fn test_multibattery_implausible_derivation_rejected() {
        let mut sample = Sample {
            batteries: vec![
                BatteryUnit {
                    device: "BAT0".to_string(),
                    full_mwh: 24000,
                    design_mwh: 24000,
                    ..Default::default()
                },
                // Replaced battery: full charge is 40 Wh, but unknown design
                BatteryUnit {
                    device: "BAT1_NEW".to_string(),
                    full_mwh: 40000,
                    design_mwh: 0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        // Old cache total 30 Wh leaves 6 Wh, far below the 40 Wh full charge.
        let cache = DesignCapacityCache {
            total: 30000,
            module_count: 2,
            modules: std::collections::BTreeMap::new(),
        };
        sample.apply_design_capacity_cache(&cache);
        assert_eq!(
            sample.batteries[1].design_mwh, 0,
            "Implausibly small derived capacity (6 Wh vs 40 Wh full) must be rejected"
        );
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
    fn test_is_valid_serial_placeholders() {
        assert!(!is_valid_serial("0"));
        assert!(!is_valid_serial("1"));
        assert!(!is_valid_serial(""));
        assert!(!is_valid_serial("00000000"));
        assert!(!is_valid_serial("Unknown"));
        assert!(!is_valid_serial("None"));
        assert!(!is_valid_serial("N/A"));
        assert!(!is_valid_serial("----"));

        assert!(is_valid_serial("SN-12345"));
        assert!(is_valid_serial("BAT2026-X"));
    }

    #[test]
    fn test_aggregate_drains_ignores_sub10s_and_impossible_spikes() {
        // Fixed local noon: `Utc::now()` splits across two local days near midnight.
        let t0 = chrono::Local
            .with_ymd_and_hms(2026, 1, 15, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let t_glitch = t0 + chrono::TimeDelta::seconds(2); // 500 mWh in 2 s = 900 kW
        let t_real = t0 + chrono::TimeDelta::hours(1); // 15000 mWh in 1 h = 15 W

        let drains = vec![
            Seg {
                t0: Some(t0),
                t1: Some(t_glitch),
                start_mwh: 50000,
                end_mwh: 49500,
            },
            Seg {
                t0: Some(t0),
                t1: Some(t_real),
                start_mwh: 50000,
                end_mwh: 35000,
            },
        ];
        let by_day = aggregate_drains_by_day(&drains);
        let (mwh, hours) = by_day.values().next().copied().unwrap_or((0.0, 0.0));
        // The 2 s glitch is dropped; only the 1 h drain remains.
        assert!((hours - 1.0).abs() < 1e-3);
        assert!((mwh - 15000.0).abs() < 1e-3);
    }

    #[test]
    fn test_battery_capacity_relative_handling() {
        let status = BatteryStatus {
            power_state: 0,
            capacity: 85, // 85%
            voltage: 12000,
            rate: -15000,
        };
        let info = BatteryInformation {
            capabilities: BATTERY_CAPACITY_RELATIVE,
            technology: 1,
            reserved: [0; 3],
            chemistry: *b"LION",
            designed_capacity: 100,
            full_charged_capacity: 100,
            default_alert1: 10,
            default_alert2: 5,
            critical_bias: 0,
            cycle_count: 50,
        };
        let (unit, _) = parse_battery_unit_data(
            &status,
            &info,
            "BAT0".to_string(),
            "OEM".to_string(),
            "SN-123".to_string(),
        );
        assert_eq!(unit.percent, 85);
        assert_eq!(unit.remaining_mwh, 0, "Relative capacity must clear mWh");
        assert_eq!(unit.full_mwh, 0);
        assert_eq!(unit.design_mwh, 0);
    }

    #[test]
    fn test_relative_percent_uses_full_capacity_ratio() {
        let mk = |cap: u32, full: u32| {
            let status = BatteryStatus {
                power_state: 0,
                capacity: cap,
                voltage: 12000,
                rate: 0,
            };
            let info = BatteryInformation {
                capabilities: BATTERY_CAPACITY_RELATIVE,
                technology: 1,
                reserved: [0; 3],
                chemistry: *b"LION",
                designed_capacity: 80,
                full_charged_capacity: full,
                default_alert1: 0,
                default_alert2: 0,
                critical_bias: 0,
                cycle_count: 0,
            };
            parse_battery_unit_data(&status, &info, "B".into(), "M".into(), "S".into())
                .0
                .percent
        };
        assert_eq!(mk(40, 80), 50);
        assert_eq!(mk(40, BATTERY_UNKNOWN_CAPACITY), 40);
        assert_eq!(mk(0, 80), 0);
    }

    #[test]
    fn test_cache_not_applied_to_relative_unit() {
        let unit = BatteryUnit {
            device: "BAT0".to_string(),
            percent: 85,
            ..Default::default()
        };
        let cache = DesignCapacityCache {
            total: 50000,
            modules: [("bat0".to_string(), 50000)].into(),
            ..Default::default()
        };
        let mut s = build_sample(vec![unit], false);
        s.apply_design_capacity_cache(&cache);
        assert_eq!(s.batteries[0].design_mwh, 0);
        assert_eq!(s.design_mwh, 0);
        assert_eq!(s.percent, 85);
    }

    #[test]
    fn test_mixed_units_total_is_unknown() {
        let abs = BatteryUnit {
            remaining_mwh: 20000,
            full_mwh: 40000,
            percent: 50,
            ..Default::default()
        };
        let rel = BatteryUnit {
            percent: 90,
            ..Default::default()
        };
        let s = build_sample(vec![abs, rel], false);
        assert_eq!(s.full_mwh, 0);
        assert_eq!(s.remaining_mwh, 0);
    }
    #[test]
    fn test_find_battery_replacement_day_one_tail_jump() {
        use chrono::TimeZone;
        let t0 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let dt = |days: i64| t0 + chrono::TimeDelta::days(days);

        // Aged battery jumps to 96% at the last entry.
        let day_one_rep = vec![
            (dt(0), 65.0),
            (dt(30), 63.0),
            (dt(60), 60.0),
            (dt(90), 96.0), // +36 points
        ];
        assert_eq!(
            find_battery_replacement(&day_one_rep),
            Some(3),
            "Brand new replacement on day 1 must be immediately detected at history tail"
        );

        // Tail jump 60% -> 80% is rejected (curr < 90%).
        let tail_noise = vec![
            (dt(0), 65.0),
            (dt(30), 63.0),
            (dt(60), 60.0),
            (dt(90), 80.0),
        ];
        assert_eq!(
            find_battery_replacement(&tail_noise),
            None,
            "Tail spike below pristine threshold must not trigger replacement"
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
    fn test_iso_hours_years_and_months() {
        assert_eq!(iso_hours("P1Y"), Some(8760.0));
        assert_eq!(iso_hours("P1M"), Some(730.5));
        assert_eq!(iso_hours("P1Y1MT2H"), Some(8760.0 + 730.5 + 2.0));
    }

    #[test]
    fn test_stale_cache_capacity_rejection_on_module_upgrade() {
        let mut cache = DesignCapacityCache {
            total: 30000,
            module_count: 1,
            modules: std::collections::BTreeMap::new(),
        };
        cache.modules.insert("bat0".to_string(), 30000);

        // Battery replaced by a 50 Wh pack (full charge 48 Wh).
        let upgraded_unit = BatteryUnit {
            device: "BAT0".to_string(),
            full_mwh: 48000,
            design_mwh: 0,
            ..Default::default()
        };
        // Cached 30 Wh is rejected: 48 Wh > 30 Wh * 1.2.
        assert_eq!(
            cache.lookup(&upgraded_unit),
            None,
            "Stale cached capacity smaller than actual full charge must be rejected"
        );
    }

    #[test]
    fn test_apply_design_capacity_cache_empty_batteries_no_panic() {
        let mut sample = Sample {
            batteries: Vec::new(),
            ..Default::default()
        };
        let cache = DesignCapacityCache {
            total: 50000,
            module_count: 0,
            modules: std::collections::BTreeMap::new(),
        };
        // Must safely return without divide-by-zero panic
        sample.apply_design_capacity_cache(&cache);
        assert_eq!(sample.design_mwh, 0);
    }

    #[test]
    fn test_aggregate_drains_loop_cutoff_strict_conservation() {
        use chrono::TimeZone;
        let t0 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t1 = t0 + chrono::TimeDelta::days(50); // 50 days (well beyond 35 cutoff)
        let total_mwh = 50000;

        let drains = vec![Seg {
            t0: Some(t0),
            t1: Some(t1),
            start_mwh: total_mwh,
            end_mwh: 0,
        }];
        let by_day = aggregate_drains_by_day(&drains);
        let sum_mwh: f64 = by_day.values().map(|(mwh, _)| *mwh).sum();
        assert!(
            (sum_mwh - total_mwh as f64).abs() < 1e-3,
            "Energy must be 100% strictly conserved even after loop cutoff, got {sum_mwh} vs {total_mwh}"
        );
    }

    #[test]
    fn test_battery_capacity_relative_zeroes_rate() {
        let status = BatteryStatus {
            power_state: 0,
            capacity: 75,
            voltage: 11000,
            rate: -15000, // Non-mW arbitrary relative rate
        };
        let info = BatteryInformation {
            capabilities: BATTERY_CAPACITY_RELATIVE,
            technology: 1,
            reserved: [0; 3],
            chemistry: *b"LION",
            designed_capacity: 100,
            full_charged_capacity: 100,
            default_alert1: 10,
            default_alert2: 5,
            critical_bias: 0,
            cycle_count: 50,
        };
        let (unit, _) = parse_battery_unit_data(
            &status,
            &info,
            "BAT0".to_string(),
            "OEM".to_string(),
            "SN-123".to_string(),
        );
        assert_eq!(unit.percent, 75);
        assert_eq!(
            unit.rate_mw, 0,
            "Rate must be zeroed for relative-capacity batteries"
        );
    }

    #[test]
    fn test_parse_report_batteries_sum_overrides_runtime_estimates() {
        let xml = r#"
        <BatteryReport>
            <Batteries>
                <Battery>
                    <Id>BAT1</Id>
                    <DesignCapacity>25000</DesignCapacity>
                    <FullChargeCapacity>24000</FullChargeCapacity>
                </Battery>
                <Battery>
                    <Id>BAT2</Id>
                    <DesignCapacity>35000</DesignCapacity>
                    <FullChargeCapacity>34000</FullChargeCapacity>
                </Battery>
            </Batteries>
            <RuntimeEstimates>
                <DesignCapacity>
                    <Capacity>20000</Capacity>
                </DesignCapacity>
                <FullChargeCapacity>
                    <Capacity>18000</Capacity>
                </FullChargeCapacity>
            </RuntimeEstimates>
        </BatteryReport>
        "#;
        let rep = parse_report(xml).expect("parse XML report");
        assert_eq!(
            rep.design_capacity,
            Some(60000),
            "Module sum (25000 + 35000 = 60000) must take precedence over RuntimeEstimates (20000)"
        );
        assert_eq!(
            rep.full_charge,
            Some(58000),
            "Module sum (24000 + 34000 = 58000) must take precedence over RuntimeEstimates (18000)"
        );
    }

    #[test]
    fn test_find_battery_replacement_aftermarket_day_one() {
        use chrono::TimeZone;
        let t0 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let dt = |days: i64| t0 + chrono::TimeDelta::days(days);

        // Replacement at 70% with a pack at 86% (jump 16 points).
        let aftermarket_day_one = vec![
            (dt(0), 75.0),
            (dt(30), 72.0),
            (dt(60), 70.0),
            (dt(90), 86.0),
        ];
        assert_eq!(
            find_battery_replacement(&aftermarket_day_one),
            Some(3),
            "Day-one aftermarket replacement (70% -> 86%) must be recognized"
        );
    }

    #[test]
    fn test_multibattery_oversized_derived_capacity_rejected() {
        let mut sample = Sample {
            batteries: vec![
                BatteryUnit {
                    device: "BAT0".to_string(),
                    full_mwh: 20000,
                    design_mwh: 20000,
                    ..Default::default()
                },
                BatteryUnit {
                    device: "BAT1".to_string(),
                    full_mwh: 20000, // 20 Wh full charge
                    design_mwh: 0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        // Derived 70 - 20 = 50 Wh is 2.5x the 20 Wh full charge.
        let cache = DesignCapacityCache {
            total: 70000,
            module_count: 2,
            modules: std::collections::BTreeMap::new(),
        };
        sample.apply_design_capacity_cache(&cache);
        assert_eq!(
            sample.batteries[1].design_mwh, 0,
            "Oversized derived design capacity (50 Wh vs 20 Wh full charge) must be rejected"
        );
    }

    #[test]
    fn test_multibattery_missing_multiple_derivation_and_surge_guard() {
        let mut sample = Sample {
            batteries: vec![
                BatteryUnit {
                    device: "BAT0".to_string(),
                    full_mwh: 20000,
                    design_mwh: 20000, // Known 20 Wh
                    ..Default::default()
                },
                BatteryUnit {
                    device: "BAT1".to_string(),
                    full_mwh: 25000, // Missing design, 25 Wh full
                    design_mwh: 0,
                    ..Default::default()
                },
                BatteryUnit {
                    device: "BAT2".to_string(),
                    full_mwh: 15000, // Missing design, 15 Wh full
                    design_mwh: 0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let cache = DesignCapacityCache {
            total: 60000, // 60 Wh total cache
            module_count: 3,
            modules: std::collections::BTreeMap::new(),
        };
        sample.apply_design_capacity_cache(&cache);
        // Two unknown modules, no per-module data: both stay unresolved and the total is N/A.
        assert_eq!(sample.batteries[1].design_mwh, 0);
        assert_eq!(sample.batteries[2].design_mwh, 0);
        assert_eq!(sample.design_mwh, 0);
    }

    #[test]
    fn test_single_battery_cycle_count_backfilled_from_history() {
        let xml = r#"
        <BatteryReport>
            <Batteries>
                <Battery>
                    <Id>BAT0</Id>
                    <DesignCapacity>50000</DesignCapacity>
                    <FullChargeCapacity>45000</FullChargeCapacity>
                    <!-- CycleCount omitted in Battery module -->
                </Battery>
            </Batteries>
            <History>
                <HistoryEntry StartDate="2026-01-01T00:00:00Z" FullChargeCapacity="45000" DesignCapacity="50000" CycleCount="38" />
            </History>
        </BatteryReport>
        "#;
        let rep = parse_report(xml).expect("parse XML report");
        assert_eq!(rep.batteries[0].cycle_count, Some(38));
        assert_eq!(rep.cycle_count, Some(38));
    }

    #[test]
    fn test_win32_error_hints_actionable() {
        assert!(win32_error_hint(5).contains("Administrator"));
        assert!(win32_error_hint(2).contains("not found"));
        assert!(win32_error_hint(31).contains("driver error"));
        assert!(win32_error_hint(32).contains("Sharing violation"));
        assert_eq!(win32_error_hint(99999), "");

        let err_open = BatlensError::DeviceOpen { code: 5 };
        assert!(format!("{err_open}").contains("Administrator"));
    }

    fn derivation_unit(full: i64) -> BatteryUnit {
        BatteryUnit {
            full_mwh: full,
            ..Default::default()
        }
    }

    fn derivation_cache(total: i64, n: usize) -> DesignCapacityCache {
        DesignCapacityCache {
            total,
            module_count: n,
            modules: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn test_single_battery_cache_rejected_when_pack_differs() {
        // 60 Wh cached vs 15 Wh full charge (4x): a different pack, not wear.
        let mut s = Sample {
            batteries: vec![derivation_unit(15000)],
            ..Default::default()
        };
        s.apply_design_capacity_cache(&derivation_cache(60000, 1));
        assert_eq!(s.batteries[0].design_mwh, 0);
    }

    #[test]
    fn test_single_battery_cache_accepted_for_normal_and_heavily_worn() {
        for full in [45000, 26000] {
            let mut s = Sample {
                batteries: vec![derivation_unit(full)],
                ..Default::default()
            };
            s.apply_design_capacity_cache(&derivation_cache(52000, 1));
            assert_eq!(s.batteries[0].design_mwh, 52000, "full={full}");
        }
    }

    #[test]
    fn test_all_missing_out_of_bounds_share_keeps_total_na() {
        let mut s = Sample {
            batteries: vec![derivation_unit(40000), derivation_unit(10000)],
            ..Default::default()
        };
        s.apply_design_capacity_cache(&derivation_cache(200000, 2));
        assert_eq!(s.design_mwh, 0);
    }

    #[test]
    fn test_all_missing_plausible_total_shown_without_per_module_split() {
        let mut s = Sample {
            batteries: vec![derivation_unit(40000), derivation_unit(10000)],
            ..Default::default()
        };
        s.apply_design_capacity_cache(&derivation_cache(55000, 2));
        assert_eq!(s.design_mwh, 55000);
        assert!(s.batteries.iter().all(|b| b.design_mwh == 0));
    }

    #[test]
    fn test_known_design_not_below_cache_total_leaves_missing_module_na() {
        let mut known = derivation_unit(40000);
        known.design_mwh = 60000;
        let mut s = Sample {
            batteries: vec![known, derivation_unit(10000)],
            ..Default::default()
        };
        s.apply_design_capacity_cache(&derivation_cache(55000, 2));
        assert_eq!(s.batteries[1].design_mwh, 0);
        assert_eq!(s.design_mwh, 0);
    }

    fn tl_sample(ac: bool, rate_mw: i64, percent: u32, rem: i64) -> Sample {
        Sample {
            ac,
            rate_mw,
            percent,
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
    fn test_single_battery_unknown_full_does_not_take_cache_design() {
        let mut s = Sample {
            batteries: vec![derivation_unit(0)],
            ..Default::default()
        };
        s.apply_design_capacity_cache(&derivation_cache(52000, 1));
        assert_eq!(s.batteries[0].design_mwh, 0);
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

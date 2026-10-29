use chrono::{DateTime, Local, TimeDelta, Utc};
use colored::{ColoredString, Colorize};
use unicode_width::UnicodeWidthStr;

use crate::stats::{aggregate_drains_by_day, clean_history, find_battery_replacement};
use crate::sys::{default_chart_width, terminal_columns};
use crate::types::*;
use crate::xml::run_report_xml;

pub fn health_paint(s: &str, v: f64) -> ColoredString {
    if v >= 90.0 {
        s.green()
    } else if v >= 80.0 {
        s.yellow()
    } else {
        s.red()
    }
}

/// Display width of `s`, ignoring the CSI (`ESC [ ... final`) color sequences this tool emits.
pub fn visual_width(s: &str) -> usize {
    if !s.contains('\x1b') {
        return s.width();
    }
    let mut stripped = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            let mut look = it.clone();
            if look.next() == Some('[') {
                it = look;
                for ch in it.by_ref() {
                    if ('\x40'..='\x7E').contains(&ch) {
                        break;
                    }
                }
                continue;
            }
        }
        stripped.push(c);
    }
    stripped.width()
}

const BR_BITS: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

pub fn resample_time(pts: &[(DateTime<Utc>, f64)], target: usize) -> Vec<f64> {
    if pts.is_empty() || target == 0 {
        return Vec::new();
    }
    let mut sorted_buf;
    let pts = if pts.windows(2).all(|w| w[0].0 <= w[1].0) {
        pts
    } else {
        sorted_buf = pts.to_vec();
        sorted_buf.sort_by_key(|p| p.0);
        &sorted_buf
    };
    if target == 1 || pts.len() == 1 {
        return vec![pts[0].1; target];
    }
    let t0 = pts[0].0;
    let t1 = pts[pts.len() - 1].0;
    let total_secs = (t1 - t0).num_milliseconds() as f64 / 1000.0;
    if total_secs <= 0.0 {
        return vec![pts[0].1; target];
    }
    let mut result = Vec::with_capacity(target);
    let mut k = 0usize;
    for i in 0..target {
        let t_target_ms = (total_secs * 1000.0 * i as f64 / (target - 1) as f64) as i64;
        let t_target = t0 + TimeDelta::milliseconds(t_target_ms);
        while k + 1 < pts.len() && pts[k + 1].0 <= t_target {
            k += 1;
        }
        if k + 1 >= pts.len() {
            result.push(pts[pts.len() - 1].1);
        } else {
            let seg_secs = (pts[k + 1].0 - pts[k].0).num_milliseconds() as f64 / 1000.0;
            if seg_secs <= 0.0 {
                result.push(pts[k].1);
            } else {
                let frac = (((t_target - pts[k].0).num_milliseconds() as f64 / 1000.0) / seg_secs)
                    .clamp(0.0, 1.0);
                result.push(pts[k].1 + frac * (pts[k + 1].1 - pts[k].1));
            }
        }
    }
    result
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum ColorTier {
    Green,
    Yellow,
    Red,
}

pub fn braille_rows(vals: &[f64], vmin: f64, vmax: f64, w: usize, h: usize) -> Vec<String> {
    let total_rows = h * 4;
    let range = (vmax - vmin).max(1.0);
    (0..h)
        .map(|r| {
            let mut s = String::new();
            let mut active_tier: Option<ColorTier> = None;
            for c in 0..w {
                let mut code = 0x2800u32;
                let mut cell_vals: [Option<f64>; 2] = [None; 2];
                for sc in 0..2 {
                    let idx = c * 2 + sc;
                    if idx >= vals.len() {
                        continue;
                    }
                    cell_vals[sc] = Some(vals[idx]);
                    let filled = if range <= 0.0 {
                        1
                    } else {
                        (((vals[idx] - vmin) / range).clamp(0.0, 1.0) * (total_rows - 1) as f64)
                            .round() as usize
                            + 1
                    };
                    for (sr, bit) in BR_BITS[sc].iter().enumerate() {
                        let dot_row_from_bottom = total_rows - 1 - (r * 4 + sr);
                        if dot_row_from_bottom < filled {
                            code |= bit;
                        }
                    }
                }
                if code == 0x2800 {
                    if active_tier.is_some() {
                        s.push_str("\x1b[0m");
                        active_tier = None;
                    }
                    s.push(' ');
                } else {
                    let cell_v = match (cell_vals[0], cell_vals[1]) {
                        (Some(a), Some(b)) => (a + b) / 2.0,
                        (Some(a), None) => a,
                        (None, Some(b)) => b,
                        (None, None) => vmin,
                    };
                    let tier = if cell_v >= 90.0 {
                        ColorTier::Green
                    } else if cell_v >= 80.0 {
                        ColorTier::Yellow
                    } else {
                        ColorTier::Red
                    };
                    if active_tier != Some(tier) {
                        match tier {
                            ColorTier::Green => s.push_str("\x1b[32m"),
                            ColorTier::Yellow => s.push_str("\x1b[33m"),
                            ColorTier::Red => s.push_str("\x1b[31m"),
                        }
                        active_tier = Some(tier);
                    }
                    let ch = char::from_u32(code).unwrap();
                    s.push(ch);
                }
            }
            if active_tier.is_some() {
                s.push_str("\x1b[0m");
            }
            s
        })
        .collect()
}

pub fn cmd_chart(
    drain: bool,
    ascii: bool,
    braille: bool,
    current: bool,
    width: Option<usize>,
) -> Result<(), BatlensError> {
    let r = run_report_xml()?;
    if drain {
        return chart_drain(&r);
    }
    let mut pts = clean_history(&r);
    if current && let Some(i) = find_battery_replacement(&pts) {
        pts = pts.split_off(i);
    }
    if pts.len() < 2 {
        println!("Not enough health history in the Windows report.");
        return Ok(());
    }
    let use_ascii = ascii && !braille;
    let w = width.unwrap_or_else(default_chart_width);
    if w < 4 {
        let last_health = pts.last().map(|p| p.1).unwrap_or(0.0);
        println!(
            "Terminal too narrow for 2D chart ({} cols). Current health: {:.1}%.",
            terminal_columns(),
            last_health
        );
        return Ok(());
    }
    let h = 11usize;
    let n = pts.len();
    let (t0, t1) = (pts[0].0, pts[n - 1].0);
    if !use_ascii {
        let resampled = resample_time(&pts, w * 2);
        let mut vmin = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let mut vmax = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        if vmax - vmin < 4.0 {
            let mid = (vmax + vmin) / 2.0;
            let mut lo = (mid - 2.0).clamp(0.0, 100.0);
            let mut hi = (mid + 2.0).clamp(0.0, 100.0);
            if hi - lo < 4.0 {
                if lo <= 0.0 {
                    hi = (lo + 4.0).min(100.0);
                } else if hi >= 100.0 {
                    lo = (hi - 4.0).max(0.0);
                }
            }
            vmin = lo;
            vmax = hi;
        }
        let rows = braille_rows(&resampled, vmin, vmax, w, h);
        let nlab = (h / 3).clamp(2, 6);
        let mut label_at: Vec<Option<f64>> = vec![None; h];
        for i in 0..nlab {
            let r = ((i as f64 * (h - 1) as f64 / (nlab - 1) as f64).round() as usize).min(h - 1);
            label_at[r] = Some(vmax - i as f64 * (vmax - vmin) / (nlab - 1) as f64);
        }
        println!("Battery health (full charge / design capacity)");
        for r in 0..h {
            match label_at[r] {
                Some(v) => println!(" {:>3.0}% │{}", v, rows[r]),
                None => println!("      │{}", rows[r]),
            }
        }
        let ntxt = (w / 14).max(2);
        let span = (t1 - t0).num_seconds() as f64;
        let mut ruler = vec!['─'; w];
        let mut lrow = vec![' '; w];
        let mut free = 0usize;
        for i in 0..ntxt {
            let pos = ((i as f64 * (w - 1) as f64) / (ntxt - 1) as f64).round() as usize;
            ruler[pos] = '┼';
            let t = t0 + TimeDelta::seconds((span * i as f64 / (ntxt - 1) as f64) as i64);
            let s = t.with_timezone(&Local).format("%Y-%m-%d").to_string();
            let s_w = s.width();
            let half_w = s_w / 2;
            let start = pos.saturating_sub(half_w).min(w.saturating_sub(s_w));
            if start >= free {
                for (j, ch) in s.chars().enumerate() {
                    if start + j < w {
                        lrow[start + j] = ch;
                    }
                }
                free = start + s_w + 2;
            }
        }
        println!("      └{}", ruler.iter().collect::<String>());
        println!("       {}", lrow.iter().collect::<String>());
        return Ok(());
    }
    let raw_min = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let raw_max = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let (adj_min, adj_max) = if raw_max - raw_min < 4.0 {
        let mid = (raw_max + raw_min) / 2.0;
        let mut lo = (mid - 2.0).clamp(0.0, 100.0);
        let mut hi = (mid + 2.0).clamp(0.0, 100.0);
        if hi - lo < 4.0 {
            if lo <= 0.0 {
                hi = (lo + 4.0).min(100.0);
            } else if hi >= 100.0 {
                lo = (hi - 4.0).max(0.0);
            }
        }
        (lo, hi)
    } else {
        (raw_min, raw_max)
    };
    let min = (adj_min.floor() - 1.0).max(0.0);
    let max = (adj_max.ceil() + 1.0).min(100.0);
    let mut canvas: Vec<Vec<Option<f64>>> = vec![vec![None; w]; h];
    let resampled = resample_time(&pts, w);
    for (col, &v) in resampled.iter().enumerate() {
        let target_row = ((max - v) / (max - min) * (h - 1) as f64).round() as usize;
        for rline in &mut canvas[target_row.min(h - 1)..h] {
            rline[col] = Some(v);
        }
    }
    println!("Battery health (full charge / design capacity)");
    for (row, rline) in canvas.iter().enumerate() {
        let line: String = rline
            .iter()
            .map(|c| match c {
                Some(v) => health_paint("█", *v).to_string(),
                None => " ".to_string(),
            })
            .collect();
        if row == 0 {
            println!(" {:>3.0}% ┤{line}", max);
        } else if row == h - 1 {
            println!(" {:>3.0}% ┤{line}", min);
        } else {
            println!("      ┤{line}");
        }
    }
    let ntxt = (w / 14).max(2);
    let span = (t1 - t0).num_seconds() as f64;
    let mut ruler = vec!['─'; w];
    let mut lrow = vec![' '; w];
    let mut free = 0usize;
    for i in 0..ntxt {
        let pos = ((i as f64 * (w - 1) as f64) / (ntxt - 1) as f64).round() as usize;
        ruler[pos] = '┴';
        let t = t0 + TimeDelta::seconds((span * i as f64 / (ntxt - 1) as f64) as i64);
        let s = t.with_timezone(&Local).format("%Y-%m-%d").to_string();
        let s_w = s.width();
        let half_w = s_w / 2;
        let start = pos.saturating_sub(half_w).min(w.saturating_sub(s_w));
        if start >= free {
            for (j, ch) in s.chars().enumerate() {
                if start + j < w {
                    lrow[start + j] = ch;
                }
            }
            free = start + s_w + 2;
        }
    }
    println!("      └{}", ruler.iter().collect::<String>());
    println!("       {}", lrow.iter().collect::<String>());
    Ok(())
}

/// Below this width the drain chart prints one short line per day. A bar row needs at least
/// 28 columns: 15 date, 3 bar, 2 gap, 7 value, 1 spare.
const NARROW_COLS: usize = 28;

pub fn chart_drain(r: &Report) -> Result<(), BatlensError> {
    let by_day = aggregate_drains_by_day(&r.drains);
    let mut days: Vec<(chrono::NaiveDate, f64)> = by_day
        .into_iter()
        .filter(|(_, (_, hh))| *hh > 0.0)
        .map(|(d, (mwh, hh))| (d, mwh / hh / 1000.0))
        .collect();
    days.sort_by_key(|a| a.0);
    if days.is_empty() {
        println!("No drain segments in the Windows report.");
        return Ok(());
    }
    let days = &days[days.len().saturating_sub(7)..];
    let max_w = days.iter().map(|d| d.1).fold(0.0f64, f64::max).max(1.0);
    let term_w = terminal_columns();
    println!("Daily avg discharge during drain segments (last 7 days)\n");

    if term_w < NARROW_COLS {
        // Narrow terminal: one short line per day, no bars.
        for (d, w) in days.iter().rev() {
            let val_str = format!("{w:.1} W");
            let colored_val = if *w > 25.0 {
                val_str.red()
            } else if *w > 12.0 {
                val_str.yellow()
            } else {
                val_str.green()
            };
            println!("{} │ {colored_val}", d.format("%m-%d"));
        }
        return Ok(());
    }

    let bar_avail = term_w.saturating_sub(NARROW_COLS);
    let max_bar = bar_avail.clamp(3, 80);

    for (d, w) in days.iter().rev() {
        let norm = if max_w > 0.0 {
            (w / max_w).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let bar = ((norm * max_bar as f64).round() as usize).min(max_bar);
        let barstr = "█".repeat(bar);
        let barstr = if *w > 25.0 {
            barstr.red()
        } else if *w > 12.0 {
            barstr.yellow()
        } else {
            barstr.green()
        };
        println!(
            "  {} │ {barstr}{}  {w:.1} W",
            d,
            " ".repeat(max_bar.saturating_sub(bar))
        );
    }
    Ok(())
}

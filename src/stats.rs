use chrono::{DateTime, Local, NaiveDate, Utc};
use std::collections::BTreeMap;

use crate::types::*;

pub fn clean_history(r: &Report) -> Vec<(DateTime<Utc>, f64)> {
    let mut pts: Vec<(DateTime<Utc>, f64)> = r
        .history
        .iter()
        .filter(|h| h.date.is_some() && h.design > 0 && h.full > 0)
        .map(|h| (h.date.unwrap(), 100.0 * h.full as f64 / h.design as f64))
        .collect();
    pts.sort_by_key(|a| a.0);
    // Keep the last value for duplicate timestamps.
    let mut deduped: Vec<(DateTime<Utc>, f64)> = Vec::with_capacity(pts.len());
    for p in pts {
        if deduped.last().is_some_and(|l| l.0 == p.0) {
            deduped.last_mut().unwrap().1 = p.1;
        } else {
            deduped.push(p);
        }
    }
    deduped
}

pub fn find_battery_replacement(pts: &[(DateTime<Utc>, f64)]) -> Option<usize> {
    if pts.len() < 2 {
        return None;
    }
    let mut candidates = Vec::new();
    for i in 1..pts.len() {
        let prev_pt = &pts[i - 1];
        let curr_pt = &pts[i];
        let prev = prev_pt.1;
        let curr = curr_pt.1;
        let delta = curr - prev;

        // Skip timestamps that do not advance.
        if (curr_pt.0 - prev_pt.0).num_seconds() <= 0 {
            continue;
        }
        // Jump of at least 15 points.
        if delta < 15.0 {
            continue;
        }

        // Level after the jump: at least 85%, or a jump of 22 points to at least 78%.
        let is_replacement_level = curr >= 85.0 || (delta >= 22.0 && curr >= 78.0);
        if !is_replacement_level {
            continue;
        }

        // Confirm the jump with up to 3 following samples.
        let next_pts = &pts[i + 1..pts.len().min(i + 4)];
        if next_pts.is_empty() {
            // Last sample in the history; nothing to confirm it, so use stricter thresholds.
            if (curr >= 85.0 && delta >= 15.0) || (delta >= 25.0 && curr >= 80.0) {
                candidates.push(i);
            }
            continue;
        }
        // Reject if a following sample falls back to the worn level.
        if next_pts.iter().any(|p| p.1 <= prev + 5.0 && p.1 < 75.0) {
            continue;
        }
        // Following samples must stay high on average.
        let avg = next_pts.iter().map(|p| p.1).sum::<f64>() / next_pts.len() as f64;
        if avg < 75.0 || avg < prev + 8.0 {
            continue;
        }
        candidates.push(i);
    }

    if candidates.is_empty() {
        return None;
    }

    // Candidates at most 4 steps apart form one cluster; use the first of the last cluster.
    let mut start = candidates.len() - 1;
    while start > 0 && candidates[start] - candidates[start - 1] <= 4 {
        start -= 1;
    }
    Some(candidates[start])
}

pub fn aggregate_drains_by_day(drains: &[Seg]) -> BTreeMap<NaiveDate, (f64, f64)> {
    let mut by_day: BTreeMap<NaiveDate, (f64, f64)> = BTreeMap::new();
    for s in drains {
        let (t0, t1) = match (s.t0, s.t1) {
            (Some(a), Some(b)) => (a, b),
            _ => continue,
        };
        let sec = (t1 - t0).num_seconds();
        let dh = sec as f64 / 3600.0;
        let dm = (s.start_mwh - s.end_mwh) as f64;
        // Skip zero or negative duration, no drain, spans under 10 s, and rates over 300 W.
        if dh <= 0.0 || dm <= 0.0 || sec < 10 || (dm / (dh * 1000.0)) > 300.0 {
            continue;
        }
        let t0_loc = t0.with_timezone(&Local);
        let t1_loc = t1.with_timezone(&Local);
        if t0_loc.date_naive() == t1_loc.date_naive() {
            let day = t0_loc.date_naive();
            let e = by_day.entry(day).or_insert((0.0, 0.0));
            e.0 += dm;
            e.1 += dh;
        } else {
            let mut cur = t0_loc;
            let mut accumulated_dm = 0.0f64;
            let mut accumulated_dh = 0.0f64;
            let mut loop_count = 0usize;
            while cur < t1_loc && loop_count < 366 {
                loop_count += 1;
                let day = cur.date_naive();
                let next_day = match day.succ_opt() {
                    Some(d) => d,
                    None => break,
                };
                let next_midnight = next_day
                    .and_hms_opt(0, 0, 0)
                    .and_then(|naive| naive.and_local_timezone(Local).earliest())
                    .or_else(|| {
                        (1..=4).find_map(|h| {
                            next_day
                                .and_hms_opt(h, 0, 0)
                                .and_then(|naive| naive.and_local_timezone(Local).earliest())
                        })
                    })
                    .unwrap_or_else(|| cur + chrono::TimeDelta::days(1));
                let is_last = next_midnight >= t1_loc;
                let sub_end = if next_midnight > cur {
                    t1_loc.min(next_midnight)
                } else {
                    t1_loc
                };
                let sub_dh = if is_last {
                    (dh - accumulated_dh).max(0.0)
                } else {
                    (sub_end - cur).num_seconds() as f64 / 3600.0
                };
                if sub_dh > 0.0 {
                    let sub_dm = if is_last {
                        (dm - accumulated_dm).max(0.0)
                    } else {
                        dm * (sub_dh / dh)
                    };
                    accumulated_dm += sub_dm;
                    accumulated_dh += sub_dh;
                    let e = by_day.entry(day).or_insert((0.0, 0.0));
                    e.0 += sub_dm;
                    e.1 += sub_dh;
                }
                if sub_end <= cur {
                    break;
                }
                cur = sub_end;
            }
            // Remainder goes to the last day so total energy is kept.
            let rem_dm = (dm - accumulated_dm).max(0.0);
            let rem_dh = (dh - accumulated_dh).max(0.0);
            if rem_dm > 0.0 || rem_dh > 0.0 {
                let e = by_day.entry(cur.date_naive()).or_insert((0.0, 0.0));
                e.0 += rem_dm;
                e.1 += rem_dh;
            }
        }
    }
    by_day
}

#[derive(Clone, Debug, PartialEq)]
pub struct WearEstimate {
    pub rate_str: String,
    pub detail_str: String,
    pub is_decay: bool,
    pub is_recalibration: bool,
}

pub fn compute_wear(now: f64, h_base: f64, days_span: f64, target: f64) -> WearEstimate {
    if days_span < 0.0 {
        return WearEstimate {
            rate_str: "N/A".to_string(),
            detail_str: "system clock skew detected (baseline timestamp is in the future)"
                .to_string(),
            is_decay: false,
            is_recalibration: false,
        };
    }
    if days_span < 7.0 {
        return WearEstimate {
            rate_str: "N/A".to_string(),
            detail_str: format!(
                "need >= 7 days of history for trend (currently {:.1} days)",
                days_span
            ),
            is_decay: false,
            is_recalibration: false,
        };
    }
    if now < h_base {
        let pm = (h_base - now) / days_span * 30.0;
        let rate_str = format!("{pm:.2} %/month");
        let detail_str = if now <= target {
            format!("already below target {:.0}%", target)
        } else if pm >= 0.05 {
            let days = (now - target) / (pm / 30.0);
            if days > 1825.0 {
                format!(">5 years to {:.0}% (stable)", target)
            } else {
                format!("~{:.0} days to {:.0}%", days, target)
            }
        } else {
            "stable decay rate".to_string()
        };
        WearEstimate {
            rate_str,
            detail_str,
            is_decay: true,
            is_recalibration: false,
        }
    } else if now > h_base {
        let gain_pm = (now - h_base) / days_span * 30.0;
        WearEstimate {
            rate_str: format!("+{gain_pm:.2} %/month"),
            detail_str: format!(
                "capacity recalibrated (+{:.1}% vs baseline over {:.0} days)",
                now - h_base,
                days_span
            ),
            is_decay: false,
            is_recalibration: true,
        }
    } else {
        WearEstimate {
            rate_str: "0.00 %/month".to_string(),
            detail_str: format!("stable capacity over {:.0} days", days_span),
            is_decay: false,
            is_recalibration: false,
        }
    }
}

//! When the disk fills, from the daemon's samples of available space as Finder counts it,
//! the figure the Disk view shows. Nothing is said until the samples span 48 hours, nor when
//! the newest is over a day old, and the trend is a Theil–Sen slope: the median of the slopes
//! between every pair of samples, so one big download or a cleanup does not swing it. The
//! days left count from now, not from the last sample.

use serde::{Deserialize, Serialize};

use crate::model::DiskSample;

/// How much history a forecast needs.
pub const NEEDS_MS: i64 = 48 * 3600 * 1000;
/// How far back the trend looks.
pub const WINDOW_MS: i64 = 14 * 86_400 * 1000;
/// A newest sample older than this means the recorder stopped; a trend from then says
/// nothing about now.
pub const STALE_MS: i64 = 24 * 3600 * 1000;
/// Further out than this reads as not filling.
const HORIZON_DAYS: f64 = 365.0;
/// Pairs grow with the square of the samples, so long histories are thinned to this many.
const MAX_POINTS: usize = 400;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Forecast {
    /// The recorder has written nothing for this volume.
    NoSamples,
    /// The newest sample is more than a day old.
    Stale { hours_ago: f64 },
    /// Under 48 hours of samples.
    Warming { have_hours: f64, need_hours: f64 },
    /// Free space is flat or growing, or would last past a year.
    NotFilling { per_day: i64, over_days: f64 },
    FullIn {
        days: f64,
        /// Change in available bytes per day; negative.
        per_day: i64,
        over_days: f64,
    },
}

impl Forecast {
    /// Whole days until full, for the header and the Overview.
    pub fn full_in_days(&self) -> Option<u32> {
        match self {
            Forecast::FullIn { days, .. } => Some(days.ceil().max(0.0) as u32),
            _ => None,
        }
    }

    pub fn sentence(&self) -> String {
        match self {
            Forecast::NoSamples => "No disk history yet. A forecast needs 48 h of samples from \
                                    the background recorder; start it with scry daemon install."
                .into(),
            Forecast::Stale { hours_ago } => format!(
                "No forecast: the newest sample is {} old, so the recorder has stopped; \
                 scry daemon status says why.",
                ago(*hours_ago)
            ),
            Forecast::Warming {
                have_hours,
                need_hours,
            } => format!(
                "A forecast needs 48 h of samples; the recorder has {}, so one appears in about {}.",
                hours(*have_hours),
                hours(need_hours - have_hours)
            ),
            Forecast::NotFilling { per_day, over_days } => format!(
                "Not filling: over the last {} available space {} {} a day.",
                days(*over_days),
                if *per_day >= 0 { "grew" } else { "shrank" },
                crate::view::worktrees::bytes(per_day.unsigned_abs())
            ),
            Forecast::FullIn {
                days: d,
                per_day,
                over_days,
            } => format!(
                "Full in ~{} at the pace of the last {}: {} less available a day.",
                days(*d),
                days(*over_days),
                crate::view::worktrees::bytes(per_day.unsigned_abs())
            ),
        }
    }
}

fn hours(h: f64) -> String {
    let h = h.max(0.0);
    if h < 1.0 {
        "under an hour".into()
    } else {
        format!("{:.0} h", h.round())
    }
}

fn ago(h: f64) -> String {
    if h < 48.0 {
        format!("{:.0} h", h.round())
    } else {
        days(h / 24.0)
    }
}

fn days(d: f64) -> String {
    match d.round() as i64 {
        n if n <= 1 => "1 day".into(),
        n => format!("{n} days"),
    }
}

/// The forecast from `samples` of one volume, oldest first, as of `now_ms`. Only samples
/// that hold Finder's available space count.
pub fn forecast(samples: &[DiskSample], now_ms: i64) -> Forecast {
    let in_window: Vec<&DiskSample> = samples
        .iter()
        .filter(|s| s.at_ms >= now_ms - WINDOW_MS && s.at_ms <= now_ms)
        .collect();
    let recent: Vec<(i64, u64)> = in_window
        .iter()
        .filter_map(|s| Some((s.at_ms, s.important_bytes?)))
        .collect();
    let Some(newest) = in_window.last() else {
        return Forecast::NoSamples;
    };
    if now_ms - newest.at_ms > STALE_MS {
        return Forecast::Stale {
            hours_ago: (now_ms - newest.at_ms) as f64 / 3_600_000.0,
        };
    }
    let (Some(first), Some(last)) = (recent.first(), recent.last()) else {
        return Forecast::Warming {
            have_hours: 0.0,
            need_hours: NEEDS_MS as f64 / 3_600_000.0,
        };
    };
    let span = last.0 - first.0;
    if span < NEEDS_MS || recent.len() < 3 {
        return Forecast::Warming {
            have_hours: span as f64 / 3_600_000.0,
            need_hours: NEEDS_MS as f64 / 3_600_000.0,
        };
    }
    let step = recent.len().div_ceil(MAX_POINTS);
    let points: Vec<(f64, f64)> = recent
        .iter()
        .step_by(step)
        .map(|&(at, avail)| (at as f64 / 86_400_000.0, avail as f64))
        .collect();
    let slope = theil_sen(&points);
    let over_days = span as f64 / 86_400_000.0;
    let per_day = slope.round() as i64;
    if slope >= 0.0 {
        return Forecast::NotFilling { per_day, over_days };
    }
    // Days from the last sample at its pace, less the time since it was taken.
    let since_last = (now_ms - last.0) as f64 / 86_400_000.0;
    let days = (last.1 as f64 / -slope - since_last).max(0.0);
    if days > HORIZON_DAYS {
        return Forecast::NotFilling { per_day, over_days };
    }
    Forecast::FullIn {
        days,
        per_day,
        over_days,
    }
}

/// The median slope between every pair of points with different x.
pub fn theil_sen(points: &[(f64, f64)]) -> f64 {
    let mut slopes = Vec::with_capacity(points.len() * points.len() / 2);
    for (i, a) in points.iter().enumerate() {
        for b in &points[i + 1..] {
            let dx = b.0 - a.0;
            if dx != 0.0 {
                slopes.push((b.1 - a.1) / dx);
            }
        }
    }
    if slopes.is_empty() {
        return 0.0;
    }
    slopes.sort_by(f64::total_cmp);
    let m = slopes.len() / 2;
    if slopes.len() % 2 == 1 {
        slopes[m]
    } else {
        (slopes[m - 1] + slopes[m]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;
    const HOUR: i64 = 3_600_000;

    /// One sample every 10 minutes for `hours`, starting with `start` bytes available and
    /// changing by `per_day`, ending at `now`.
    fn series(now: i64, hours: i64, start: u64, per_day: i64) -> Vec<DiskSample> {
        let n = hours * 6;
        (0..=n)
            .map(|i| {
                let at = now - (n - i) * 10 * 60 * 1000;
                let avail = start as i64 + per_day * i / (6 * 24);
                DiskSample {
                    at_ms: at,
                    volume: "/".into(),
                    total_bytes: 1000 * GB,
                    free_bytes: avail as u64,
                    avail_bytes: avail as u64,
                    important_bytes: Some(avail as u64),
                }
            })
            .collect()
    }

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn says_nothing_before_48_hours() {
        assert_eq!(forecast(&[], NOW), Forecast::NoSamples);
        let f = forecast(&series(NOW, 47, 100 * GB, -(GB as i64)), NOW);
        let Forecast::Warming {
            have_hours,
            need_hours,
        } = f
        else {
            panic!("{f:?}")
        };
        assert_eq!(need_hours, 48.0);
        assert!((have_hours - 47.0).abs() < 0.01);
        assert!(f.sentence().contains("about 1 h"), "{}", f.sentence());
    }

    #[test]
    fn a_steady_loss_gives_days_until_full() {
        let f = forecast(&series(NOW, 72, 100 * GB, -2 * GB as i64), NOW);
        let Forecast::FullIn { days, per_day, .. } = f else {
            panic!("{f:?}")
        };
        // 100 GB minus 3 days at 2 GB a day leaves 94 GB, 47 days of room.
        assert!((days - 47.0).abs() < 0.5, "{days}");
        assert!(
            (per_day + 2 * GB as i64).abs() < (GB / 100) as i64,
            "{per_day}"
        );
        assert_eq!(f.full_in_days(), Some(47));
        assert!(
            f.sentence().starts_with("Full in ~47 days"),
            "{}",
            f.sentence()
        );
    }

    #[test]
    fn growth_or_a_slow_trickle_is_not_filling() {
        let f = forecast(&series(NOW, 72, 100 * GB, GB as i64), NOW);
        assert!(
            matches!(f, Forecast::NotFilling { per_day, .. } if per_day > 0),
            "{f:?}"
        );
        // 1 MB a day against 100 GB is centuries away.
        let f = forecast(&series(NOW, 72, 100 * GB, -(1 << 20)), NOW);
        assert!(matches!(f, Forecast::NotFilling { .. }), "{f:?}");
        assert!(f.sentence().starts_with("Not filling"), "{}", f.sentence());
    }

    #[test]
    fn one_big_download_and_its_cleanup_do_not_swing_the_slope() {
        let mut s = series(NOW, 96, 200 * GB, -(GB as i64));
        // A 40 GB download that sat for 6 hours, then went away.
        let n = s.len();
        for x in &mut s[n / 2..n / 2 + 36] {
            x.avail_bytes -= 40 * GB;
        }
        let Forecast::FullIn { per_day, .. } = forecast(&s, NOW) else {
            panic!()
        };
        assert!(
            (per_day + GB as i64).abs() < (GB / 10) as i64,
            "the median ignores the spike: {per_day}"
        );
        // An ordinary least-squares line through the same points would not.
        let pts: Vec<(f64, f64)> = s
            .iter()
            .map(|x| (x.at_ms as f64 / 86_400_000.0, x.avail_bytes as f64))
            .collect();
        assert!((theil_sen(&pts) + GB as f64).abs() < GB as f64 / 10.0);
    }

    #[test]
    fn only_the_last_two_weeks_count_and_long_histories_are_thinned() {
        // A month: losing 5 GB a day until two weeks ago, flat since.
        let mut s = series(NOW - 14 * 24 * HOUR, 16 * 24, 300 * GB, -5 * GB as i64);
        let flat = series(NOW, 14 * 24, s.last().unwrap().avail_bytes, 0);
        s.extend(flat);
        let f = forecast(&s, NOW);
        assert!(
            matches!(f, Forecast::NotFilling { per_day: 0, .. }),
            "{f:?}"
        );
    }

    #[test]
    fn the_countdown_runs_from_now_not_from_the_last_sample() {
        let at = |hours_ago: i64, gb: u64| DiskSample {
            at_ms: NOW - hours_ago * HOUR,
            volume: "/".into(),
            total_bytes: 1000 * GB,
            free_bytes: gb * GB,
            avail_bytes: gb * GB,
            important_bytes: Some(gb * GB),
        };
        // 70, 60 and 50 GB seven, six and five days ago, and nothing since: no forecast.
        let mut s = vec![at(7 * 24, 70), at(6 * 24, 60), at(5 * 24, 50)];
        assert!(matches!(forecast(&s, NOW), Forecast::Stale { .. }));
        // The same pace, last sampled twenty hours ago with 8 GB left: from that sample it
        // would read as most of a day, but those twenty hours have passed.
        s.push(at(20, 8));
        let f = forecast(&s, NOW);
        let Forecast::FullIn { days, .. } = f else {
            panic!("{f:?}")
        };
        assert!(days < 0.1, "{days}");
    }

    #[test]
    fn a_stopped_recorder_gives_no_forecast() {
        let s = series(NOW - 3 * 24 * HOUR, 72, 100 * GB, -2 * GB as i64);
        let f = forecast(&s, NOW);
        assert!(
            matches!(f, Forecast::Stale { hours_ago } if (hours_ago - 72.0).abs() < 0.1),
            "{f:?}"
        );
        assert!(f.sentence().contains("3 days old"), "{}", f.sentence());
        assert_eq!(f.full_in_days(), None);
    }

    #[test]
    fn the_forecast_follows_finders_available_not_statfs() {
        // statfs's figure is flat, but Finder's available falls 2 GB a day: purgeable space
        // is being used up.
        let mut s = series(NOW, 72, 100 * GB, -2 * GB as i64);
        for x in &mut s {
            x.avail_bytes = 20 * GB;
            x.free_bytes = 20 * GB;
        }
        assert!(matches!(forecast(&s, NOW), Forecast::FullIn { .. }));
        // Samples from before Scry kept Finder's figure do not count.
        for x in &mut s {
            x.important_bytes = None;
        }
        assert!(matches!(forecast(&s, NOW), Forecast::Warming { .. }));
    }

    #[test]
    fn theil_sen_of_a_line_is_its_slope() {
        let pts: Vec<(f64, f64)> = (0..10).map(|i| (i as f64, 3.0 * i as f64 + 1.0)).collect();
        assert_eq!(theil_sen(&pts), 3.0);
        assert_eq!(theil_sen(&[(1.0, 5.0)]), 0.0);
    }
}

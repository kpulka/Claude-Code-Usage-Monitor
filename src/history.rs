//! Usage samples per provider window: thirty-one days hourly, and a finer
//! fifteen-minute ring for the fast session window.
//!
//! Every successful poll folds its figures into a small ring buffer keyed by
//! the same series names alerts use (`claude:session`, `antigravity#external:weekly`,
//! ...). One bucket per hour keeps the highest share seen in that hour, so a
//! five-minute poll interval never grows the file and short spikes survive.
//! Session windows also get a fifteen-minute ring for the last day, because a
//! five-hour window only has a handful of hourly points.
//!
//! The data is persisted next to the usage cache and exposed to themes as a
//! comma-separated list of points per window, which a
//! [`crate::theme_engine::SceneContent::Sparkline`] can draw.

use std::collections::BTreeMap;
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::models::{AppUsageData, UsageData};
use crate::providers::ProviderId;

/// How long a sample stays in the file. A 30-day window needs the whole
/// month, so the hourly ring keeps a little more than that.
pub const RETENTION_HOURS: u64 = 31 * 24;

/// How many hourly points a theme binding carries (the sparkline window).
pub const BINDING_POINTS: usize = 24;

/// How many points the normalized per-window cycle chart samples.
pub const CYCLE_POINTS: usize = 48;

/// A fifteen-minute bucket is `900` seconds wide.
pub const QUICK_SLOT_SECS: u64 = 15 * 60;

/// The session window is short and changes fast, so it keeps a finer ring:
/// one quarter-hour sample for the last day.
pub const QUICK_RETENTION_SLOTS: u64 = 24 * 60 * 60 / QUICK_SLOT_SECS;

/// Hard cap on tracked series so a misconfigured provider cannot bloat the file.
pub const MAX_SERIES: usize = 128;

const HISTORY_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryPoint {
    /// Hours since the Unix epoch; each bucket holds exactly one of these.
    pub hour: u64,
    /// Highest share of the window spent inside that hour, 0 to 100.
    pub percent: f64,
}

/// A fifteen-minute bucket for the fast-moving session window.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuickPoint {
    /// Quarter-hours since the Unix epoch.
    pub quarter: u64,
    /// Highest share of the window spent inside that quarter-hour, 0 to 100.
    pub percent: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct History {
    #[serde(default = "history_version")]
    pub version: u32,
    /// Series key (`<provider_key>:<window>`) to its hourly buckets, oldest first.
    #[serde(default)]
    pub series: BTreeMap<String, Vec<HistoryPoint>>,
    /// Session windows only: fifteen-minute buckets for the last day. Older
    /// files simply lack this map and fall back to the hourly ring.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub quick: BTreeMap<String, Vec<QuickPoint>>,
}

fn history_version() -> u32 {
    HISTORY_VERSION
}

impl Default for History {
    fn default() -> Self {
        Self {
            version: HISTORY_VERSION,
            series: BTreeMap::new(),
            quick: BTreeMap::new(),
        }
    }
}

impl History {
    pub fn key(provider_key: &str, window: &str) -> String {
        format!("{provider_key}:{window}")
    }

    /// Fold one poll's samples into the current hour's buckets.
    pub fn fold(&mut self, samples: &[(String, f64)], hour: u64) {
        for (key, percent) in samples {
            if !percent.is_finite() {
                continue;
            }
            let percent = percent.clamp(0.0, 100.0);
            let buckets = self.series.entry(key.clone()).or_default();
            match buckets.binary_search_by_key(&hour, |point| point.hour) {
                Ok(index) => buckets[index].percent = buckets[index].percent.max(percent),
                Err(index) => buckets.insert(index, HistoryPoint { hour, percent }),
            }
            // Retention is measured from the newest sample of this series, so a
            // clock that jumps backwards cannot silently wipe the week.
            let newest = buckets.last().map_or(hour, |point| point.hour.max(hour));
            buckets.retain(|point| point.hour + RETENTION_HOURS > newest);
            if buckets.is_empty() {
                self.series.remove(key);
            }
        }
        self.enforce_series_cap();
    }

    /// Fold one poll's session samples into the fifteen-minute ring. The
    /// quarter-hour bucket keeps the highest share, exactly like the hourly one.
    pub fn fold_quick(&mut self, samples: &[(String, f64)], quarter: u64) {
        for (key, percent) in samples {
            if !percent.is_finite() {
                continue;
            }
            let percent = percent.clamp(0.0, 100.0);
            let buckets = self.quick.entry(key.clone()).or_default();
            match buckets.binary_search_by_key(&quarter, |point| point.quarter) {
                Ok(index) => buckets[index].percent = buckets[index].percent.max(percent),
                Err(index) => buckets.insert(index, QuickPoint { quarter, percent }),
            }
            // Retention follows the newest sample of this series, so a clock
            // that jumps backwards cannot silently wipe the day.
            let newest = buckets
                .last()
                .map_or(quarter, |point| point.quarter.max(quarter));
            buckets.retain(|point| point.quarter + QUICK_RETENTION_SLOTS > newest);
            if buckets.is_empty() {
                self.quick.remove(key);
            }
        }
    }

    /// Drop the least recently updated series until the cap holds again.
    fn enforce_series_cap(&mut self) {
        while self.series.len() > MAX_SERIES {
            let victim = self
                .series
                .iter()
                .min_by(|(left_key, left), (right_key, right)| {
                    let left_hour = left.last().map_or(0, |point| point.hour);
                    let right_hour = right.last().map_or(0, |point| point.hour);
                    // Oldest sample loses; ties break on the name so the choice
                    // is deterministic rather than dependent on map iteration.
                    left_hour
                        .cmp(&right_hour)
                        .then_with(|| right_key.cmp(left_key))
                })
                .map(|(key, _)| key.clone());
            match victim {
                Some(key) => {
                    self.series.remove(&key);
                }
                None => break,
            }
        }
    }

    /// Dense window of `count` hours ending at `hour`; `None` marks an hour
    /// without a sample (the provider was not polled or did not report).
    pub fn points(&self, key: &str, hour: u64, count: usize) -> Vec<Option<f64>> {
        let buckets = self.series.get(key);
        (0..count)
            .map(|offset| {
                let target = hour.saturating_sub((count - 1 - offset) as u64);
                let buckets = buckets?;
                let index = buckets
                    .binary_search_by_key(&target, |point| point.hour)
                    .ok()?;
                Some(buckets[index].percent)
            })
            .collect()
    }

    /// Compact binding value: the last day's points, comma separated, with an
    /// empty token for an hour that has no sample. Trailing gaps are dropped so
    /// the newest sample always ends the list.
    pub fn binding(&self, key: &str, hour: u64) -> Option<String> {
        if !self.series.contains_key(key) {
            return None;
        }
        let mut tokens: Vec<String> = self
            .points(key, hour, BINDING_POINTS)
            .into_iter()
            .map(|point| point.map_or_else(String::new, |percent| format!("{percent:.0}")))
            .collect();
        while tokens.last().is_some_and(String::is_empty) {
            tokens.pop();
        }
        Some(tokens.join(","))
    }

    /// Points of the current window from its start to now, spread over a fixed
    /// number of slots. The x axis is the time already spent inside the window:
    /// slot 0 is the window start, the last slot is the present. A curve fills
    /// the chart shortly after a reset instead of leaving the rest of the
    /// window blank, and it never invents future samples.
    ///
    /// Session series read the fifteen-minute ring when it has data; every
    /// other window keeps the hourly buckets.
    pub fn cycle_binding(
        &self,
        key: &str,
        now: SystemTime,
        reset: SystemTime,
        duration: std::time::Duration,
        points: usize,
    ) -> Option<String> {
        let now_secs = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
        let reset_secs = reset.duration_since(UNIX_EPOCH).ok()?.as_secs();
        let duration_secs = duration.as_secs();
        if duration_secs == 0 || points < 2 || reset_secs <= now_secs {
            return None;
        }
        let start = reset_secs.saturating_sub(duration_secs);
        let span = now_secs.saturating_sub(start).min(duration_secs);
        if span == 0 {
            return None;
        }
        let hourly = self.series.get(key);
        let quick = self.quick.get(key).filter(|buckets| !buckets.is_empty());
        // Use the fine ring only once it actually reaches the window start.
        // Upgraded installs have an hourly history but an empty or young quick
        // ring, and switching early would drop the older part of the session.
        let quick_covers_start = quick.is_some_and(|buckets| {
            let start_quarter = start / QUICK_SLOT_SECS;
            let index = buckets.partition_point(|point| point.quarter < start_quarter);
            buckets
                .get(index)
                .is_some_and(|point| point.quarter <= start_quarter + 1)
        });
        let quick = quick.filter(|_| quick_covers_start);
        let slot_secs = if quick.is_some() {
            QUICK_SLOT_SECS
        } else {
            3_600
        };
        // Do not invent a full window at the start when the app was not yet
        // running: the first retained sample has to sit at the start for the
        // synthetic 0% to be honest.
        let covers_start = start + RETENTION_HOURS * 3_600 >= now_secs
            && if let Some(quick) = quick {
                let start_slot = start / QUICK_SLOT_SECS;
                let index = quick.partition_point(|point| point.quarter < start_slot);
                quick
                    .get(index)
                    .is_some_and(|point| point.quarter <= start_slot + 1)
            } else {
                let start_hour = start / 3_600;
                hourly.is_some_and(|series| {
                    let index = series.partition_point(|point| point.hour < start_hour);
                    series
                        .get(index)
                        .is_some_and(|point| point.hour <= start_hour + 1)
                })
            };
        let mut tokens = Vec::with_capacity(points);
        for index in 0..points {
            let offset = span.saturating_mul(index as u64) / (points as u64 - 1);
            let slot = (start + offset) / slot_secs;
            let value = if let Some(quick) = quick {
                quick
                    .binary_search_by_key(&slot, |point| point.quarter)
                    .ok()
                    .map(|index| quick[index].percent)
            } else {
                hourly.and_then(|series| {
                    series
                        .binary_search_by_key(&slot, |point| point.hour)
                        .ok()
                        .map(|index| series[index].percent)
                })
            };
            let value = if index == 0 && covers_start {
                Some(0.0)
            } else {
                value
            };
            tokens.push(value.map_or_else(String::new, |percent| format!("{percent:.0}")));
        }
        Some(tokens.join(","))
    }
}

/// Hours since the Unix epoch, the bucket identity of a sample.
pub fn hour_of(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 3_600
}

/// Quarter-hours since the Unix epoch, the bucket identity of a quick sample.
pub fn quarter_of(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / QUICK_SLOT_SECS
}

/// Every reported window of a poll as `(series key, percentage)` samples.
pub fn samples_of(data: &AppUsageData) -> Vec<(String, f64)> {
    let mut samples = Vec::new();
    for provider in ProviderId::ALL {
        let Some(usage) = data.get(provider) else {
            continue;
        };
        let key = provider.descriptor().key;
        push_windows(&mut samples, key, "", usage);
        if let Some(pool) = usage.external_pool.as_deref() {
            // Antigravity meters external models separately, exactly like alerts.
            push_windows(&mut samples, key, "#external", pool);
        }
    }
    samples
}

fn push_windows(
    samples: &mut Vec<(String, f64)>,
    provider_key: &str,
    suffix: &str,
    usage: &UsageData,
) {
    let mut add = |window: &str, section: &crate::models::UsageSection| {
        // An unreported window carries no figure; recording 0% would draw a
        // fictional dip in the sparkline.
        if section.available {
            samples.push((
                History::key(&format!("{provider_key}{suffix}"), window),
                section.percentage,
            ));
        }
    };
    add("session", &usage.session);
    add("weekly", &usage.weekly);
    if let Some(monthly) = usage.monthly.as_ref() {
        add("monthly", monthly);
    }
    // A prepaid wallet has no window, but the share already spent against the
    // current top-up is the one trend worth drawing. Providers that report no
    // credits contribute nothing, so their series stay untouched.
    if let Some(credits) = usage.credits.as_ref() {
        samples.push((
            History::key(&format!("{provider_key}{suffix}"), "credits"),
            credits.percentage,
        ));
    }
}

/// Process-wide snapshot: folded by the window's poll callback and read while
/// building the theme data context. Lazily loaded from disk on first use.
static SNAPSHOT: RwLock<Option<History>> = RwLock::new(None);

fn read_snapshot() -> RwLockReadGuard<'static, Option<History>> {
    SNAPSHOT.read().unwrap_or_else(PoisonError::into_inner)
}

fn write_snapshot() -> RwLockWriteGuard<'static, Option<History>> {
    SNAPSHOT.write().unwrap_or_else(PoisonError::into_inner)
}

/// Fold a successful poll into the history and persist it. Independent of the
/// alert settings: the graph is a record, not a notification.
pub fn record_poll(data: &AppUsageData, now: SystemTime) {
    let hour = hour_of(now);
    let quarter = quarter_of(now);
    let samples = samples_of(data);
    let quick: Vec<(String, f64)> = samples
        .iter()
        .filter(|(key, _)| key.ends_with(":session"))
        .cloned()
        .collect();
    let mut guard = write_snapshot();
    let history =
        guard.get_or_insert_with(|| crate::app_settings::load_history().unwrap_or_default());
    history.fold(&samples, hour);
    history.fold_quick(&quick, quarter);
    if let Err(error) = crate::app_settings::save_history(history) {
        crate::diagnose::log_error("unable to save usage history", error);
    }
}

/// Binding value for one series at the current hour, when it has samples.
pub fn binding(provider_key: &str, window: &str) -> Option<String> {
    binding_at(provider_key, window, SystemTime::now())
}

/// Binding value for the current window, from its start to now. `None` when
/// the history, the reset or the duration is missing.
pub fn cycle_binding(
    provider_key: &str,
    window: &str,
    reset: SystemTime,
    duration: std::time::Duration,
    points: usize,
) -> Option<String> {
    let guard = read_snapshot();
    guard.as_ref()?.cycle_binding(
        &History::key(provider_key, window),
        SystemTime::now(),
        reset,
        duration,
        points,
    )
}

fn binding_at(provider_key: &str, window: &str, now: SystemTime) -> Option<String> {
    let hour = hour_of(now);
    let guard = read_snapshot();
    guard
        .as_ref()?
        .binding(&History::key(provider_key, window), hour)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::UsageSection;

    fn sample(key: &str, percent: f64) -> (String, f64) {
        (History::key(key, "session"), percent)
    }

    fn series<'a>(history: &'a History, key: &str) -> &'a [HistoryPoint] {
        history
            .series
            .get(&History::key(key, "session"))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    #[test]
    fn history_keeps_one_bucket_per_hour_and_the_highest_share() {
        let mut history = History::default();
        history.fold(&[sample("claude", 10.0), sample("claude", 55.0)], 100);
        history.fold(&[sample("claude", 20.0)], 100);
        assert_eq!(
            series(&history, "claude"),
            &[HistoryPoint {
                hour: 100,
                percent: 55.0
            }]
        );
        history.fold(&[sample("claude", 40.0)], 101);
        assert_eq!(series(&history, "claude").len(), 2);
        assert_eq!(series(&history, "claude")[1].percent, 40.0);
    }

    #[test]
    fn history_rolls_buckets_over_when_the_clock_moves_back() {
        let mut history = History::default();
        history.fold(&[sample("claude", 30.0)], 200);
        // A backwards clock must not create a duplicate bucket or drop data.
        history.fold(&[sample("claude", 12.0)], 199);
        history.fold(&[sample("claude", 18.0)], 199);
        assert_eq!(
            series(&history, "claude"),
            &[
                HistoryPoint {
                    hour: 199,
                    percent: 18.0
                },
                HistoryPoint {
                    hour: 200,
                    percent: 30.0
                }
            ]
        );
    }

    #[test]
    fn cycle_binding_is_normalized_to_the_window_and_stops_at_now() {
        let mut history = History::default();
        history.fold(&[sample("claude", 0.0)], 95);
        history.fold(&[sample("claude", 10.0)], 96);
        history.fold(&[sample("claude", 30.0)], 97);
        let reset = UNIX_EPOCH + std::time::Duration::from_secs(100 * 3_600);
        let now = UNIX_EPOCH + std::time::Duration::from_secs(97 * 3_600 + 1_800);
        let binding = history
            .cycle_binding(
                &History::key("claude", "session"),
                now,
                reset,
                std::time::Duration::from_secs(5 * 3_600),
                6,
            )
            .expect("cycle binding");
        assert_eq!(
            binding.split(',').collect::<Vec<_>>(),
            ["0", "0", "10", "10", "30", "30"],
            "the curve fills the elapsed part of the window and stops at now"
        );
    }

    #[test]
    fn session_cycle_prefers_the_fifteen_minute_ring() {
        let mut history = History::default();
        // The hourly ring holds the coarse figures, the quick ring the finer
        // ones; a session chart has to read the finer ones.
        history.fold(&[sample("claude", 10.0)], 50);
        history.fold_quick(&[(History::key("claude", "session"), 40.0)], 200);
        history.fold_quick(&[(History::key("claude", "session"), 55.0)], 201);
        let reset = UNIX_EPOCH + std::time::Duration::from_secs(55 * 3_600);
        let now = UNIX_EPOCH + std::time::Duration::from_secs(50 * 3_600 + 900);
        let binding = history
            .cycle_binding(
                &History::key("claude", "session"),
                now,
                reset,
                std::time::Duration::from_secs(5 * 3_600),
                4,
            )
            .expect("cycle binding");
        assert_eq!(
            binding.split(',').collect::<Vec<_>>(),
            ["0", "40", "40", "55"]
        );

        // Without quick samples the same series falls back to the hourly ring.
        let mut hourly_only = History::default();
        hourly_only.fold(&[sample("claude", 10.0)], 50);
        let binding = hourly_only
            .cycle_binding(
                &History::key("claude", "session"),
                now,
                reset,
                std::time::Duration::from_secs(5 * 3_600),
                4,
            )
            .expect("hourly cycle binding");
        assert_eq!(
            binding.split(',').collect::<Vec<_>>(),
            ["0", "10", "10", "10"]
        );
    }

    #[test]
    fn a_young_quick_ring_does_not_hide_the_hourly_history() {
        let mut history = History::default();
        history.fold(&[sample("claude", 10.0)], 50);
        // The fine ring starts three quarters after the window did, so it
        // cannot speak for the whole session yet; the hourly history must win.
        history.fold_quick(&[(History::key("claude", "session"), 70.0)], 203);
        let reset = UNIX_EPOCH + std::time::Duration::from_secs(55 * 3_600);
        let now = UNIX_EPOCH + std::time::Duration::from_secs(50 * 3_600 + 2_700);
        let binding = history
            .cycle_binding(
                &History::key("claude", "session"),
                now,
                reset,
                std::time::Duration::from_secs(5 * 3_600),
                4,
            )
            .expect("cycle binding");
        assert_eq!(
            binding.split(',').collect::<Vec<_>>(),
            ["0", "10", "10", "10"]
        );
    }

    #[test]
    fn quick_ring_forgets_samples_older_than_a_day() {
        let mut history = History::default();
        for quarter in 0..(QUICK_RETENTION_SLOTS + 10) {
            history.fold_quick(&[sample("claude", (quarter % 100) as f64)], quarter);
        }
        let buckets = &history.quick[&History::key("claude", "session")];
        assert_eq!(buckets.len() as u64, QUICK_RETENTION_SLOTS);
        assert_eq!(
            buckets.last().expect("newest").quarter,
            QUICK_RETENTION_SLOTS + 9
        );
    }

    #[test]
    fn history_forgets_samples_older_than_the_retention() {
        let mut history = History::default();
        for hour in 0..(RETENTION_HOURS + 50) {
            history.fold(&[sample("claude", (hour % 100) as f64)], hour);
        }
        let buckets = series(&history, "claude");
        assert_eq!(buckets.len() as u64, RETENTION_HOURS);
        let newest = *buckets.last().expect("newest bucket");
        assert_eq!(newest.hour, RETENTION_HOURS + 49);
        assert!(buckets[0].hour >= newest.hour - RETENTION_HOURS + 1);
        assert_eq!(newest.percent, ((RETENTION_HOURS + 49) % 100) as f64);
    }

    #[test]
    fn history_caps_the_number_of_tracked_series() {
        let mut history = History::default();
        for index in 0..(MAX_SERIES + 25) {
            let key = format!("provider{index}");
            history.fold(&[sample(&key, 5.0)], 500 + index as u64);
        }
        assert_eq!(history.series.len(), MAX_SERIES);
        // The oldest samples were evicted, the newest ones stayed.
        assert!(!history
            .series
            .contains_key(&History::key("provider0", "session")));
        assert!(history.series.contains_key(&History::key(
            &format!("provider{}", MAX_SERIES + 24),
            "session"
        )));
    }

    #[test]
    fn history_binding_is_dense_for_the_last_day_and_skips_trailing_gaps() {
        let mut history = History::default();
        history.fold(&[sample("claude", 20.0)], 300);
        history.fold(&[sample("claude", 60.0)], 302);
        let binding = history
            .binding(&History::key("claude", "session"), 302)
            .expect("binding");
        let tokens: Vec<&str> = binding.split(',').collect();
        // One token per hour of the last day, aligned to the right-hand edge:
        // hour 301 has no sample, so it stays an empty gap.
        assert_eq!(tokens.len(), BINDING_POINTS);
        assert_eq!(&tokens[BINDING_POINTS - 3..], &["20", "", "60"]);
        assert!(tokens[..BINDING_POINTS - 3].iter().all(|t| t.is_empty()));
        // Three hours later the samples still sit in their own hour slots.
        let later = history
            .binding(&History::key("claude", "session"), 305)
            .expect("binding");
        let later: Vec<&str> = later.split(',').collect();
        assert_eq!(
            &later[BINDING_POINTS - 6..BINDING_POINTS - 3],
            &["20", "", "60"]
        );
        // Trailing hours without a sample are trimmed, not padded.
        assert_eq!(later.len(), BINDING_POINTS - 3);
        assert_eq!(
            history.binding(&History::key("nobody", "session"), 305),
            None
        );
    }

    #[test]
    fn history_binding_drops_points_older_than_a_day() {
        let mut history = History::default();
        history.fold(&[sample("claude", 80.0)], 400);
        let hour = 400 + BINDING_POINTS as u64 + 3;
        history.fold(&[sample("claude", 30.0)], hour);
        let binding = history
            .binding(&History::key("claude", "session"), hour)
            .expect("binding");
        let tokens: Vec<&str> = binding.split(',').collect();
        assert_eq!(tokens.last(), Some(&"30"));
        // The sample from 27 hours ago is outside the window.
        assert!(!tokens.contains(&"80"));
    }

    #[test]
    fn history_samples_skip_unreported_windows_and_track_the_external_pool() {
        let mut usage = UsageData {
            session: UsageSection {
                available: true,
                percentage: 12.0,
                resets_at: None,
            },
            weekly: UsageSection::default(),
            ..Default::default()
        };
        usage.external_pool = Some(Box::new(UsageData {
            session: UsageSection {
                available: true,
                percentage: 64.0,
                resets_at: None,
            },
            ..Default::default()
        }));
        let data = AppUsageData::from_iter([(ProviderId::Antigravity, usage)]);
        let samples = samples_of(&data);
        assert!(samples.contains(&(History::key("antigravity", "session"), 12.0)));
        assert!(samples.contains(&(History::key("antigravity#external", "session"), 64.0)));
        assert!(!samples
            .iter()
            .any(|(key, _)| key == &History::key("antigravity", "weekly")));
    }
}

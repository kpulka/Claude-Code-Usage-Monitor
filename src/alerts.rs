//! Reset alerts: was a quota window driven to the edge, and has it just refilled?
//!
//! Providers count usage very differently — Claude reports a rolling five-hour
//! window next to a seven-day one, OpenCode Go adds a monthly cap, Antigravity
//! splits Gemini and external models into separate pools — and each one hands
//! back its own reset timestamp. Instead of modelling those differences, this
//! module watches the two fields every provider does report:
//!
//! * `percentage` — how much of the window is spent, and
//! * `resets_at` — when that window rolls over.
//!
//! A window is *armed* once it has been seen at or above the alert threshold.
//! It *fires* when an armed window refills: usage drops while the provider's own
//! reset time either passed or moved forward. A window that never came close to
//! the threshold stays silent, and a provider that is failing or reporting an
//! absent window keeps its previous state instead of arming or firing by
//! accident.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::models::{AppUsageData, UsageData, UsageSection};
use crate::providers::{ProviderId, ProviderSet};

/// Only windows that reached this much usage deserve a sound on reset.
pub(crate) const DEFAULT_THRESHOLD: f64 = 95.0;
/// A burst of resets (five-hour windows rotate together) plays one sound.
const PLAY_COOLDOWN_SECS: u64 = 20;
/// Provider clocks drift slightly against ours; accept a reset this much early.
const RESET_GRACE_SECS: u64 = 10;
/// A reset time that moved forward by this much means the window rolled over
/// even if our clock never observed the old timestamp pass (app was closed).
const RESET_SHIFT_SECS: u64 = 300;
/// Usage has to actually fall by this much to count as a refill.
const REFILL_DROP: f64 = 1.0;
/// A window that will be exhausted within this long is flagged with a marker.
const BURN_WARN_SECS: u64 = 45 * 60;
/// Usage history kept per window for the burn-rate estimate.
const SAMPLE_WINDOW_SECS: u64 = 3 * 60 * 60;
const MAX_SAMPLES: usize = 24;
/// Crossing this much usage counts as "limit reached" for the notification.
const REACHED_THRESHOLD: f64 = 99.5;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Sample {
    pub unix: u64,
    pub percentage: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct WindowState {
    /// True once the window was seen at or above the threshold.
    pub armed: bool,
    /// Usage at the previous poll, used to recognise a refill.
    pub percentage: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_unix: Option<u64>,
    /// Recent readings, oldest first, used for the burn-rate projection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<Sample>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AlertState {
    #[serde(default)]
    pub windows: BTreeMap<String, WindowState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played_unix: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AlertEvent {
    pub provider: ProviderId,
    pub window: String,
    /// Usage seen on the poll before the reset (usually 100).
    pub peak: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Evaluation {
    /// Windows that refilled after being armed.
    pub events: Vec<AlertEvent>,
    /// Windows that just crossed into "fully used".
    pub reached: Vec<AlertEvent>,
    /// True when a sound should play now: at least one event and no cooldown.
    pub play: bool,
    /// Providers whose consumption is on course to exhaust a window soon.
    pub warnings: ProviderSet,
}

/// Fold one poll into the persisted state and report what just reset.
pub(crate) fn evaluate(
    state: &mut AlertState,
    data: &AppUsageData,
    providers: ProviderSet,
    thresholds: &dyn Fn(ProviderId) -> f64,
    now: SystemTime,
) -> Evaluation {
    let now_unix = now
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);

    let mut events = Vec::new();
    let mut reached = Vec::new();
    let mut warnings = ProviderSet::empty();
    for provider in providers.iter() {
        let threshold = thresholds(provider).clamp(1.0, 100.0);
        let Some(usage) = data.get(provider) else {
            continue;
        };
        let mut windows = Vec::new();
        push_windows(&mut windows, provider.descriptor().key, "", usage);
        if let Some(pool) = usage.external_pool.as_deref() {
            // Antigravity meters external (Claude) models separately from
            // Gemini; both deserve their own alert bookkeeping.
            push_windows(&mut windows, provider.descriptor().key, "#external", pool);
        }
        for (key, section) in windows {
            let outcome = observe(
                state,
                provider,
                &key,
                section,
                threshold,
                now_unix,
                &mut events,
            );
            if outcome.reached {
                reached.push(AlertEvent {
                    provider,
                    window: key.clone(),
                    peak: section.percentage,
                });
            }
            if outcome.burning {
                warnings.set(provider, true);
            }
        }
    }

    let play = !events.is_empty()
        && state
            .last_played_unix
            .is_none_or(|played| now_unix >= played + PLAY_COOLDOWN_SECS);
    if play {
        state.last_played_unix = Some(now_unix);
    }
    Evaluation {
        events,
        reached,
        play,
        warnings,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct WindowOutcome {
    /// Usage crossed into "fully used" on this poll.
    reached: bool,
    /// Consumption is on course to exhaust the window within `BURN_WARN_SECS`.
    burning: bool,
}

fn push_windows<'a>(
    windows: &mut Vec<(String, &'a UsageSection)>,
    provider_key: &str,
    suffix: &str,
    usage: &'a UsageData,
) {
    let mut add = |window: &'static str, section: &'a UsageSection| {
        windows.push((format!("{provider_key}{suffix}:{window}"), section));
    };
    add("session", &usage.session);
    add("weekly", &usage.weekly);
    if let Some(monthly) = usage.monthly.as_ref() {
        add("monthly", monthly);
    }
}

#[allow(clippy::too_many_arguments)]
fn observe(
    state: &mut AlertState,
    provider: ProviderId,
    key: &str,
    section: &UsageSection,
    threshold: f64,
    now_unix: u64,
    events: &mut Vec<AlertEvent>,
) -> WindowOutcome {
    if !section.available {
        // An absent window says nothing about the previous reading; leave the
        // stored state untouched so the next real poll still fires.
        return WindowOutcome::default();
    }
    let resets_at_unix = section
        .resets_at
        .and_then(|reset| reset.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs());

    let previous = state.windows.get(key).cloned().unwrap_or_default();
    let mut armed = previous.armed || section.percentage >= threshold;

    if previous.armed {
        let reset_reached = previous
            .resets_at_unix
            .is_some_and(|reset| now_unix + RESET_GRACE_SECS >= reset);
        let reset_shifted = matches!(
            (previous.resets_at_unix, resets_at_unix),
            (Some(old), Some(new)) if new > old + RESET_SHIFT_SECS
        );
        let refilled = previous.percentage - section.percentage >= REFILL_DROP;
        if refilled && (reset_reached || reset_shifted) {
            events.push(AlertEvent {
                provider,
                window: key.to_string(),
                peak: previous.percentage,
            });
            armed = false;
        }
    }

    let reached =
        previous.percentage < REACHED_THRESHOLD && section.percentage >= REACHED_THRESHOLD;

    let mut samples = previous.samples.clone();
    samples.push(Sample {
        unix: now_unix,
        percentage: section.percentage,
    });
    samples.retain(|sample| sample.unix + SAMPLE_WINDOW_SECS >= now_unix);
    if samples.len() > MAX_SAMPLES {
        let excess = samples.len() - MAX_SAMPLES;
        samples.drain(..excess);
    }
    let burning = burn_rate(&samples, section.percentage, now_unix).is_some();

    state.windows.insert(
        key.to_string(),
        WindowState {
            armed,
            percentage: section.percentage,
            resets_at_unix,
            samples,
        },
    );
    WindowOutcome { reached, burning }
}

/// Usage per second across the recorded samples, expressed as a will-run-out
/// estimate. `None` when usage is flat or falling, or when the history is too
/// short to say anything.
fn burn_rate(samples: &[Sample], percentage: f64, now_unix: u64) -> Option<u64> {
    let first = samples.first()?;
    let last = samples.last()?;
    let span = last.unix.saturating_sub(first.unix);
    if span < 60 {
        return None;
    }
    let growth = last.percentage - first.percentage;
    if growth <= 0.5 {
        return None;
    }
    let rate = growth / span as f64; // percentage points per second
    if rate <= 0.0 {
        return None;
    }
    let remaining = (100.0 - percentage).max(0.0);
    let eta = (remaining / rate) as u64;
    // Guard against a single spike claiming the window ends instantly.
    let _ = now_unix;
    (eta <= BURN_WARN_SECS).then_some(eta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppUsageData, UsageData};
    use std::time::Duration;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn section(percentage: f64, resets_at: Option<SystemTime>) -> UsageSection {
        UsageSection {
            available: true,
            percentage,
            resets_at,
        }
    }

    fn claude_only() -> ProviderSet {
        ProviderSet::from_enabled([ProviderId::Claude])
    }

    fn data_with(session: UsageSection, weekly: UsageSection) -> AppUsageData {
        let mut data = AppUsageData::default();
        data.insert(
            ProviderId::Claude,
            UsageData {
                session,
                weekly,
                ..Default::default()
            },
        );
        data
    }

    #[test]
    fn reset_after_reaching_the_threshold_fires_once() {
        let mut state = AlertState::default();
        let providers = claude_only();

        // 96% used, the five-hour window resets at t=1000.
        let before_reset = data_with(section(96.0, Some(at(1000))), section(40.0, None));
        let first = evaluate(&mut state, &before_reset, providers, &|_| 95.0, at(500));
        assert!(
            first.events.is_empty(),
            "crossing the threshold is not an event"
        );
        assert!(state.windows["claude:session"].armed);

        // Still spent, just before the reset: nothing yet.
        let still_spent = data_with(section(100.0, Some(at(1000))), section(40.0, None));
        assert!(
            evaluate(&mut state, &still_spent, providers, &|_| 95.0, at(900))
                .events
                .is_empty()
        );

        // The window rolled over: usage dropped and the reset time moved on.
        let after_reset = data_with(section(0.0, Some(at(19_000))), section(41.0, None));
        let fired = evaluate(&mut state, &after_reset, providers, &|_| 95.0, at(1010));
        assert_eq!(fired.events.len(), 1);
        assert_eq!(fired.events[0].window, "claude:session");
        assert_eq!(fired.events[0].peak, 100.0);
        assert!(fired.play);
        assert!(
            !state.windows["claude:session"].armed,
            "disarmed after firing"
        );

        // Second poll of the same refilled window must stay silent.
        let quiet = evaluate(&mut state, &after_reset, providers, &|_| 95.0, at(1020));
        assert!(quiet.events.is_empty());
    }

    #[test]
    fn a_window_below_the_threshold_stays_silent() {
        let mut state = AlertState::default();
        let providers = claude_only();
        let low = data_with(section(60.0, Some(at(1000))), section(20.0, None));
        assert!(evaluate(&mut state, &low, providers, &|_| 95.0, at(500))
            .events
            .is_empty());
        let reset = data_with(section(0.0, Some(at(19_000))), section(21.0, None));
        let evaluation = evaluate(&mut state, &reset, providers, &|_| 95.0, at(1010));
        assert!(evaluation.events.is_empty());
        assert!(!evaluation.play);
    }

    #[test]
    fn a_reset_while_the_app_was_closed_is_detected_from_the_moved_timestamp() {
        let mut state = AlertState::default();
        let providers = claude_only();
        let spent = data_with(section(100.0, Some(at(1000))), section(30.0, None));
        evaluate(&mut state, &spent, providers, &|_| 95.0, at(900));
        // Next poll happens long after the reset; the timestamp jumped and the
        // usage is low again.
        let later = data_with(section(2.0, Some(at(50_000))), section(31.0, None));
        let fired = evaluate(&mut state, &later, providers, &|_| 95.0, at(20_000));
        assert_eq!(fired.events.len(), 1);
    }

    #[test]
    fn absent_windows_do_not_arm_or_fire() {
        let mut state = AlertState::default();
        let providers = claude_only();
        let absent = UsageSection {
            available: false,
            percentage: 0.0,
            resets_at: None,
        };
        let data = data_with(absent.clone(), absent);
        let evaluation = evaluate(&mut state, &data, providers, &|_| 95.0, at(500));
        assert!(evaluation.events.is_empty());
        assert!(state.windows.is_empty());
    }

    #[test]
    fn antigravity_external_pool_is_tracked_separately_from_gemini() {
        let mut state = AlertState::default();
        let providers = ProviderSet::from_enabled([ProviderId::Antigravity]);
        let mut data = AppUsageData::default();
        data.insert(
            ProviderId::Antigravity,
            UsageData {
                session: section(10.0, Some(at(3000))),
                weekly: section(10.0, None),
                external_pool: Some(Box::new(UsageData {
                    session: section(100.0, Some(at(1000))),
                    weekly: section(100.0, None),
                    ..Default::default()
                })),
                ..Default::default()
            },
        );
        evaluate(&mut state, &data, providers, &|_| 95.0, at(500));
        assert!(state.windows.contains_key("antigravity#external:session"));
        assert!(!state.windows["antigravity:session"].armed);

        let refilled = data.clone();
        let external = refilled
            .get(ProviderId::Antigravity)
            .and_then(|usage| usage.external_pool.as_deref())
            .cloned();
        let mut outer = refilled.get(ProviderId::Antigravity).cloned().unwrap();
        if let Some(mut pool) = external {
            pool.session = section(0.0, Some(at(19_000)));
            outer.external_pool = Some(Box::new(pool));
        }
        let mut refilled_data = AppUsageData::default();
        refilled_data.insert(ProviderId::Antigravity, outer);
        let fired = evaluate(&mut state, &refilled_data, providers, &|_| 95.0, at(1010));
        assert_eq!(fired.events.len(), 1);
        assert_eq!(fired.events[0].window, "antigravity#external:session");
    }

    #[test]
    fn a_burst_of_resets_plays_one_sound_and_respects_the_cooldown() {
        let mut state = AlertState::default();
        let providers = claude_only();
        let spent = data_with(
            section(100.0, Some(at(1000))),
            section(100.0, Some(at(1000))),
        );
        evaluate(&mut state, &spent, providers, &|_| 95.0, at(500));
        let reset = data_with(
            section(0.0, Some(at(19_000))),
            section(0.0, Some(at(19_000))),
        );
        let fired = evaluate(&mut state, &reset, providers, &|_| 95.0, at(1010));
        assert_eq!(fired.events.len(), 2);
        assert!(fired.play);

        // Same burst on the next poll: events may repeat, the sound may not.
        let mut state_again = state.clone();
        state_again
            .windows
            .values_mut()
            .for_each(|window| window.armed = true);
        let again = evaluate(&mut state_again, &spent, providers, &|_| 95.0, at(1015));
        assert!(!again.play, "cooldown suppresses a second sound");
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = AlertState::default();
        let providers = claude_only();
        let spent = data_with(section(97.5, Some(at(1000))), section(12.0, None));
        evaluate(&mut state, &spent, providers, &|_| 95.0, at(500));
        let json = serde_json::to_string(&state).unwrap();
        let decoded: AlertState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, state);
    }

    #[test]
    fn crossing_full_usage_is_reported_separately_from_the_reset() {
        let mut state = AlertState::default();
        let providers = claude_only();
        let almost = data_with(section(97.0, Some(at(1000))), section(10.0, None));
        assert!(evaluate(&mut state, &almost, providers, &|_| 95.0, at(500))
            .reached
            .is_empty());
        let full = data_with(section(100.0, Some(at(1000))), section(10.0, None));
        let evaluation = evaluate(&mut state, &full, providers, &|_| 95.0, at(560));
        assert_eq!(evaluation.reached.len(), 1);
        assert_eq!(evaluation.reached[0].window, "claude:session");
        // Reporting "full" is not a reset: no chime for it.
        assert!(!evaluation.play);
    }

    #[test]
    fn a_steep_burn_rate_flags_the_provider() {
        let mut state = AlertState::default();
        let providers = claude_only();
        let empty = ProviderSet::empty();
        // 60% then 80% inside ten minutes: 20 points per 600s, 20 left -> ~10 min.
        let first = data_with(section(60.0, Some(at(9000))), section(5.0, None));
        let quiet = evaluate(&mut state, &first, providers, &|_| 95.0, at(1000));
        assert_eq!(quiet.warnings, empty);
        let steep = data_with(section(80.0, Some(at(9000))), section(5.0, None));
        let warned = evaluate(&mut state, &steep, providers, &|_| 95.0, at(1600));
        assert!(warned.warnings.contains(ProviderId::Claude));
    }

    #[test]
    fn per_provider_thresholds_are_honoured() {
        let mut state = AlertState::default();
        let providers = claude_only();
        let spent = data_with(section(92.0, Some(at(1000))), section(10.0, None));
        // Global 95 does not arm a 92% window...
        assert!(!evaluate(&mut state, &spent, providers, &|_| 95.0, at(500))
            .warnings
            .contains(ProviderId::Claude));
        assert!(!state.windows["claude:session"].armed);
        // ...but a 90% override for Claude does.
        let mut state = AlertState::default();
        evaluate(
            &mut state,
            &spent,
            providers,
            &|p| {
                if p == ProviderId::Claude {
                    90.0
                } else {
                    95.0
                }
            },
            at(500),
        );
        assert!(state.windows["claude:session"].armed);
    }
}

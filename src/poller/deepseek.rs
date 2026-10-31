//! DeepSeek's prepaid API wallet.
//!
//! DeepSeek sells no quota: `GET /user/balance` returns what is left in a
//! prepaid wallet, with no ceiling, no percentage and no reset date. The only
//! thing that can be learned is the trend, so the gauge measures the current
//! top-up period: the balance recorded when the wallet was seeded (or last
//! topped up) is the denominator, and the spend rate since that moment gives
//! how long the money lasts.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use super::{build_agent, PollError};
use crate::app_settings;
use crate::diagnose;
use crate::models::{CreditsSection, DeepSeekCreditsState, UsageData};

const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";
const USAGE_USER_AGENT: &str = concat!("claude-code-usage-monitor/", env!("CARGO_PKG_VERSION"));
const API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
/// Point at a different `auth.json` (used by tests and unusual installs).
const AUTH_FILE_ENV: &str = "DEEPSEEK_AUTH_FILE";
const SECONDS_PER_DAY: f64 = 86_400.0;
/// A yuan account lists its wallet in CNY; the card is labelled in dollars, so
/// prefer the dollar entry when an account reports more than one.
const PREFERRED_CURRENCY: &str = "USD";

/// Plain API key, as stored by the Command Code CLI or supplied by the user.
struct ApiKeyCredentials {
    api_key: String,
    source: String,
}

/// `~/.commandcode/auth.json`, which stores the DeepSeek key under a
/// `deepseek` object beside the Command Code one.
#[derive(Deserialize)]
struct AuthFile {
    #[serde(default)]
    deepseek: Option<AuthSection>,
}

#[derive(Deserialize)]
struct AuthSection {
    #[serde(default, alias = "apiKey", alias = "api_key", alias = "token")]
    key: String,
}

// Wire shape of `GET /user/balance`. Amounts are decimal strings in whole
// currency units and `balance_infos` is an array, because an account can hold
// a wallet per currency.
#[derive(Deserialize)]
struct BalancePayload {
    #[serde(default)]
    is_available: bool,
    #[serde(default)]
    balance_infos: Vec<BalanceInfo>,
}

#[derive(Deserialize)]
struct BalanceInfo {
    #[serde(default)]
    currency: String,
    /// Kept untyped so a decimal string and a bare number both parse, and a
    /// malformed amount is rejected instead of silently read as zero.
    #[serde(default)]
    total_balance: serde_json::Value,
}

pub(super) fn poll_deepseek() -> Result<UsageData, PollError> {
    poll_sources(read_api_key_credentials().as_ref(), BALANCE_URL, now_unix())
}

pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    vec![credential_watch_signature()]
}

/// DeepSeek has a single credential: one API key. Without it there is nothing
/// to ask the balance API, so report a missing login rather than an empty row.
fn poll_sources(
    credentials: Option<&ApiKeyCredentials>,
    url: &str,
    now: u64,
) -> Result<UsageData, PollError> {
    let Some(credentials) = credentials else {
        diagnose::log("DeepSeek balance poll failed: no API key found");
        return Err(PollError::NoCredentials);
    };

    let payload = fetch_balance(credentials, url)?;
    let Some((balance, currency)) = wallet_from_payload(&payload) else {
        return Err(PollError::RequestFailed);
    };

    let (state, section) =
        wallet_section(app_settings::load_deepseek_credits(), balance, &currency, now);
    if let Err(error) = app_settings::save_deepseek_credits(&state) {
        diagnose::log(format!(
            "unable to persist the DeepSeek wallet baseline: {error}"
        ));
    }

    Ok(UsageData {
        // A prepaid wallet has no session, weekly or monthly window, so those
        // sections stay unavailable and the themes gate on `credits.available`.
        credits: Some(section),
        ..Default::default()
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The balance to show, in whole currency units, and the currency it uses.
fn wallet_from_payload(payload: &BalancePayload) -> Option<(f64, String)> {
    if !payload.is_available {
        // Still worth showing: an empty wallet is exactly what the user needs
        // to see, so the amount is reported rather than dropped.
        diagnose::log("DeepSeek reports the wallet can no longer pay for calls");
    }
    let Some(entry) = preferred_balance(&payload.balance_infos) else {
        diagnose::log("DeepSeek balance response carried no wallet");
        return None;
    };
    let Some(balance) = amount(&entry.total_balance) else {
        diagnose::log("DeepSeek balance response carried no usable total");
        return None;
    };
    if !balance.is_finite() || balance < 0.0 {
        diagnose::log("DeepSeek balance is not a usable amount");
        return None;
    }
    if entry.currency.trim().is_empty() {
        diagnose::log("DeepSeek balance response carried no currency");
    }
    Some((balance, entry.currency.trim().to_string()))
}

fn preferred_balance(infos: &[BalanceInfo]) -> Option<&BalanceInfo> {
    infos
        .iter()
        .find(|info| info.currency.trim().eq_ignore_ascii_case(PREFERRED_CURRENCY))
        .or_else(|| infos.first())
}

/// Amounts arrive as decimal strings such as `"18.69"`; a bare number is
/// accepted too, so a shape change cannot blank the wallet.
fn amount(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok(),
        serde_json::Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

/// Tracks the wallet across polls and turns it into a gauge plus a trend.
///
/// The balance only falls as calls are paid for, so any rise is a top-up and
/// re-baselines the gauge: the percentage then measures the current top-up
/// period rather than a period that has already been topped up.
fn wallet_section(
    previous: Option<DeepSeekCreditsState>,
    balance: f64,
    currency: &str,
    now: u64,
) -> (DeepSeekCreditsState, CreditsSection) {
    let (baseline, seeded_at) = match previous {
        Some(previous) if balance > previous.balance => {
            diagnose::log("DeepSeek wallet was topped up; re-baselining");
            (balance, now)
        }
        // A different currency is a different wallet; comparing its amounts to
        // the old baseline would invent spending.
        Some(previous) if previous.currency != currency => (balance, now),
        Some(previous) if previous.baseline > 0.0 => {
            (previous.baseline.max(balance), previous.seeded_at_unix)
        }
        _ => (balance, now),
    };

    // A clock that went backwards would otherwise produce a negative elapsed
    // time, and with it a negative burn. Restart the clock instead.
    let seeded_at = if seeded_at > now {
        diagnose::log("DeepSeek wallet clock moved backwards; restarting the burn clock");
        now
    } else {
        seeded_at
    };

    let spent = (baseline - balance).max(0.0);
    let elapsed_days = now.saturating_sub(seeded_at) as f64 / SECONDS_PER_DAY;
    let burn_per_day = if elapsed_days > 0.0 {
        spent / elapsed_days
    } else {
        0.0
    };
    // Only a wallet that is actually being drawn down has a projected end.
    let eta_seconds = (burn_per_day > 0.0)
        .then(|| balance / burn_per_day * SECONDS_PER_DAY)
        .filter(|eta| eta.is_finite() && *eta >= 0.0);
    let percentage = if baseline > 0.0 {
        (spent / baseline * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let state = DeepSeekCreditsState {
        currency: currency.to_string(),
        balance,
        baseline,
        seeded_at_unix: seeded_at,
        updated_at_unix: now,
    };
    (
        state,
        CreditsSection {
            percentage,
            remaining: balance,
            total: baseline,
            currency: Some(currency.to_string()),
            eta_seconds,
        },
    )
}

fn fetch_balance(credentials: &ApiKeyCredentials, url: &str) -> Result<BalancePayload, PollError> {
    let mut response = match build_agent()?
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", credentials.api_key))
        .header("User-Agent", USAGE_USER_AGENT)
        .call()
        .and_then(super::check_http_status)
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(401 | 403)) => {
            diagnose::log("DeepSeek balance request was rejected");
            return Err(PollError::AuthRequired);
        }
        Err(ureq::Error::StatusCode(429)) => {
            diagnose::log("DeepSeek balance request was rate limited");
            return Err(PollError::RequestFailed);
        }
        Err(error) => {
            diagnose::log_error("DeepSeek balance request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    response
        .body_mut()
        .read_json::<Option<BalancePayload>>()
        .map_err(|error| {
            diagnose::log_error("DeepSeek balance response is not valid JSON", error);
            PollError::RequestFailed
        })?
        .ok_or_else(|| {
            diagnose::log("DeepSeek balance response was empty");
            PollError::RequestFailed
        })
}

fn read_api_key_credentials() -> Option<ApiKeyCredentials> {
    if let Some(api_key) = non_empty_environment(API_KEY_ENV).filter(|value| valid_api_key(value)) {
        return Some(ApiKeyCredentials {
            api_key,
            source: "environment".to_string(),
        });
    }

    auth_file_paths()
        .into_iter()
        .find_map(|path| read_api_key_from_auth_file(&path))
}

fn read_api_key_from_auth_file(path: &Path) -> Option<ApiKeyCredentials> {
    let content = std::fs::read_to_string(path).ok()?;
    let auth: AuthFile = serde_json::from_str(&content).ok()?;
    let api_key = auth.deepseek?.key.trim().to_string();
    valid_api_key(&api_key).then(|| ApiKeyCredentials {
        api_key,
        source: path.display().to_string(),
    })
}

fn auth_file_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = non_empty_environment(AUTH_FILE_ENV).map(PathBuf::from) {
        paths.push(path);
    }
    if let Some(home) = crate::known_folders::home_dir() {
        paths.push(home.join(".commandcode").join("auth.json"));
    }
    paths
}

fn non_empty_environment(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// API keys are short opaque tokens; anything with whitespace or quotes would
/// break the request header, so reject it rather than send a mangled request.
fn valid_api_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\')
}

fn credential_watch_signature() -> String {
    let mut parts = Vec::new();
    match read_api_key_credentials() {
        Some(credentials) => {
            let mut hasher = DefaultHasher::new();
            credentials.api_key.hash(&mut hasher);
            parts.push(format!(
                "api_key|present|{}|{:x}|{}",
                credentials.api_key.len(),
                hasher.finish(),
                credentials.source
            ));
        }
        None => parts.push("api_key|missing".to_string()),
    }
    for path in auth_file_paths() {
        parts.push(path_signature(&path));
    }
    parts.join(";;")
}

fn path_signature(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!(
                "auth:{}|present|{}|{modified}",
                path.display(),
                metadata.len()
            )
        }
        Err(_) => format!("auth:{}|missing", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    // Wire shape verified against the live API: string amounts in whole
    // currency units, one entry per wallet.
    const BALANCE_JSON: &str = r#"{
        "is_available": true,
        "balance_infos": [
            {
                "currency": "USD",
                "total_balance": "18.69",
                "granted_balance": "0.00",
                "topped_up_balance": "18.69"
            }
        ]
    }"#;

    fn credentials() -> ApiKeyCredentials {
        ApiKeyCredentials {
            api_key: ["sk", "test", "key"].join("-"),
            source: "test".into(),
        }
    }

    fn payload(json: &str) -> BalancePayload {
        serde_json::from_str(json).unwrap()
    }

    fn wallet(json: &str) -> (f64, String) {
        wallet_from_payload(&payload(json)).expect("the fixture should carry a wallet")
    }

    #[test]
    fn the_balance_parses_from_string_amounts() {
        assert_eq!(wallet(BALANCE_JSON), (18.69, "USD".into()));
    }

    #[test]
    fn one_wallet_per_currency_is_read_in_order_of_preference() {
        let multiple = r#"{
            "is_available": true,
            "balance_infos": [
                {"currency": "CNY", "total_balance": "120.00"},
                {"currency": "usd", "total_balance": "17.5"}
            ]
        }"#;
        assert_eq!(wallet(multiple), (17.5, "usd".into()));

        // With no dollar wallet the first entry is the account's own currency.
        let cny_only = r#"{
            "is_available": true,
            "balance_infos": [{"currency": "CNY", "total_balance": "120.00"}]
        }"#;
        assert_eq!(wallet(cny_only), (120.0, "CNY".into()));

        // A bare number is tolerated as well as the documented string.
        let numeric = r#"{
            "is_available": true,
            "balance_infos": [{"currency": "USD", "total_balance": 4.25}]
        }"#;
        assert_eq!(wallet(numeric), (4.25, "USD".into()));
    }

    #[test]
    fn unusable_wallets_are_rejected_instead_of_read_as_zero() {
        for json in [
            r#"{"is_available":true,"balance_infos":[]}"#,
            r#"{"is_available":true}"#,
            r#"{"is_available":true,"balance_infos":[{"currency":"USD"}]}"#,
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"n/a"}]}"#,
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":null}]}"#,
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"-2.00"}]}"#,
        ] {
            assert!(
                wallet_from_payload(&payload(json)).is_none(),
                "accepted {json}"
            );
        }
    }

    #[test]
    fn an_empty_wallet_is_still_shown() {
        // `is_available` describes whether calls can still be paid for; a zero
        // balance is exactly what the user needs to see, not a missing row.
        let empty = r#"{
            "is_available": false,
            "balance_infos": [{"currency": "USD", "total_balance": "0.00"}]
        }"#;
        assert_eq!(wallet(empty), (0.0, "USD".into()));
    }

    #[test]
    fn the_first_balance_seeds_the_baseline_and_reads_untouched() {
        let now = 1_800_000_000;
        let (state, section) = wallet_section(None, 18.69, "USD", now);

        assert_eq!(state.baseline, 18.69);
        assert_eq!(state.balance, 18.69);
        assert_eq!(state.seeded_at_unix, now);
        assert_eq!(state.currency, "USD");
        assert_eq!(section.percentage, 0.0);
        assert_eq!(section.remaining, 18.69);
        assert_eq!(section.total, 18.69);
        assert_eq!(section.currency.as_deref(), Some("USD"));
        // Nothing has been spent yet, so there is no rate and no projection.
        assert_eq!(section.eta_seconds, None);
    }

    #[test]
    fn spending_against_a_baseline_fills_the_gauge_and_the_trend() {
        let seeded = 1_800_000_000;
        let previous = DeepSeekCreditsState {
            currency: "USD".into(),
            balance: 60.0,
            baseline: 60.0,
            seeded_at_unix: seeded,
            updated_at_unix: seeded,
        };
        // Three days later, 24 dollars of the 60 are gone: 8 a day, so the
        // remaining 36 lasts four and a half more days.
        let now = seeded + 3 * DAY;
        let (state, section) = wallet_section(Some(previous), 36.0, "USD", now);

        assert_eq!(state.baseline, 60.0);
        assert_eq!(state.seeded_at_unix, seeded, "the burn clock keeps running");
        assert_eq!(section.percentage, 40.0);
        assert_eq!(section.remaining, 36.0);
        assert_eq!(section.total, 60.0);
        let eta = section.eta_seconds.expect("a falling balance has a trend");
        assert!((eta - 4.5 * DAY as f64).abs() < 1.0, "{eta}");
    }

    #[test]
    fn a_top_up_reseeds_the_baseline_and_restarts_the_burn_clock() {
        let seeded = 1_800_000_000;
        let previous = DeepSeekCreditsState {
            currency: "USD".into(),
            balance: 10.0,
            baseline: 50.0,
            seeded_at_unix: seeded,
            updated_at_unix: seeded + 4 * DAY,
        };
        let now = seeded + 4 * DAY + 60;
        let (state, section) = wallet_section(Some(previous), 25.0, "USD", now);

        assert_eq!(state.baseline, 25.0, "the top-up is the new denominator");
        assert_eq!(state.seeded_at_unix, now, "the clock restarts at the top-up");
        // A minute in, nothing measurable has been spent against the top-up.
        assert_eq!(section.percentage, 0.0);
        assert_eq!(section.eta_seconds, None);
    }

    #[test]
    fn a_clock_that_moved_backwards_restarts_the_burn_clock() {
        let now = 1_800_000_000;
        let previous = DeepSeekCreditsState {
            currency: "USD".into(),
            balance: 40.0,
            baseline: 60.0,
            // Written by a clock that was ahead of this one.
            seeded_at_unix: now + 10 * DAY,
            updated_at_unix: now + 10 * DAY,
        };
        let (state, section) = wallet_section(Some(previous), 30.0, "USD", now);

        assert_eq!(state.seeded_at_unix, now);
        assert_eq!(state.baseline, 60.0);
        // No elapsed time, so no negative burn and no imaginary projection.
        assert_eq!(section.percentage, 50.0);
        assert_eq!(section.eta_seconds, None);
    }

    #[test]
    fn a_currency_change_starts_a_new_wallet() {
        let seeded = 1_800_000_000;
        let previous = DeepSeekCreditsState {
            currency: "CNY".into(),
            balance: 120.0,
            baseline: 400.0,
            seeded_at_unix: seeded,
            updated_at_unix: seeded + DAY,
        };
        let now = seeded + DAY;
        let (state, section) = wallet_section(Some(previous), 18.0, "USD", now);

        assert_eq!(state.currency, "USD");
        assert_eq!(state.baseline, 18.0, "a different wallet is not spending");
        assert_eq!(state.seeded_at_unix, now);
        assert_eq!(section.percentage, 0.0);
    }

    #[test]
    fn a_state_without_an_effective_baseline_is_reseeded() {
        let now = 1_800_000_000;
        for baseline in [0.0, -1.0] {
            let previous = DeepSeekCreditsState {
                currency: "USD".into(),
                balance: 5.0,
                baseline,
                seeded_at_unix: now - 30 * DAY,
                updated_at_unix: now - DAY,
            };
            let (state, section) = wallet_section(Some(previous), 5.0, "USD", now);
            assert_eq!(state.baseline, 5.0, "baseline {baseline}");
            assert_eq!(state.seeded_at_unix, now);
            assert_eq!(section.percentage, 0.0);
        }
    }

    #[test]
    fn a_spent_wallet_pins_the_gauge_full_and_tracks_no_further() {
        let seeded = 1_800_000_000;
        let previous = DeepSeekCreditsState {
            currency: "USD".into(),
            balance: 1.0,
            baseline: 20.0,
            seeded_at_unix: seeded,
            updated_at_unix: seeded + DAY,
        };
        let (state, section) = wallet_section(Some(previous), 0.0, "USD", seeded + 2 * DAY);

        assert_eq!(state.baseline, 20.0);
        assert_eq!(section.percentage, 100.0);
        assert_eq!(section.remaining, 0.0);
        // Nothing is left, so there is nothing left to count down.
        assert_eq!(section.eta_seconds, Some(0.0));
    }

    /// Serves canned responses on 127.0.0.1 and records the requests it saw.
    fn mock_server(
        routes: Vec<(String, u16, String)>,
        expected_requests: usize,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                let text = String::from_utf8(request).unwrap();
                let path = text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let (_, status, body) = routes
                    .iter()
                    .find(|(route, _, _)| path.starts_with(route.as_str()))
                    .expect("unexpected request path");
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                seen.push(text);
            }
            seen
        });
        (base, handle)
    }

    fn balance_url(base: &str) -> String {
        format!("{base}/user/balance")
    }

    #[test]
    fn balance_requests_send_a_bearer_token_without_a_cookie() {
        let (base, requests) = mock_server(
            vec![("/user/balance".into(), 200, BALANCE_JSON.into())],
            1,
        );
        let credentials = credentials();
        let usage = poll_sources(
            Some(&credentials),
            &balance_url(&base),
            1_800_000_000,
        )
        .unwrap();

        let credits = usage.credits.as_ref().expect("the wallet is reported");
        assert_eq!(credits.remaining, 18.69);
        assert_eq!(credits.currency.as_deref(), Some("USD"));
        // No quota windows: the row set for this provider is the wallet.
        assert!(!usage.session.available);
        assert!(!usage.weekly.available);
        assert!(usage.monthly.is_none());

        let requests = requests.join().unwrap();
        let request = requests.first().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /user/balance http/1.1"));
        assert!(request.contains("\r\nauthorization: bearer "));
        assert!(request.contains("\r\naccept: application/json\r\n"));
        assert!(!request.contains("\r\ncookie:"));
        // The key itself never leaves the process in a form we assert on.
        assert!(!request.contains("x-api-key"));
    }

    #[test]
    fn a_rejected_key_is_reported_as_an_auth_failure() {
        for status in [401u16, 403] {
            let (base, requests) = mock_server(
                vec![("/user/balance".into(), status, "{}".into())],
                1,
            );
            assert_eq!(
                poll_sources(Some(&credentials()), &balance_url(&base), 1_800_000_000)
                    .unwrap_err(),
                PollError::AuthRequired
            );
            requests.join().unwrap();
        }
    }

    #[test]
    fn rate_limits_and_broken_bodies_are_reported_as_request_failures() {
        for (status, body) in [
            (429u16, "{}"),
            (200, "{invalid"),
            (200, "{}"),
            (200, "null"),
            (200, r#"{"balance_infos":[{"currency":"USD","total_balance":"lots"}]}"#),
        ] {
            let (base, requests) =
                mock_server(vec![("/user/balance".into(), status, body.into())], 1);
            assert_eq!(
                poll_sources(Some(&credentials()), &balance_url(&base), 1_800_000_000)
                    .unwrap_err(),
                PollError::RequestFailed,
                "{status} {body}"
            );
            requests.join().unwrap();
        }
    }

    #[test]
    fn missing_credentials_are_reported_as_a_missing_login() {
        assert_eq!(
            poll_sources(None, "http://127.0.0.1:9/user/balance", 1_800_000_000).unwrap_err(),
            PollError::NoCredentials
        );
    }

    #[test]
    fn the_wallet_state_round_trips_through_the_settings_helpers() {
        let state = DeepSeekCreditsState {
            currency: "USD".into(),
            balance: 12.5,
            baseline: 20.0,
            seeded_at_unix: 1_800_000_000,
            updated_at_unix: 1_800_003_600,
        };
        app_settings::save_deepseek_credits(&state).unwrap();
        assert!(app_settings::deepseek_credits_path().is_file());
        assert_eq!(app_settings::load_deepseek_credits(), Some(state));
    }

    #[test]
    fn an_older_state_file_still_loads() {
        // Written before the burn clock existed: every missing field falls back
        // to a default instead of discarding the whole file.
        let path = app_settings::deepseek_credits_path();
        std::fs::write(&path, r#"{"currency":"USD","balance":9.5,"baseline":20.0}"#).unwrap();
        let state = app_settings::load_deepseek_credits().expect("an older file should load");
        assert_eq!(state.balance, 9.5);
        assert_eq!(state.baseline, 20.0);
        assert_eq!(state.seeded_at_unix, 0);
        assert_eq!(state.updated_at_unix, 0);
    }

    #[test]
    fn the_auth_file_supplies_the_deepseek_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let key = ["sk", "file", "key"].join("-");
        std::fs::write(
            &path,
            format!(
                r#"{{"apiKey":"cc-key","userId":"usr_example","deepseek":{{"type":"api","key":"{key}"}}}}"#
            ),
        )
        .unwrap();
        let credentials = read_api_key_from_auth_file(&path).unwrap();
        assert_eq!(credentials.api_key, key);
        assert_eq!(credentials.source, path.display().to_string());

        // Without the nested object there is nothing to read: the Command Code
        // key at the top level belongs to a different API.
        std::fs::write(&path, r#"{"apiKey":"cc-key","userId":"usr_example"}"#).unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());

        std::fs::write(&path, r#"{"deepseek":{"key":"   "}}"#).unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());

        std::fs::write(&path, r#"{"deepseek":{"key":"sk bad\r\nX-Test: injected"}}"#).unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());

        std::fs::write(&path, "not json").unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());
    }

    /// Live check against the real endpoint and the real on-disk key.
    /// Run with `cargo test live_deepseek_balance -- --ignored --nocapture`.
    #[test]
    #[ignore = "hits the live DeepSeek API"]
    fn live_deepseek_balance_returns_real_numbers() {
        let credentials =
            read_api_key_credentials().expect("no DeepSeek API key found on this machine");
        let usage = poll_sources(Some(&credentials), BALANCE_URL, now_unix())
            .expect("live balance request failed");
        println!("credential source: {}", credentials.source);
        let credits = usage.credits.expect("the live response carried no wallet");
        println!(
            "balance: {:.2} {} (baseline {:.2}, eta {:?})",
            credits.remaining,
            credits.currency.as_deref().unwrap_or("?"),
            credits.total,
            credits.eta_seconds
        );
        assert!(credits.remaining.is_finite());
    }
}

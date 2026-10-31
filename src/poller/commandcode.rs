use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::{build_agent, parse_iso8601, PollError};
use crate::diagnose;
use crate::models::{UsageData, UsageSection};

const CREDITS_URL: &str = "https://api.commandcode.ai/alpha/billing/credits";
const SUBSCRIPTIONS_URL: &str = "https://api.commandcode.ai/alpha/billing/subscriptions";
const USAGE_USER_AGENT: &str = concat!("claude-code-usage-monitor/", env!("CARGO_PKG_VERSION"));
const API_KEY_ENV: &str = "COMMAND_CODE_API_KEY";
/// Point at a different `auth.json` (used by tests and unusual installs).
const AUTH_FILE_ENV: &str = "COMMAND_CODE_AUTH_FILE";

/// Monthly allowance in whole US dollars, copied from the official `cmdc` CLI
/// bundle. The CLI matches a plan id exactly first and then falls back to the
/// longest key the plan id starts with, so `individual-goat-2026` still maps to
/// the goat tier while `individual-go` stays its own tier.
const PLAN_ALLOWANCES_USD: [(&str, f64); 9] = [
    ("individual-go", 10.0),
    ("individual-go-v1", 10.0),
    ("individual-goat", 70.0),
    ("individual-pro", 30.0),
    ("individual-pro-v1", 80.0),
    ("individual-provider", 15.0),
    ("individual-max", 150.0),
    ("individual-ultra", 300.0),
    ("teams-pro", 40.0),
];

/// Plain API key, as stored by the Command Code CLI or supplied by the user.
struct ApiKeyCredentials {
    api_key: String,
    source: String,
}

/// `~/.commandcode/auth.json`, written by the Command Code CLI on sign-in.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthFile {
    #[serde(default, alias = "api_key", alias = "key")]
    api_key: String,
}

// Wire shape of `GET /alpha/billing/credits`. Both rolling windows carry
// `used`/`cap` in dollars while `credits.monthlyCredits` is what is left of the
// plan's monthly allowance.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreditsPayload {
    credits: Option<CreditsSummary>,
    window_limits: Option<WindowLimits>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreditsSummary {
    #[serde(default)]
    monthly_credits: f64,
    /// Pay-as-you-go top-ups. They buy extra headroom beyond the plan, so they
    /// are deliberately excluded from the monthly allowance percentage.
    #[serde(default)]
    #[allow(dead_code)]
    purchased_credits: f64,
    #[serde(default)]
    #[allow(dead_code)]
    free_credits: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WindowLimits {
    five_hour: Option<LimitWindow>,
    weekly: Option<LimitWindow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LimitWindow {
    #[serde(default)]
    used: f64,
    #[serde(default)]
    cap: f64,
    /// Epoch milliseconds; zero means the window has not started yet.
    #[serde(default)]
    reset_at: f64,
    #[serde(default)]
    exceeded: bool,
}

// Wire shape of `GET /alpha/billing/subscriptions`.
#[derive(Deserialize)]
struct SubscriptionPayload {
    #[serde(default)]
    success: bool,
    data: Option<Subscription>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscription {
    #[serde(default)]
    plan_id: String,
    #[serde(default)]
    current_period_end: String,
}

pub(super) fn poll_command_code() -> Result<UsageData, PollError> {
    poll_sources(
        read_api_key_credentials().as_ref(),
        CREDITS_URL,
        SUBSCRIPTIONS_URL,
    )
}

pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    vec![credential_watch_signature()]
}

/// Command Code has a single credential: one API key. Without it there is
/// nothing to ask the billing API, so report a missing login rather than an
/// empty row.
fn poll_sources(
    credentials: Option<&ApiKeyCredentials>,
    credits_url: &str,
    subscriptions_url: &str,
) -> Result<UsageData, PollError> {
    let Some(credentials) = credentials else {
        diagnose::log("Command Code usage poll failed: no API key found");
        return Err(PollError::NoCredentials);
    };

    let credits = fetch_credits(credentials, credits_url)?;
    // The subscription only supplies the monthly denominator. A failure there
    // leaves the 5h and 7d windows intact, so it is logged instead of failing
    // the whole poll.
    let subscription = match fetch_subscription(credentials, subscriptions_url) {
        Ok(subscription) => Some(subscription),
        Err(error) => {
            diagnose::log(format!(
                "Command Code subscription poll failed via {}: {error:?}",
                credentials.source
            ));
            None
        }
    };
    usage_from_payload(credits, subscription.as_ref())
}

fn usage_from_payload(
    credits: CreditsPayload,
    subscription: Option<&Subscription>,
) -> Result<UsageData, PollError> {
    let Some(windows) = credits.window_limits else {
        diagnose::log("Command Code billing response carried no window limits");
        return Err(PollError::RequestFailed);
    };
    let session = match windows.five_hour.as_ref() {
        Some(window) => section_from_window(window),
        None => {
            diagnose::log("Command Code billing response omitted the five-hour window");
            UsageSection::default()
        }
    };
    let weekly = match windows.weekly.as_ref() {
        Some(window) => section_from_window(window),
        None => {
            diagnose::log("Command Code billing response omitted the weekly window");
            UsageSection::default()
        }
    };
    if !session.available && !weekly.available {
        diagnose::log("Command Code billing response had no usable usage windows");
        return Err(PollError::RequestFailed);
    }

    Ok(UsageData {
        limits: Vec::new(),
        session,
        session_label: Some("5h".to_string()),
        weekly,
        weekly_label: Some("7d".to_string()),
        monthly: monthly_section(credits.credits.as_ref(), subscription),
        credits: None,
        stale: false,
        ..Default::default()
    })
}

/// The monthly row measures the plan's allowance against what the account can
/// still spend. Without a known plan there is no denominator, so the row is
/// left out instead of guessing one.
fn monthly_section(
    credits: Option<&CreditsSummary>,
    subscription: Option<&Subscription>,
) -> Option<UsageSection> {
    let Some(subscription) = subscription else {
        diagnose::log("Command Code monthly window needs an active subscription");
        return None;
    };
    let Some(allowance) = plan_allowance_usd(&subscription.plan_id) else {
        diagnose::log(format!(
            "Command Code plan '{}' has no known monthly allowance",
            subscription.plan_id
        ));
        return None;
    };
    let Some(credits) = credits else {
        diagnose::log("Command Code billing response carried no credit balance");
        return None;
    };
    if !allowance.is_finite() || allowance <= 0.0 {
        diagnose::log("Command Code plan allowance is not a usable number");
        return None;
    }

    // Only the monthly balance counts against the plan allowance: purchased and
    // free credits are separate pay-as-you-go headroom and would otherwise make
    // a topped-up account read as untouched.
    let remaining = credits.monthly_credits;
    if !remaining.is_finite() {
        diagnose::log("Command Code credit balance is not a finite number");
        return None;
    }
    let percentage = ((allowance - remaining) / allowance * 100.0).clamp(0.0, 100.0);
    let resets_at = match parse_iso8601(Some(&subscription.current_period_end)) {
        Some(resets_at) => Some(resets_at),
        None => {
            diagnose::log("Command Code subscription contains an invalid period end");
            None
        }
    };
    Some(UsageSection {
        available: true,
        percentage,
        resets_at,
    })
}

fn section_from_window(window: &LimitWindow) -> UsageSection {
    UsageSection {
        available: true,
        percentage: window_percentage(window),
        resets_at: epoch_millis_to_system_time(window.reset_at),
    }
}

fn window_percentage(window: &LimitWindow) -> f64 {
    if !window.used.is_finite() || !window.cap.is_finite() {
        diagnose::log("Command Code usage window reported a non-finite amount");
        return 0.0;
    }
    if window.exceeded {
        diagnose::log("Command Code usage window is over its cap");
    }
    if window.cap <= 0.0 {
        // A window with no cap cannot be measured; report it as untouched
        // rather than as fully spent.
        return 0.0;
    }
    (window.used / window.cap * 100.0).clamp(0.0, 100.0)
}

/// Plan ids arrive in whichever shape the CLI stored them; normalise case and
/// underscores, then match exactly before falling back to the longest prefix.
fn plan_allowance_usd(plan_id: &str) -> Option<f64> {
    let normalized = plan_id.trim().to_ascii_lowercase().replace('_', "-");
    if normalized.is_empty() {
        return None;
    }
    if let Some((_, allowance)) = PLAN_ALLOWANCES_USD
        .iter()
        .find(|(key, _)| *key == normalized)
    {
        return Some(*allowance);
    }
    PLAN_ALLOWANCES_USD
        .iter()
        .filter(|(key, _)| normalized.starts_with(*key))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, allowance)| *allowance)
}

/// `resetAt` is epoch milliseconds and zero means "this window has not started
/// yet", which has no reset time to show.
fn epoch_millis_to_system_time(millis: f64) -> Option<SystemTime> {
    if !millis.is_finite() || millis <= 0.0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_millis(millis as u64))
}

fn fetch_credits(
    credentials: &ApiKeyCredentials,
    url: &str,
) -> Result<CreditsPayload, PollError> {
    fetch_json(credentials, url, "billing credits")
}

fn fetch_subscription(
    credentials: &ApiKeyCredentials,
    url: &str,
) -> Result<Subscription, PollError> {
    let payload: SubscriptionPayload = fetch_json(credentials, url, "billing subscriptions")?;
    if !payload.success {
        diagnose::log("Command Code subscription request was not successful");
    }
    payload.data.ok_or_else(|| {
        diagnose::log("Command Code subscription response carried no subscription");
        PollError::RequestFailed
    })
}

fn fetch_json<T: serde::de::DeserializeOwned>(
    credentials: &ApiKeyCredentials,
    url: &str,
    what: &str,
) -> Result<T, PollError> {
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
            diagnose::log(format!("Command Code {what} request was rejected"));
            return Err(PollError::AuthRequired);
        }
        Err(ureq::Error::StatusCode(429)) => {
            diagnose::log(format!("Command Code {what} request was rate limited"));
            return Err(PollError::RequestFailed);
        }
        Err(error) => {
            diagnose::log_error(&format!("Command Code {what} request failed"), error);
            return Err(PollError::RequestFailed);
        }
    };

    response
        .body_mut()
        .read_json::<Option<T>>()
        .map_err(|error| {
            diagnose::log_error(
                &format!("Command Code {what} response is not valid JSON"),
                error,
            );
            PollError::RequestFailed
        })?
        .ok_or_else(|| {
            diagnose::log(format!("Command Code {what} response was empty"));
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
    let api_key = auth.api_key.trim().to_string();
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

    // Wire shape verified against the live billing API: dollars for the two
    // rolling windows, epoch milliseconds for their resets, cents-like
    // precision for the monthly credit balance.
    const CREDITS_JSON: &str = r#"{
        "credits": {
            "belowThreshold": false,
            "creditThreshold": 0,
            "monthlyCredits": 34.9599109603,
            "purchasedCredits": 0,
            "freeCredits": 0
        },
        "windowLimits": {
            "limited": true,
            "exceeded": "weekly",
            "fiveHour": {"used": 0, "cap": 14, "exceeded": false, "resetAt": 0},
            "weekly": {"used": 35.0400890397, "cap": 35, "exceeded": true, "resetAt": 1793747161060}
        },
        "sandboxAccess": true
    }"#;

    const SUBSCRIPTIONS_JSON: &str = r#"{
        "success": true,
        "data": {
            "id": "sub_example",
            "status": "active",
            "createdAt": "2026-10-27T22:50:27.000Z",
            "currentPeriodStart": "2026-10-27T22:50:27.000Z",
            "currentPeriodEnd": "2026-11-27T22:50:27.000Z",
            "planId": "individual-goat"
        }
    }"#;

    fn credits() -> CreditsPayload {
        serde_json::from_str(CREDITS_JSON).unwrap()
    }

    fn subscription() -> Subscription {
        serde_json::from_str::<SubscriptionPayload>(SUBSCRIPTIONS_JSON)
            .unwrap()
            .data
            .unwrap()
    }

    fn api_key_credentials() -> ApiKeyCredentials {
        ApiKeyCredentials {
            api_key: ["cc", "test", "key"].join("_"),
            source: "test".into(),
        }
    }

    #[test]
    fn credits_map_both_windows_with_epoch_resets() {
        let usage = usage_from_payload(credits(), Some(&subscription())).unwrap();
        assert_eq!(usage.session_label.as_deref(), Some("5h"));
        assert!(usage.session.available);
        assert_eq!(usage.session.percentage, 0.0);
        // `resetAt: 0` means the window has not started, so it has no reset.
        assert_eq!(usage.session.resets_at, None);

        assert_eq!(usage.weekly_label.as_deref(), Some("7d"));
        assert!(usage.weekly.available);
        // 35.0400890397 of a 35 cap is over the limit and clamps to 100%.
        assert_eq!(usage.weekly.percentage, 100.0);
        assert_eq!(
            usage.weekly.resets_at,
            epoch_millis_to_system_time(1_793_747_161_060.0)
        );

        assert!(usage.credits.is_none());
        assert!(usage.limits.is_empty());
        assert!(!usage.stale);
    }

    #[test]
    fn monthly_window_measures_the_plan_allowance_against_the_credit_balance() {
        let usage = usage_from_payload(credits(), Some(&subscription())).unwrap();
        let monthly = usage.monthly.as_ref().unwrap();
        assert!(monthly.available);
        let expected = (70.0 - 34.959_910_960_3) / 70.0 * 100.0;
        assert!((monthly.percentage - expected).abs() < 1e-9);
        assert_eq!(
            monthly.resets_at,
            parse_iso8601(Some("2026-11-27T22:50:27Z"))
        );
    }

    #[test]
    fn purchased_and_free_credits_do_not_distort_the_monthly_allowance() {
        let json = CREDITS_JSON
            .replace("\"purchasedCredits\": 0", "\"purchasedCredits\": 25.0")
            .replace("\"freeCredits\": 0", "\"freeCredits\": 5.0");
        let usage =
            usage_from_payload(serde_json::from_str(&json).unwrap(), Some(&subscription())).unwrap();
        let monthly = usage.monthly.as_ref().unwrap();
        let expected = (70.0 - 34.959_910_960_3) / 70.0 * 100.0;
        assert!(
            (monthly.percentage - expected).abs() < 1e-9,
            "pay-as-you-go credits must not be counted as unspent allowance"
        );
    }

    #[test]
    fn unknown_plan_leaves_the_monthly_window_out() {
        let mut unknown = subscription();
        unknown.plan_id = "enterprise-unknown".into();
        let usage = usage_from_payload(credits(), Some(&unknown)).unwrap();
        assert!(usage.monthly.is_none(), "unknown plans have no denominator");
        // The short windows do not depend on the plan table.
        assert!(usage.session.available);
        assert!(usage.weekly.available);

        let mut unpublished = subscription();
        unpublished.plan_id = String::new();
        assert!(usage_from_payload(credits(), Some(&unpublished))
            .unwrap()
            .monthly
            .is_none());
        assert!(usage_from_payload(credits(), None).unwrap().monthly.is_none());
    }

    #[test]
    fn plan_allowances_match_the_official_cli_table() {
        for (plan, allowance) in [
            ("individual-go", 10.0),
            ("individual-go-v1", 10.0),
            ("individual-goat", 70.0),
            ("individual-pro", 30.0),
            ("individual-pro-v1", 80.0),
            ("individual-provider", 15.0),
            ("individual-max", 150.0),
            ("individual-ultra", 300.0),
            ("teams-pro", 40.0),
        ] {
            assert_eq!(plan_allowance_usd(plan), Some(allowance), "{plan}");
            assert_eq!(
                plan_allowance_usd(&plan.to_ascii_uppercase()),
                Some(allowance),
                "{plan} in upper case"
            );
            assert_eq!(
                plan_allowance_usd(&plan.replace('-', "_")),
                Some(allowance),
                "{plan} with underscores"
            );
        }
        // Longest prefix wins, so `individual-go` must not swallow `goat`.
        assert_eq!(plan_allowance_usd("individual-goat-2026"), Some(70.0));
        assert_eq!(plan_allowance_usd("individual-pro-v1-legacy"), Some(80.0));
        assert_eq!(plan_allowance_usd("Individual_Goat"), Some(70.0));
        assert_eq!(plan_allowance_usd("individual-gold"), Some(10.0));
        assert_eq!(plan_allowance_usd("enterprise-unknown"), None);
        assert_eq!(plan_allowance_usd(""), None);
        assert_eq!(plan_allowance_usd("   "), None);
    }

    #[test]
    fn zero_and_broken_window_amounts_never_report_usage() {
        let window = |used: f64, cap: f64, exceeded: bool| LimitWindow {
            used,
            cap,
            reset_at: 0.0,
            exceeded,
        };
        assert_eq!(window_percentage(&window(0.0, 0.0, false)), 0.0);
        assert_eq!(window_percentage(&window(5.0, 0.0, false)), 0.0);
        assert_eq!(window_percentage(&window(7.0, 14.0, false)), 50.0);
        assert_eq!(window_percentage(&window(21.0, 14.0, true)), 100.0);
        assert_eq!(window_percentage(&window(f64::NAN, 14.0, false)), 0.0);
        assert_eq!(window_percentage(&window(7.0, f64::INFINITY, false)), 0.0);
    }

    #[test]
    fn epoch_milliseconds_convert_to_absolute_reset_times() {
        assert_eq!(epoch_millis_to_system_time(0.0), None);
        assert_eq!(epoch_millis_to_system_time(-1_000.0), None);
        assert_eq!(epoch_millis_to_system_time(f64::NAN), None);
        assert_eq!(
            epoch_millis_to_system_time(1_000.0),
            Some(UNIX_EPOCH + Duration::from_secs(1))
        );
        assert_eq!(
            epoch_millis_to_system_time(1_793_747_161_060.0),
            Some(UNIX_EPOCH + Duration::from_millis(1_793_747_161_060))
        );
    }

    #[test]
    fn missing_window_limits_are_rejected() {
        for json in [
            r#"{"credits":{"monthlyCredits":1}}"#,
            r#"{"windowLimits":{}}"#,
            r#"{"windowLimits":{"limited":true}}"#,
        ] {
            let payload: CreditsPayload = serde_json::from_str(json).unwrap();
            assert_eq!(
                usage_from_payload(payload, Some(&subscription())),
                Err(PollError::RequestFailed),
                "{json}"
            );
        }
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

    fn billing_urls(base: &str) -> (String, String) {
        (
            format!("{base}/alpha/billing/credits"),
            format!("{base}/alpha/billing/subscriptions"),
        )
    }

    fn both_routes(credits_body: &str, subscriptions_body: &str) -> Vec<(String, u16, String)> {
        vec![
            (
                "/alpha/billing/credits".into(),
                200,
                credits_body.to_string(),
            ),
            (
                "/alpha/billing/subscriptions".into(),
                200,
                subscriptions_body.to_string(),
            ),
        ]
    }

    #[test]
    fn billing_requests_send_a_bearer_token_without_a_cookie() {
        let (base, requests) = mock_server(both_routes(CREDITS_JSON, SUBSCRIPTIONS_JSON), 2);
        let (credits_url, subscriptions_url) = billing_urls(&base);
        let credentials = api_key_credentials();
        let usage = poll_sources(Some(&credentials), &credits_url, &subscriptions_url).unwrap();
        assert_eq!(usage.session.percentage, 0.0);
        assert_eq!(usage.weekly.percentage, 100.0);
        assert!(usage.monthly.is_some());

        let requests = requests.join().unwrap();
        let bearer = format!("bearer {}", credentials.api_key);
        for (request, path) in requests
            .iter()
            .zip(["/alpha/billing/credits", "/alpha/billing/subscriptions"])
        {
            let request = request.to_ascii_lowercase();
            assert!(request.starts_with(&format!("get {path} http/1.1")));
            assert!(request.contains(&format!("\r\nauthorization: {bearer}\r\n")));
            assert!(request.contains("\r\naccept: application/json\r\n"));
            assert!(!request.contains("\r\ncookie:"));
        }
    }

    #[test]
    fn rejected_api_keys_and_broken_bodies_are_reported() {
        for status in [401u16, 403] {
            let (base, requests) = mock_server(
                vec![("/alpha/billing/credits".into(), status, "{}".into())],
                1,
            );
            let (credits_url, subscriptions_url) = billing_urls(&base);
            assert_eq!(
                poll_sources(Some(&api_key_credentials()), &credits_url, &subscriptions_url)
                    .unwrap_err(),
                PollError::AuthRequired
            );
            requests.join().unwrap();
        }

        for body in [
            "{invalid",
            "{}",
            "null",
            r#"{"windowLimits":{"fiveHour":{"used":"high"}}}"#,
        ] {
            let (base, requests) = mock_server(both_routes(body, SUBSCRIPTIONS_JSON), 1);
            let (credits_url, subscriptions_url) = billing_urls(&base);
            assert_eq!(
                poll_sources(Some(&api_key_credentials()), &credits_url, &subscriptions_url)
                    .unwrap_err(),
                PollError::RequestFailed,
                "{body}"
            );
            requests.join().unwrap();
        }
    }

    #[test]
    fn a_failing_subscription_request_keeps_the_short_windows() {
        let (base, requests) = mock_server(
            vec![
                (
                    "/alpha/billing/credits".into(),
                    200,
                    CREDITS_JSON.to_string(),
                ),
                ("/alpha/billing/subscriptions".into(), 500, "{}".into()),
            ],
            2,
        );
        let (credits_url, subscriptions_url) = billing_urls(&base);
        let usage = poll_sources(
            Some(&api_key_credentials()),
            &credits_url,
            &subscriptions_url,
        )
        .unwrap();
        assert_eq!(usage.session_label.as_deref(), Some("5h"));
        assert!(usage.monthly.is_none());
        requests.join().unwrap();
    }

    #[test]
    fn missing_command_code_credentials_are_reported_as_missing_login() {
        assert_eq!(
            poll_sources(
                None,
                "http://127.0.0.1:9/alpha/billing/credits",
                "http://127.0.0.1:9/alpha/billing/subscriptions"
            )
            .unwrap_err(),
            PollError::NoCredentials
        );
    }

    #[test]
    fn auth_file_supplies_the_command_code_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let key = ["cc", "file", "key"].join("_");
        std::fs::write(
            &path,
            format!(
                r#"{{"apiKey":"{key}","userId":"usr_example","userName":"example","keyName":"default"}}"#
            ),
        )
        .unwrap();
        let credentials = read_api_key_from_auth_file(&path).unwrap();
        assert_eq!(credentials.api_key, key);
        assert_eq!(credentials.source, path.display().to_string());

        // The CLI also writes camelCase metadata; only the key matters here.
        std::fs::write(&path, r#"{"userId":"usr_example"}"#).unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());

        std::fs::write(&path, r#"{"apiKey":"   "}"#).unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());

        std::fs::write(&path, r#"{"apiKey":"cc bad\r\nX-Test: injected"}"#).unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());

        std::fs::write(&path, "not json").unwrap();
        assert!(read_api_key_from_auth_file(&path).is_none());

        assert!(!valid_api_key(""));
        assert!(!valid_api_key("cc key with spaces"));
        assert!(valid_api_key(&["cc", "file", "key"].join("_")));
    }

    /// Live check against the real endpoints and the real on-disk key.
    /// Run with `cargo test live_command_code_api_key -- --ignored --nocapture`.
    #[test]
    #[ignore = "hits the live Command Code API"]
    fn live_command_code_api_key_returns_real_numbers() {
        let credentials =
            read_api_key_credentials().expect("no Command Code API key found on this machine");
        let usage = poll_sources(Some(&credentials), CREDITS_URL, SUBSCRIPTIONS_URL)
            .expect("live usage request failed");
        println!("credential source: {}", credentials.source);
        for (name, section) in [
            ("session", Some(&usage.session)),
            ("weekly", Some(&usage.weekly)),
            ("monthly", usage.monthly.as_ref()),
        ] {
            match section {
                Some(section) => println!(
                    "{name}: {:.2}% used, resets at {:?}",
                    section.percentage, section.resets_at
                ),
                None => println!("{name}: not reported"),
            }
        }
        assert!(
            usage.session.available || usage.weekly.available,
            "live response carried no windows"
        );
    }
}

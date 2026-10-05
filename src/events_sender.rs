//! Dedicated, authenticated HTTPS sender for Fabric MCP Events.
//! The CLI owner mounts this router behind an Access-protected Tunnel. It is
//! deliberately incapable of forwarding arbitrary methods, headers or bodies.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::lookup_host;
use url::{Host, Url};
use uuid::Uuid;

const MAX_BODY: usize = 262_144;
const MAX_RESPONSE: usize = 4096;

#[derive(Clone)]
struct SenderState {
    bearer: Arc<str>,
}

/// Mount only on a dedicated listener behind Cloudflare Access. The bearer is
/// supplied by runtime injection and must never be logged or put in metadata.
pub fn router(bearer: String) -> Router {
    Router::new()
        .route("/events/send", post(send))
        .route("/events/health", get(health))
        .layer(DefaultBodyLimit::max(300 * 1024))
        .with_state(SenderState {
            bearer: bearer.into(),
        })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SendEnvelope {
    kind: String,
    url: String,
    subscription_id: String,
    secret: String,
    previous_secret: Option<String>,
    challenge: Option<String>,
    event_body: Option<String>,
}

#[derive(Serialize)]
struct SendResult {
    status: u16,
    verified: bool,
}

fn authenticated(headers: &HeaderMap, state: &SenderState) -> bool {
    let supplied = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let expected = format!("Bearer {}", state.bearer);
    state.bearer.len() >= 32 && constant_time_equal(supplied.as_bytes(), expected.as_bytes())
}

async fn health(State(state): State<SenderState>, headers: HeaderMap) -> StatusCode {
    if authenticated(&headers, &state) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::UNAUTHORIZED
    }
}

async fn send(State(state): State<SenderState>, headers: HeaderMap, body: Bytes) -> Response {
    // Parse manually so typed serde errors can never echo a secret or caller
    // supplied field name in an ordinary HTTP response.
    let input = serde_json::from_slice::<SendEnvelope>(&body);
    let result = match input {
        Ok(input) => send_checked(state, headers, input).await,
        Err(_) => Err((StatusCode::BAD_REQUEST, "invalid_envelope")),
    };
    match result {
        Ok(value) => Json(value).into_response(),
        Err((status, code)) => (status, Json(json!({"error": code}))).into_response(),
    }
}

async fn send_checked(
    state: SenderState,
    headers: HeaderMap,
    input: SendEnvelope,
) -> Result<SendResult, (StatusCode, &'static str)> {
    if !authenticated(&headers, &state) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/json")
    {
        return Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, "invalid_content_type"));
    }
    let parsed = safe_url(&input.url).ok_or((StatusCode::BAD_REQUEST, "invalid_url"))?;
    if !valid_digest_identifier(&input.subscription_id, "sub_") {
        return Err((StatusCode::BAD_REQUEST, "invalid_subscription"));
    }
    let secret = signing_key(&input.secret).ok_or((StatusCode::BAD_REQUEST, "invalid_secret"))?;
    let old = input
        .previous_secret
        .as_deref()
        .map(signing_key)
        .transpose_option()
        .ok_or((StatusCode::BAD_REQUEST, "invalid_secret"))?;
    let (webhook_id, body, challenge) = match input.kind.as_str() {
        "verification" if input.previous_secret.is_none() && input.event_body.is_none() => {
            let challenge = input
                .challenge
                .as_deref()
                .filter(|value| Uuid::parse_str(value).is_ok())
                .ok_or((StatusCode::BAD_REQUEST, "invalid_challenge"))?;
            (
                format!("msg_verification_{}", Uuid::new_v4()),
                serde_json::to_vec(&json!({"type":"verification","challenge":challenge})).unwrap(),
                Some(challenge.to_owned()),
            )
        }
        "delivery" if input.challenge.is_none() => {
            let body = input
                .event_body
                .as_deref()
                .ok_or((StatusCode::BAD_REQUEST, "invalid_event"))?;
            if body.len() > MAX_BODY {
                return Err((StatusCode::PAYLOAD_TOO_LARGE, "event_too_large"));
            }
            let event: Value = serde_json::from_str(body)
                .map_err(|_| (StatusCode::BAD_REQUEST, "invalid_event"))?;
            let event_id = event
                .get("eventId")
                .and_then(Value::as_str)
                .filter(|value| valid_digest_identifier(value, "evt_"))
                .ok_or((StatusCode::BAD_REQUEST, "invalid_event"))?;
            let name = event.get("name").and_then(Value::as_str).unwrap_or("");
            if !matches!(name, "job.state.changed" | "session.state.changed")
                || !valid_event_data(name, event.get("data"))
                || !event
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .is_some_and(valid_utc_timestamp)
                || event.get("timestamp")
                    != event.get("data").and_then(|data| data.get("timestamp"))
                || event.get("cursor") != Some(&Value::Null)
                || event.as_object().is_none_or(|object| object.len() != 5)
            {
                return Err((StatusCode::BAD_REQUEST, "invalid_event"));
            }
            (event_id.to_owned(), body.as_bytes().to_vec(), None)
        }
        _ => return Err((StatusCode::BAD_REQUEST, "invalid_envelope")),
    };
    if body.len() > MAX_BODY {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "event_too_large"));
    }

    // Re-resolve on every call, validate the complete answer set, and pin the
    // original DNS name to validated sockets while retaining hostname TLS/SNI.
    let host = parsed
        .host_str()
        .ok_or((StatusCode::BAD_REQUEST, "invalid_url"))?;
    let port = parsed
        .port_or_known_default()
        .ok_or((StatusCode::BAD_REQUEST, "invalid_url"))?;
    let sockets = match parsed
        .host()
        .ok_or((StatusCode::BAD_REQUEST, "invalid_url"))?
    {
        Host::Ipv4(ip) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Host::Ipv6(ip) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        Host::Domain(domain) => resolve_public(domain, port)
            .await
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid_address"))?,
    };
    if sockets.iter().any(|socket| !public_ip(socket.ip())) {
        return Err((StatusCode::BAD_REQUEST, "invalid_address"));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(0)
        .resolve_to_addrs(host, &sockets)
        .build()
        .map_err(|_| (StatusCode::BAD_GATEWAY, "transport_unavailable"))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "clock_unavailable"))?
        .as_secs()
        .to_string();
    let mut signatures = vec![signature(&secret, &webhook_id, &timestamp, &body)];
    if let Some(old) = old {
        signatures.push(signature(&old, &webhook_id, &timestamp, &body));
    }
    let response = client
        .post(parsed)
        .header("Content-Type", "application/json")
        .header("webhook-id", webhook_id)
        .header("webhook-timestamp", timestamp)
        .header("webhook-signature", signatures.join(" "))
        .header("X-MCP-Subscription-Id", input.subscription_id)
        .body(body)
        .send()
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "delivery_failed"))?;
    tokio::time::timeout(
        Duration::from_secs(10),
        finish_response(response, challenge.as_deref()),
    )
    .await
    .map_err(|_| (StatusCode::GATEWAY_TIMEOUT, "response_timeout"))?
}

async fn finish_response(
    mut response: reqwest::Response,
    challenge: Option<&str>,
) -> Result<SendResult, (StatusCode, &'static str)> {
    let status = response.status().as_u16();
    let Some(challenge) = challenge else {
        // Application delivery only needs the status. In particular, a 410 or
        // 413 with a large, slow, or broken body remains terminal.
        return Ok(SendResult {
            status,
            verified: false,
        });
    };
    let mut received = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "response_failed"))?
    {
        if chunk.len() > MAX_RESPONSE - received.len() {
            return Err((StatusCode::BAD_GATEWAY, "response_too_large"));
        }
        received.extend_from_slice(&chunk);
    }
    let echoed = serde_json::from_slice::<Value>(&received)
        .ok()
        .and_then(|value| {
            value
                .get("challenge")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let verified = (200..300).contains(&status)
        && echoed.is_some_and(|value| constant_time_equal(value.as_bytes(), challenge.as_bytes()));
    Ok(SendResult { status, verified })
}

trait TransposeOption<T> {
    fn transpose_option(self) -> Option<Option<T>>;
}
impl<T> TransposeOption<T> for Option<Option<T>> {
    fn transpose_option(self) -> Option<Option<T>> {
        match self {
            Some(Some(value)) => Some(Some(value)),
            Some(None) => None,
            None => Some(None),
        }
    }
}

fn signing_key(value: &str) -> Option<Vec<u8>> {
    let encoded = value.strip_prefix("whsec_")?;
    let bytes = STANDARD.decode(encoded).ok()?;
    (bytes.len() >= 24 && bytes.len() <= 64 && STANDARD.encode(&bytes) == encoded).then_some(bytes)
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    const COMPARISON_BOUND: usize = 4096;
    let mut difference = left.len() ^ right.len();
    difference |= usize::from(left.len() > COMPARISON_BOUND || right.len() > COMPARISON_BOUND);
    for index in 0..COMPARISON_BOUND {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn valid_event_data(name: &str, value: Option<&Value>) -> bool {
    let Some(object) = value.and_then(Value::as_object) else {
        return false;
    };
    let expected: &[&str] = if name == "job.state.changed" {
        &[
            "host_id",
            "session_id",
            "job_id",
            "previous_state",
            "state",
            "timestamp",
        ]
    } else {
        &[
            "host_id",
            "session_id",
            "previous_state",
            "state",
            "timestamp",
        ]
    };
    if object.len() != expected.len() || object.keys().any(|key| !expected.contains(&key.as_str()))
    {
        return false;
    }
    let host = object.get("host_id").and_then(Value::as_str).unwrap_or("");
    let session = object
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let state = object.get("state").and_then(Value::as_str).unwrap_or("");
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("");
    let previous = object.get("previous_state");
    valid_host_id(host)
        && valid_session_id(session)
        && valid_state(name, state)
        && valid_utc_timestamp(timestamp)
        && previous.is_some_and(|value| {
            value.is_null() || value.as_str().is_some_and(|text| valid_state(name, text))
        })
        && (name != "job.state.changed"
            || object
                .get("job_id")
                .and_then(Value::as_str)
                .is_some_and(valid_job_id))
}

fn signature(key: &[u8], id: &str, timestamp: &str, body: &[u8]) -> String {
    let signing_key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut data = Vec::with_capacity(id.len() + timestamp.len() + body.len() + 2);
    data.extend_from_slice(id.as_bytes());
    data.push(b'.');
    data.extend_from_slice(timestamp.as_bytes());
    data.push(b'.');
    data.extend_from_slice(body);
    format!(
        "v1,{}",
        STANDARD.encode(hmac::sign(&signing_key, &data).as_ref())
    )
}

fn valid_digest_identifier(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 64 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn valid_host_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_job_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn valid_state(name: &str, value: &str) -> bool {
    if name == "job.state.changed" {
        matches!(
            value,
            "running" | "completed" | "failed" | "stopped" | "unknown"
        )
    } else {
        matches!(
            value,
            "starting" | "active" | "stopping" | "stopped" | "crashed" | "failed"
        )
    }
}

fn valid_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.len() > 35
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || *bytes.last().unwrap() != b'Z'
    {
        return false;
    }
    for index in [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
        if !bytes[index].is_ascii_digit() {
            return false;
        }
    }
    if bytes.len() > 20
        && (bytes.len() < 22
            || bytes[19] != b'.'
            || !bytes[20..bytes.len() - 1].iter().all(u8::is_ascii_digit))
    {
        return false;
    }
    let number = |start: usize, end: usize| -> u32 { value[start..end].parse().unwrap_or(0) };
    let year = number(0, 4);
    let month = number(5, 7);
    let day = number(8, 10);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    year > 0
        && day > 0
        && day <= days
        && number(11, 13) <= 23
        && number(14, 16) <= 59
        && number(17, 19) <= 59
}

fn safe_url(value: &str) -> Option<Url> {
    if value.len() > 2048 {
        return None;
    }
    let url = Url::parse(value).ok()?;
    let host = url.host_str()?;
    if url.scheme() != "https"
        || url.port().is_some_and(|port| port != 443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || host.ends_with(".local")
        || host.ends_with(".localhost")
        || host.ends_with(".internal")
        || host == "localhost"
    {
        return None;
    }
    Some(url)
}

async fn resolve_public(host: &str, port: u16) -> Result<Vec<SocketAddr>, ()> {
    let addresses: Vec<_> = tokio::time::timeout(Duration::from_secs(3), lookup_host((host, port)))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?
        .collect();
    if addresses.is_empty()
        || addresses.len() > 64
        || addresses.iter().any(|address| !public_ip(address.ip()))
    {
        return Err(());
    }
    Ok(addresses)
}

pub fn public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !((a == 0)
        || (a == 10)
        || (a == 100 && (64..=127).contains(&b))
        || (a == 127)
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && (b == 0 || b == 168 || (b == 88 && c == 99)))
        || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return public_v4(mapped);
    }
    let first = ip.segments()[0];
    // Global unicast only. Exclude IETF special-purpose and documentation
    // ranges, translation/tunnel prefixes, and all local/multicast space.
    (first & 0xe000) == 0x2000
        && !(first == 0x2001 && (ip.segments()[1] & 0xfe00) == 0)
        && !(first == 0x2001 && ip.segments()[1] == 0x0db8)
        && first != 0x2002
        && !(first == 0x3fff && (ip.segments()[1] & 0xf000) == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    #[test]
    fn rejects_nonpublic_and_mapped_destinations() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "100.100.100.200",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "{address}");
        }
        assert!(public_ip("8.8.8.8".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
    #[test]
    fn rejects_unsafe_urls_and_secret_shapes() {
        for url in [
            "http://example.com",
            "https://localhost/a",
            "https://user@example.com/a",
            "https://example.local/a",
            "https://example.com/#fragment",
        ] {
            assert!(safe_url(url).is_none());
        }
        assert!(safe_url("https://example.com/a").is_some());
        assert!(signing_key("whsec_bad").is_none());
        let secret = format!("whsec_{}", STANDARD.encode([7_u8; 32]));
        assert_eq!(signing_key(&secret).unwrap().len(), 32);
    }
    #[test]
    fn signs_exact_body_bytes() {
        let key = [8_u8; 32];
        assert_ne!(
            signature(&key, "evt_1", "10", b"{\"a\":1}"),
            signature(&key, "evt_1", "10", b"{ \"a\":1}")
        );
        assert_ne!(
            signature(&key, "evt_1", "10", b"{}"),
            signature(&key, "evt_1", "11", b"{}")
        );
    }

    #[test]
    fn envelope_schema_rejects_unbounded_or_malformed_fields() {
        assert!(valid_digest_identifier(
            &format!("evt_{}", "a".repeat(64)),
            "evt_"
        ));
        assert!(!valid_digest_identifier("evt_1", "evt_"));
        assert!(!valid_digest_identifier(
            &format!("sub_{}", "z".repeat(64)),
            "sub_"
        ));
        assert!(valid_utc_timestamp("2026-10-05T00:00:00Z"));
        assert!(valid_utc_timestamp("2026-10-05T00:00:00.123Z"));
        for invalid in [
            "2026-13-05T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-10-05T25:00:00Z",
            "2026-10-05T00:00:00.Z",
            "tomorrow",
        ] {
            assert!(!valid_utc_timestamp(invalid), "{invalid}");
        }
        let payload = json!({"host_id":"host-a","session_id":"session-a","job_id":"job-1",
            "previous_state":null,"state":"completed","timestamp":"2026-10-05T00:00:00Z"});
        assert!(valid_event_data("job.state.changed", Some(&payload)));
        let mut malformed = payload;
        malformed["state"] = json!("arbitrary");
        assert!(!valid_event_data("job.state.changed", Some(&malformed)));
        malformed["state"] = json!("completed");
        malformed["secret"] = json!("do-not-return");
        assert!(!valid_event_data("job.state.changed", Some(&malformed)));
    }

    #[tokio::test]
    async fn delivery_classification_never_reads_application_response_body() {
        for status in [410, 413] {
            let response: reqwest::Response = axum::http::Response::builder()
                .status(status)
                .body("x".repeat(MAX_RESPONSE + 1))
                .unwrap()
                .into();
            let result = finish_response(response, None).await.unwrap();
            assert_eq!(result.status, status);
            assert!(!result.verified);
        }
        let mismatch: reqwest::Response = axum::http::Response::builder()
            .status(200)
            .body(r#"{"challenge":"different"}"#)
            .unwrap()
            .into();
        assert!(
            !finish_response(mismatch, Some("expected"))
                .await
                .unwrap()
                .verified
        );
        let oversized: reqwest::Response = axum::http::Response::builder()
            .status(200)
            .body("x".repeat(MAX_RESPONSE + 1))
            .unwrap()
            .into();
        assert_eq!(
            finish_response(oversized, Some("expected"))
                .await
                .err()
                .unwrap()
                .1,
            "response_too_large"
        );
    }

    #[tokio::test]
    async fn typed_serde_errors_never_echo_secret_values() {
        let request = Request::builder().method("POST").uri("/events/send")
            .header("authorization", format!("Bearer {}", "b".repeat(40)))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"kind":"delivery","url":"https://example.com/","subscriptionId":"sub_bad","secret":{"private":"SENSITIVE_SENTINEL"}}"#)).unwrap();
        let response = router("b".repeat(40)).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(body.as_ref(), br#"{"error":"invalid_envelope"}"#);
    }

    #[tokio::test]
    async fn malformed_or_oversized_delivery_is_rejected_before_dns() {
        let state = SenderState {
            bearer: Arc::from("b".repeat(40)),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", "b".repeat(40)).parse().unwrap(),
        );
        headers.insert("content-type", "application/json".parse().unwrap());
        let key = format!("whsec_{}", STANDARD.encode([9_u8; 32]));
        let mut envelope = SendEnvelope {
            kind: "delivery".into(),
            url: "https://example.com/".into(),
            subscription_id: format!("sub_{}", "a".repeat(64)),
            secret: key,
            previous_secret: None,
            challenge: None,
            event_body: Some("x".repeat(MAX_BODY + 1)),
        };
        assert_eq!(
            send_checked(state.clone(), headers.clone(), envelope)
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let invalid = json!({"eventId":format!("evt_{}", "a".repeat(64)),"name":"job.state.changed",
            "timestamp":"not-a-timestamp","data":{"host_id":"host-a","session_id":"session-a",
            "job_id":"job-a","previous_state":null,"state":"completed","timestamp":"not-a-timestamp"},"cursor":null});
        envelope = SendEnvelope {
            kind: "delivery".into(),
            url: "https://example.com/".into(),
            subscription_id: format!("sub_{}", "a".repeat(64)),
            secret: format!("whsec_{}", STANDARD.encode([9_u8; 32])),
            previous_secret: None,
            challenge: None,
            event_body: Some(invalid.to_string()),
        };
        assert_eq!(
            send_checked(state, headers, envelope)
                .await
                .err()
                .unwrap()
                .1,
            "invalid_event"
        );
    }
}

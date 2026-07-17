//! Alert webhook sinks (RFC-0008 C5): deliver flag/hit annotations (and their reorg retractions) to
//! operator-configured HTTP endpoints, **at-least-once**, without ever blocking the indexer.
//!
//! Delivery semantics are host state by nature, so they live here rather than in a WASM component:
//! - **Durable outbox** in the hot store (redb) — enqueuing an alert is one fast write, and pending
//!   deliveries survive a restart, so at-least-once holds across a process bounce.
//! - **A stalled sink never blocks indexing.** Enqueue is decoupled from delivery (a background
//!   worker drains the outbox); if a webhook is down the outbox grows only to a bound, then sheds its
//!   oldest entries loudly (`outbox_trim`) — the indexer never waits on the network.
//! - **Retractions.** A reorg re-fires the same annotation as a `flag_retracted` event, so a consumer
//!   that acted on a flag learns when the chain took it back.
//! - **One cursor.** Alerts ride the single indexing cursor; there is no second cursor or reconciler.
//!
//! The endpoint is the allowlist: a sink only ever POSTs to a URL the nest's `[[alerts]]` declares.
//! `wasi:http`-sandboxed egress (the C4 grant model) is available for an untrusted enricher, but a
//! webhook sink is operator-configured and delivery-guaranteed, so the host owns it.

use crate::config::Alert;
use crate::store::Store;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;

/// Bound on the durable outbox. A dead webhook can't grow it without limit; past this the oldest
/// undelivered alerts are shed (loudly). Generous — a transient outage of minutes is absorbed.
pub const OUTBOX_MAX: u64 = 10_000;
/// How many pending deliveries a single drain attempts.
const DELIVERY_BATCH: usize = 100;
/// Poll interval between outbox drains — also the (constant) retry backoff for a failed delivery.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Per-request timeout: a slow endpoint can't wedge the delivery worker.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Routes annotation kinds to the webhook sinks that want them. Built once from `[[alerts]]`.
#[derive(Clone, Default)]
pub struct AlertRouter {
    sinks: Vec<Alert>,
}

impl AlertRouter {
    pub fn new(sinks: Vec<Alert>) -> Self {
        Self { sinks }
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }

    /// The URLs that want annotations of `kind`.
    fn urls_for(&self, kind: &str) -> Vec<&str> {
        self.sinks
            .iter()
            .filter(|s| s.kinds.iter().any(|k| k == kind))
            .map(|s| s.url.as_str())
            .collect()
    }

    /// Whether any sink watches `kind` — a cheap pre-check before building a retraction payload.
    pub fn watches(&self, kind: &str) -> bool {
        self.sinks.iter().any(|s| s.kinds.iter().any(|k| k == kind))
    }
}

/// Enqueue an alert for every sink that watches `kind`. `event` is `"flag"` or `"flag_retracted"`.
/// A no-op when nothing is configured or nothing matches. Bounds the outbox after pushing.
pub fn enqueue(
    store: &Store,
    router: &AlertRouter,
    event: &str,
    kind: &str,
    annotation: &Value,
) -> Result<()> {
    if router.is_empty() {
        return Ok(());
    }
    let mut pushed = false;
    for url in router.urls_for(kind) {
        let payload = json!({
            "event": event,
            "kind": kind,
            "url": url,
            "annotation": annotation,
        });
        store.outbox_push(&payload.to_string())?;
        pushed = true;
    }
    if pushed {
        let dropped = store.outbox_trim(OUTBOX_MAX)?;
        if dropped > 0 {
            tracing::warn!(
                "alert outbox full ({OUTBOX_MAX}): dropped {dropped} oldest undelivered alert(s) — \
                 a webhook is down or too slow"
            );
        }
    }
    Ok(())
}

/// Drain up to [`DELIVERY_BATCH`] pending deliveries once, POSTing the exact stored payload bytes to
/// its URL (so an HMAC signature covers what's actually sent). A payload whose `webhook` names a
/// secret in `secrets` gets an `X-Nuthatch-Signature: sha256=<hmac>` header (RFC-0010 Part B). A
/// delivered entry (2xx) is removed; a failure is left for retry (at-least-once) and doesn't stop the
/// drain. Returns how many were delivered.
pub async fn deliver_pending(
    store: &Store,
    client: &reqwest::Client,
    secrets: &std::collections::HashMap<String, String>,
) -> usize {
    let pending = store.outbox_pending(DELIVERY_BATCH).unwrap_or_default();
    let mut delivered = 0usize;
    for (seq, payload_str) in pending {
        let Ok(payload) = serde_json::from_str::<Value>(&payload_str) else {
            let _ = store.outbox_remove(seq); // corrupt entry — shed it
            continue;
        };
        let url = payload.get("url").and_then(Value::as_str).unwrap_or("");
        // Send the stored bytes verbatim so a signature matches; sign if the webhook has a secret.
        let mut req = client
            .post(url)
            .header("content-type", "application/json")
            .body(payload_str.clone());
        if let Some(secret) = payload
            .get("webhook")
            .and_then(Value::as_str)
            .and_then(|n| secrets.get(n))
        {
            let sig = crate::webhooks::hmac_sha256_hex(secret.as_bytes(), payload_str.as_bytes());
            req = req.header("X-Nuthatch-Signature", format!("sha256={sig}"));
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let _ = store.outbox_remove(seq);
                delivered += 1;
            }
            Ok(resp) => {
                tracing::warn!(
                    "alert webhook {url} returned {} — will retry",
                    resp.status()
                );
            }
            Err(e) => {
                tracing::warn!("alert webhook {url} delivery failed: {e} — will retry");
            }
        }
    }
    delivered
}

/// The background delivery worker: drain the outbox, publish the depth gauge, sleep, repeat. Runs
/// alongside the indexer and API on its own task; enqueuing is decoupled from it so a slow webhook
/// never blocks indexing. The `POLL_INTERVAL` sleep is also the retry backoff for failed deliveries.
pub async fn run_delivery_worker(store: Store, secrets: std::collections::HashMap<String, String>) {
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("alert delivery worker could not start (HTTP client): {e}");
            return;
        }
    };
    tracing::info!("alert delivery worker started");
    loop {
        let delivered = deliver_pending(&store, &client, &secrets).await;
        crate::metrics::METRICS.set_alert_outbox(store.outbox_len());
        if delivered > 0 {
            tracing::debug!(
                "delivered {delivered} alert(s); {} pending",
                store.outbox_len()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_matches_kinds() {
        let r = AlertRouter::new(vec![
            Alert {
                kinds: vec!["sanction_hit".into()],
                url: "https://a".into(),
            },
            Alert {
                kinds: vec!["sanction_hit".into(), "threshold_flag".into()],
                url: "https://b".into(),
            },
        ]);
        assert_eq!(r.urls_for("sanction_hit"), vec!["https://a", "https://b"]);
        assert_eq!(r.urls_for("threshold_flag"), vec!["https://b"]);
        assert!(r.watches("sanction_hit"));
        assert!(!r.watches("nope"));
    }

    #[test]
    fn enqueue_is_noop_without_sinks() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        enqueue(
            &store,
            &AlertRouter::default(),
            "flag",
            "sanction_hit",
            &json!({}),
        )
        .unwrap();
        assert_eq!(store.outbox_len(), 0);
    }

    #[test]
    fn outbox_trims_oldest_when_over_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        for i in 0..5 {
            store.outbox_push(&format!("{{\"n\":{i}}}")).unwrap();
        }
        assert_eq!(store.outbox_trim(3).unwrap(), 2, "dropped the 2 oldest");
        let pending = store.outbox_pending(10).unwrap();
        assert_eq!(pending.len(), 3);
        // The survivors are the 3 newest (n = 2, 3, 4), oldest-first.
        assert!(pending[0].1.contains("\"n\":2"));
    }

    /// The C5 gate: a real local webhook server receives a `flag` on a raised annotation and a
    /// `flag_retracted` on a reorg, and delivered entries leave the durable outbox (at-least-once).
    #[tokio::test]
    async fn delivers_flag_and_retraction_to_a_live_webhook() {
        use axum::{routing::post, Json, Router};
        use std::sync::{Arc, Mutex};

        // A local webhook that records every payload it receives.
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = received.clone();
        let app = Router::new().route(
            "/hook",
            post(move |Json(body): Json<Value>| {
                let sink = sink.clone();
                async move {
                    sink.lock().unwrap().push(body);
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/hook");

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        let router = AlertRouter::new(vec![Alert {
            kinds: vec!["sanction_hit".into()],
            url: url.clone(),
        }]);

        // A hit is raised, then a reorg retracts it.
        let ann = json!({ "kind": "sanction_hit", "address": "0xbad", "block_number": 10 });
        enqueue(&store, &router, "flag", "sanction_hit", &ann).unwrap();
        enqueue(&store, &router, "flag_retracted", "sanction_hit", &ann).unwrap();
        assert_eq!(store.outbox_len(), 2);

        // Drain the outbox to the live webhook.
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let delivered = deliver_pending(&store, &client, &std::collections::HashMap::new()).await;
        assert_eq!(delivered, 2);
        assert_eq!(store.outbox_len(), 0, "delivered entries leave the outbox");

        let got = received.lock().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["event"], "flag");
        assert_eq!(got[0]["annotation"]["address"], "0xbad");
        assert_eq!(got[1]["event"], "flag_retracted");
    }

    /// RFC-0010 egress security: a webhook whose payload names a secret is signed with
    /// `X-Nuthatch-Signature: sha256=<hmac>` over the *exact bytes sent*, so the receiver can verify
    /// provenance. A payload without a matching secret goes unsigned.
    #[tokio::test]
    async fn signs_delivery_when_the_webhook_has_a_secret() {
        use axum::{extract::State, http::HeaderMap, routing::post, Router};
        use std::sync::{Arc, Mutex};

        // Records (signature-header, raw-body) for each request.
        type Seen = Arc<Mutex<Vec<(Option<String>, String)>>>;
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/hook",
                post(
                    |State(sink): State<Seen>, headers: HeaderMap, body: String| async move {
                        let sig = headers
                            .get("x-nuthatch-signature")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        sink.lock().unwrap().push((sig, body));
                        "ok"
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let url = format!("http://{addr}/hook");

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();

        // One payload names webhook "signed" (secret configured), one names "plain" (no secret).
        let signed = json!({ "event": "rows", "webhook": "signed", "url": url, "rows": [1] });
        let plain = json!({ "event": "rows", "webhook": "plain", "url": url, "rows": [2] });
        store.outbox_push(&signed.to_string()).unwrap();
        store.outbox_push(&plain.to_string()).unwrap();

        let mut secrets = std::collections::HashMap::new();
        secrets.insert("signed".to_string(), "topsecret".to_string());

        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let delivered = deliver_pending(&store, &client, &secrets).await;
        assert_eq!(delivered, 2);

        let got = seen.lock().unwrap();
        let signed_row = got
            .iter()
            .find(|(_, b)| b.contains("\"webhook\":\"signed\""))
            .unwrap();
        let plain_row = got
            .iter()
            .find(|(_, b)| b.contains("\"webhook\":\"plain\""))
            .unwrap();

        // The signature covers the exact bytes POSTed, under the configured secret.
        let expect = crate::webhooks::hmac_sha256_hex(b"topsecret", signed_row.1.as_bytes());
        assert_eq!(
            signed_row.0.as_deref(),
            Some(format!("sha256={expect}").as_str())
        );
        // No secret → no signature header.
        assert_eq!(plain_row.0, None, "an unsecreted webhook is not signed");
    }

    /// A dead endpoint leaves the alert in the outbox for retry — at-least-once, never dropped on the
    /// first failure.
    #[tokio::test]
    async fn failed_delivery_stays_in_outbox_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        // A URL that refuses connections (nothing listening on this port).
        let router = AlertRouter::new(vec![Alert {
            kinds: vec!["sanction_hit".into()],
            url: "http://127.0.0.1:9/hook".into(),
        }]);
        enqueue(
            &store,
            &router,
            "flag",
            "sanction_hit",
            &json!({ "kind": "sanction_hit" }),
        )
        .unwrap();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let delivered = deliver_pending(&store, &client, &std::collections::HashMap::new()).await;
        assert_eq!(delivered, 0);
        assert_eq!(
            store.outbox_len(),
            1,
            "a failed delivery is retained, not dropped"
        );
    }
}

//! The wire adapters (D-024): the OpenAI-compatible endpoint and the loopback
//! local model. The egress contract is structural — the request URL is the
//! configured endpoint, built once at construction; no redirect is followed,
//! no proxy is consulted, no URL arrives from request or answer content. Every
//! call carries a deadline, at most two retries for transient failures, a
//! response cap, and a spend from the job's [`Budget`] (failure case 9).

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::{Client, StatusCode, Url, redirect};
use serde_json::{Value, json};
use thiserror::Error;

use crate::answer::Answer;
use crate::port::{BoxFuture, ProposeError, Proposer};
use crate::request::ProposeRequest;
use crate::validate::validate;

/// The answer body cap (D-024): a larger response is malformed, not truncated.
const RESPONSE_CAP: usize = 256 * 1024;
/// Transient failures retry at most this many times on top of the first call.
const MAX_RETRIES: u32 = 2;

/// Why an endpoint URL is unusable.
#[derive(Debug, Error)]
pub enum EndpointError {
    /// The URL did not parse as an absolute URL.
    #[error("the endpoint is not an absolute URL")]
    NotAbsolute,
    /// A user:password in the endpoint would leak through a URL-shaped log.
    #[error("the endpoint must not carry credentials")]
    CredentialsInUrl,
    /// `http` is allowed only for the loopback local-model adapter.
    #[error("the endpoint is not https — plain http is loopback-only")]
    NotTls,
    /// The HTTP client could not be built.
    #[error("the client could not be built")]
    Build,
}

/// A checked outbound URL: `https` only, except `http` to a loopback host when
/// the adapter is the configured local-model kind. Never carries credentials.
#[derive(Clone)]
pub struct Endpoint {
    url: Url,
}

impl Endpoint {
    /// Parses the configured endpoint. `allow_http_loopback` is the local
    /// model's carve-out; every other non-TLS scheme is refused.
    pub fn parse(raw: &str, allow_http_loopback: bool) -> Result<Self, EndpointError> {
        let url = Url::parse(raw).map_err(|_| EndpointError::NotAbsolute)?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(EndpointError::CredentialsInUrl);
        }
        let host = url.host_str().unwrap_or_default();
        let allowed = match url.scheme() {
            "https" => true,
            "http" => allow_http_loopback && is_loopback(host),
            _ => false,
        };
        allowed.then_some(Self { url }).ok_or(EndpointError::NotTls)
    }
}

/// A loopback literal or `localhost`; `[::1]` arrives bracketed from the URL.
fn is_loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The per-job call budget (D-024): a call spends one; exhaustion is the named
/// refusal, never a wait and never a call made anyway.
pub struct Budget {
    left: AtomicUsize,
}

impl Budget {
    /// A budget of `calls` calls for one job.
    pub const fn calls(calls: usize) -> Self {
        Self {
            left: AtomicUsize::new(calls),
        }
    }

    /// Spends one call; false when the budget is already spent.
    fn spend(&self) -> bool {
        self.left
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
    }
}

/// Why one HTTP attempt failed — the retry and refusal rules differ by kind.
enum CallFailure {
    /// Timeout, connect failure, 429 or 5xx: worth one of the bounded retries.
    Transient,
    /// The provider answered but the answer is unusable: no retry helps.
    Malformed,
    /// A non-transient refusal (4xx): retrying changes nothing.
    Unavailable,
}

/// The OpenAI-compatible adapter: chat-completions POSTs to one configured
/// endpoint. [`HttpProposer::loopback`] is the local-model variant.
pub struct HttpProposer {
    client: Client,
    endpoint: Endpoint,
    model: String,
    api_key: Option<String>,
    budget: Budget,
}

impl HttpProposer {
    /// A proposer for an `https` endpoint with a per-call `deadline` and a
    /// `budget` of calls for the job it serves.
    pub fn new(
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
        deadline: Duration,
        budget: Budget,
    ) -> Result<Self, EndpointError> {
        Self::with(
            Endpoint::parse(endpoint, false)?,
            model,
            api_key,
            deadline,
            budget,
        )
    }

    /// The local model: `http` only, and only to a loopback host.
    pub fn loopback(
        endpoint: &str,
        model: &str,
        deadline: Duration,
        budget: Budget,
    ) -> Result<Self, EndpointError> {
        Self::with(
            Endpoint::parse(endpoint, true)?,
            model,
            None,
            deadline,
            budget,
        )
    }

    fn with(
        endpoint: Endpoint,
        model: &str,
        api_key: Option<&str>,
        deadline: Duration,
        budget: Budget,
    ) -> Result<Self, EndpointError> {
        let client = Client::builder()
            // One deadline bounds connect, head and body — the call, not parts.
            .timeout(deadline)
            .connect_timeout(deadline)
            // D-024: a redirect must never carry the request to another host.
            .redirect(redirect::Policy::none())
            // An environment proxy is an outbound hop off the allowlist.
            .no_proxy()
            .build()
            .map_err(|_| EndpointError::Build)?;
        Ok(Self {
            client,
            endpoint,
            model: model.to_owned(),
            api_key: api_key.map(str::to_owned),
            budget,
        })
    }

    /// One POST to the endpoint; returns the body bytes within the cap or the
    /// kind of failure the retry rule applies.
    async fn call(&self, body: &Value) -> Result<Vec<u8>, CallFailure> {
        let mut send = self.client.post(self.endpoint.url.clone()).json(body);
        if let Some(key) = &self.api_key {
            send = send.bearer_auth(key);
        }
        let mut response = send.send().await.map_err(|e| {
            if e.is_timeout() || e.is_connect() {
                CallFailure::Transient
            } else {
                CallFailure::Unavailable
            }
        })?;
        if !response.status().is_success() {
            let retryable = response.status() == StatusCode::TOO_MANY_REQUESTS
                || response.status().is_server_error();
            return Err(if retryable {
                CallFailure::Transient
            } else {
                CallFailure::Unavailable
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| CallFailure::Malformed)? {
            if body.len() + chunk.len() > RESPONSE_CAP {
                return Err(CallFailure::Malformed);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

impl Proposer for HttpProposer {
    fn propose<'a>(
        &'a self,
        request: &'a ProposeRequest,
    ) -> BoxFuture<'a, Result<Answer, ProposeError>> {
        Box::pin(async move {
            if !self.budget.spend() {
                return Err(ProposeError::Budget);
            }
            let user = serde_json::to_string(request).map_err(|_| ProposeError::Malformed)?;
            let body = json!({
                "model": self.model,
                "temperature": 0,
                "response_format": {"type": "json_object"},
                "messages": [
                    {"role": "system", "content": format!("prompt_version={}", request.prompt_version)},
                    {"role": "user", "content": user},
                ],
            });
            let mut retries = 0;
            let raw = loop {
                match self.call(&body).await {
                    Ok(bytes) => break bytes,
                    Err(CallFailure::Transient) if retries < MAX_RETRIES => {
                        retries += 1;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(CallFailure::Malformed) => return Err(ProposeError::Malformed),
                    Err(_) => return Err(ProposeError::Unavailable),
                }
            };
            let text = std::str::from_utf8(&raw).map_err(|_| ProposeError::Malformed)?;
            let answer = Answer::parse(&chat_content(text).ok_or(ProposeError::Malformed)?)
                .map_err(|_| ProposeError::Malformed)?;
            // The answer must survive its own validator before it leaves the
            // adapter — an unvalidated answer is never handed up (D-016).
            validate(request, &answer).map_err(|_| ProposeError::Malformed)?;
            Ok(answer)
        })
    }
}

/// Extracts `choices[0].message.content` from an OpenAI-shaped response.
fn chat_content(body: &str) -> Option<String> {
    let body: Value = serde_json::from_str(body).ok()?;
    body.get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()
        .map(str::to_owned)
}

//! Go's `http.ResponseWriter`, for the ported handlers.
//!
//! The pool's request path decides what the client sees late: the pre-commit
//! window holds headers and lifecycle events until a capacity refusal can no
//! longer be retried, the early stream writer sends headers and keepalive
//! comments on its own clock, and an error after early headers becomes an
//! in-stream failure. That logic is written against Go's writer: headers are
//! mutable until the first `write_header` or `write`, the first status wins, a
//! write without one sends an implicit 200, and nothing is sent until then. The
//! writers here keep exactly those rules, so the ported code keeps its shape.
//!
//! `ChannelWriter` serves a real connection: the status and headers go out the
//! moment they are written (the hyper response is returned then), and body
//! chunks follow as they are written. `Recorder` is Go's
//! httptest.ResponseRecorder, for tests.

use bytes::Bytes;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::StatusCode;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub trait ResponseWriter: Send {
    fn header(&mut self) -> &mut HeaderMap;
    fn write_header(&mut self, status: StatusCode);
    fn write(&mut self, data: &[u8]);
    /// Chunks already go out as they are written; Flush is a no-op kept so the
    /// ported code reads like Go's.
    fn flush(&mut self) {}
    /// Go's `w.(*earlyStreamWriter)` check: an early stream writer keeps the
    /// connection alive itself, so the timed commit applies only to others.
    fn manages_keepalive(&self) -> bool {
        false
    }
}

pub fn set_header(writer: &mut dyn ResponseWriter, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        writer.header().insert(name, value);
    }
}

pub fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// Go writeJSON: the value as JSON with a newline (json.Encoder), and the
/// status.
pub fn write_json(writer: &mut dyn ResponseWriter, status: StatusCode, value: &serde_json::Value) {
    set_header(writer, "content-type", "application/json");
    writer.write_header(status);
    let mut body = super::gojson::marshal(value);
    body.push(b'\n');
    writer.write(&body);
}

/// Go writeOpenAIError.
pub fn write_openai_error(
    writer: &mut dyn ResponseWriter,
    status: StatusCode,
    code: &str,
    message: &str,
) {
    write_json(
        writer,
        status,
        &serde_json::json!({"error": {"message": message, "type": "invalid_request_error", "code": code}}),
    );
}

/// Go writeStreamingBytes: write, then flush.
pub fn write_streaming_bytes(writer: &mut dyn ResponseWriter, data: &[u8]) {
    if !data.is_empty() {
        writer.write(data);
    }
    writer.flush();
}

// ---- the caller going away ----

/// Go's request context: cancelled when the client disconnects, so the
/// request path stops instead of spending upstream capacity on a caller that
/// already gave up.
#[derive(Clone, Default)]
pub struct Cancel {
    inner: Arc<(std::sync::atomic::AtomicBool, tokio::sync::Notify)>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.inner
            .0
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.1.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.1.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    /// Waits out `delay`; false when the caller went away first (Go
    /// sleepForRetry).
    pub async fn sleep(&self, delay: std::time::Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(delay) => true,
            () = self.cancelled() => false,
        }
    }
}

/// Go's http.Header: canonical names (textproto.CanonicalMIMEHeaderKey), each
/// with its values in order. Go writes a request's headers sorted by name, and
/// the hop to codex-upstream carried them in that order, which decides where
/// the Codex headers that no native order names land; keeping the map sorted
/// keeps that order.
#[derive(Clone, Debug, Default)]
pub struct GoHeader(pub std::collections::BTreeMap<String, Vec<String>>);

pub fn canonical_header_key(name: &str) -> String {
    let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    if !name.chars().all(token) {
        return name.to_string();
    }
    let mut upper = true;
    name.chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}

impl GoHeader {
    pub fn from_map(headers: &HeaderMap) -> Self {
        let mut out = Self::default();
        for (name, value) in headers {
            if let Ok(value) = value.to_str() {
                out.add(name.as_str(), value);
            }
        }
        out
    }

    pub fn get(&self, name: &str) -> &str {
        self.0
            .get(&canonical_header_key(name))
            .and_then(|values| values.first())
            .map_or("", String::as_str)
    }

    pub fn set(&mut self, name: &str, value: &str) {
        self.0
            .insert(canonical_header_key(name), vec![value.to_string()]);
    }

    pub fn add(&mut self, name: &str, value: &str) {
        self.0
            .entry(canonical_header_key(name))
            .or_default()
            .push(value.to_string());
    }

    pub fn del(&mut self, name: &str) {
        self.0.remove(&canonical_header_key(name));
    }

    /// The headers in the order Go writes them.
    pub fn to_map(&self) -> HeaderMap {
        let mut out = HeaderMap::new();
        for (name, values) in &self.0 {
            let Ok(name) = HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()) else {
                continue;
            };
            for value in values {
                if let Ok(value) = HeaderValue::from_str(value) {
                    out.append(name.clone(), value);
                }
            }
        }
        out
    }
}

// ---- a real connection ----

pub struct ChannelWriter {
    headers: HeaderMap,
    head: Option<oneshot::Sender<(StatusCode, HeaderMap)>>,
    body: mpsc::UnboundedSender<Bytes>,
}

pub struct ChannelResponse {
    pub head: oneshot::Receiver<(StatusCode, HeaderMap)>,
    pub body: mpsc::UnboundedReceiver<Bytes>,
}

impl ChannelWriter {
    pub fn new() -> (Self, ChannelResponse) {
        let (head_tx, head_rx) = oneshot::channel();
        let (body_tx, body_rx) = mpsc::unbounded_channel();
        (
            Self {
                headers: HeaderMap::new(),
                head: Some(head_tx),
                body: body_tx,
            },
            ChannelResponse {
                head: head_rx,
                body: body_rx,
            },
        )
    }
}

impl ResponseWriter for ChannelWriter {
    fn header(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    fn write_header(&mut self, status: StatusCode) {
        if let Some(head) = self.head.take() {
            let _ = head.send((status, self.headers.clone()));
        }
    }

    fn write(&mut self, data: &[u8]) {
        if self.head.is_some() {
            self.write_header(StatusCode::OK);
        }
        if !data.is_empty() {
            let _ = self.body.send(Bytes::copy_from_slice(data));
        }
    }
}

impl Drop for ChannelWriter {
    /// A handler that returned without writing sends an empty 200, as
    /// net/http does.
    fn drop(&mut self) {
        if self.head.is_some() {
            self.write_header(StatusCode::OK);
        }
    }
}

// ---- tests ----

#[derive(Clone, Debug, Default)]
pub struct Recorded {
    pub status: Option<StatusCode>,
    /// The headers as they were when the status went out (Go's
    /// ResponseRecorder snapshots them then).
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub flushed: bool,
}

impl Recorded {
    pub fn code(&self) -> u16 {
        self.status.map_or(200, |status| status.as_u16())
    }

    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> &str {
        header_value(&self.headers, name)
    }
}

/// Go httptest.ResponseRecorder. Clones share what was recorded, so a test
/// keeps one while the handler owns another.
#[derive(Clone, Default)]
pub struct Recorder {
    headers: HeaderMap,
    recorded: Arc<Mutex<Recorded>>,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn result(&self) -> Recorded {
        self.recorded
            .lock()
            .map(|recorded| recorded.clone())
            .unwrap_or_default()
    }

    fn with(&self, apply: impl FnOnce(&mut Recorded)) {
        if let Ok(mut recorded) = self.recorded.lock() {
            apply(&mut recorded);
        }
    }
}

impl ResponseWriter for Recorder {
    fn header(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    fn write_header(&mut self, status: StatusCode) {
        let headers = self.headers.clone();
        self.with(|recorded| {
            if recorded.status.is_none() {
                recorded.status = Some(status);
                recorded.headers = headers;
            }
        });
    }

    fn write(&mut self, data: &[u8]) {
        self.write_header(StatusCode::OK);
        self.with(|recorded| recorded.body.extend_from_slice(data));
    }

    fn flush(&mut self) {
        self.write_header(StatusCode::OK);
        self.with(|recorded| recorded.flushed = true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writers_keep_go_rules() {
        let mut recorder = Recorder::new();
        let handle = recorder.clone();
        set_header(&mut recorder, "x-one", "1");
        recorder.write(b"hello");
        // After the implicit 200, a status and new headers change nothing.
        recorder.write_header(StatusCode::BAD_GATEWAY);
        set_header(&mut recorder, "x-two", "2");
        let result = handle.result();
        assert_eq!(result.code(), 200);
        assert_eq!(result.header("x-one"), "1");
        assert_eq!(result.header("x-two"), "");
        assert_eq!(result.body_string(), "hello");
        let mut recorder = Recorder::new();
        write_openai_error(
            &mut recorder,
            StatusCode::SERVICE_UNAVAILABLE,
            "code",
            "a <b>",
        );
        let result = recorder.result();
        assert_eq!(result.code(), 503);
        assert_eq!(
            result.body_string(),
            "{\"error\":{\"code\":\"code\",\"message\":\"a \\u003cb\\u003e\",\"type\":\"invalid_request_error\"}}\n"
        );
    }

    #[tokio::test]
    async fn channel_writer_sends_head_then_body() {
        let (mut writer, mut response) = ChannelWriter::new();
        set_header(&mut writer, "content-type", "text/event-stream");
        writer.write(b"data: 1\n\n");
        drop(writer);
        let (status, headers) = response
            .head
            .await
            .unwrap_or((StatusCode::IM_A_TEAPOT, HeaderMap::new()));
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header_value(&headers, "content-type"), "text/event-stream");
        assert_eq!(
            response.body.recv().await.as_deref(),
            Some(&b"data: 1\n\n"[..])
        );
        assert_eq!(response.body.recv().await, None);
    }
}

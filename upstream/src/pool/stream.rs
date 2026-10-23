//! Relaying an upstream Responses stream: the pre-commit retry window and the
//! early stream writer that keeps a waiting client connected.
//!
//! A port of copyStreamingProxyResponseWithPrecommit, readSSEBlock,
//! precommitLifecycleSSEBlock and earlyStreamWriter in the Go pool's main.go.
//! The comments that explain each rule come with it: every one records a
//! production stream that committed too early (so a capacity refusal reached
//! Codex as final) or held the client too long (so it was cut and resent the
//! turn).

use super::http_io::ResponseWriter;
use super::http_io::write_streaming_bytes;
use super::plan::clean_metadata_token;
use super::response_info::ProxyResponseInfo;
use super::response_info::response_event_type_from_sse_block;
use super::response_info::response_info_from_sse_block;
use super::response_info::retryable_terminal_failure_class;
use super::time::Timestamp;
use super::tuning::Tuning;
use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use http::HeaderMap;
use http::StatusCode;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

pub const MAX_REQUEST_BODY: usize = 16 << 20;
/// The fixed part of the pre-commit byte bound. The effective bound adds a
/// multiple of the inbound request size (precommit_byte_limit): the first
/// lifecycle event echoes the request back (instructions, tool definitions,
/// input), so a bound that ignores request size closes the retry window on the
/// very first block for any large context. That is exactly what happened in
/// production: "bounds" had been closing 44% of all streams.
pub const STREAMING_PRECOMMIT_MAX_BYTES: usize = 64 << 10;
/// How many lifecycle blocks may be held before the stream commits. Only
/// event-bearing lifecycle blocks count, each a few hundred bytes, so this is a
/// sanity limit on upstream verbosity rather than the memory guard; the byte
/// bound is that.
pub const STREAMING_PRECOMMIT_MAX_BLOCKS: usize = 64;
/// How many times the inbound request the buffered preamble may reach before
/// the window commits.
pub const PRECOMMIT_REQUEST_ECHO_FACTOR: usize = 4;

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

pub fn byte_stream_from(bytes: impl Into<Bytes>) -> ByteStream {
    let bytes: Bytes = bytes.into();
    Box::pin(futures::stream::iter(if bytes.is_empty() {
        vec![]
    } else {
        vec![Ok(bytes)]
    }))
}

/// How a read of the stream ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadEnd {
    Eof,
    Error(String),
}

/// Go's bufio.Reader + readSSEBlock over an upstream body. Blocks are the raw
/// bytes, forwarded exactly as upstream sent them; reading is cancellation
/// safe, so a timer racing a read loses nothing.
pub struct SseReader {
    stream: ByteStream,
    buffer: Vec<u8>,
    end: Option<ReadEnd>,
}

impl SseReader {
    pub fn new(stream: ByteStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
            end: None,
        }
    }

    /// One block: lines up to and including a blank line, or what is left
    /// when the stream ends, with how it ended.
    pub async fn read_block(&mut self) -> (Vec<u8>, Option<ReadEnd>) {
        let mut block = Vec::new();
        loop {
            while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=newline).collect();
                let blank = line.iter().all(|byte| *byte == b'\r' || *byte == b'\n');
                block.extend_from_slice(&line);
                if blank {
                    return (block, None);
                }
            }
            if let Some(end) = self.end.clone() {
                block.append(&mut self.buffer);
                return (block, Some(end));
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buffer.extend_from_slice(&chunk),
                Some(Err(error)) => self.end = Some(ReadEnd::Error(error)),
                None => self.end = Some(ReadEnd::Eof),
            }
        }
    }

    /// The rest of the body (Go io.ReadAll), bounded.
    pub async fn read_all(&mut self, limit: usize) -> Result<Vec<u8>, String> {
        let mut out = std::mem::take(&mut self.buffer);
        loop {
            if out.len() > limit {
                out.truncate(limit);
                return Ok(out);
            }
            match &self.end {
                Some(ReadEnd::Eof) => return Ok(out),
                Some(ReadEnd::Error(error)) => return Err(error.clone()),
                None => {}
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => out.extend_from_slice(&chunk),
                Some(Err(error)) => self.end = Some(ReadEnd::Error(error)),
                None => self.end = Some(ReadEnd::Eof),
            }
        }
    }
}

fn text(block: &[u8]) -> String {
    String::from_utf8_lossy(block).into_owned()
}

/// Sizes the pre-commit buffer for one request. Upstream's first lifecycle
/// event carries the request back, expanded (tool schemas, injected
/// instructions), so the created event runs several times what was sent; a
/// one-to-one margin still tripped at 783 KB on two blocks, closing the retry
/// window on a subagent request. A multiple with an absolute ceiling keeps the
/// buffer the same order as the request body already held for the attempt.
pub fn precommit_byte_limit(request_bytes: usize) -> usize {
    request_bytes
        .checked_mul(PRECOMMIT_REQUEST_ECHO_FACTOR)
        .and_then(|echo| echo.checked_add(STREAMING_PRECOMMIT_MAX_BYTES))
        .filter(|limit| *limit <= MAX_REQUEST_BODY)
        .unwrap_or(MAX_REQUEST_BODY)
}

/// Whether a buffered block spends the block budget. Only blocks that carry
/// response state do; comments and named keepalive events exist to hold a
/// waiting connection open and say nothing about the response, so a long wait
/// for capacity must not exhaust the budget that keeps the retry window open.
/// Bytes are still counted for every block.
pub fn precommit_counts_toward_block_bound(block: &str) -> bool {
    !matches!(
        response_event_type_from_sse_block(block).as_str(),
        "" | "keepalive" | "ping"
    )
}

pub fn precommit_lifecycle_sse_block(block: &str) -> bool {
    let event_type = response_event_type_from_sse_block(block);
    // The Codex backend opens a Responses WebSocket with codex.rate_limits and
    // codex.response.metadata before response.created, and ends it with
    // responsesapi.websocket_timing. These are the backend's informational and
    // telemetry events, never model output. Treated as unknown they committed
    // the socket on its first event, so every overload behind them reached
    // Codex, which stops the turn on an overload without retry advice.
    if event_type.starts_with("codex.") || event_type.starts_with("responsesapi.") {
        return true;
    }
    match event_type.as_str() {
        "response.created" | "response.in_progress" | "response.queued" => true,
        // Upstream started sending response.metadata right after
        // response.created, before any output. As an unlisted event it failed
        // closed and committed the stream, so every server_is_overloaded behind
        // it reached Codex as a final failure instead of a retry on another
        // identity: from 2026-10-19 all client-visible capacity failures closed
        // the window here. It describes the response, not model output, so it
        // stays buffered; it is still forwarded when the window commits and is
        // discarded only with the rest of a failed attempt.
        "response.metadata" => true,
        // These announce an empty shell (an output item, a content part, a
        // reasoning summary part) and carry no text, arguments or reasoning of
        // their own. The rule this path enforces is that nothing
        // client-visible has been written yet, not that no semantic event has
        // arrived, so a shell must not close the window. In production every
        // one of these announcements committed the stream one block before the
        // capacity failure behind it could be retried, three quarters of all
        // client-visible failures. The first delta still commits: it is the
        // first event carrying content a retry would have to duplicate.
        "response.output_item.added"
        | "response.content_part.added"
        | "response.reasoning_summary_part.added" => true,
        // Upstream keeps a waiting stream alive with named events, not only SSE
        // comments. They carry no response state; treated as unknown one
        // committed the stream three blocks in, so the capacity failure behind
        // it was forwarded verbatim instead of retried.
        "keepalive" | "ping" => true,
        // The Codex backend announces a refusal as a bare `error` event and
        // only then sends the `response.failed` that carries the
        // classification. The error block is not model output, so it stays
        // buffered and keeps the retry window open for the terminal event
        // behind it. If anything other than a retryable failure follows, that
        // event closes the window and the buffered error is forwarded verbatim.
        "error" => true,
        // SSE keepalive comments and reconnection metadata are not semantic
        // model output. Unknown event/data blocks fail closed and commit
        // immediately so a future upstream event cannot be silently discarded.
        "" => block.split('\n').all(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.is_empty()
                || line.starts_with(':')
                || line.starts_with("id:")
                || line.starts_with("retry:")
        }),
        _ => false,
    }
}

/// Writes the response headers when the retry window commits.
pub type BeforeCommit<'a> = dyn FnMut(&mut dyn ResponseWriter) + Send + 'a;

#[derive(Debug, Default)]
pub struct StreamingProxyResult {
    pub info: ProxyResponseInfo,
    pub buffered: Vec<u8>,
    pub retryable_terminal_failure: bool,
}

/// Relays a Responses stream, holding its lifecycle-only prefix until it
/// carries content or a bound is reached, so a capacity refusal that arrives
/// first can be discarded and retried on another identity. `before_commit`
/// writes the response headers when the window commits.
pub async fn copy_streaming_proxy_response_with_precommit(
    writer: &mut dyn ResponseWriter,
    reader: &mut SseReader,
    max_bytes: usize,
    tuning: &Tuning,
    mut before_commit: Option<&mut BeforeCommit<'_>>,
) -> StreamingProxyResult {
    let mut info = ProxyResponseInfo::default();
    let mut buffered: Vec<u8> = Vec::new();
    let mut buffered_blocks = 0usize;
    let mut committed = false;
    let started = Instant::now();
    let mut commit = |writer: &mut dyn ResponseWriter,
                      buffered: &mut Vec<u8>,
                      info: &mut ProxyResponseInfo,
                      committed: &mut bool| {
        if *committed {
            return;
        }
        *committed = true;
        info.precommit_hold_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        if let Some(before) = before_commit.as_mut() {
            before(writer);
        }
        if !buffered.is_empty() {
            write_streaming_bytes(writer, buffered);
            buffered.clear();
        }
    };
    // An early stream writer keeps the client connection alive itself and
    // leaves the retry window open, so the timed commit applies only to other
    // writers.
    let hold = (!writer.manages_keepalive() && !tuning.unmanaged_precommit_commit_after.is_zero())
        .then(|| tokio::time::Instant::now() + tuning.unmanaged_precommit_commit_after);
    let mut hold_pending = hold.is_some();
    loop {
        let (block, end) = if hold_pending {
            let deadline = hold.unwrap_or_else(tokio::time::Instant::now);
            tokio::select! {
                read = reader.read_block() => read,
                () = tokio::time::sleep_until(deadline) => {
                    hold_pending = false;
                    if !committed {
                        // Release headers and the held lifecycle events. A
                        // capacity refusal arriving after this is forwarded
                        // in-stream like any committed one.
                        info.precommit_blocks = i64::try_from(buffered_blocks).unwrap_or(i64::MAX);
                        info.precommit_bytes = i64::try_from(buffered.len()).unwrap_or(i64::MAX);
                        info.precommit_close_reason = "hold_timeout".to_string();
                        commit(writer, &mut buffered, &mut info, &mut committed);
                    }
                    continue;
                }
            }
        } else {
            reader.read_block().await
        };
        if !block.is_empty() {
            let block_text = text(&block);
            let next = response_info_from_sse_block(&block_text);
            let next_terminal = next.terminal_event.clone();
            let next_class = next.terminal_failure_class.clone();
            info.merge(next);
            if committed {
                write_streaming_bytes(writer, &block);
            } else {
                buffered.extend_from_slice(&block);
                // Keepalive comments and reconnection metadata carry no
                // upstream state, so they must not spend the block budget.
                // They still count toward the byte bound, which is the guard
                // that actually caps memory.
                if precommit_counts_toward_block_bound(&block_text) {
                    buffered_blocks += 1;
                }
                let within_bounds = buffered.len() <= max_bytes
                    && buffered_blocks <= STREAMING_PRECOMMIT_MAX_BLOCKS;
                if next_terminal == "response.failed"
                    && retryable_terminal_failure_class(&next_class)
                    && within_bounds
                {
                    // No headers, lifecycle event, response id, model output,
                    // or tool activity has reached the client. Return the
                    // exact buffered failure so the caller can either discard
                    // it for a distinct-identity retry or forward it when no
                    // backup exists.
                    info.completed_at = Timestamp::now();
                    return StreamingProxyResult {
                        info,
                        buffered,
                        retryable_terminal_failure: true,
                    };
                }
                if !within_bounds || !precommit_lifecycle_sse_block(&block_text) {
                    // Record what closed the retry window before committing. A
                    // capacity failure arriving after this point cannot be
                    // retried at all, so this is the only signal that
                    // separates "no eligible fallback" from "the retry window
                    // was already shut".
                    if info.precommit_close_reason.is_empty() {
                        info.precommit_blocks = i64::try_from(buffered_blocks).unwrap_or(i64::MAX);
                        info.precommit_bytes = i64::try_from(buffered.len()).unwrap_or(i64::MAX);
                        info.precommit_close_reason = if !within_bounds {
                            "bounds".to_string()
                        } else {
                            let event_type = response_event_type_from_sse_block(&block_text);
                            if event_type.is_empty() {
                                "unknown_event".to_string()
                            } else {
                                clean_metadata_token(&event_type)
                            }
                        };
                    }
                    commit(writer, &mut buffered, &mut info, &mut committed);
                }
            }
        }
        match end {
            Some(ReadEnd::Eof) => {
                if !committed
                    && info.terminal_event == "error"
                    && retryable_terminal_failure_class(&info.terminal_failure_class)
                {
                    // A stream that ends with the error event itself instead
                    // of a following response.failed: EOF is when that
                    // sequence is known. Waiting until then keeps an error
                    // followed by normal output forwarded in order rather than
                    // discarded.
                    info.completed_at = Timestamp::now();
                    return StreamingProxyResult {
                        info,
                        buffered,
                        retryable_terminal_failure: true,
                    };
                }
                commit(writer, &mut buffered, &mut info, &mut committed);
                break;
            }
            Some(ReadEnd::Error(_)) => {
                commit(writer, &mut buffered, &mut info, &mut committed);
                break;
            }
            None => {}
        }
    }
    info.completed_at = Timestamp::now();
    info.precommit_committed = committed;
    StreamingProxyResult {
        info,
        ..StreamingProxyResult::default()
    }
}

/// Used only after callers have already committed headers. It must preserve
/// an upstream failure verbatim; only the main proxy loop can discard an
/// uncommitted prefix and retry.
pub async fn copy_streaming_proxy_response(
    writer: &mut dyn ResponseWriter,
    reader: &mut SseReader,
    tuning: &Tuning,
) -> ProxyResponseInfo {
    let result = copy_streaming_proxy_response_with_precommit(
        writer,
        reader,
        precommit_byte_limit(0),
        tuning,
        None,
    )
    .await;
    if result.retryable_terminal_failure {
        write_streaming_bytes(writer, &result.buffered);
    }
    result.info
}

// ---- the early stream writer ----

struct EarlyState {
    inner: Box<dyn ResponseWriter>,
    created: Instant,
    hold: Duration,
    header_written: bool,
    sse: bool,
    early: bool,
    early_at: Option<Instant>,
    real: bool,
    last_write: Instant,
    fail_status: Option<StatusCode>,
    fail_body: Vec<u8>,
    stopped: bool,
}

/// Wraps the client writer of a streaming Responses request.
///
/// The pre-commit window withholds a response so an upstream capacity
/// refusal can be retried on another identity before anything is
/// client-visible. Two earlier shapes of it each failed in production:
///   - Unbounded, it withheld the headers too, until the first content event:
///     27s against 1s straight to upstream, minutes at xhigh. Remote clients
///     were cut while waiting for headers ("error sending request") and
///     resent the whole turn.
///   - Bounded at 5s by committing, it let a capacity refusal that upstream
///     sends after queueing for longer than that reach the client, because a
///     committed stream can never be retried. Every capacity failure after that
///     change was final.
///
/// So the two jobs are separated. After the hold this writer sends 200
/// headers and then SSE comments, which Codex ignores, so the connection is
/// never silent, while the window itself stays open and the pool can still
/// retry invisibly, including across capacity backoff waits. Comments continue
/// for the whole stream: a reasoning model goes silent again between output
/// items for as long as it thinks, and that silence trips the same client
/// timeout. They are written only on an SSE response and only between whole
/// writes, and every streaming write is one complete SSE block, so a comment
/// never lands inside an event. Once headers are out early a status can no
/// longer be sent, so a later error response is captured and emitted at finish
/// as an SSE response.failed event carrying the same code and message. Do not
/// collapse this back into a timed commit: that is the version that made
/// capacity refusals final.
///
/// The client is not what needs the keepalives: Codex CLI 0.130.0, 0.141.0
/// and 0.162.1 waited 120s with no headers and 60s of mid-stream silence on a
/// direct connection without failing. What can cut a silent connection is the
/// path between remote clients and this pool (home router NAT, ISP, corporate
/// proxies and VPNs), whose idle timeouts are not ours to set.
pub struct EarlyStreamWriter {
    state: Arc<Mutex<EarlyState>>,
    headers: HeaderMap,
    ticker: Option<tokio::task::JoinHandle<()>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

fn lock(state: &Mutex<EarlyState>) -> std::sync::MutexGuard<'_, EarlyState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl EarlyStreamWriter {
    pub fn new(inner: Box<dyn ResponseWriter>, hold: Duration, tuning: &Tuning) -> Self {
        let now = Instant::now();
        let state = Arc::new(Mutex::new(EarlyState {
            inner,
            created: now,
            hold,
            header_written: false,
            sse: false,
            early: false,
            early_at: None,
            real: false,
            last_write: now,
            fail_status: None,
            fail_body: Vec::new(),
            stopped: false,
        }));
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let ticking = state.clone();
        let tick = tuning.stream_keepalive_tick;
        let interval = tuning.stream_keepalive_interval;
        let ticker = tokio::spawn(async move {
            let mut clock = tokio::time::interval(tick);
            clock.tick().await;
            loop {
                tokio::select! {
                    _ = clock.tick() => Self::tick(&ticking, Instant::now(), interval),
                    _ = &mut stop_rx => return,
                }
            }
        });
        Self {
            state,
            headers: HeaderMap::new(),
            ticker: Some(ticker),
            stop: Some(stop_tx),
        }
    }

    /// Sends early headers once the hold has passed with nothing written, and
    /// a keepalive comment whenever an SSE response has been silent for the
    /// keepalive interval.
    fn tick(state: &Mutex<EarlyState>, now: Instant, interval: Duration) {
        let mut state = lock(state);
        if !state.header_written {
            let due = !state.hold.is_zero() && now.duration_since(state.created) >= state.hold;
            if due {
                Self::start_early_locked(&mut state);
            }
            return;
        }
        if !state.sse
            || state.fail_status.is_some()
            || now.duration_since(state.last_write) < interval
        {
            return;
        }
        state.inner.write(b": keepalive\n\n");
        state.last_write = now;
        state.inner.flush();
    }

    fn start_early_locked(state: &mut EarlyState) -> bool {
        if state.header_written {
            return false;
        }
        super::http_io::set_header(state.inner.as_mut(), "content-type", "text/event-stream");
        super::http_io::set_header(state.inner.as_mut(), "cache-control", "no-cache");
        state.inner.write_header(StatusCode::OK);
        state.header_written = true;
        state.sse = true;
        state.early = true;
        let now = Instant::now();
        state.early_at = Some(now);
        state.last_write = now;
        state.inner.write(b": keepalive\n\n");
        state.inner.flush();
        true
    }

    /// Sends the 200 SSE headers and an opening comment now, if nothing has
    /// been written yet; whether it did.
    pub fn start_early(&self) -> bool {
        Self::start_early_locked(&mut lock(&self.state))
    }

    /// Whether headers went out before any response content, and when.
    pub fn headers_sent_early(&self) -> Option<Instant> {
        let state = lock(&self.state);
        if state.early { state.early_at } else { None }
    }

    /// Records whether the response is a successful SSE stream, the only kind
    /// a keepalive comment may be written into: in a JSON body it would
    /// corrupt the response.
    fn mark_sse_locked(state: &mut EarlyState, status: StatusCode) {
        let content_type =
            super::http_io::header_value(state.inner.header(), "content-type").to_lowercase();
        state.sse = status.as_u16() < 400 && content_type.contains("text/event-stream");
        state.last_write = Instant::now();
    }

    fn sync_headers(&self, state: &mut EarlyState) {
        for (name, value) in &self.headers {
            state.inner.header().insert(name.clone(), value.clone());
        }
    }

    /// Stops the keepalive task and, if an error response arrived after early
    /// headers, emits it as an SSE response.failed event. It must run before
    /// the handler returns.
    pub async fn finish(&mut self) {
        {
            let mut state = lock(&self.state);
            state.stopped = true;
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.await;
        }
        let mut state = lock(&self.state);
        let Some(fail_status) = state.fail_status else {
            return;
        };
        let mut code = "upstream_error".to_string();
        let mut message = fail_status
            .canonical_reason()
            .unwrap_or_default()
            .to_string();
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&state.fail_body) {
            if let Some(value) = parsed
                .pointer("/error/code")
                .and_then(serde_json::Value::as_str)
                && !value.is_empty()
            {
                code = value.to_string();
            }
            if let Some(value) = parsed
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                && !value.is_empty()
            {
                message = value.to_string();
            }
        }
        let event = super::gojson::marshal(&serde_json::json!({
            "type": "response.failed",
            "response": {"object": "response", "status": "failed", "error": {"code": code, "message": message}},
        }));
        let mut line = b"event: response.failed\ndata: ".to_vec();
        line.extend_from_slice(&event);
        line.extend_from_slice(b"\n\n");
        state.inner.write(&line);
        state.inner.flush();
    }
}

impl ResponseWriter for EarlyStreamWriter {
    fn header(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    fn write_header(&mut self, status: StatusCode) {
        let state_arc = self.state.clone();
        let mut state = lock(&state_arc);
        if state.early {
            // 200 is already on the wire. An error status becomes an in-stream
            // failure at finish; anything else is the stream continuing.
            if status.as_u16() >= 400 && !state.real && state.fail_status.is_none() {
                state.fail_status = Some(status);
            }
            return;
        }
        if state.header_written {
            return;
        }
        self.sync_headers(&mut state);
        state.header_written = true;
        Self::mark_sse_locked(&mut state, status);
        state.inner.write_header(status);
    }

    fn write(&mut self, data: &[u8]) {
        let state_arc = self.state.clone();
        let mut state = lock(&state_arc);
        if state.fail_status.is_some() {
            state.fail_body.extend_from_slice(data);
            return;
        }
        if !state.header_written {
            // An implicit 200, as net/http would send it.
            self.sync_headers(&mut state);
            state.header_written = true;
            Self::mark_sse_locked(&mut state, StatusCode::OK);
            state.inner.write_header(StatusCode::OK);
        }
        state.real = true;
        state.last_write = Instant::now();
        state.inner.write(data);
    }

    fn flush(&mut self) {
        let mut state = lock(&self.state);
        if state.fail_status.is_none() {
            state.inner.flush();
        }
    }

    fn manages_keepalive(&self) -> bool {
        true
    }
}

impl Drop for EarlyStreamWriter {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::http_io::Recorder;

    fn stream_of(chunks: &[&str]) -> SseReader {
        let chunks: Vec<Result<Bytes, String>> = chunks
            .iter()
            .map(|chunk| Ok(Bytes::from(chunk.to_string())))
            .collect();
        SseReader::new(Box::pin(futures::stream::iter(chunks)))
    }

    #[tokio::test]
    async fn reads_blocks_across_chunks() {
        let mut reader = stream_of(&["event: a\nda", "ta: 1\n\n: keep", "alive\n\ndata: tail"]);
        assert_eq!(
            reader.read_block().await,
            (b"event: a\ndata: 1\n\n".to_vec(), None)
        );
        assert_eq!(
            reader.read_block().await,
            (b": keepalive\n\n".to_vec(), None)
        );
        assert_eq!(
            reader.read_block().await,
            (b"data: tail".to_vec(), Some(ReadEnd::Eof))
        );
    }

    #[test]
    fn lifecycle_blocks_keep_the_window_open() {
        for block in [
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            "event: response.metadata\ndata: {}\n\n",
            "event: codex.rate_limits\ndata: {}\n\n",
            ": keepalive\n\n",
            "id: 7\nretry: 100\n\n",
            "event: error\ndata: {\"type\":\"error\"}\n\n",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\"}\n\n",
        ] {
            assert!(precommit_lifecycle_sse_block(block), "{block}");
        }
        for block in [
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "data: {\"surprise\":true}\n\n",
        ] {
            assert!(!precommit_lifecycle_sse_block(block), "{block}");
        }
        assert!(!precommit_counts_toward_block_bound(": keepalive\n\n"));
        assert!(precommit_counts_toward_block_bound(
            "event: response.created\ndata: {}\n\n"
        ));
        assert_eq!(precommit_byte_limit(0), STREAMING_PRECOMMIT_MAX_BYTES);
        assert_eq!(precommit_byte_limit(usize::MAX), MAX_REQUEST_BODY);
    }

    // A capacity refusal behind lifecycle events is returned, nothing written.
    #[tokio::test]
    async fn retryable_failure_before_content_is_held_back() {
        let mut recorder = Recorder::new();
        let mut reader = stream_of(&[
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\"}}}\n\n",
        ]);
        let result = copy_streaming_proxy_response_with_precommit(
            &mut recorder,
            &mut reader,
            1 << 20,
            &Tuning::default(),
            None,
        )
        .await;
        assert!(result.retryable_terminal_failure);
        assert!(String::from_utf8_lossy(&result.buffered).contains("server_is_overloaded"));
        assert_eq!(recorder.result().body.len(), 0);
        assert_eq!(recorder.result().status, None);

        // Content commits the window: the held prefix is written first.
        let mut recorder = Recorder::new();
        let mut reader = stream_of(&[
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\"}}}\n\n",
        ]);
        let result = copy_streaming_proxy_response_with_precommit(
            &mut recorder,
            &mut reader,
            1 << 20,
            &Tuning::default(),
            None,
        )
        .await;
        assert!(!result.retryable_terminal_failure);
        assert!(result.info.precommit_committed);
        assert_eq!(
            result.info.precommit_close_reason,
            "response.output_text.delta"
        );
        let body = recorder.result().body_string();
        assert!(body.starts_with("event: response.created"));
        assert!(body.ends_with("server_is_overloaded\"}}}\n\n"));
    }

    // Early headers go out after the hold, a later error becomes an in-stream
    // response.failed at finish.
    #[tokio::test]
    async fn early_writer_sends_headers_then_keepalives_and_reports_errors_in_stream() {
        let recorder = Recorder::new();
        let tuning = Tuning {
            stream_keepalive_interval: Duration::from_millis(30),
            stream_keepalive_tick: Duration::from_millis(10),
            ..Tuning::default()
        };
        let mut early = EarlyStreamWriter::new(
            Box::new(recorder.clone()),
            Duration::from_millis(20),
            &tuning,
        );
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(early.headers_sent_early().is_some());
        let result = recorder.result();
        assert_eq!(result.code(), 200);
        assert_eq!(result.header("content-type"), "text/event-stream");
        assert!(result.body_string().matches(": keepalive").count() >= 2);
        crate::pool::http_io::write_openai_error(
            &mut early,
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
            "all eligible upstream accounts failed",
        );
        early.finish().await;
        let body = recorder.result().body_string();
        assert!(body.ends_with(
            "event: response.failed\ndata: {\"response\":{\"error\":{\"code\":\"bad_gateway\",\"message\":\"all eligible upstream accounts failed\"},\"object\":\"response\",\"status\":\"failed\"},\"type\":\"response.failed\"}\n\n"
        ), "{body}");
    }
}

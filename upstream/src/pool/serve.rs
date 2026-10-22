//! The pool's process: settings from the environment, the data directory
//! loaded, the quota poll started, and one listener for the provider API and
//! the control UI, served until a stop signal and then drained.
//!
//! A port of main, newAppFromEnv, serve and serveUntilStopped in
//! cmd/codex-pool/main.go (RUST_PORT.md step 4.6). With this the Rust process
//! is the pool itself: there is no Go process behind a front door any more,
//! and no loopback hop between them.

use super::app::App;
use super::http_io::Cancel;
use super::log::Logger;
use super::settings::Settings;
use super::tuning::Tuning;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

/// Go main: the pool from the process environment, served until SIGTERM or
/// an interrupt.
pub async fn run() -> Result<(), String> {
    let env = |name: &str| std::env::var(name).ok();
    let settings = Settings::from_env(env)?;
    let tuning = Tuning::from_env(env)?;
    // Each deploy recreates the container, and Docker's log goes with it, so
    // the log is also kept under /data, which outlives the container.
    let logger = Logger::production(&settings.data_dir);
    let transports = Arc::new(
        crate::transport::Transports::new().map_err(|error| format!("codex transport: {error}"))?,
    );
    let app = Arc::new(App::load(settings, tuning, logger, transports, env)?);
    let listener = bind(&app.settings.listen_address)
        .await
        .map_err(|error| format!("listen combined HTTP service: {error}"))?;
    app.logger.printf(&format!(
        "provider API and control UI listening on {}",
        app.settings.listen_address
    ));
    // Never stopped: like Go's, the poll ends with the process.
    let _poll = app.start_quota_refresher(Cancel::default());
    let served = app.clone();
    serve_until_stopped(
        &app,
        listener,
        stop_signal(),
        move |stream, peer, shutdown| {
            super::listener::serve_connection(served.clone(), stream, peer, shutdown)
        },
    )
    .await;
    Ok(())
}

/// Binds the combined address. A Go-style `:port` means every IPv4
/// interface. Settings::from_env already refused a non-loopback address
/// without CODEX_POOL_ALLOW_REMOTE_ADMIN=true: merging the provider API and
/// the control UI on one port must not silently expose password login and
/// public repair wherever `/v1` is reachable.
pub async fn bind(address: &str) -> std::io::Result<TcpListener> {
    let address = match address.strip_prefix(':') {
        Some(port) => format!("0.0.0.0:{port}"),
        None => address.to_string(),
    };
    TcpListener::bind(&address).await
}

/// SIGTERM (docker stop) or an interrupt (Go signal.NotifyContext with
/// syscall.SIGTERM and os.Interrupt).
async fn stop_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        () = terminate => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Go serveUntilStopped: serves until `stop`, then drains. Every deploy used
/// to kill the process outright, cutting each open stream, and remote Codex
/// clients reported those as error responses. On stop the listener closes,
/// so no new request starts here; every connection finishes the request it
/// is serving and closes, idle ones at once; and in-flight requests,
/// streams included, run to completion for up to the shutdown grace before
/// this returns and the process exits. Do not replace this with a bare
/// accept loop: the drain is the point.
///
/// `connection` serves one accepted connection and must close it once the
/// `Cancel` it is given fires (listener::serve_connection).
pub async fn serve_until_stopped<C, Fut>(
    app: &App,
    listener: TcpListener,
    stop: impl Future<Output = ()>,
    connection: C,
) where
    C: Fn(TcpStream, SocketAddr, Cancel) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let shutdown = Cancel::default();
    let mut connections = tokio::task::JoinSet::new();
    let mut backoff = Duration::ZERO;
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    backoff = Duration::ZERO;
                    // Go's net/http disables Nagle on every accepted
                    // connection; stream deltas must not wait for an ACK.
                    let _ = stream.set_nodelay(true);
                    connections.spawn(connection(stream, peer, shutdown.clone()));
                }
                Err(error) => {
                    // Go's Serve retries a failed accept (a full descriptor
                    // table) with a backoff from 5ms doubling to 1s instead
                    // of spinning or giving up.
                    backoff = (backoff * 2).clamp(Duration::from_millis(5), Duration::from_secs(1));
                    app.logger.printf(&format!(
                        "http: Accept error: {error}; retrying in {}",
                        go_duration(backoff)
                    ));
                    tokio::time::sleep(backoff).await;
                }
            },
            // Reap finished connections so a long run does not hold them.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);
    let grace = if app.settings.shutdown_grace.is_zero() {
        super::settings::DEFAULT_SHUTDOWN_GRACE
    } else {
        app.settings.shutdown_grace
    };
    app.logger.printf(&format!(
        "stop signal received: draining {} in-flight request(s) for up to {}",
        app.in_flight.load(Ordering::SeqCst),
        go_duration(grace)
    ));
    shutdown.cancel();
    // Connections end once their request is answered. A Responses WebSocket
    // left its connection at the upgrade, so a response still streaming on
    // one is waited for through the in-flight count, within the same grace.
    // Idle sockets are not counted and close when the process exits; Codex
    // reconnects to the new container.
    let deadline = tokio::time::Instant::now() + grace;
    let drained = loop {
        if connections.is_empty() && app.in_flight.load(Ordering::SeqCst) <= 0 {
            break true;
        }
        tokio::select! {
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            () = tokio::time::sleep(Duration::from_millis(200)) => {}
            () = tokio::time::sleep_until(deadline) => break false,
        }
    };
    if drained {
        app.logger
            .printf("drain complete; all in-flight requests finished");
    } else {
        app.logger.printf(&format!(
            "shutdown grace expired with {} request(s) still in flight; they will be cut",
            app.in_flight.load(Ordering::SeqCst)
        ));
    }
}

/// Go's time.Duration String: "2m0s", "150ms", "1h0m0s".
pub fn go_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos == 0 {
        return "0s".to_string();
    }
    let fraction = |value: u128, unit: u128| {
        let whole = value / unit;
        let rest = value % unit;
        if rest == 0 {
            return whole.to_string();
        }
        let digits = unit.to_string().len() - 1;
        let fractional = format!("{rest:0digits$}");
        format!("{whole}.{}", fractional.trim_end_matches('0'))
    };
    if nanos < 1_000 {
        return format!("{nanos}ns");
    }
    if nanos < 1_000_000 {
        return format!("{}µs", fraction(nanos, 1_000));
    }
    if nanos < 1_000_000_000 {
        return format!("{}ms", fraction(nanos, 1_000_000));
    }
    let seconds = nanos / 1_000_000_000;
    let (hours, minutes) = (seconds / 3600, seconds / 60 % 60);
    let second = fraction(nanos % 60_000_000_000, 1_000_000_000);
    match (hours, minutes) {
        (0, 0) => format!("{second}s"),
        (0, _) => format!("{minutes}m{second}s"),
        _ => format!("{hours}h{minutes}m{second}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::listener::ResponseBody;
    use crate::pool::testing::test_app_with;
    use bytes::Bytes;
    use futures::StreamExt;
    use hyper::body::Frame;
    use std::convert::Infallible;
    use tokio::sync::oneshot;

    fn body(text: &'static str) -> ResponseBody {
        http_body_util::StreamBody::new(
            futures::stream::iter([Ok::<_, Infallible>(Frame::data(Bytes::from(text)))]).boxed(),
        )
    }

    /// The pool served until `stop`, behind a handler (Go a.trackInFlight
    /// around it) that reports `started` and holds its answer until
    /// `release` turns true.
    struct Harness {
        served: tokio::task::JoinHandle<()>,
        app: Arc<App>,
        address: SocketAddr,
        started: tokio::sync::mpsc::Receiver<()>,
        release: tokio::sync::watch::Sender<bool>,
        stop: oneshot::Sender<()>,
        _dir: crate::pool::testing::TempDir,
    }

    async fn start(grace: Duration) -> Harness {
        let (app, dir) = test_app_with(Vec::new(), |settings| settings.shutdown_grace = grace);
        let app = Arc::new(app);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"));
        let (started_tx, started) = tokio::sync::mpsc::channel(1);
        let (release, releases) = tokio::sync::watch::channel(false);
        let (stop, stopped) = oneshot::channel::<()>();
        let counted = app.clone();
        let handler = move |_request| {
            let (app, started, mut releases) =
                (counted.clone(), started_tx.clone(), releases.clone());
            async move {
                app.in_flight.fetch_add(1, Ordering::SeqCst);
                let _ = started.send(()).await;
                let _ = releases.wait_for(|released| *released).await;
                app.in_flight.fetch_sub(1, Ordering::SeqCst);
                hyper::Response::new(body("finished"))
            }
        };
        let handler = Arc::new(handler);
        let pool = app.clone();
        let served = tokio::spawn(async move {
            serve_until_stopped(
                &pool,
                listener,
                async {
                    let _ = stopped.await;
                },
                move |stream, _, shutdown| {
                    let handler = handler.clone();
                    crate::pool::listener::serve_http1(
                        stream,
                        move |request| handler(request),
                        shutdown,
                    )
                },
            )
            .await;
        });
        Harness {
            served,
            app,
            address,
            started,
            release,
            stop,
            _dir: dir,
        }
    }

    async fn get(address: SocketAddr) -> Result<String, String> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        let mut stream = TcpStream::connect(address)
            .await
            .map_err(|error| error.to_string())?;
        stream
            .write_all(b"GET / HTTP/1.1\r\nhost: pool\r\n\r\n")
            .await
            .map_err(|error| error.to_string())?;
        let mut answer = Vec::new();
        stream
            .read_to_end(&mut answer)
            .await
            .map_err(|error| error.to_string())?;
        Ok(String::from_utf8_lossy(&answer).into_owned())
    }

    // Go TestStopSignalDrainsInFlightRequests: a stop signal lets an
    // in-flight request run to completion, refuses new connections meanwhile,
    // and only then returns.
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_signal_drains_in_flight_requests() {
        let Harness {
            mut served,
            app,
            address,
            mut started,
            release,
            stop,
            _dir,
        } = start(Duration::from_secs(10)).await;
        let response = tokio::spawn(get(address));
        started.recv().await;
        assert_eq!(app.in_flight.load(Ordering::SeqCst), 1);
        let _ = stop.send(());
        // The listener closes when the drain starts, so a new request is
        // refused.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while TcpStream::connect(address).await.is_ok() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the listener still accepted connections during the drain"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut served)
                .await
                .is_err(),
            "serve returned before the in-flight request finished"
        );
        let _ = release.send(true);
        let answer = response
            .await
            .unwrap_or_else(|error| panic!("{error}"))
            .unwrap_or_else(|error| panic!("in-flight request was cut: {error}"));
        assert!(
            answer.starts_with("HTTP/1.1 200")
                // The whole chunked body, through its last chunk.
                && answer.ends_with("\r\n8\r\nfinished\r\n0\r\n\r\n"),
            "in-flight request was cut: {answer:?}"
        );
        tokio::time::timeout(Duration::from_secs(5), served)
            .await
            .unwrap_or_else(|_| panic!("serve did not return after the drain"))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(app.in_flight.load(Ordering::SeqCst), 0);
    }

    // Go TestStopSignalDrainIsBoundedByGrace: a request that never finishes
    // must not hold the process past the grace.
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_signal_drain_is_bounded_by_grace() {
        let Harness {
            served,
            address,
            mut started,
            release: _release,
            stop,
            _dir,
            ..
        } = start(Duration::from_millis(150)).await;
        let _request = tokio::spawn(get(address));
        started.recv().await;
        let begin = std::time::Instant::now();
        let _ = stop.send(());
        tokio::time::timeout(Duration::from_secs(5), served)
            .await
            .unwrap_or_else(|_| panic!("the drain ignored its grace"))
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(
            begin.elapsed() <= Duration::from_secs(3),
            "drain took {:?} with a 150ms grace",
            begin.elapsed()
        );
    }

    #[test]
    fn go_duration_matches_go() {
        for (duration, want) in [
            (Duration::ZERO, "0s"),
            (Duration::from_millis(150), "150ms"),
            (Duration::from_millis(5), "5ms"),
            (Duration::from_secs(1), "1s"),
            (Duration::from_millis(1500), "1.5s"),
            (Duration::from_secs(120), "2m0s"),
            (Duration::from_secs(300), "5m0s"),
            (Duration::from_secs(3600), "1h0m0s"),
            (Duration::from_micros(1500), "1.5ms"),
        ] {
            assert_eq!(go_duration(duration), want, "{duration:?}");
        }
    }
}

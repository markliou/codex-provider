//! The Codex account pool (RUST_PORT.md step 4.6).
//!
//! One process: it accepts Codex CLI and Desktop, the admin UI and the
//! public dashboard on the combined address, picks the account, and sends
//! upstream with Codex's own HTTP and WebSocket clients, so what reaches
//! upstream is what a native Codex of the pinned release sends
//! (codex_pool::pool). Before this step a Go pool made those decisions and
//! this process was only its loopback transport and front door; that split
//! is gone, and nothing here may grow a second decider again.
//!
//! `codex-pool hash-password` (with CODEX_POOL_ADMIN_PASSWORD) prints the
//! value for CODEX_POOL_ADMIN_PASSWORD_HASH, as the Go binary did.

use codex_pool::pool::crypto::new_password_hash;
use codex_pool::pool::serve;

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("hash-password") {
        let password = std::env::var("CODEX_POOL_ADMIN_PASSWORD").unwrap_or_default();
        if password.is_empty() {
            fail("CODEX_POOL_ADMIN_PASSWORD is required for hash-password");
        }
        println!("{}", new_password_hash(&password));
        return;
    }
    if let Err(error) = serve::run().await {
        fail(&error);
    }
}

/// Go log.Fatal: the message on stderr and exit status 1.
fn fail(message: &str) -> ! {
    eprintln!(
        "{} {message}",
        chrono::Utc::now().format("%Y/%m/%d %H:%M:%S")
    );
    std::process::exit(1);
}

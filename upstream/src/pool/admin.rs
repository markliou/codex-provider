//! The control page and its static assets.
//!
//! A port of admin_assets.go. The files are the Go pool's own web assets
//! (cmd/codex-pool/web), compiled in unchanged, so the page the browser gets
//! does not change with the move to Rust. The visible version is tied to
//! build metadata: the HTML holds a placeholder instead of a hand-edited
//! release string, because stale manual footer bumps made it hard to tell
//! which fix was actually deployed.

use super::http_io::ResponseWriter;
use super::http_io::set_header;
use http::StatusCode;

const ADMIN_VERSION_PLACEHOLDER: &str = "{{CODEX_POOL_VERSION}}";
const ADMIN_PAGE_HTML: &str = include_str!("../../../cmd/codex-pool/web/admin.html");
const ADMIN_CSS: &str = include_str!("../../../cmd/codex-pool/web/app.css");
const ADMIN_JS: &str = include_str!("../../../cmd/codex-pool/web/app.js");
const ADMIN_LOGO_SVG: &str = include_str!("../../../cmd/codex-pool/web/logo.svg");
const ADMIN_MANIFEST: &str = include_str!("../../../cmd/codex-pool/web/manifest.webmanifest");

/// The build's version and commit, set at build time like Go's ldflags
/// (CODEX_POOL_BUILD_VERSION, CODEX_POOL_BUILD_COMMIT).
pub fn admin_display_version() -> String {
    format_display_version(
        option_env!("CODEX_POOL_BUILD_VERSION").unwrap_or("dev"),
        option_env!("CODEX_POOL_BUILD_COMMIT").unwrap_or("unknown"),
    )
}

/// The version with the short commit appended unless it is already part of
/// it (Go adminDisplayVersion).
pub fn format_display_version(version: &str, commit: &str) -> String {
    let mut version = version.trim().to_string();
    if version.is_empty() {
        version = "dev".to_string();
    }
    let commit = commit.trim();
    if !commit.is_empty() && commit != "unknown" && !version.contains(commit) {
        let short: String = commit.chars().take(8).collect();
        version.push('+');
        version.push_str(&short);
    }
    version
}

/// The placeholder the HTML carries for the version.
pub const VERSION_PLACEHOLDER: &str = ADMIN_VERSION_PLACEHOLDER;

pub fn admin_page() -> String {
    ADMIN_PAGE_HTML.replace(ADMIN_VERSION_PLACEHOLDER, &admin_display_version())
}

fn serve(writer: &mut dyn ResponseWriter, content_type: &str, body: &str) {
    set_header(writer, "content-type", content_type);
    set_header(writer, "cache-control", "no-store");
    writer.write_header(StatusCode::OK);
    writer.write(body.as_bytes());
}

pub fn handle_admin_page(writer: &mut dyn ResponseWriter) {
    set_header(writer, "cache-control", "no-store");
    set_header(
        writer,
        "content-security-policy",
        "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
    );
    set_header(writer, "x-codex-pool-version", &admin_display_version());
    serve(writer, "text/html; charset=utf-8", &admin_page());
}

/// The static asset at `path`, if it is one.
pub fn handle_admin_asset(writer: &mut dyn ResponseWriter, path: &str) -> bool {
    match path {
        "/admin/assets/app.css" => serve(writer, "text/css; charset=utf-8", ADMIN_CSS),
        "/admin/assets/app.js" => serve(writer, "text/javascript; charset=utf-8", ADMIN_JS),
        "/admin/assets/logo.svg" => serve(writer, "image/svg+xml", ADMIN_LOGO_SVG),
        "/admin/manifest.webmanifest" => serve(
            writer,
            "application/manifest+json; charset=utf-8",
            ADMIN_MANIFEST,
        ),
        _ => return false,
    }
    true
}

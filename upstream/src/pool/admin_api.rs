//! The control page's API: owner login and session, the owner's management
//! actions, and the public dashboard.
//!
//! A port of the /admin/api handlers in the Go pool's main.go. Owner actions
//! require the signed session cookie, and every non-GET one also the CSRF
//! token (Go requireAdmin); the public dashboard is deliberately
//! unauthenticated and acts only through opaque per-process references, never
//! a local or upstream account id. Do not collapse these policies because
//! the page and the provider share one port.

use super::app::App;
use super::app::Inner;
use super::dashboard::public_account;
use super::dashboard::public_accounts;
use super::http_io::ResponseWriter;
use super::http_io::header_value;
use super::http_io::set_header;
use super::http_io::write_json;
use super::http_io::write_openai_error;
use super::login::LoginStartError;
use super::login::public_repair_job;
use super::plan;
use super::routes::Request;
use super::state::Account;
use super::stream::MAX_REQUEST_BODY;
use super::time::Timestamp;
use super::time::hours;
use super::time::minutes;
use http::Method;
use http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

const ADMIN_LOGIN_MAX_FAILURES: u32 = 5;
const ADMIN_LOGIN_LOCKOUT_MINUTES: i64 = 15;
const SESSION_LIFETIME_HOURS: i64 = 12;
/// Go io.LimitReader(r.Body, 4096) for the small JSON bodies.
const SMALL_BODY: usize = 4096;

#[derive(Clone, Default)]
struct LoginFailure {
    count: u32,
    last_failure: Timestamp,
    locked_out_at: Timestamp,
}

/// Failed owner logins per client, for the lockout.
#[derive(Default)]
pub struct LoginFailures {
    failures: Mutex<HashMap<String, LoginFailure>>,
}

impl LoginFailures {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, LoginFailure>> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn locked_out(&self, key: &str) -> Option<Timestamp> {
        let mut failures = self.map();
        let failure = failures.get(key)?;
        if failure.locked_out_at.is_zero() {
            return None;
        }
        let until = failure
            .locked_out_at
            .add(minutes(ADMIN_LOGIN_LOCKOUT_MINUTES));
        if Timestamp::now().before(&until) {
            return Some(until);
        }
        failures.remove(key);
        None
    }

    fn record(&self, key: &str) {
        let mut failures = self.map();
        let now = Timestamp::now();
        let mut failure = failures.get(key).cloned().unwrap_or_default();
        if !failure.last_failure.is_zero()
            && now.sub(&failure.last_failure) > minutes(ADMIN_LOGIN_LOCKOUT_MINUTES)
        {
            failure = LoginFailure::default();
        }
        failure.count += 1;
        failure.last_failure = now;
        if failure.count >= ADMIN_LOGIN_MAX_FAILURES && failure.locked_out_at.is_zero() {
            failure.locked_out_at = now;
        }
        failures.insert(key.to_string(), failure);
    }

    fn clear(&self, key: &str) {
        self.map().remove(key);
    }
}

/// A cookie as Go's http.Cookie.String() writes it.
#[derive(Clone, Copy)]
struct Cookie<'a> {
    name: &'a str,
    value: &'a str,
    expires: Option<Timestamp>,
    max_age_delete: bool,
    http_only: bool,
    secure: bool,
    same_site_strict: bool,
}

impl Cookie<'_> {
    fn header(&self) -> String {
        let mut out = format!("{}={}; Path=/admin", self.name, self.value);
        if let Some(expires) = self.expires {
            out.push_str(&format!(
                "; Expires={}",
                expires
                    .0
                    .with_timezone(&chrono::Utc)
                    .format("%a, %d %b %Y %H:%M:%S GMT")
            ));
        }
        if self.max_age_delete {
            out.push_str("; Max-Age=0");
        }
        if self.http_only {
            out.push_str("; HttpOnly");
        }
        if self.secure {
            out.push_str("; Secure");
        }
        if self.same_site_strict {
            out.push_str("; SameSite=Strict");
        }
        out
    }
}

fn add_cookie(writer: &mut dyn ResponseWriter, cookie: &Cookie<'_>) {
    if let Ok(value) = http::HeaderValue::from_str(&cookie.header()) {
        writer.header().append(http::header::SET_COOKIE, value);
    }
}

/// The request's cookie of that name (Go r.Cookie): the first match.
fn request_cookie(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.trim_matches('"').to_string())
}

fn set_no_store(writer: &mut dyn ResponseWriter) {
    set_header(writer, "cache-control", "no-store");
}

fn decode<T: for<'de> Deserialize<'de>>(body: &[u8], limit: usize) -> Option<T> {
    let body = &body[..body.len().min(limit)];
    // Go's json.Decoder reads the first value and ignores what follows.
    let mut stream = serde_json::Deserializer::from_slice(body).into_iter::<T>();
    stream.next().and_then(Result::ok)
}

/// The client a failed login counts against. Behind a trusted proxy the
/// last X-Forwarded-For entry, the address the proxy vouches for; otherwise
/// the connection's own host, so a forwarded address from anywhere else
/// cannot step around the lockout.
pub fn admin_login_key(app: &App, request: &Request) -> String {
    let host = request
        .remote_addr
        .rsplit_once(':')
        .map(|(host, _)| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_string()
        })
        .filter(|host| !host.is_empty());
    if let Some(host) = host {
        if app.settings.trusted_proxy(&host) {
            let forwarded = header_value(&request.headers, "x-forwarded-for");
            let client = forwarded.rsplit(',').next().unwrap_or_default().trim();
            if client.parse::<std::net::IpAddr>().is_ok() {
                return client.to_string();
            }
        }
        return host;
    }
    if request.remote_addr.is_empty() {
        "unknown".to_string()
    } else {
        request.remote_addr.clone()
    }
}

fn cookie_secure(app: &App, request: &Request) -> bool {
    app.settings.cookie_secure
        || header_value(&request.headers, "x-forwarded-proto").eq_ignore_ascii_case("https")
}

/// Go requireAdmin: a valid session, and for anything but GET and HEAD the
/// CSRF token. Admin responses carry live state and must never be cached.
pub fn require_admin(app: &App, request: &Request, writer: &mut dyn ResponseWriter) -> bool {
    set_no_store(writer);
    let session = request_cookie(request, "codex_pool_session");
    if !session.is_some_and(|token| app.valid_session(&token)) {
        write_openai_error(
            writer,
            StatusCode::UNAUTHORIZED,
            "admin_unauthorized",
            "admin login required",
        );
        return false;
    }
    if request.method != Method::GET && request.method != Method::HEAD {
        let csrf = request_cookie(request, "codex_pool_csrf").unwrap_or_default();
        let presented = header_value(&request.headers, "x-csrf-token");
        if csrf.is_empty()
            || !super::crypto::constant_time_eq(csrf.as_bytes(), presented.as_bytes())
        {
            write_openai_error(
                writer,
                StatusCode::FORBIDDEN,
                "csrf_invalid",
                "valid CSRF token required",
            );
            return false;
        }
    }
    true
}

pub fn handle_admin_login(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    set_no_store(writer);
    let key = admin_login_key(app, request);
    if let Some(until) = app.login_failures.locked_out(&key) {
        set_header(
            writer,
            "retry-after",
            &until.sub(&Timestamp::now()).num_seconds().to_string(),
        );
        write_openai_error(
            writer,
            StatusCode::TOO_MANY_REQUESTS,
            "admin_rate_limited",
            "too many failed login attempts",
        );
        return;
    }
    #[derive(Deserialize)]
    struct Credentials {
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
    }
    let Some(credentials) = decode::<Credentials>(&request.body, SMALL_BODY) else {
        app.login_failures.record(&key);
        write_openai_error(
            writer,
            StatusCode::UNAUTHORIZED,
            "invalid_admin_credentials",
            "invalid username or password",
        );
        return;
    };
    let mut username = credentials.username.trim().to_string();
    if username.is_empty() {
        username.clone_from(&app.settings.admin_user);
    }
    if username != app.settings.admin_user
        || !super::crypto::verify_password_hash(&app.settings.admin_hash, &credentials.password)
    {
        app.login_failures.record(&key);
        write_openai_error(
            writer,
            StatusCode::UNAUTHORIZED,
            "invalid_admin_credentials",
            "invalid username or password",
        );
        return;
    }
    app.login_failures.clear(&key);
    let expires = Timestamp::now().add(hours(SESSION_LIFETIME_HOURS));
    let token = app.sign_session(&app.settings.admin_user, expires);
    let csrf = super::crypto::random_id();
    let secure = cookie_secure(app, request);
    let base = Cookie {
        name: "",
        value: "",
        expires: Some(expires),
        max_age_delete: false,
        http_only: true,
        secure,
        same_site_strict: true,
    };
    add_cookie(
        writer,
        &Cookie {
            name: "codex_pool_session",
            value: &token,
            ..base
        },
    );
    add_cookie(
        writer,
        &Cookie {
            name: "codex_pool_csrf",
            value: &csrf,
            http_only: false,
            ..base
        },
    );
    write_json(
        writer,
        StatusCode::OK,
        &json!({"ok": true, "csrfToken": csrf}),
    );
}

pub fn handle_admin_logout(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    let secure = cookie_secure(app, request);
    let base = Cookie {
        name: "",
        value: "",
        expires: None,
        max_age_delete: true,
        http_only: true,
        secure,
        same_site_strict: false,
    };
    add_cookie(
        writer,
        &Cookie {
            name: "codex_pool_session",
            ..base
        },
    );
    add_cookie(
        writer,
        &Cookie {
            name: "codex_pool_csrf",
            http_only: false,
            ..base
        },
    );
    write_json(writer, StatusCode::OK, &json!({"ok": true}));
}

pub fn handle_admin_state(app: &App, writer: &mut dyn ResponseWriter) {
    let state = app.read().admin_state(Timestamp::now());
    write_json(writer, StatusCode::OK, &json!({"ok": true, "state": state}));
}

pub fn handle_admin_settings(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Switches {
        #[serde(default)]
        preserve_pro_quota: Option<bool>,
        #[serde(default)]
        lineage_affinity: Option<bool>,
        #[serde(default)]
        quota_prime: Option<bool>,
    }
    let Some(switches) = decode::<Switches>(&request.body, SMALL_BODY) else {
        write_openai_error(
            writer,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid settings JSON",
        );
        return;
    };
    // Each Console switch posts only its own field, so an omitted field leaves
    // that setting untouched rather than resetting it.
    if switches.preserve_pro_quota.is_none()
        && switches.lineage_affinity.is_none()
        && switches.quota_prime.is_none()
    {
        write_openai_error(
            writer,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "preserveProQuota, lineageAffinity or quotaPrime is required",
        );
        return;
    }
    let mut inner = app.write();
    if let Some(value) = switches.preserve_pro_quota {
        inner.preserve_pro_quota = value;
        inner.config.preserve_pro_quota = Some(value);
    }
    if let Some(value) = switches.lineage_affinity {
        inner.lineage_affinity = value;
        inner.config.lineage_affinity = Some(value);
    }
    if let Some(value) = switches.quota_prime {
        inner.quota_prime = value;
        inner.config.quota_prime = Some(value);
    }
    if inner.save().is_err() {
        write_storage_error(writer, "unable to persist settings");
        return;
    }
    let state = inner.admin_state(Timestamp::now());
    write_json(writer, StatusCode::OK, &json!({"ok": true, "state": state}));
}

/// Intentionally unauthenticated: it powers the public control page. It is
/// redacted and limited to pool state; owner-only state stays behind
/// require_admin.
pub fn handle_public_dashboard(app: &App, writer: &mut dyn ResponseWriter) {
    set_no_store(writer);
    if !app.settings.public_dashboard {
        write_openai_error(writer, StatusCode::NOT_FOUND, "not_found", "not found");
        return;
    }
    let dashboard = app.read().public_dashboard(Timestamp::now());
    write_json(
        writer,
        StatusCode::OK,
        &json!({"ok": true, "dashboard": dashboard}),
    );
}

/// Public users act only through an opaque per-process reference. Repair is
/// a deliberately narrow exception to owner-only login: offered only for an
/// invalid in-pool credential, exposing a redacted job, and requiring the
/// exact previously verified upstream identity. Never return local account
/// or raw job ids from this surface.
pub fn handle_public_account_action(
    app: &Arc<App>,
    request: &Request,
    writer: &mut dyn ResponseWriter,
) {
    set_no_store(writer);
    if !app.settings.public_dashboard {
        write_openai_error(writer, StatusCode::NOT_FOUND, "not_found", "not found");
        return;
    }
    let not_found = |writer: &mut dyn ResponseWriter| {
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "not_found",
            "public account action not found",
        )
    };
    let rest = request
        .path
        .strip_prefix("/admin/api/public-dashboard/accounts/")
        .unwrap_or_default();
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() {
        return not_found(writer);
    }
    let (reference, action) = (parts[0], parts[1]);
    if !matches!(action, "pool-add" | "pool-remove" | "repair" | "note") {
        return not_found(writer);
    }
    let get = request.method == Method::GET;
    if get && action != "repair" || !get && request.method != Method::POST {
        return not_found(writer);
    }
    let now = Timestamp::now();
    let Some((index, item)) = ({
        let inner = app.read();
        inner
            .config
            .accounts()
            .iter()
            .enumerate()
            .find(|(_, item)| inner.public_account_ref_matches(&item.id, reference))
            .map(|(index, item)| (index, item.clone()))
    }) else {
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "not_found",
            "account not found",
        );
        return;
    };
    match action {
        "repair" if get => {
            let job = app
                .read()
                .latest_public_repair_job_for_account(&item.id)
                .cloned();
            match job {
                Some(job) => write_json(
                    writer,
                    StatusCode::OK,
                    &json!({"ok": true, "job": public_repair_job(&job)}),
                ),
                None => write_openai_error(
                    writer,
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "repair job not found",
                ),
            }
        }
        "repair" => {
            if !app.read().public_repair_available(&item) {
                write_openai_error(
                    writer,
                    StatusCode::CONFLICT,
                    "repair_unavailable",
                    "this credential does not currently require public repair",
                );
                return;
            }
            match app.start_public_repair_job(&item.id) {
                Ok(job) => write_json(
                    writer,
                    StatusCode::ACCEPTED,
                    &json!({"ok": true, "job": public_repair_job(&job)}),
                ),
                Err(LoginStartError::AnotherJobInProgress) => {
                    write_openai_error(
                        writer,
                        StatusCode::CONFLICT,
                        "login_in_progress",
                        "another sign-in is already in progress",
                    );
                }
                Err(LoginStartError::RepairIdentityMismatch) => {
                    write_openai_error(
                        writer,
                        StatusCode::CONFLICT,
                        "repair_unavailable",
                        "this credential cannot be repaired from the public page",
                    );
                }
                Err(_) => write_storage_error(writer, "unable to start sign-in repair"),
            }
        }
        _ => {
            let mut inner = app.write();
            let Some(index) = inner
                .account_index(&item.id)
                .filter(|current| *current == index)
            else {
                drop(inner);
                write_openai_error(
                    writer,
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "account not found",
                );
                return;
            };
            match action {
                "note" => {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase")]
                    struct Note {
                        #[serde(default)]
                        owner_note: String,
                    }
                    let Some(note) = decode::<Note>(&request.body, MAX_REQUEST_BODY) else {
                        write_openai_error(
                            writer,
                            StatusCode::BAD_REQUEST,
                            "invalid_request",
                            "invalid account note JSON",
                        );
                        return;
                    };
                    match plan::validate_owner_note(&note.owner_note) {
                        Ok(note) => inner.config.accounts_mut()[index].owner_note = note,
                        Err(error) => {
                            write_openai_error(
                                writer,
                                StatusCode::BAD_REQUEST,
                                "invalid_request",
                                &error,
                            );
                            return;
                        }
                    }
                }
                "pool-add" | "pool-remove" => {
                    let busy =
                        item.pending_auth_verification || inner.account_login_in_progress(&item.id);
                    // The public contract replaces Leave pool with Repair while
                    // the credential is invalid; enforced here too so a stale
                    // page cannot discard affinity instead of repairing it.
                    if busy || action == "pool-remove" && inner.public_repair_available(&item) {
                        write_openai_error(
                            writer,
                            StatusCode::CONFLICT,
                            "account_authenticating",
                            "complete sign-in repair before changing pool participation",
                        );
                        return;
                    }
                    if action == "pool-add" {
                        let slot = &mut inner.config.accounts_mut()[index];
                        slot.enabled = true;
                        slot.in_pool = true;
                    } else {
                        inner.config.accounts_mut()[index].in_pool = false;
                        inner.clear_sticky_for_account(&item.id);
                    }
                }
                _ => {}
            }
            inner.config.accounts_mut()[index].updated_at = now;
            if inner.save().is_err() {
                let message = if action == "note" {
                    "unable to persist account note"
                } else {
                    "unable to persist account"
                };
                write_storage_error(writer, message);
                return;
            }
            let current = inner.config.accounts()[index].clone();
            let account = inner.public_dashboard_account(&current, index, now);
            drop(inner);
            write_json(
                writer,
                StatusCode::OK,
                &json!({"ok": true, "account": account}),
            );
        }
    }
}

impl Inner {
    pub fn unique_account_id(&self, base: &str) -> String {
        let mut base = base.trim_matches('-').to_string();
        if !plan::valid_account_id(&base) {
            base = "acct-account".to_string();
        }
        let mut id = base.clone();
        let mut index = 2;
        while self.account(&id).is_some() {
            let suffix = format!("-{index}");
            let mut prefix = base.clone();
            if prefix.len() + suffix.len() > 80 {
                prefix.truncate(80 - suffix.len());
                prefix = prefix.trim_end_matches('-').to_string();
            }
            id = format!("{prefix}{suffix}");
            index += 1;
        }
        id
    }
}

pub fn handle_accounts(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    if request.method == Method::GET || request.method == Method::HEAD {
        let accounts = public_accounts(app.read().config.accounts());
        write_json(
            writer,
            StatusCode::OK,
            &json!({"ok": true, "accounts": accounts}),
        );
        return;
    }
    let Some(mut input) = decode::<Account>(&request.body, MAX_REQUEST_BODY) else {
        write_openai_error(
            writer,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid account JSON",
        );
        return;
    };
    // Go's account type drops fields it does not declare; nothing a client
    // posts beyond them may reach config.json.
    input.extra = Default::default();
    input.id = input.id.trim().to_string();
    match plan::validate_owner_note(&input.owner_note) {
        Ok(note) => input.owner_note = note,
        Err(error) => {
            write_openai_error(writer, StatusCode::BAD_REQUEST, "invalid_request", &error);
            return;
        }
    }
    input.email = plan::normalize_email(&input.email);
    input.organization_name = plan::clean_organization_name(&input.organization_name);
    input.label = input.label.trim().to_string();
    input.upstream_base_url = input.upstream_base_url.trim_end_matches('/').to_string();
    input.wire_api = plan::normal_wire_api(&input.wire_api).to_string();
    if input.auth_type.is_empty() {
        input.auth_type =
            if !input.upstream_base_url.is_empty() || !input.upstream_api_key.is_empty() {
                "provider_api_key"
            } else {
                "codex_device_auth"
            }
            .to_string();
    }
    if plan::is_codex_device_auth(&input) {
        input.email.clear();
        input.account_id.clear();
        input.organization_name.clear();
        input.plan_type.clear();
        input.raw_plan_type.clear();
        input.plan_family.clear();
        input.plan_limit.clear();
        input.seat_type.clear();
        input.seat_type_raw.clear();
        input.seat_type_inferred = false;
        input.quota_policy.clear();
        input.plan_rank = 0;
        if !(0..=100).contains(&input.quota_protection_threshold) {
            write_openai_error(
                writer,
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "quotaProtectionThreshold must be an integer from 0 to 100",
            );
            return;
        }
        // Verification markers are internal lifecycle state, never client
        // input: accepting them could create a permanently blocked slot or
        // inject an identity baseline that never came from a verified login.
        input.pending_auth_verification = false;
        input.pending_auth_expected_account_id.clear();
    } else {
        plan::normalize_account_plan_metadata(&mut input);
        input.plan_limit = plan::clean_plan_limit(&input.plan_limit);
        input.plan_rank = plan::plan_rank(&plan::effective_plan_family(&input));
        input.seat_type.clear();
        input.seat_type_raw.clear();
        input.seat_type_inferred = false;
        input.quota_policy.clear();
        input.quota_protection_enabled = false;
        input.quota_protection_threshold = 0;
    }
    let generate = input.id.is_empty();
    if !generate && !plan::valid_account_id(&input.id) {
        write_openai_error(
            writer,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "id must contain only letters, numbers, underscores, or dashes",
        );
        return;
    }
    let now = Timestamp::now();
    input.created_at = now;
    input.updated_at = now;
    let mut inner = app.write();
    if generate {
        input.id = inner.unique_account_id(plan::generated_account_id_base(&input));
    }
    if input.label.is_empty() {
        input.label = plan::management_credential_display_name(&input);
    }
    if plan::is_codex_device_auth(&input) {
        input.upstream_base_url.clear();
        input.upstream_api_key.clear();
        input.allowed_models.clear();
        input.excluded_models.clear();
        input.codex_home = inner.account_codex_home(&input.id).display().to_string();
        if input.enabled || input.in_pool {
            // A device-auth slot is not routable until the CLI has written
            // auth.json and quota is prepared, so new slots are staged even
            // when callers ask for immediate membership.
            input.pending_pool_activation = input.enabled && input.in_pool;
            input.enabled = false;
            input.in_pool = false;
        }
    } else if input.upstream_base_url.trim().is_empty() {
        drop(inner);
        write_openai_error(
            writer,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "upstreamBaseUrl is required for provider API key accounts",
        );
        return;
    }
    if inner.account(&input.id).is_some() {
        drop(inner);
        write_openai_error(
            writer,
            StatusCode::CONFLICT,
            "account_exists",
            "account id already exists",
        );
        return;
    }
    inner.config.accounts_mut().push(input.clone());
    if inner.save().is_err() {
        drop(inner);
        write_storage_error(writer, "unable to persist account");
        return;
    }
    drop(inner);
    write_json(
        writer,
        StatusCode::CREATED,
        &json!({"ok": true, "account": public_account(&input)}),
    );
}

pub fn handle_account_health(app: &App, writer: &mut dyn ResponseWriter) {
    let now = Timestamp::now();
    let inner = app.read();
    let items: Vec<serde_json::Value> = inner
        .config
        .accounts()
        .iter()
        .map(|item| inner.account_health_item(item, now))
        .collect();
    drop(inner);
    write_json(
        writer,
        StatusCode::OK,
        &json!({"ok": true, "accounts": items}),
    );
}

/// The 500 the admin API answers when the state or an account's files could
/// not be written.
fn write_storage_error(writer: &mut dyn ResponseWriter, message: &str) {
    write_openai_error(
        writer,
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage_error",
        message,
    );
}

pub fn quota_refresh_fields(snapshot: &super::state::QuotaSnapshot) -> serde_json::Value {
    json!({
        "quota": super::status::to_value(&snapshot.quota),
        "organizationName": plan::public_organization_name(&snapshot.organization_name),
        "rawPlanType": snapshot.raw_plan_type,
        "planFamily": snapshot.plan_family,
        "planType": snapshot.plan_type,
        "planLimit": snapshot.plan_limit,
        "seatType": snapshot.seat_type,
        "seatTypeRaw": snapshot.seat_type_raw,
        "quotaPolicy": snapshot.quota_policy,
        "usageUpdatedAt": snapshot.usage_updated_at,
        "lastSuccessfulRefreshAt": snapshot.last_successful_refresh_at,
        "freshness": snapshot.freshness,
        "provenance": snapshot.provenance,
    })
}

pub async fn handle_refresh_all_quota(app: &App, writer: &mut dyn ResponseWriter) {
    let mut results = Vec::new();
    for item in app.quota_poll_accounts() {
        // Refresh-all leaves out an account whose tokens upstream refused, as
        // the background poll does; its own refresh button still asks.
        if app.credential_refused(&item) {
            results.push(json!({"accountId": item.id, "ok": false, "error": {"code": "credential_refused", "message": "upstream refused this account's credentials; log in again"}}));
            continue;
        }
        match app.refresh_account_quota(&item.id).await {
            Ok(snapshot) => {
                let snapshot = super::quota::quota_snapshot_for_display(&snapshot, Timestamp::now());
                let mut result = quota_refresh_fields(&snapshot);
                result["accountId"] = json!(item.id);
                result["ok"] = json!(true);
                results.push(result);
            }
            Err(error) => results.push(json!({"accountId": item.id, "ok": false, "error": {"code": "quota_refresh_failed", "message": error.to_string()}})),
        }
    }
    write_json(
        writer,
        StatusCode::OK,
        &json!({"ok": true, "results": results}),
    );
}

pub fn handle_job(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    let id = request
        .path
        .strip_prefix("/admin/api/jobs/")
        .unwrap_or_default();
    let job = app.read().jobs.get(id).cloned();
    match job {
        Some(job) => write_json(writer, StatusCode::OK, &json!({"ok": true, "job": job})),
        None => write_openai_error(writer, StatusCode::NOT_FOUND, "not_found", "job not found"),
    }
}

pub fn handle_job_cancel(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    let path = request
        .path
        .strip_prefix("/admin/api/jobs/")
        .unwrap_or_default();
    let id = path
        .strip_suffix("/cancel")
        .unwrap_or(path)
        .trim_end_matches('/');
    if id.is_empty() || id == path {
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "not_found",
            "job action not found",
        );
        return;
    }
    match app.cancel_login_job(id) {
        Ok(job) => write_json(writer, StatusCode::OK, &json!({"ok": true, "job": job})),
        Err(error) => write_openai_error(writer, StatusCode::NOT_FOUND, "not_found", &error),
    }
}

pub fn handle_reset_prompt_cache_window(app: &App, writer: &mut dyn ResponseWriter) {
    let mut inner = app.write();
    inner.reset_prompt_cache_window(Timestamp::now());
    if inner.save().is_err() {
        drop(inner);
        write_storage_error(writer, "unable to persist cache window reset");
        return;
    }
    let window = inner.prompt_cache_window();
    drop(inner);
    write_json(
        writer,
        StatusCode::OK,
        &json!({"ok": true, "promptCacheWindow": window}),
    );
}

pub fn handle_sticky_sessions(app: &App, writer: &mut dyn ResponseWriter) {
    let mut inner = app.write();
    let pruned = inner.prune_expired_runtime_state(Timestamp::now());
    let mut items: Vec<super::state::StickySession> =
        inner.state.sticky_sessions().values().cloned().collect();
    for item in &mut items {
        item.expires_at = inner.sticky_expires_at(item);
    }
    items.sort_by(|left, right| left.key.cmp(&right.key));
    if pruned {
        let _ = inner.save();
    }
    drop(inner);
    write_json(
        writer,
        StatusCode::OK,
        &json!({"ok": true, "sessions": items}),
    );
}

pub fn handle_sticky_session_delete(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    let key = request
        .path
        .strip_prefix("/admin/api/sticky-sessions/")
        .unwrap_or_default()
        .to_string();
    if key.is_empty() {
        write_openai_error(
            writer,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "session key is required",
        );
        return;
    }
    let mut inner = app.write();
    inner.state.sticky_sessions_mut().remove(&key);
    inner
        .state
        .thread_bindings_mut()
        .retain(|_, binding| binding.sticky_key != key);
    if inner.save().is_err() {
        drop(inner);
        write_storage_error(writer, "unable to persist state");
        return;
    }
    drop(inner);
    write_json(writer, StatusCode::OK, &json!({"ok": true}));
}

/// POST /admin/api/accounts/{id}/{action...}.
pub async fn handle_account_action(
    app: &Arc<App>,
    request: &Request,
    writer: &mut dyn ResponseWriter,
) {
    let rest = request
        .path
        .strip_prefix("/admin/api/accounts/")
        .unwrap_or_default();
    let Some((id, action)) = rest.split_once('/') else {
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "not_found",
            "account action not found",
        );
        return;
    };
    let Some(index) = app.read().account_index(id) else {
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "not_found",
            "account not found",
        );
        return;
    };
    match action {
        "quota/reset-credit" => {
            return super::reset_credit::handle_reset_credit(app, id, writer).await;
        }
        "quota/refresh" => return super::reset_credit::handle_quota_refresh(app, id, writer).await,
        "login" => {
            let item = app.read().config.accounts()[index].clone();
            if !plan::is_codex_device_auth(&item) {
                write_openai_error(
                    writer,
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "device auth login is only available for Codex accounts",
                );
                return;
            }
            match app.start_login_job(id) {
                Ok(job) => write_json(
                    writer,
                    StatusCode::ACCEPTED,
                    &json!({"ok": true, "job": job}),
                ),
                Err(LoginStartError::AnotherJobInProgress) => {
                    write_openai_error(
                        writer,
                        StatusCode::CONFLICT,
                        "login_in_progress",
                        &LoginStartError::AnotherJobInProgress.to_string(),
                    );
                }
                Err(_) => write_storage_error(writer, "unable to persist sign-in repair state"),
            }
            return;
        }
        _ => {}
    }
    let mut inner = app.write();
    let item = inner.config.accounts()[index].clone();
    if item.pending_auth_verification
        && matches!(action, "enable" | "disable" | "pool-add" | "pool-remove")
    {
        drop(inner);
        write_openai_error(
            writer,
            StatusCode::CONFLICT,
            "account_authenticating",
            "complete sign-in repair before changing pool participation",
        );
        return;
    }
    let now = Timestamp::now();
    let bad = |writer: &mut dyn ResponseWriter, message: &str| {
        write_openai_error(writer, StatusCode::BAD_REQUEST, "invalid_request", message)
    };
    match action {
        "note" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Note {
                #[serde(default)]
                owner_note: String,
            }
            let Some(note) = decode::<Note>(&request.body, MAX_REQUEST_BODY) else {
                drop(inner);
                return bad(writer, "invalid account note JSON");
            };
            match plan::validate_owner_note(&note.owner_note) {
                Ok(note) => {
                    let slot = &mut inner.config.accounts_mut()[index];
                    slot.owner_note = note;
                    slot.updated_at = now;
                }
                Err(error) => {
                    drop(inner);
                    return bad(writer, &error);
                }
            }
            if inner.save().is_err() {
                drop(inner);
                write_storage_error(writer, "unable to persist account note");
                return;
            }
            let account = public_account(&inner.config.accounts()[index]);
            drop(inner);
            write_json(
                writer,
                StatusCode::OK,
                &json!({"ok": true, "account": account}),
            );
            return;
        }
        "enable" => inner.config.accounts_mut()[index].enabled = true,
        "disable" => {
            let slot = &mut inner.config.accounts_mut()[index];
            slot.enabled = false;
            slot.in_pool = false;
            inner.clear_sticky_for_account(id);
        }
        "pool-add" => inner.config.accounts_mut()[index].in_pool = true,
        "pool-remove" => {
            inner.config.accounts_mut()[index].in_pool = false;
            inner.clear_sticky_for_account(id);
        }
        "cooldowns/clear" => {
            inner.state.cooldowns_mut().remove(id);
        }
        "cache/reset" => {
            inner.reset_prompt_cache_window_for_account(id, now);
            if inner.save().is_err() {
                drop(inner);
                write_storage_error(writer, "unable to persist cache window reset");
                return;
            }
            let window = inner.prompt_cache_window_for_account(id);
            drop(inner);
            write_json(
                writer,
                StatusCode::OK,
                &json!({"ok": true, "accountId": id, "promptCacheWindow": window}),
            );
            return;
        }
        "quota/set" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Remaining {
                #[serde(default)]
                remaining_quota: Option<i64>,
            }
            match decode::<Remaining>(&request.body, SMALL_BODY)
                .and_then(|value| value.remaining_quota)
                .filter(|value| (0..=100).contains(value))
            {
                Some(value) => inner.config.accounts_mut()[index].remaining_quota = Some(value),
                None => {
                    drop(inner);
                    return bad(writer, "remainingQuota must be an integer from 0 to 100");
                }
            }
        }
        "quota-protection/set" => {
            if !plan::is_codex_device_auth(&item) {
                drop(inner);
                return bad(
                    writer,
                    "quota protection is only available for Codex device-auth accounts",
                );
            }
            #[derive(Deserialize)]
            struct Protection {
                #[serde(default)]
                enabled: bool,
                #[serde(default)]
                threshold: Option<i64>,
            }
            let Some((enabled, threshold)) = decode::<Protection>(&request.body, SMALL_BODY)
                .and_then(|value| {
                    value
                        .threshold
                        .filter(|threshold| (0..=100).contains(threshold))
                        .map(|threshold| (value.enabled, threshold))
                })
            else {
                drop(inner);
                return bad(writer, "threshold must be an integer from 0 to 100");
            };
            // Deliberately per local slot: duplicates may share upstream
            // capacity, but changing one slot must not mutate another slot's
            // protection policy or its sticky history.
            let slot = &mut inner.config.accounts_mut()[index];
            slot.quota_protection_enabled = enabled;
            slot.quota_protection_threshold = threshold;
        }
        _ => {
            drop(inner);
            write_openai_error(
                writer,
                StatusCode::NOT_FOUND,
                "not_found",
                "account action not found",
            );
            return;
        }
    }
    inner.config.accounts_mut()[index].updated_at = now;
    if inner.save().is_err() {
        drop(inner);
        write_storage_error(writer, "unable to persist account");
        return;
    }
    let account = public_account(&inner.config.accounts()[index]);
    drop(inner);
    write_json(
        writer,
        StatusCode::OK,
        &json!({"ok": true, "account": account}),
    );
}

pub fn handle_account_delete(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    let id = request
        .path
        .strip_prefix("/admin/api/accounts/")
        .unwrap_or_default()
        .to_string();
    let mut inner = app.write();
    let Some(index) = inner.account_index(&id) else {
        drop(inner);
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "not_found",
            "account not found",
        );
        return;
    };
    app.cancel_login_jobs_for_account(&mut inner, &id);
    if plan::is_codex_device_auth(&inner.config.accounts()[index]) {
        let root = inner.account_root(&id);
        if root.exists() && std::fs::remove_dir_all(&root).is_err() {
            drop(inner);
            write_storage_error(writer, "unable to purge account credentials");
            return;
        }
    }
    inner.config.accounts_mut().remove(index);
    inner.clear_account_identity_scoped_state(&id);
    if inner.save().is_err() {
        drop(inner);
        write_storage_error(writer, "unable to persist account");
        return;
    }
    drop(inner);
    write_json(writer, StatusCode::OK, &json!({"ok": true}));
}

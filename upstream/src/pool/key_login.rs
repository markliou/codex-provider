//! A Codex signed in with an API key on the native endpoint.
//!
//! The device limit counts ChatGPT logins, so a client may sign its Codex in
//! with an API key (`codex login --with-api-key`) and still use the pool's
//! native endpoint. Such a Codex sends the request its built-in provider sends
//! the Codex backend, less what only a ChatGPT login adds (Codex rust-v0.167.0,
//! measured by scripts/conformance/wire-capture.sh):
//!
//! - the MCP resource helpers (list_mcp_resources, list_mcp_resource_templates,
//!   read_mcp_resource), which a ChatGPT login offers because its apps MCP
//!   server is configured (core/src/tools/spec_plan.rs add_mcp_resource_tools:
//!   `context.mcp.has_servers()`);
//! - the image_gen namespace (spec_plan.rs image_generation_available:
//!   `current_auth_uses_codex_backend`, not on a Free plan, an image model);
//! - `guardian_credits_requested` in client_metadata (core/src/client.rs
//!   set_guardian_metadata: ChatGPT auth, not a Guardian reviewer).
//!
//! Upstream receives every request with a pool account's ChatGPT credential,
//! so the pool completes these per attempt, for the account the request goes
//! to, and the request is the one a native Codex signed in with that account
//! sends (SPEC.md 2.3.3, key login). The key-login Codex cannot run the
//! tools it did not offer: it answers the model's call to one with
//! "unsupported call: <name>" (core/src/tools/registry.rs
//! unsupported_tool_call_message). The pool replaces that answer with the one
//! a native Codex gives in the same situation, computed as Codex computes it
//! (mcp_resource.rs, image-generation tool.rs), so the conversation upstream
//! sees stays a native one. The answer is a pure function of the call, so the
//! same item is rewritten the same way in every request that repeats it and
//! the cached prompt prefix does not move.
//!
//! Only the spans these touch change; every other byte is forwarded as Codex
//! wrote it. What must not be simplified away: the detection (an API-key
//! Codex sends no ChatGPT-Account-ID, model-provider/src/bearer_auth_provider.rs;
//! every ChatGPT-backed login does, model-provider/src/auth.rs), the per-account
//! image gate, and leaving an answer alone when its call cannot be seen.

use http::HeaderMap;
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;

/// The tools as Codex serializes them, measured from a native Codex.
const TOOLS_JSON: &str = include_str!("key_login_tools.json");

/// The apps MCP server a ChatGPT login configures (codex-mcp
/// CODEX_APPS_MCP_SERVER_NAME); the model may reach it, since
/// `orchestrator.mcp.enabled` defaults to on (core/src/config/mod.rs
/// resolve_feature_enabled).
const CODEX_APPS_MCP_SERVER: &str = "codex_apps";

/// The shell tools Codex registers first (spec_plan.rs add_core_tool_sources:
/// add_shell_tools, then add_mcp_resource_tools).
const SHELL_TOOLS: &[&str] = &[
    "exec_command",
    "write_stdin",
    "shell",
    "shell_command",
    "local_shell",
];

/// What Codex registers after its extension tools, image_gen among them
/// (spec_plan.rs append_tool_search_executor), and the hosted tools that
/// follow every function tool.
const AFTER_EXTENSION_TOOLS: &[&str] = &["tool_search", "web_search"];

const RESOURCE_TOOLS: [&str; 3] = [
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
];
const IMAGE_GEN_NAMESPACE: &str = "image_gen";
const IMAGEGEN_TOOL: &str = "imagegen";

/// A request a Codex signed in with an API key sent: a bearer credential and
/// no ChatGPT-Account-ID. Read before the native endpoint drops the client's
/// credential.
pub fn is_key_login(headers: &HeaderMap) -> bool {
    headers.contains_key(http::header::AUTHORIZATION) && !headers.contains_key("chatgpt-account-id")
}

#[derive(Deserialize)]
struct ToolsFile {
    tools: HashMap<String, String>,
}

fn tool(name: &str) -> Option<&'static str> {
    static TOOLS: OnceLock<HashMap<String, String>> = OnceLock::new();
    TOOLS
        .get_or_init(|| {
            serde_json::from_str::<ToolsFile>(TOOLS_JSON)
                .map(|file| file.tools)
                .unwrap_or_default()
        })
        .get(name)
        .map(String::as_str)
}

/// A model's catalog entry, as far as completion reads it: whether it takes
/// images, and the catalog's own text for the MCP resource helpers
/// (protocol/src/openai_models.rs ModelMessages.tools.mcp_resources).
#[derive(Clone, Debug, Default)]
pub struct ModelFacts {
    pub takes_images: bool,
    pub resource_messages: HashMap<String, ToolMessage>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ToolMessage {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<String>,
}

#[derive(Deserialize)]
struct Catalog {
    #[serde(default)]
    models: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
    #[serde(default)]
    model_messages: Option<CatalogMessages>,
}

#[derive(Deserialize)]
struct CatalogMessages {
    #[serde(default)]
    tools: Option<CatalogTools>,
}

#[derive(Deserialize)]
struct CatalogTools {
    #[serde(default)]
    mcp_resources: Option<HashMap<String, Option<ToolMessage>>>,
}

/// What the pool has seen that completion needs: the models catalog upstream
/// last served a native-endpoint client, and the resource and image calls
/// upstream made on a socket, whose arguments a later message no longer
/// carries (over the WebSocket Codex sends only the items after
/// previous_response_id).
#[derive(Default)]
pub struct KeyLogin {
    catalog: Mutex<HashMap<String, ModelFacts>>,
    calls: Mutex<CallLog>,
}

#[derive(Default)]
struct CallLog {
    order: VecDeque<String>,
    calls: HashMap<String, Call>,
}

const CALL_LOG_LIMIT: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Call {
    tool: Tool,
    arguments: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    ListResources,
    ListTemplates,
    ReadResource,
    Imagegen,
}

impl Tool {
    fn from_call(namespace: Option<&str>, name: &str) -> Option<Self> {
        match (namespace, name) {
            (None, "list_mcp_resources") => Some(Self::ListResources),
            (None, "list_mcp_resource_templates") => Some(Self::ListTemplates),
            (None, "read_mcp_resource") => Some(Self::ReadResource),
            (Some(IMAGE_GEN_NAMESPACE), IMAGEGEN_TOOL) => Some(Self::Imagegen),
            _ => None,
        }
    }

    /// The answer of a Codex that did not offer the tool
    /// (core/src/tools/registry.rs unsupported_tool_call_message, with
    /// protocol/src/tool_name.rs Display: the namespace, then the name).
    fn unsupported(self) -> &'static str {
        match self {
            Self::ListResources => "unsupported call: list_mcp_resources",
            Self::ListTemplates => "unsupported call: list_mcp_resource_templates",
            Self::ReadResource => "unsupported call: read_mcp_resource",
            Self::Imagegen => "unsupported call: image_genimagegen",
        }
    }
}

impl KeyLogin {
    /// Keeps a models catalog upstream served (native.rs), by slug.
    pub fn note_catalog(&self, body: &[u8]) {
        let Ok(catalog) = serde_json::from_slice::<Catalog>(body) else {
            return;
        };
        let mut facts = HashMap::new();
        for model in catalog.models {
            if model.slug.is_empty() {
                continue;
            }
            let resource_messages = model
                .model_messages
                .and_then(|messages| messages.tools)
                .and_then(|tools| tools.mcp_resources)
                .map(|messages| {
                    messages
                        .into_iter()
                        .filter_map(|(name, message)| Some((name, message?)))
                        .collect()
                })
                .unwrap_or_default();
            facts.insert(
                model.slug,
                ModelFacts {
                    // A model without the field takes images, as Codex's
                    // default model info does (openai_models.rs
                    // default_input_modalities).
                    takes_images: model
                        .input_modalities
                        .is_none_or(|modalities| modalities.iter().any(|m| m == "image")),
                    resource_messages,
                },
            );
        }
        *self
            .catalog
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = facts;
    }

    /// The catalog's entry for `model`; an unknown model reads as one that
    /// takes images and keeps the built-in tool text.
    pub fn model(&self, model: &str) -> ModelFacts {
        self.catalog
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(model)
            .cloned()
            .unwrap_or(ModelFacts {
                takes_images: true,
                resource_messages: HashMap::new(),
            })
    }

    /// Records a resource or image call from an upstream event
    /// (`response.output_item.done` with a function_call item).
    pub fn note_event(&self, event: &[u8]) {
        if !memchr_contains(event, b"\"function_call\"") {
            return;
        }
        #[derive(Deserialize)]
        struct Event {
            #[serde(rename = "type", default)]
            kind: String,
            #[serde(default)]
            item: Option<Item>,
        }
        let Ok(event) = serde_json::from_slice::<Event>(event) else {
            return;
        };
        if event.kind != "response.output_item.done" {
            return;
        }
        let Some(item) = event.item else {
            return;
        };
        if let Some((call_id, call)) = item.call() {
            self.remember(call_id, call);
        }
    }

    fn remember(&self, call_id: String, call: Call) {
        let mut log = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if log.calls.insert(call_id.clone(), call).is_none() {
            log.order.push_back(call_id);
        }
        while log.order.len() > CALL_LOG_LIMIT {
            if let Some(old) = log.order.pop_front() {
                log.calls.remove(&old);
            }
        }
    }

    fn recall(&self, call_id: &str) -> Option<Call> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .calls
            .get(call_id)
            .cloned()
    }
}

fn memchr_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    arguments: String,
    #[serde(default)]
    output: Option<Value>,
}

impl Item {
    fn call(&self) -> Option<(String, Call)> {
        if self.kind != "function_call" || self.call_id.is_empty() {
            return None;
        }
        let tool = Tool::from_call(self.namespace.as_deref(), &self.name)?;
        Some((
            self.call_id.clone(),
            Call {
                tool,
                arguments: self.arguments.clone(),
            },
        ))
    }
}

/// What to add to one request, decided for the account it goes to.
pub struct Completion<'a> {
    /// image_gen is offered: the account is not on the Free plan and the
    /// model takes images (spec_plan.rs image_generation_available).
    pub image_gen: bool,
    /// guardian_credits_requested is added: a Responses request (HTTP or
    /// socket), not a compaction, and not from a Guardian reviewer
    /// (client.rs set_guardian_metadata; ext/guardian-reviewer session.rs
    /// is_basic_session_source).
    pub guardian: bool,
    /// The catalog's text for the resource helpers.
    pub resource_messages: &'a HashMap<String, ToolMessage>,
}

/// Whether a request comes from a Guardian reviewer, which Codex sends
/// without guardian_credits_requested: its socket names itself
/// (x-codex-guardian: reviewer, client.rs is_guardian_reviewer) and its
/// session is the "guardian" subagent (session.rs GUARDIAN_REVIEWER_NAME).
pub fn is_guardian_reviewer(headers: &HeaderMap) -> bool {
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    value("x-codex-guardian") == "reviewer" || value("x-openai-subagent") == "guardian"
}

#[derive(Deserialize)]
struct Top<'a> {
    #[serde(borrow, default)]
    tools: Option<&'a RawValue>,
    #[serde(borrow, default)]
    client_metadata: Option<&'a RawValue>,
    #[serde(borrow, default)]
    input: Option<&'a RawValue>,
}

/// One edit: replace body[start..end] with text.
struct Splice {
    start: usize,
    end: usize,
    text: String,
}

fn offset(body: &str, span: &str) -> usize {
    span.as_ptr() as usize - body.as_ptr() as usize
}

/// `body` (a Responses request or a response.create message) completed into
/// the request a Codex signed in with ChatGPT sends.
pub fn complete<'a>(body: &'a str, completion: &Completion<'_>, seen: &KeyLogin) -> Cow<'a, str> {
    let Ok(top) = serde_json::from_str::<Top<'_>>(body) else {
        return Cow::Borrowed(body);
    };
    let mut splices = Vec::new();
    if let Some(tools) = top.tools
        && let Some(text) = completed_tools(tools.get(), completion)
    {
        splices.push(Splice {
            start: offset(body, tools.get()),
            end: offset(body, tools.get()) + tools.get().len(),
            text,
        });
    }
    if completion.guardian
        && let Some(metadata) = top.client_metadata
        && let Some(text) = with_guardian_credits(metadata.get())
    {
        splices.push(Splice {
            start: offset(body, metadata.get()),
            end: offset(body, metadata.get()) + metadata.get().len(),
            text,
        });
    }
    if let Some(input) = top.input {
        splices.extend(answered_calls(body, input.get(), seen));
    }
    if splices.is_empty() {
        return Cow::Borrowed(body);
    }
    splices.sort_by_key(|splice| std::cmp::Reverse(splice.start));
    let mut out = body.to_string();
    for splice in splices {
        out.replace_range(splice.start..splice.end, &splice.text);
    }
    Cow::Owned(out)
}

#[derive(Deserialize)]
struct ToolHead {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    name: String,
}

/// The tools array with what a ChatGPT login offers and this one does not,
/// in Codex's order; None when nothing is missing. A tool the client already
/// offers (its own MCP servers bring the resource helpers) is not repeated.
fn completed_tools(span: &str, completion: &Completion<'_>) -> Option<String> {
    let items: Vec<&RawValue> = serde_json::from_str(span).ok()?;
    let heads: Vec<ToolHead> = items
        .iter()
        .map(|item| {
            serde_json::from_str(item.get()).unwrap_or(ToolHead {
                kind: String::new(),
                name: String::new(),
            })
        })
        .collect();
    let offered = |name: &str| heads.iter().any(|head| head.name == name);
    // Where each addition goes in the client's array, applied from the back
    // so an earlier position stays valid; at the same position the resource
    // helpers go first, as Codex registers them first.
    let mut additions: Vec<(usize, Vec<String>)> = Vec::new();
    if !RESOURCE_TOOLS.iter().any(|name| offered(name))
        && let Some(helpers) = resource_helpers(completion)
    {
        let at = heads
            .iter()
            .take_while(|head| SHELL_TOOLS.contains(&head.name.as_str()))
            .count();
        additions.push((at, helpers));
    }
    if completion.image_gen
        && !offered(IMAGE_GEN_NAMESPACE)
        && let Some(image_gen) = tool(IMAGE_GEN_NAMESPACE)
    {
        let at = heads
            .iter()
            .position(|head| {
                AFTER_EXTENSION_TOOLS.contains(&head.kind.as_str())
                    || AFTER_EXTENSION_TOOLS.contains(&head.name.as_str())
            })
            .unwrap_or(heads.len());
        additions.push((at, vec![image_gen.to_string()]));
    }
    if additions.is_empty() {
        return None;
    }
    additions.sort_by_key(|(at, _)| *at);
    let mut texts: Vec<String> = items.iter().map(|item| item.get().to_string()).collect();
    for (at, added) in additions.into_iter().rev() {
        for (i, text) in added.into_iter().enumerate() {
            texts.insert(at + i, text);
        }
    }
    Some(format!("[{}]", texts.join(",")))
}

/// The three resource helpers with the catalog's text; None when the catalog
/// gives one a schema of its own, which Codex applies through its own schema
/// parser (tools/src/catalog_tool_message.rs). The pool does not reproduce
/// that parser, so such a model gets none of the helpers rather than a set
/// Codex would not send.
fn resource_helpers(completion: &Completion<'_>) -> Option<Vec<String>> {
    RESOURCE_TOOLS
        .iter()
        .map(|name| {
            let text = tool(name)?;
            match completion.resource_messages.get(*name) {
                Some(message) if message.parameters.is_some() => None,
                Some(ToolMessage {
                    description: Some(description),
                    ..
                }) => with_description(text, name, description),
                _ => Some(text.to_string()),
            }
        })
        .collect()
}

/// A resource helper with the catalog's description in place of Codex's
/// built-in one (catalog_tool_message.rs with_catalog_tool_message).
fn with_description(text: &str, name: &str, description: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Described {
        description: String,
    }
    let built_in = serde_json::from_str::<Described>(text).ok()?.description;
    let old = format!(
        "\"name\":{},\"description\":{}",
        serde_json::to_string(name).ok()?,
        serde_json::to_string(&built_in).ok()?
    );
    let new = format!(
        "\"name\":{},\"description\":{}",
        serde_json::to_string(name).ok()?,
        serde_json::to_string(description).ok()?
    );
    text.contains(&old).then(|| text.replacen(&old, &new, 1))
}

/// client_metadata with guardian_credits_requested; None when it is there.
/// Codex builds client_metadata from a HashMap, so its key order is no
/// order a reader can rely on.
fn with_guardian_credits(span: &str) -> Option<String> {
    let metadata: HashMap<String, &RawValue> = serde_json::from_str(span).ok()?;
    if metadata.contains_key("guardian_credits_requested") {
        return None;
    }
    let rest = span.strip_prefix('{')?;
    let separator = if metadata.is_empty() { "" } else { "," };
    Some(format!(
        "{{\"guardian_credits_requested\":\"true\"{separator}{rest}"
    ))
}

/// The splices that turn each "unsupported call" answer to a completed tool
/// into a native Codex's answer. The call is read from the same input or,
/// over a socket, from the upstream event that made it; an answer whose call
/// is in neither is left as it is.
fn answered_calls(body: &str, span: &str, seen: &KeyLogin) -> Vec<Splice> {
    let Ok(items) = serde_json::from_str::<Vec<&RawValue>>(span) else {
        return Vec::new();
    };
    let parsed: Vec<Option<Item>> = items
        .iter()
        .map(|item| serde_json::from_str::<Item>(item.get()).ok())
        .collect();
    let calls: HashMap<String, Call> = parsed.iter().flatten().filter_map(Item::call).collect();
    let mut splices = Vec::new();
    for (raw, item) in items.iter().zip(&parsed) {
        let Some(item) = item else { continue };
        if item.kind != "function_call_output" {
            continue;
        }
        let Some(Value::String(output)) = &item.output else {
            continue;
        };
        let Some(call) = calls
            .get(&item.call_id)
            .cloned()
            .or_else(|| seen.recall(&item.call_id))
        else {
            continue;
        };
        if output != call.tool.unsupported() {
            continue;
        }
        let Some(answer) = native_answer(call.tool, &call.arguments) else {
            continue;
        };
        let (Ok(old), Ok(new)) = (
            serde_json::to_string(output),
            serde_json::to_string(&answer),
        ) else {
            continue;
        };
        let needle = format!("\"output\":{old}");
        let text = raw.get();
        let Some(at) = text.find(&needle) else {
            continue;
        };
        let start = offset(body, text) + at;
        splices.push(Splice {
            start,
            end: start + needle.len(),
            text: format!("\"output\":{new}"),
        });
    }
    splices
}

#[derive(Debug, Default, Deserialize)]
struct ListResourceArgs {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadResourceArgs {
    server: String,
    uri: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ImagegenArgs {
    prompt: String,
    #[serde(default)]
    transparent_background: bool,
    referenced_image_paths: Option<Vec<String>>,
    num_last_images_to_include: Option<usize>,
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim().to_string();
        (!trimmed.is_empty()).then_some(trimmed)
    })
}

/// The answer a native Codex signed in with ChatGPT gives the call, with only
/// its apps MCP server configured and that server listing nothing. None when
/// that answer depends on something the pool cannot know (a read from the
/// apps server, an image edit of local files); the client's answer then
/// stands.
fn native_answer(tool: Tool, arguments: &str) -> Option<String> {
    match tool {
        Tool::ListResources | Tool::ListTemplates => Some(list_answer(tool, arguments)),
        Tool::ReadResource => read_answer(arguments),
        Tool::Imagegen => imagegen_answer(arguments),
    }
}

/// core/src/tools/handlers/mcp_resource.rs parse_arguments.
fn parse_arguments(arguments: &str) -> Result<Option<Value>, String> {
    if arguments.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(arguments)
        .map_err(|err| format!("failed to parse function arguments: {err}"))?;
    Ok((!value.is_null()).then_some(value))
}

/// list_mcp_resources and list_mcp_resource_templates: resource_environments
/// (a named server must be known), then ListResourceArgs::target and the
/// aggregate listing over every server the model may reach.
fn list_answer(tool: Tool, arguments: &str) -> String {
    let field = if tool == Tool::ListResources {
        "resources"
    } else {
        "resourceTemplates"
    };
    let arguments = match parse_arguments(arguments) {
        Ok(arguments) => arguments,
        Err(message) => return message,
    };
    let args = match arguments {
        None => ListResourceArgs::default(),
        Some(value) => match serde_json::from_value::<ListResourceArgs>(value) {
            Ok(args) => args,
            Err(err) => return format!("failed to parse function arguments: {err}"),
        },
    };
    match normalize_optional(args.server) {
        Some(server) if server == CODEX_APPS_MCP_SERVER => {
            format!("{{\"server\":\"{CODEX_APPS_MCP_SERVER}\",\"{field}\":[]}}")
        }
        Some(server) => format!("unknown MCP server '{server}'"),
        None if normalize_optional(args.cursor).is_some() => {
            "cursor can only be used when a server is specified".to_string()
        }
        None => format!("{{\"{field}\":[]}}"),
    }
}

/// read_mcp_resource: ReadResourceArgs::parse, then resource_environments.
fn read_answer(arguments: &str) -> Option<String> {
    let arguments = match parse_arguments(arguments) {
        Ok(arguments) => arguments,
        Err(message) => return Some(message),
    };
    let Some(value) = arguments else {
        return Some("failed to parse function arguments: expected value".to_string());
    };
    let args = match serde_json::from_value::<ReadResourceArgs>(value) {
        Ok(args) => args,
        Err(err) => return Some(format!("failed to parse function arguments: {err}")),
    };
    let Some(server) = normalize_optional(Some(args.server)) else {
        return Some("server must be provided".to_string());
    };
    if normalize_optional(Some(args.uri)).is_none() {
        return Some("uri must be provided".to_string());
    }
    if server == CODEX_APPS_MCP_SERVER {
        return None;
    }
    Some(format!("unknown MCP server '{server}'"))
}

/// image_gen.imagegen (ext/image-generation/src/tool.rs): the arguments are
/// parsed, and a new image is requested from images/generations. The pool
/// does not run the request; the answer is Codex's own for an answer with no
/// image in it. An edit of earlier images reads them on the client, which the
/// pool cannot answer for.
fn imagegen_answer(arguments: &str) -> Option<String> {
    match serde_json::from_str::<ImagegenArgs>(arguments) {
        Err(err) => Some(err.to_string()),
        Ok(args)
            if args.referenced_image_paths.is_some()
                || args.num_last_images_to_include.is_some() =>
        {
            None
        }
        Ok(_) => Some("image generation returned no image data".to_string()),
    }
}

impl super::app::App {
    /// `body` completed for `account` (see the module comment): image_gen
    /// unless the account is on the Free plan (Codex reads the plan from the
    /// login, login/src/auth/manager.rs account_plan_type) or the model takes
    /// no images, and guardian_credits_requested on a Responses request that
    /// is not a Guardian reviewer's.
    pub fn complete_key_login(
        &self,
        account: &super::state::Account,
        body: bytes::Bytes,
        headers: &HeaderMap,
        responses: bool,
    ) -> bytes::Bytes {
        #[derive(Deserialize)]
        struct Model {
            #[serde(default)]
            model: String,
        }
        let Ok(text) = std::str::from_utf8(&body) else {
            return body;
        };
        let model = serde_json::from_str::<Model>(text)
            .map(|top| top.model)
            .unwrap_or_default();
        let facts = self.key_login.model(&model);
        let plan = if account.raw_plan_type.is_empty() {
            &account.plan_type
        } else {
            &account.raw_plan_type
        };
        let completion = Completion {
            image_gen: !plan.eq_ignore_ascii_case("free") && facts.takes_images,
            guardian: responses && !is_guardian_reviewer(headers),
            resource_messages: &facts.resource_messages,
        };
        match complete(text, &completion, &self.key_login) {
            Cow::Borrowed(_) => body,
            Cow::Owned(completed) => bytes::Bytes::from(completed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_messages() -> HashMap<String, ToolMessage> {
        HashMap::new()
    }

    fn completion(messages: &HashMap<String, ToolMessage>, image_gen: bool) -> Completion<'_> {
        Completion {
            image_gen,
            guardian: true,
            resource_messages: messages,
        }
    }

    // The tools of a key-login Codex 0.166 request, by name only; enough for
    // the order.
    fn key_body() -> String {
        r#"{"model":"gpt-5.5","instructions":"x","input":[],"tools":[{"type":"function","name":"exec_command"},{"type":"function","name":"write_stdin"},{"type":"function","name":"request_user_input"},{"type":"custom","name":"apply_patch"},{"type":"function","name":"update_goal"},{"type":"tool_search"},{"type":"web_search"}],"tool_choice":"auto","client_metadata":{"session_id":"s","x-codex-window-id":"w:0"},"prompt_cache_key":"k"}"#.to_string()
    }

    fn tool_names(body: &str) -> Vec<String> {
        let value: Value = serde_json::from_str(body).unwrap();
        value["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                tool["name"]
                    .as_str()
                    .or(tool["type"].as_str())
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn detects_a_key_login() {
        let mut headers = HeaderMap::new();
        assert!(!is_key_login(&headers));
        headers.insert("authorization", "Bearer sk-x".parse().unwrap());
        assert!(is_key_login(&headers));
        headers.insert("chatgpt-account-id", "acct".parse().unwrap());
        assert!(!is_key_login(&headers));
    }

    #[test]
    fn adds_what_a_chatgpt_login_offers_in_codex_order() {
        let messages = no_messages();
        let body = key_body();
        let out = complete(&body, &completion(&messages, true), &KeyLogin::default());
        assert_eq!(
            tool_names(&out),
            [
                "exec_command",
                "write_stdin",
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource",
                "request_user_input",
                "apply_patch",
                "update_goal",
                "image_gen",
                "tool_search",
                "web_search",
            ]
        );
        // The added tools are Codex's bytes, and nothing outside the edited
        // spans moved.
        assert!(out.contains(tool("read_mcp_resource").unwrap()));
        assert!(out.contains(tool("image_gen").unwrap()));
        assert!(out.starts_with(r#"{"model":"gpt-5.5","instructions":"x","input":[],"tools":["#));
        assert!(out.ends_with(r#""tool_choice":"auto","client_metadata":{"guardian_credits_requested":"true","session_id":"s","x-codex-window-id":"w:0"},"prompt_cache_key":"k"}"#));
        // Done twice, the second pass adds nothing.
        let again = complete(&out, &completion(&messages, true), &KeyLogin::default());
        assert_eq!(again, out);
    }

    #[test]
    fn leaves_out_image_gen_when_codex_would_and_keeps_the_clients_own_helpers() {
        let messages = no_messages();
        let body = key_body();
        let out = complete(&body, &completion(&messages, false), &KeyLogin::default());
        assert!(!tool_names(&out).contains(&"image_gen".to_string()));
        let own = body.replace(
            r#"{"type":"function","name":"write_stdin"}"#,
            r#"{"type":"function","name":"write_stdin"},{"type":"function","name":"list_mcp_resources"}"#,
        );
        let out = complete(&own, &completion(&messages, false), &KeyLogin::default());
        assert_eq!(
            tool_names(&out)
                .iter()
                .filter(|name| name.contains("mcp_resource"))
                .count(),
            1
        );
    }

    #[test]
    fn a_reviewer_and_a_compaction_get_no_guardian_credits() {
        let messages = no_messages();
        let body = key_body();
        let mut no_guardian = completion(&messages, true);
        no_guardian.guardian = false;
        let out = complete(&body, &no_guardian, &KeyLogin::default());
        assert!(!out.contains("guardian_credits_requested"));
        let mut headers = HeaderMap::new();
        assert!(!is_guardian_reviewer(&headers));
        headers.insert("x-codex-guardian", "reviewer".parse().unwrap());
        assert!(is_guardian_reviewer(&headers));
        let mut headers = HeaderMap::new();
        headers.insert("x-openai-subagent", "guardian".parse().unwrap());
        assert!(is_guardian_reviewer(&headers));
        assert_eq!(
            with_guardian_credits("{}").unwrap(),
            r#"{"guardian_credits_requested":"true"}"#
        );
    }

    #[test]
    fn takes_the_catalogs_description() {
        let mut messages = HashMap::new();
        messages.insert(
            "list_mcp_resources".to_string(),
            ToolMessage {
                description: Some("Catalog text.".to_string()),
                parameters: None,
            },
        );
        let body = key_body();
        let out = complete(&body, &completion(&messages, false), &KeyLogin::default());
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["tools"][2]["description"], "Catalog text.");
        assert_eq!(
            value["tools"][2]["parameters"]["properties"]["server"]["description"],
            "MCP server name. Omit to list resources from every configured server."
        );
        messages.insert(
            "read_mcp_resource".to_string(),
            ToolMessage {
                description: None,
                parameters: Some("{}".to_string()),
            },
        );
        let out = complete(&body, &completion(&messages, false), &KeyLogin::default());
        assert!(!out.contains("mcp_resource"));
    }

    // The answers a native Codex 0.166 gave in the keytools scenario of
    // wire-capture.sh, and the rest of mcp_resource.rs's branches.
    #[test]
    fn answers_as_a_native_codex() {
        assert_eq!(
            list_answer(Tool::ListResources, "{}"),
            r#"{"resources":[]}"#
        );
        assert_eq!(
            list_answer(Tool::ListTemplates, ""),
            r#"{"resourceTemplates":[]}"#
        );
        assert_eq!(
            list_answer(Tool::ListResources, r#"{"server":" codex_apps "}"#),
            r#"{"server":"codex_apps","resources":[]}"#
        );
        assert_eq!(
            list_answer(Tool::ListTemplates, r#"{"server":"docs"}"#),
            "unknown MCP server 'docs'"
        );
        assert_eq!(
            list_answer(Tool::ListResources, r#"{"cursor":"c"}"#),
            "cursor can only be used when a server is specified"
        );
        assert!(
            list_answer(Tool::ListResources, "{")
                .starts_with("failed to parse function arguments: ")
        );
        assert_eq!(
            read_answer(r#"{"server":"docs","uri":"docs://readme"}"#).unwrap(),
            "unknown MCP server 'docs'"
        );
        assert_eq!(read_answer(r#"{"server":"codex_apps","uri":"u"}"#), None);
        assert_eq!(
            read_answer(r#"{"server":" ","uri":"u"}"#).unwrap(),
            "server must be provided"
        );
        assert_eq!(
            read_answer("").unwrap(),
            "failed to parse function arguments: expected value"
        );
        assert_eq!(
            read_answer(r#"{"server":"docs"}"#).unwrap(),
            "failed to parse function arguments: missing field `uri`"
        );
        assert_eq!(
            imagegen_answer(r#"{"prompt":"A red square on a white background."}"#).unwrap(),
            "image generation returned no image data"
        );
        assert_eq!(
            imagegen_answer(r#"{"prompt":"p","num_last_images_to_include":1}"#),
            None
        );
        assert!(
            imagegen_answer(r#"{"prompt":"p","size":"big"}"#)
                .unwrap()
                .starts_with("unknown field `size`")
        );
    }

    #[test]
    fn rewrites_the_unsupported_answer_from_the_input_or_the_socket() {
        let seen = KeyLogin::default();
        let messages = no_messages();
        let mut quiet = completion(&messages, false);
        quiet.guardian = false;
        let call = r#"{"type":"function_call","status":"completed","id":"fc_1","name":"list_mcp_resources","arguments":"{}","call_id":"call_1"}"#;
        let output = r#"{"type":"function_call_output","id":"fco_1","call_id":"call_1","output":"unsupported call: list_mcp_resources","internal_chat_message_metadata_passthrough":{"turn_id":"t"}}"#;
        let tools = r#""tools":[{"type":"function","name":"list_mcp_resources"},{"type":"function","name":"list_mcp_resource_templates"},{"type":"function","name":"read_mcp_resource"}]"#;
        let body = format!(r#"{{"input":[{call},{output}],{tools}}}"#);
        let out = complete(&body, &quiet, &seen);
        assert_eq!(
            out,
            body.replace(
                r#""output":"unsupported call: list_mcp_resources""#,
                r#""output":"{\"resources\":[]}""#
            )
        );
        // Over a socket the call came in an upstream event and the message
        // carries only its output.
        let alone = format!(
            r#"{{"type":"response.create","previous_response_id":"resp_1","input":[{}],{tools}}}"#,
            output
                .replace("call_1", "call_9")
                .replace("list_mcp_resources", "image_genimagegen")
        );
        assert_eq!(complete(&alone, &quiet, &seen), alone);
        seen.note_event(br#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_9","call_id":"call_9","name":"imagegen","namespace":"image_gen","arguments":"{\"prompt\":\"p\"}","status":"completed"}}"#);
        assert!(
            complete(&alone, &quiet, &seen)
                .contains(r#""output":"image generation returned no image data""#)
        );
        // Any other answer, and an answer to a tool Codex runs, stay.
        let other = body.replace(
            "unsupported call: list_mcp_resources",
            "{\"resources\":[1]}",
        );
        assert_eq!(complete(&other, &quiet, &seen), other);
    }

    #[test]
    fn reads_the_catalog() {
        let seen = KeyLogin::default();
        seen.note_catalog(br#"{"models":[{"slug":"text-only","input_modalities":["text"]},{"slug":"m","model_messages":{"tools":{"mcp_resources":{"read_mcp_resource":{"description":"d"}}}}}]}"#);
        assert!(!seen.model("text-only").takes_images);
        let m = seen.model("m");
        assert!(m.takes_images);
        assert_eq!(
            m.resource_messages["read_mcp_resource"]
                .description
                .as_deref(),
            Some("d")
        );
        assert!(seen.model("unknown").takes_images);
    }

    #[test]
    fn the_tools_file_is_codexs_release() {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Release {
            codex_release: String,
        }
        let release: Release = serde_json::from_str(TOOLS_JSON).unwrap();
        let cargo = include_str!("../../Cargo.toml");
        assert!(cargo.contains(&format!("tag = \"{}\"", release.codex_release)));
        for name in [
            "list_mcp_resources",
            "list_mcp_resource_templates",
            "read_mcp_resource",
            "image_gen",
        ] {
            let text = tool(name).unwrap();
            let value: Value = serde_json::from_str(text).unwrap();
            assert_eq!(value["name"], name);
        }
    }
}

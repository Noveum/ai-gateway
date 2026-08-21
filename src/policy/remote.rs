//! Native HTTP bridge to the Noveum platform NovaGuard API.
//!
//! When `NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` are set, the gateway fetches
//! its Nova Guard policies from the platform on startup (instead of, or in
//! addition to, the local bundle) and queries the live cost/rate counters
//! (`.../policies/state`) per request so `cost_cap`/`rate_limit` enforce against
//! real spend instead of failing open. The pure JSON↔gateway mapping lives in
//! [`crate::policy::platform`]; this module is the native HTTP + cache layer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::policy::config::PolicyBundle;
use crate::policy::engine::PolicyEngine;
use crate::policy::platform;
use crate::policy::rules::LiveState;

/// Dedicated HTTP client for the Noveum platform API. Unlike `proxy::CLIENT`
/// (which forces HTTP/2 prior knowledge for HTTPS provider calls), this
/// negotiates the HTTP version normally so it works against a plain HTTP/1.1
/// control plane (e.g. a local `http://localhost:3000`) as well as HTTPS.
pub(crate) static PLATFORM_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .use_rustls_tls()
        // A stable, explicit User-Agent. The production api.noveum.ai edge
        // (CDN/WAF) rejects requests with no UA with a 403 before they reach the
        // application, which silently killed the whole platform bridge (policies,
        // state, and usage all go through this client).
        .user_agent(concat!("noveum-ai-gateway/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build Noveum platform HTTP client")
});

/// Default platform base URL when `NOVEUM_API_URL` is unset.
const DEFAULT_API_URL: &str = "https://api.noveum.ai";
/// How long a fetched live-state snapshot is reused before refetching. The
/// platform's `/state` is itself cached ~30s, so a short client TTL is plenty.
const STATE_TTL: Duration = Duration::from_secs(10);
/// How often the background poller re-fetches policy definitions from
/// `/policies/effective`. The platform sets `Cache-Control: max-age=30`; ~60s
/// (with conditional `If-None-Match`) keeps policies fresh cheaply.
const POLICY_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Connection details for the platform NovaGuard API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteConfig {
    pub base_url: String,
    pub api_key: String,
    pub project_id: String,
}

/// An explicitly-configured platform bridge that cannot be used as configured.
///
/// Distinct from "not configured": an operator who set one of the two required
/// variables, or set one to an empty value, is *trying* to enable enforcement.
/// Starting a silent pass-through there is the worst outcome — the gateway
/// looks healthy while no cap is in force. Callers surface this and refuse to
/// start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfigError {
    pub message: String,
}

impl std::fmt::Display for RemoteConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RemoteConfigError {}

// The env var names the platform bridge is configured with. Defined in the
// shared `platform` module (the Worker validates the same pair) and re-exported
// here so native callers can keep reaching for them via `remote::`.
pub use crate::policy::platform::{API_KEY_VAR, PROJECT_ID_VAR, TENANCY_VAR};

impl RemoteConfig {
    /// Build from env. See [`RemoteConfig::from_values`] for the semantics;
    /// this only reads the variables (`None` = unset) and supplies
    /// `NOVEUM_API_URL`.
    pub fn from_env() -> Result<Option<Self>, RemoteConfigError> {
        Self::from_values(
            std::env::var(API_KEY_VAR).ok().as_deref(),
            std::env::var(PROJECT_ID_VAR).ok().as_deref(),
            std::env::var("NOVEUM_API_URL").ok().as_deref(),
        )
    }

    /// Decide what a given pair of configuration values means. Pure, so the
    /// matrix below is testable without mutating process env.
    ///
    /// * neither variable present (`None`) → `Ok(None)`: platform-managed Nova
    ///   Guard is intentionally disabled;
    /// * both present and non-empty → `Ok(Some(config))`;
    /// * exactly one present, or either present but empty/whitespace →
    ///   `Err(RemoteConfigError)`.
    ///
    /// That last case used to be indistinguishable from "disabled", so a
    /// typo'd or half-deployed secret silently started an unguarded
    /// pass-through while the operator believed caps were being enforced.
    pub fn from_values(
        api_key: Option<&str>,
        project_id: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<Option<Self>, RemoteConfigError> {
        let err = |message: String| Err(RemoteConfigError { message });
        let blank = |v: Option<&str>| v.is_some_and(|s| s.trim().is_empty());

        // Present but blank: an unresolved template or a stripped CI variable,
        // never a deliberate choice.
        if blank(api_key) {
            return err(format!(
                "{API_KEY_VAR} is set but empty; platform-managed Nova Guard cannot start. \
                 Provide a Noveum service key with `guardrails:read` + `guardrails:ingest`, \
                 or unset both {API_KEY_VAR} and {PROJECT_ID_VAR} to run without the platform bridge."
            ));
        }
        if blank(project_id) {
            return err(format!(
                "{PROJECT_ID_VAR} is set but empty; platform-managed Nova Guard cannot start. \
                 Provide the project id to enforce for, or unset both {API_KEY_VAR} and \
                 {PROJECT_ID_VAR} to run without the platform bridge."
            ));
        }

        let (api_key, project_id) = match (api_key, project_id) {
            (None, None) => return Ok(None),
            (Some(k), Some(p)) => (k.trim().to_string(), p.trim().to_string()),
            // Exactly one of the pair: a half-applied configuration.
            (Some(_), None) => {
                return err(format!(
                    "{API_KEY_VAR} is set but {PROJECT_ID_VAR} is not; platform-managed Nova \
                     Guard needs both. Set {PROJECT_ID_VAR}, or unset {API_KEY_VAR} to run \
                     without the platform bridge."
                ))
            }
            (None, Some(_)) => {
                return err(format!(
                    "{PROJECT_ID_VAR} is set but {API_KEY_VAR} is not; platform-managed Nova \
                     Guard needs both. Set {API_KEY_VAR}, or unset {PROJECT_ID_VAR} to run \
                     without the platform bridge."
                ))
            }
        };

        let base_url = base_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_API_URL)
            .trim_end_matches('/')
            .to_string();
        Ok(Some(Self {
            base_url,
            api_key,
            project_id,
        }))
    }

    fn policies_url(&self) -> String {
        // `/effective` returns the org+project *merged*, enabled-only,
        // priority-ordered set (the plain `/policies` list is project-local,
        // unmerged, and includes disabled rows). The SDK enforces the effective
        // set, so the gateway must too.
        format!(
            "{}/api/v1/projects/{}/policies/effective",
            self.base_url, self.project_id
        )
    }

    fn state_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/state",
            self.base_url, self.project_id
        )
    }

    /// `POST` target for reporting per-call usage (ALLOWED/BLOCKED events).
    pub(crate) fn usage_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/usage",
            self.base_url, self.project_id
        )
    }

    /// Build the platform connection for ONE derived tenant in shared mode.
    ///
    /// `project_id` must come from [`select_tenant`] — i.e. from the set the
    /// credential is entitled to — never from a request header. `api_key` is
    /// the caller's own credential, because platform API keys are org-scoped:
    /// there is no gateway-wide key that could read another org's policies.
    pub fn for_tenant(base_url: &str, api_key: &str, project_id: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            project_id: project_id.to_string(),
        }
    }
}

// ===========================================================================
// Shared-gateway tenancy (§6.4 / NOV-117)
// ===========================================================================

/// How long one credential→tenant resolution is reused, in seconds.
pub const TENANT_TTL_VAR: &str = "NOVEUM_GUARD_TENANT_TTL_SECS";
/// How many distinct tenants one process keeps warm (policies + counters).
pub const TENANT_CACHE_MAX_VAR: &str = "NOVEUM_GUARD_TENANT_CACHE_MAX";

/// Header carrying the caller's Noveum platform credential in shared mode.
///
/// Deliberately **not** `Authorization`: on this gateway that header already
/// carries the upstream *provider* key and is forwarded to the provider. The
/// tenancy layer strips this one before forwarding, so a tenant credential
/// never reaches OpenAI/Anthropic/etc.
pub const TENANT_CREDENTIAL_HEADER: &str = "x-noveum-api-key";

/// Caller-supplied routing hint naming a project.
///
/// **Never identity.** It may only *select among* the projects the credential
/// is already entitled to; anything else is rejected (see [`select_tenant`]).
pub const ROUTING_PROJECT_HEADER: &str = "x-project-id";
/// Caller-supplied routing hints naming an organization (both spellings, as
/// elsewhere in the gateway). Same rule: checked against the derived org, never
/// trusted as identity.
pub const ROUTING_ORG_HEADERS: [&str; 2] = ["x-organization-id", "x-organisation-id"];

/// Read the caller's credential out of a header value. **Pure.**
///
/// Tolerates a `Bearer ` prefix (SDKs habitually add one) and surrounding
/// whitespace; a header that is present but empty is treated as absent, so a
/// stripped or unresolved template never authenticates anyone.
pub fn extract_credential(raw: Option<&str>) -> Option<&str> {
    let value = raw?.trim();
    let value = match value.split_once(char::is_whitespace) {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
        // A bare scheme with nothing after it carries no credential.
        None if value.eq_ignore_ascii_case("bearer") => "",
        _ => value,
    };
    (!value.is_empty()).then_some(value)
}

/// Default lifetime of a cached credential→tenant resolution. Matches the
/// platform's own 300s API-key cache, so the gateway is never *more* stale than
/// the control plane it mirrors.
const TENANT_TTL: Duration = Duration::from_secs(300);
/// Lifetime of a cached **denial** (401/403). Short, so that granting a key a
/// permission or creating its first project takes effect quickly, but non-zero
/// so a flood of invalid keys cannot be turned into a resolution stampede
/// against the platform.
const TENANT_DENY_TTL: Duration = Duration::from_secs(30);
/// A tenant runtime (policies, counters, reservations) untouched for this long
/// is dropped. Also what makes a revoked credential self-heal: the runtime that
/// was warmed with it goes away and the next caller re-resolves.
pub const TENANT_IDLE_TTL: Duration = Duration::from_secs(600);
/// Default cap on warm tenants per process.
const TENANT_CACHE_MAX: usize = 1024;
/// Hard bound on an in-request credential resolution, mirroring
/// `STATE_REFRESH_BUDGET`/`ADMIT_BUDGET`. Exceeding it is *unavailable* —
/// never an implicit allow, and never a fallback to some default project.
const RESOLVE_BUDGET: Duration = Duration::from_secs(2);

/// The two mutually exclusive deployment modes of platform-managed Nova Guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardTenancy {
    /// **Dedicated**: the process/Worker environment fixes ONE project id
    /// (`NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID`). Every request a replica
    /// handles is metered and capped against that project regardless of who
    /// sent it, so the deployment must serve exactly one tenant.
    Dedicated(RemoteConfig),
    /// **Shared**: every caller is authenticated and its project +
    /// organization are derived server-side from the credential. Required for
    /// any deployment that serves more than one tenant (e.g. the public
    /// `gateway.noveum.ai`).
    Shared(SharedTenancyConfig),
}

/// Connection details for shared mode. There is deliberately **no** API key
/// here: each request's platform calls are made with the caller's own
/// credential, so no single process-wide secret is ever applied to another
/// tenant's traffic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedTenancyConfig {
    pub base_url: String,
    /// TTL for the credential→tenant resolution cache.
    pub resolution_ttl: Duration,
    /// Cap on warm tenants held in one process.
    pub max_tenants: usize,
}

impl GuardTenancy {
    /// Read the mode from the environment. See [`GuardTenancy::from_values`].
    pub fn from_env() -> Result<Option<Self>, RemoteConfigError> {
        Self::from_values(
            std::env::var(TENANCY_VAR).ok().as_deref(),
            std::env::var(API_KEY_VAR).ok().as_deref(),
            std::env::var(PROJECT_ID_VAR).ok().as_deref(),
            std::env::var("NOVEUM_API_URL").ok().as_deref(),
            std::env::var(TENANT_TTL_VAR).ok().as_deref(),
            std::env::var(TENANT_CACHE_MAX_VAR).ok().as_deref(),
        )
    }

    /// Decide which deployment mode a given configuration means. Pure, so the
    /// whole matrix is testable without mutating process env.
    ///
    /// * `NOVEUM_GUARD_TENANCY` unset → the historical behavior, unchanged:
    ///   `NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` mean dedicated, neither
    ///   means disabled, one of the two is an error. **Shared mode is never
    ///   inferred** — it is only ever entered by asking for it.
    /// * `dedicated` → the same, but the operator said so; the pair is then
    ///   *required* (asking for dedicated with no project id is an error, not a
    ///   silent "disabled").
    /// * `shared` → per-caller tenancy. `NOVEUM_GUARD_PROJECT_ID` **and**
    ///   `NOVEUM_API_KEY` must both be absent: a process-wide project id is the
    ///   other mode, and a process-wide key would be one tenant's credential
    ///   applied to everyone's traffic.
    /// * anything else → an error naming the two valid values.
    pub fn from_values(
        tenancy: Option<&str>,
        api_key: Option<&str>,
        project_id: Option<&str>,
        base_url: Option<&str>,
        ttl_secs: Option<&str>,
        max_tenants: Option<&str>,
    ) -> Result<Option<Self>, RemoteConfigError> {
        let err = |message: String| Err(RemoteConfigError { message });
        let mode = tenancy.map(str::trim).filter(|s| !s.is_empty());

        // `NOVEUM_GUARD_TENANCY=` (present but blank) is an unresolved template
        // or a stripped CI variable — the same class of failure
        // `RemoteConfig::from_values` refuses for the credential pair.
        if tenancy.is_some_and(|s| s.trim().is_empty()) {
            return err(format!(
                "{TENANCY_VAR} is set but empty; set it to `dedicated` (one process-wide \
                 {PROJECT_ID_VAR}) or `shared` (project + organization derived per request from \
                 the caller's credential), or unset it."
            ));
        }

        match mode.map(str::to_ascii_lowercase).as_deref() {
            // Unset: exactly the pre-existing semantics.
            None => Ok(RemoteConfig::from_values(api_key, project_id, base_url)?
                .map(GuardTenancy::Dedicated)),
            Some("dedicated") => match RemoteConfig::from_values(api_key, project_id, base_url)? {
                Some(cfg) => Ok(Some(GuardTenancy::Dedicated(cfg))),
                None => err(format!(
                    "{TENANCY_VAR}=dedicated pins this deployment to one project, but neither \
                     {API_KEY_VAR} nor {PROJECT_ID_VAR} is set. Set both, or unset {TENANCY_VAR} \
                     to run without the platform bridge."
                )),
            },
            Some("shared") => {
                // Both modes configured at once. Which one wins is exactly the
                // kind of question that must never be answered by precedence.
                if project_id.is_some() {
                    return err(format!(
                        "{TENANCY_VAR}=shared and {PROJECT_ID_VAR} are both set, but they are \
                         mutually exclusive deployment modes: shared mode derives the project \
                         from each caller's credential, while {PROJECT_ID_VAR} pins every request \
                         to one project. Unset {PROJECT_ID_VAR} for a shared gateway, or set \
                         {TENANCY_VAR}=dedicated for a single-project one."
                    ));
                }
                if api_key.is_some() {
                    return err(format!(
                        "{TENANCY_VAR}=shared and {API_KEY_VAR} are both set. In shared mode every \
                         platform call is made with the CALLER's credential; a process-wide \
                         {API_KEY_VAR} would apply one tenant's key — and one tenant's \
                         organization — to every caller's traffic. Unset {API_KEY_VAR}."
                    ));
                }
                let base_url = base_url
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(DEFAULT_API_URL)
                    .trim_end_matches('/')
                    .to_string();
                Ok(Some(GuardTenancy::Shared(SharedTenancyConfig {
                    base_url,
                    resolution_ttl: parse_duration_secs(ttl_secs, TENANT_TTL_VAR, TENANT_TTL)?,
                    max_tenants: parse_positive(
                        max_tenants,
                        TENANT_CACHE_MAX_VAR,
                        TENANT_CACHE_MAX,
                    )?,
                })))
            }
            Some(other) => err(format!(
                "{TENANCY_VAR}={other:?} is not a deployment mode; use `dedicated` (one \
                 process-wide {PROJECT_ID_VAR}) or `shared` (project + organization derived per \
                 request from the caller's credential)."
            )),
        }
    }

    /// The dedicated-mode configuration, if this is dedicated mode.
    pub fn dedicated(&self) -> Option<&RemoteConfig> {
        match self {
            GuardTenancy::Dedicated(cfg) => Some(cfg),
            GuardTenancy::Shared(_) => None,
        }
    }

    /// The platform base URL, whichever mode this is.
    pub fn base_url(&self) -> &str {
        match self {
            GuardTenancy::Dedicated(cfg) => &cfg.base_url,
            GuardTenancy::Shared(cfg) => &cfg.base_url,
        }
    }
}

/// Parse a positive integer env value, refusing a present-but-unusable one.
fn parse_positive(
    raw: Option<&str>,
    var: &str,
    default: usize,
) -> Result<usize, RemoteConfigError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(default),
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| RemoteConfigError {
                message: format!("{var}={v:?} is not a positive integer."),
            }),
    }
}

/// Parse a positive whole-second duration env value.
fn parse_duration_secs(
    raw: Option<&str>,
    var: &str,
    default: Duration,
) -> Result<Duration, RemoteConfigError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(default),
        Some(v) => v
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .map(Duration::from_secs)
            .ok_or_else(|| RemoteConfigError {
                message: format!("{var}={v:?} is not a positive number of seconds."),
            }),
    }
}

/// A local policy bundle configured alongside shared tenancy is a silent no-op:
/// in shared mode every guarded request is evaluated against its own tenant's
/// policy set, so a process-wide bundle would be loaded and never consulted.
/// Returns the error message to abort startup with, if any.
pub fn shared_mode_local_bundle_conflict(
    shared: bool,
    policies_file: Option<&str>,
    policies_inline: Option<&str>,
) -> Option<String> {
    if !shared {
        return None;
    }
    let set = |v: Option<&str>| v.is_some_and(|s| !s.trim().is_empty());
    let which = match (set(policies_file), set(policies_inline)) {
        (true, true) => "NOVEUM_GUARD_POLICIES_FILE and NOVEUM_GUARD_POLICIES are",
        (true, false) => "NOVEUM_GUARD_POLICIES_FILE is",
        (false, true) => "NOVEUM_GUARD_POLICIES is",
        (false, false) => return None,
    };
    Some(format!(
        "{which} set together with {TENANCY_VAR}=shared. In shared mode every request is \
         evaluated against the policy set of the tenant derived from its credential, so a \
         process-wide local bundle would be loaded and never enforced — and a local \
         `cost_cap`/`rate_limit` would be one counter shared by every tenant. Remove the local \
         bundle, or run a dedicated gateway."
    ))
}

/// The tenant a request is enforced, metered and cached under.
///
/// **Derived, never received.** Both halves come from the platform's answer for
/// the caller's credential; nothing a client can set reaches this type except
/// by matching a value the credential is already entitled to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId {
    pub organization_id: String,
    pub project_id: String,
}

impl TenantId {
    pub fn new(organization_id: impl Into<String>, project_id: impl Into<String>) -> Self {
        Self {
            organization_id: organization_id.into(),
            project_id: project_id.into(),
        }
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.organization_id, self.project_id)
    }
}

/// What a credential is allowed to act as, as reported by the platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub organization_id: String,
    /// Every project the credential may enforce/meter against.
    pub projects: Vec<String>,
}

/// Full, non-reversible identity used for authorization-bearing caches.
///
/// It deliberately implements neither `Debug` nor `Display`: cache diagnostics
/// use [`credential_fingerprint`] instead, so a full credential identity cannot
/// accidentally become a log or metrics label.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CredentialCacheKey([u8; 32]);

pub(crate) fn credential_cache_key(secret: &str) -> CredentialCacheKey {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    CredentialCacheKey(key)
}

/// A short, non-reversible credential label for logs only.
///
/// Forty-eight bits are sufficient for human correlation but not for an
/// authorization cache key; [`credential_cache_key`] is always used internally
/// wherever a collision could otherwise reuse another caller's identity.
pub fn credential_fingerprint(secret: &str) -> String {
    let key = credential_cache_key(secret);
    let mut out = String::with_capacity(16);
    for b in key.0.iter().take(6) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Why a request was refused before any tenant could be established.
///
/// Every variant is a refusal: there is deliberately no "could not resolve, so
/// use the default project" path. On a shared gateway that would bill and cap
/// an unauthenticated stranger against whatever tenant happened to be first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TenantRejection {
    /// No credential presented at all.
    MissingCredential,
    /// The platform rejected the credential (unknown, expired, revoked).
    InvalidCredential,
    /// Authenticated, but the credential lacks a permission the gateway needs.
    InsufficientScope(String),
    /// Authenticated, but `x-project-id` names a project outside the
    /// credential's entitlements.
    ProjectNotEntitled { requested: String },
    /// Authenticated, but `x-organization-id` disagrees with the derived org.
    OrganizationMismatch { requested: String },
    /// The credential is entitled to several projects and the request named
    /// none, so there is nothing to meter against without guessing.
    AmbiguousProject { candidates: usize },
    /// The credential's organization has no project at all.
    NoProjects,
    /// Resolution could not be completed (transport, 5xx, timeout, malformed
    /// answer). Fail closed — never an allow.
    Unavailable(String),
}

impl TenantRejection {
    /// The HTTP status this refusal is served with.
    ///
    /// * `401` — no credential, or one the platform does not accept. Presenting
    ///   a different credential is what fixes it.
    /// * `403` — the caller *is* authenticated but is not entitled to what it
    ///   asked for. Note the gateway answers 403 identically for "that project
    ///   belongs to another organization" and "that project does not exist", so
    ///   it discloses nothing beyond the caller's own entitlements. A silent
    ///   override to the derived project would be worse than any status code:
    ///   the caller would believe it routed to X while being billed and capped
    ///   under Y.
    /// * `400` — the credential is entitled to several projects and the request
    ///   selected none; the request is missing a parameter this deployment
    ///   needs, and guessing would meter the wrong project.
    /// * `503` — the gateway could not reach a verdict. Fail closed.
    pub fn status(&self) -> u16 {
        match self {
            Self::MissingCredential | Self::InvalidCredential => 401,
            Self::InsufficientScope(_)
            | Self::ProjectNotEntitled { .. }
            | Self::OrganizationMismatch { .. }
            | Self::NoProjects => 403,
            Self::AmbiguousProject { .. } => 400,
            Self::Unavailable(_) => 503,
        }
    }

    /// Stable machine-readable code for the error envelope.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingCredential => "missing_noveum_credential",
            Self::InvalidCredential => "invalid_noveum_credential",
            Self::InsufficientScope(_) => "insufficient_scope",
            Self::ProjectNotEntitled { .. } => "project_not_entitled",
            Self::OrganizationMismatch { .. } => "organization_mismatch",
            Self::AmbiguousProject { .. } => "project_id_required",
            Self::NoProjects => "no_entitled_project",
            Self::Unavailable(_) => "tenant_resolution_unavailable",
        }
    }

    /// OpenAI-style `error.type`, so SDK error handling behaves sensibly.
    pub fn error_type(&self) -> &'static str {
        match self.status() {
            401 => "authentication_error",
            403 => "permission_error",
            400 => "invalid_request_error",
            _ => "gateway_error",
        }
    }

    /// Operator-facing message. Never echoes the credential.
    pub fn message(&self) -> String {
        match self {
            Self::MissingCredential => format!(
                "this gateway is shared: every request must present a Noveum API key in the \
                 `{TENANT_CREDENTIAL_HEADER}` header, from which the project and organization to \
                 enforce against are derived."
            ),
            Self::InvalidCredential => {
                "the Noveum API key in `".to_string()
                    + TENANT_CREDENTIAL_HEADER
                    + "` was not accepted by the Noveum platform (unknown, expired or revoked)."
            }
            Self::InsufficientScope(detail) => format!(
                "the Noveum API key is valid but lacks a required permission: {detail}. A shared \
                 gateway needs `projects:read` (to derive the caller's project), `guardrails:read` \
                 (policies + live state) and `guardrails:ingest` (usage reporting)."
            ),
            Self::ProjectNotEntitled { requested } => format!(
                "the Noveum API key is not entitled to project {requested:?}; \
                 `{ROUTING_PROJECT_HEADER}` may only select among the projects the key already \
                 has access to and is never trusted as identity on its own."
            ),
            Self::OrganizationMismatch { requested } => format!(
                "`{}` requested organization {requested:?}, which is not the organization this \
                 Noveum API key belongs to.",
                ROUTING_ORG_HEADERS[0]
            ),
            Self::AmbiguousProject { candidates } => format!(
                "this Noveum API key is entitled to {candidates} projects, so the gateway cannot \
                 tell which one to meter this request against. Send \
                 `{ROUTING_PROJECT_HEADER}` naming one of them."
            ),
            Self::NoProjects => {
                "the Noveum API key's organization has no project, so there is nothing to enforce \
                 or meter against."
                    .to_string()
            }
            Self::Unavailable(detail) => format!(
                "could not verify the Noveum API key with the platform ({detail}); the request is \
                 refused rather than served under an unverified tenant."
            ),
        }
    }
}

impl std::fmt::Display for TenantRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Pick the tenant for one request. **Pure** — this is the whole authorization
/// decision, and it is decided against the platform's answer, not the headers.
///
/// The routing headers are *filters*, not identity:
/// * an org header that disagrees with the derived org is rejected outright
///   (it cannot be a harmless hint — the caller believes it is routing
///   elsewhere);
/// * a project header is honored only if the credential is entitled to it;
/// * no project header and exactly one entitlement → that project;
/// * no project header and several entitlements → refused, because any choice
///   the gateway invented would meter someone's spend against the wrong budget.
pub fn select_tenant(
    identity: &ResolvedIdentity,
    requested_project: Option<&str>,
    requested_org: Option<&str>,
) -> Result<TenantId, TenantRejection> {
    fn hint(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|s| !s.is_empty())
    }

    if let Some(org) = hint(requested_org) {
        if org != identity.organization_id {
            return Err(TenantRejection::OrganizationMismatch {
                requested: org.to_string(),
            });
        }
    }

    match hint(requested_project) {
        Some(project) => {
            if identity.projects.iter().any(|p| p == project) {
                Ok(TenantId::new(&identity.organization_id, project))
            } else {
                Err(TenantRejection::ProjectNotEntitled {
                    requested: project.to_string(),
                })
            }
        }
        None => match identity.projects.as_slice() {
            [] => Err(TenantRejection::NoProjects),
            [only] => Ok(TenantId::new(&identity.organization_id, only)),
            many => Err(TenantRejection::AmbiguousProject {
                candidates: many.len(),
            }),
        },
    }
}

/// Parse the platform's project listing into an identity. **Pure.**
///
/// Accepts the bare array the platform returns today, plus `{data:[…]}` /
/// `{projects:[…]}` envelopes. An entry without an `organizationId` — or a
/// listing whose entries disagree about it — is an error, not a guess: the
/// organization every counter and cap is keyed by must come from the platform
/// or not at all.
pub fn parse_projects_payload(json: &serde_json::Value) -> Result<ResolvedIdentity, String> {
    let items = json
        .get("projects")
        .and_then(|v| v.as_array())
        .or_else(|| json.get("data").and_then(|v| v.as_array()))
        .or_else(|| json.as_array())
        .ok_or_else(|| "project listing was not an array".to_string())?;

    let mut organization_id: Option<String> = None;
    let mut projects = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "a project entry has no `id`".to_string())?;
        let org = item
            .get("organizationId")
            .or_else(|| item.get("organization_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("project {id:?} has no `organizationId`"))?;
        match &organization_id {
            None => organization_id = Some(org.to_string()),
            Some(seen) if seen != org => {
                return Err(format!(
                    "project listing spans several organizations ({seen:?} and {org:?}); refusing \
                     to guess which one this credential acts as"
                ))
            }
            Some(_) => {}
        }
        projects.push(id.to_string());
    }
    projects.sort();
    projects.dedup();

    Ok(ResolvedIdentity {
        // An empty listing carries no organization; `select_tenant` refuses it
        // with `NoProjects` before the id is ever used.
        organization_id: organization_id.unwrap_or_default(),
        projects,
    })
}

/// Turn one resolution response into an identity or a refusal. **Pure**, so the
/// whole status-code matrix is testable without a server (mirrors
/// [`crate::policy::admission::classify_admit`]).
pub fn classify_resolution(status: u16, body: &[u8]) -> Result<ResolvedIdentity, TenantRejection> {
    match status {
        200..=299 => match serde_json::from_slice::<serde_json::Value>(body) {
            Ok(json) => parse_projects_payload(&json).map_err(|e| {
                // A 200 we cannot read is NOT an authentication failure and must
                // not be cached as one — but it is certainly not an allow.
                TenantRejection::Unavailable(format!("unreadable project listing: {e}"))
            }),
            Err(e) => Err(TenantRejection::Unavailable(format!(
                "invalid JSON in project listing: {e}"
            ))),
        },
        401 => Err(TenantRejection::InvalidCredential),
        403 => Err(TenantRejection::InsufficientScope(
            "the platform refused shared-gateway resolution with 403 (needs `projects:read`, \
             `guardrails:read`, and `guardrails:ingest`)"
                .to_string(),
        )),
        other => Err(TenantRejection::Unavailable(format!(
            "project listing returned {other}: {}",
            truncate_body(&String::from_utf8_lossy(body))
        ))),
    }
}

/// A cached resolution outcome.
#[derive(Clone)]
enum CachedIdentity {
    Ok(Arc<ResolvedIdentity>),
    /// A *definitive* refusal (401/403). Transport failures are never cached:
    /// an outage must not lock a valid key out for the whole deny TTL.
    Denied(TenantRejection),
}

struct ResolverSlot {
    entry: Mutex<Option<(CachedIdentity, Instant)>>,
}

/// Resolves a caller credential to the tenant it may act as, with a TTL cache.
///
/// One async slot per credential means the resolution is single-flighted per
/// key (a burst of first requests from one tenant makes one platform call)
/// without serializing unrelated tenants behind a global lock.
pub struct TenantResolver {
    base_url: String,
    ttl: Duration,
    deny_ttl: Duration,
    budget: Duration,
    max_entries: usize,
    slots: std::sync::Mutex<HashMap<CredentialCacheKey, Arc<ResolverSlot>>>,
}

impl TenantResolver {
    pub fn new(base_url: impl Into<String>, ttl: Duration, max_entries: usize) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            ttl,
            deny_ttl: TENANT_DENY_TTL,
            budget: RESOLVE_BUDGET,
            max_entries,
            slots: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Like [`TenantResolver::new`] with explicit deny-TTL and request budget (tests).
    pub fn with_timings(
        base_url: impl Into<String>,
        ttl: Duration,
        deny_ttl: Duration,
        budget: Duration,
        max_entries: usize,
    ) -> Self {
        let mut r = Self::new(base_url, ttl, max_entries);
        r.deny_ttl = deny_ttl;
        r.budget = budget;
        r
    }

    /// The platform call that establishes identity.
    ///
    /// This is deliberately the guardrail-specific project resolver rather
    /// than the general project listing. The platform protects it with every
    /// permission a shared gateway needs (`projects:read`, `guardrails:read`,
    /// and `guardrails:ingest`), so a read-only key is refused synchronously
    /// before a provider call can escape while asynchronous usage batches are
    /// being rejected.
    fn resolution_url(&self) -> String {
        format!("{}/api/v1/projects/guardrails/resolve", self.base_url)
    }

    /// Resolve `credential`, using the cache when it is fresh.
    pub async fn resolve(
        &self,
        credential: &str,
    ) -> Result<Arc<ResolvedIdentity>, TenantRejection> {
        let cache_key = credential_cache_key(credential);
        let fingerprint = credential_fingerprint(credential);
        let slot = self.slot(&cache_key);
        let mut entry = slot.entry.lock().await;

        if let Some((cached, at)) = entry.as_ref() {
            let ttl = match cached {
                CachedIdentity::Ok(_) => self.ttl,
                CachedIdentity::Denied(_) => self.deny_ttl,
            };
            if at.elapsed() < ttl {
                return match cached.clone() {
                    CachedIdentity::Ok(id) => Ok(id),
                    CachedIdentity::Denied(r) => Err(r),
                };
            }
        }

        let outcome = self.fetch_identity(credential).await;
        match &outcome {
            Ok(identity) => {
                debug!(
                    credential = %fingerprint, org = %identity.organization_id,
                    projects = identity.projects.len(),
                    "Nova Guard: resolved a caller credential to its tenant"
                );
                *entry = Some((CachedIdentity::Ok(identity.clone()), Instant::now()));
            }
            // Only a definitive verdict is cached. `Unavailable` must be retried
            // on the next request, or a blip would deny a valid key for the
            // whole deny TTL.
            Err(
                r @ (TenantRejection::InvalidCredential | TenantRejection::InsufficientScope(_)),
            ) => {
                warn!(
                    credential = %fingerprint, code = r.code(),
                    "Nova Guard: rejecting a caller credential the platform does not accept"
                );
                *entry = Some((CachedIdentity::Denied(r.clone()), Instant::now()));
            }
            Err(_) => {}
        }
        outcome
    }

    fn slot(&self, cache_key: &CredentialCacheKey) -> Arc<ResolverSlot> {
        let mut slots = self.slots.lock().expect("tenant resolver lock poisoned");
        if let Some(slot) = slots.get(cache_key) {
            return slot.clone();
        }
        // Cheap bound: a flood of distinct invalid keys must not grow this map
        // without limit. Slots are tiny and re-created on demand, so clearing
        // is safe (at worst it costs one extra platform call per live tenant).
        if slots.len() >= self.max_entries {
            warn!(
                entries = slots.len(),
                "Nova Guard: credential resolution cache full; clearing it"
            );
            slots.clear();
        }
        let slot = Arc::new(ResolverSlot {
            entry: Mutex::new(None),
        });
        slots.insert(*cache_key, slot.clone());
        slot
    }

    async fn fetch_identity(
        &self,
        credential: &str,
    ) -> Result<Arc<ResolvedIdentity>, TenantRejection> {
        let request = PLATFORM_CLIENT
            .get(self.resolution_url())
            .bearer_auth(credential)
            .send();
        let resp = match tokio::time::timeout(self.budget, request).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(TenantRejection::Unavailable(format!("request failed: {e}"))),
            Err(_) => {
                return Err(TenantRejection::Unavailable(format!(
                    "exceeded the {}s in-request resolution budget",
                    self.budget.as_secs()
                )))
            }
        };
        let status = resp.status().as_u16();
        let body = resp.bytes().await.unwrap_or_default();
        classify_resolution(status, &body).map(Arc::new)
    }
}

/// Which warm tenants to drop. **Pure**, so the bound is testable.
///
/// Entries are `(tenant, last_used_ms)`. Everything idle beyond `idle_ttl_ms`
/// goes — that is also what makes a revoked credential self-heal — and if the
/// map is still over `max`, the least recently used are dropped until it fits.
pub fn tenant_evictions(
    entries: &[(TenantId, u64)],
    now_ms: u64,
    idle_ttl_ms: u64,
    max: usize,
) -> Vec<TenantId> {
    let mut evicted: Vec<TenantId> = Vec::new();
    let mut live: Vec<&(TenantId, u64)> = Vec::with_capacity(entries.len());
    for e in entries {
        if now_ms.saturating_sub(e.1) >= idle_ttl_ms {
            evicted.push(e.0.clone());
        } else {
            live.push(e);
        }
    }
    if live.len() > max {
        // Oldest first, then by tenant id so the choice is deterministic.
        live.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        for e in live.iter().take(live.len() - max) {
            evicted.push(e.0.clone());
        }
    }
    evicted
}

/// Result of a conditional policy fetch.
pub enum PolicyFetch {
    /// The platform returned `304 Not Modified` — keep the current policies.
    NotModified,
    /// A fresh policy set (with its `ETag`, if any, for the next conditional GET).
    Modified {
        bundle: PolicyBundle,
        etag: Option<String>,
    },
}

/// Fetch + translate the platform's effective policies, sending `If-None-Match`
/// when a prior `ETag` is known so an unchanged policy set costs a cheap `304`.
pub async fn fetch_bundle_conditional(
    cfg: &RemoteConfig,
    prev_etag: Option<&str>,
) -> Result<PolicyFetch, String> {
    let mut req = PLATFORM_CLIENT
        .get(cfg.policies_url())
        .bearer_auth(&cfg.api_key);
    if let Some(tag) = prev_etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, tag);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(PolicyFetch::NotModified);
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    // Read the body as TEXT, then decide: it is often not JSON at all (a CDN/WAF
    // serves HTML for a 403, and an edge login/challenge page can come back with
    // a 200), so a bare "invalid JSON" would mask the real failure. Both the
    // non-2xx and the unparseable-2xx error carry the raw body, truncated.
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "policies fetch returned {status}: {}",
            truncate_body(&body)
        ));
    }
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("invalid JSON ({status}): {e}: {}", truncate_body(&body)))?;
    let bundle = platform::translate_bundle(&json)?;
    Ok(PolicyFetch::Modified { bundle, etag })
}

pub(crate) use crate::policy::admission_wire::truncate_body;

/// Fetch + translate the platform's effective policies, returning the `ETag` too
/// so the caller can seed the background poller.
pub async fn fetch_bundle_with_etag(
    cfg: &RemoteConfig,
) -> Result<(PolicyBundle, Option<String>), String> {
    match fetch_bundle_conditional(cfg, None).await? {
        PolicyFetch::Modified { bundle, etag } => Ok((bundle, etag)),
        PolicyFetch::NotModified => Err("unexpected 304 without a prior ETag".to_string()),
    }
}

/// The deliberate override that lets a configured platform bridge start with no
/// policy set at all. Emergency operation only: the platform is unreachable and
/// serving traffic without caps beats serving none.
pub const ALLOW_UNGUARDED_START_VAR: &str = "NOVEUM_GUARD_ALLOW_UNGUARDED_START";

/// Build the startup engine for a configured platform bridge.
///
/// On a successful first fetch, returns the compiled engine and the `ETag` to
/// seed the poller with. When the first fetch **fails** there is no known
/// policy set, so every request would be forwarded unguarded — that is an
/// `Err`, and the caller aborts startup, unless `allow_unguarded_start` opts
/// into it explicitly.
///
/// The old behavior (fall back to the local bundle, else an empty one) is
/// deliberately gone: an operator who configured the platform bridge did not
/// ask for whatever happens to be on disk, and an empty bundle enforces
/// nothing while looking perfectly healthy.
pub async fn bootstrap_engine(
    cfg: &RemoteConfig,
    opts: crate::policy::engine::EngineOptions,
    allow_unguarded_start: bool,
) -> Result<(PolicyEngine, Option<String>), String> {
    match fetch_bundle_with_etag(cfg).await {
        Ok((bundle, etag)) => Ok((PolicyEngine::from_bundle(&bundle, opts), etag)),
        Err(e) if allow_unguarded_start => {
            // ERROR, not WARN: the gateway is up and enforcing nothing. This
            // line is the only signal that a cap an operator believes is live
            // is not, so it must clear any WARN-level log filter.
            error!(
                error = %e, override_var = ALLOW_UNGUARDED_START_VAR,
                "Nova Guard: initial platform policy fetch FAILED and the unguarded-start override \
                 is set; serving traffic with NO enforcement until a poll succeeds"
            );
            Ok((
                PolicyEngine::from_bundle(&PolicyBundle::default(), opts),
                None,
            ))
        }
        Err(e) => Err(format!(
            "the initial platform policy fetch from {} failed ({e}), so no policy set is known and \
             every request would be forwarded unguarded. Startup is aborted deliberately. Fix \
             connectivity/credentials and restart, or set {ALLOW_UNGUARDED_START_VAR}=true to \
             start unguarded anyway (emergency use only — caps and fail-closed policies will NOT \
             be enforced until a later poll succeeds).",
            cfg.base_url
        )),
    }
}

/// Spawn a background task that re-fetches `/policies/effective` every
/// [`POLICY_POLL_INTERVAL`] and hot-swaps the engine's policy set when it
/// changes. Uses `If-None-Match`/304 so an unchanged set is nearly free.
///
/// A fetch *error* is logged and the current policies are kept (a transient
/// platform blip never wipes enforcement). A *successful* empty set, by contrast,
/// is applied — the platform is the source of truth, and a project legitimately
/// having zero policies must clear the local set (mirrors the startup fetch).
pub fn spawn_policy_poller(
    cfg: RemoteConfig,
    engine: Arc<PolicyEngine>,
    initial_etag: Option<String>,
) {
    // The process-wide (dedicated-mode) poller lives as long as the process, so
    // its handle is deliberately dropped.
    std::mem::drop(spawn_policy_poller_handle(cfg, engine, initial_etag));
}

/// [`spawn_policy_poller`], returning the task handle.
///
/// Shared mode runs one poller *per warm tenant*, so it needs to abort the task
/// when a tenant is evicted — otherwise every tenant a shared gateway ever saw
/// would keep polling the platform forever.
pub fn spawn_policy_poller_handle(
    cfg: RemoteConfig,
    engine: Arc<PolicyEngine>,
    initial_etag: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut etag = initial_etag;
        let mut ticker = tokio::time::interval(POLICY_POLL_INTERVAL);
        ticker.tick().await; // consume the immediate first tick (already loaded at startup)
        loop {
            ticker.tick().await;
            match fetch_bundle_conditional(&cfg, etag.as_deref()).await {
                Ok(PolicyFetch::NotModified) => {
                    debug!("Nova Guard: policies unchanged (304)");
                }
                Ok(PolicyFetch::Modified { bundle, etag: new }) => {
                    engine.swap_bundle(&bundle);
                    etag = new;
                    info!(
                        active = engine.active_policy_count(),
                        "Nova Guard: policies refreshed from the platform"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "Nova Guard: policy refresh failed; keeping current policies");
                }
            }
        }
    })
}

/// A cached snapshot of the platform's live state, plus the metadata needed for
/// conditional revalidation.
struct StateSnapshot {
    at: Instant,
    state: LiveState,
    etag: Option<String>,
}

struct StateCache {
    snap: Option<StateSnapshot>,
    /// When the last refresh attempt *failed* (`None` = last attempt succeeded).
    /// Within [`ERROR_BACKOFF`] of a failure, callers get `None` (state
    /// unavailable) immediately instead of re-hitting an unreachable platform on
    /// every request.
    last_error_at: Option<Instant>,
}

/// After a failed refresh, how long callers report "state unavailable" without
/// retrying the fetch. Keeps an outage from adding connect-timeout latency to
/// every guarded request while staying short enough to recover quickly.
const ERROR_BACKOFF: Duration = Duration::from_secs(3);

/// Hard bound on how long an in-request `/state` refresh may run. The refresh
/// happens on the request path with the cache lock held (that is what
/// single-flights it), so a *slow* platform must not be allowed to stall every
/// guarded request for the full client timeout (10s) — beyond this budget the
/// refresh is abandoned, treated as a fetch error (unavailable + backoff), and
/// `failClosed` semantics take over. `/state` is served from a 30s server-side
/// cache, so a healthy platform answers well inside this.
const STATE_REFRESH_BUDGET: Duration = Duration::from_secs(2);

/// A cached provider of the platform's live cost/rate counters, used by the guard
/// middleware to supply `LiveState` to `cost_cap`/`rate_limit` enforcement.
pub struct RemoteLiveState {
    cfg: RemoteConfig,
    cache: Mutex<StateCache>,
    ttl: Duration,
}

impl RemoteLiveState {
    pub fn new(cfg: RemoteConfig) -> Self {
        Self::with_ttl(cfg, STATE_TTL)
    }

    /// Like [`RemoteLiveState::new`] with an explicit snapshot TTL (tests).
    pub fn with_ttl(cfg: RemoteConfig, ttl: Duration) -> Self {
        Self {
            cfg,
            cache: Mutex::new(StateCache {
                snap: None,
                last_error_at: None,
            }),
            ttl,
        }
    }

    /// Return the current live state, revalidating if the cache is older than
    /// the TTL. Sends `If-None-Match`; a `304` just refreshes the snapshot's
    /// timestamp.
    ///
    /// **An expired snapshot that cannot be revalidated is unavailable** —
    /// `None` is returned so `failClosed` policies block and fail-open policies
    /// allow, exactly as they would before any snapshot existed. Serving an
    /// arbitrarily old under-cap snapshot here would let a `/state` outage
    /// silently defeat fail-closed enforcement after warm-up.
    ///
    /// The lock is held across the refresh, which single-flights it: concurrent
    /// callers hitting an expired TTL wait for the one in-flight fetch and then
    /// read its (fresh) result, rather than serving stale data or stampeding
    /// `/state`. After a failed refresh, callers within [`ERROR_BACKOFF`] get
    /// `None` immediately (no per-request connect timeouts during an outage).
    pub async fn get(&self) -> Option<LiveState> {
        let mut guard = self.cache.lock().await;
        if let Some(snap) = &guard.snap {
            if snap.at.elapsed() < self.ttl {
                return Some(snap.state.clone());
            }
        }
        // Snapshot expired (or none yet). If the platform just failed, don't
        // pile on — report unavailable until the backoff lapses.
        if let Some(t) = guard.last_error_at {
            if t.elapsed() < ERROR_BACKOFF {
                return None;
            }
        }

        let prev_etag = guard.snap.as_ref().and_then(|s| s.etag.clone());
        let result = match tokio::time::timeout(
            STATE_REFRESH_BUDGET,
            fetch_state_conditional(&self.cfg, prev_etag.as_deref()),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(format!(
                "state fetch exceeded the {}s in-request refresh budget",
                STATE_REFRESH_BUDGET.as_secs()
            )),
        };
        match result {
            Ok(StateFetch::NotModified) => {
                guard.last_error_at = None;
                if let Some(snap) = guard.snap.as_mut() {
                    snap.at = Instant::now(); // revalidated; reset the freshness clock
                    return Some(snap.state.clone());
                }
                // A 304 with no cached snapshot shouldn't happen (we only send
                // If-None-Match when we have one) but a caching proxy in front of
                // the platform could produce it — treat as no live state.
                warn!("Nova Guard: live-state returned 304 with no cached snapshot; treating as unavailable");
                None
            }
            Ok(StateFetch::Modified { state, etag, stale }) => {
                guard.last_error_at = None;
                if stale {
                    // Counters came from the durable fallback (cache was down);
                    // they're conservative but real, so we still enforce against
                    // them — just make the condition observable.
                    warn!("Nova Guard: live-state served stale (durable fallback)");
                }
                let state = *state;
                guard.snap = Some(StateSnapshot {
                    at: Instant::now(),
                    state: state.clone(),
                    etag,
                });
                Some(state)
            }
            Err(e) => {
                guard.last_error_at = Some(Instant::now());
                warn!(
                    error = %e,
                    "Nova Guard: live-state fetch failed and snapshot is expired; treating as unavailable"
                );
                None
            }
        }
    }
}

/// How long a COMPLETED request's reservation stays in the pending ledger
/// before it is assumed to have landed in the platform counters (usage-flush
/// interval + `/state` cache TTLs, with margin). The clock starts at request
/// completion, not admission — an active request never loses its reservation.
const PENDING_SPEND_TTL: Duration = Duration::from_secs(45);

/// Backstop for reservations whose completion guard never fires (which should
/// not happen — the guard is RAII on the response body — but a leak here must
/// not poison admission forever). Far above any real request duration.
const ACTIVE_RESERVATION_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// Aggregate of the *other* in-flight/pending reservations, folded into the
/// live counters before policy evaluation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PendingTotals {
    pub cost_usd: f64,
    pub requests: u64,
    pub tokens: u64,
}

#[derive(Clone, Copy)]
struct PendingEntry {
    id: u64,
    at: Instant,
    cost_usd: f64,
    tokens: u64,
}

/// In-process ledger of usage the gateway has admitted but the platform's
/// counters cannot reflect yet (usage is posted asynchronously and `/state` is
/// cached). Each admitted request reserves its *estimated* cost, its request
/// count, and its estimated tokens; the totals are counted on top of the
/// platform counters when evaluating `cost_cap` AND `rate_limit`, closing the
/// window in which a burst of requests all pass an almost-exhausted limit.
///
/// Admission is race-free by construction: [`PendingSpend::reserve`] records
/// this request's reservation and returns the other reservations' totals in
/// one critical section, *before* the policy evaluation reads them — so two
/// concurrent requests always see each other's reservation, whichever
/// evaluates first. A request that ends up blocked releases its reservation.
///
/// Reservation lifecycle: a reservation is **active** (never expires — a
/// long-lived or streaming request keeps its cap protection for its whole
/// duration) until [`PendingSpend::complete`] fires — via the RAII
/// [`ReservationGuard`] attached to the response body, which also covers
/// client-cancellation — and then ages out [`PENDING_SPEND_TTL`] after
/// completion, by which time the real usage event has been reported and folded
/// into `/state`. Briefly double-counting a completed entry that already
/// landed only errs toward blocking *near the limit*, the correct direction
/// for a hard cap. Totals are maintained incrementally (O(1) amortized reads;
/// completed-entry pruning walks only the expired prefix, active entries are
/// bounded by in-flight concurrency). This is per-process: a true
/// cross-instance guarantee needs an atomic reservation on the platform side.
#[derive(Default)]
pub struct PendingSpend {
    inner: std::sync::Mutex<PendingInner>,
}

#[derive(Default)]
struct PendingInner {
    /// Reservations for requests still in flight (`at` = admission time).
    active: Vec<PendingEntry>,
    /// Reservations for completed requests (`at` = completion time), in
    /// completion order so pruning pops the expired prefix.
    completed: std::collections::VecDeque<PendingEntry>,
    /// Running totals over `active` + `completed`.
    totals: PendingTotals,
    next_id: u64,
}

impl PendingInner {
    fn subtract(totals: &mut PendingTotals, e: &PendingEntry) {
        totals.cost_usd -= e.cost_usd;
        totals.requests = totals.requests.saturating_sub(1);
        totals.tokens = totals.tokens.saturating_sub(e.tokens);
    }

    fn prune(&mut self) {
        while let Some(front) = self.completed.front() {
            if front.at.elapsed() >= PENDING_SPEND_TTL {
                let e = self.completed.pop_front().expect("front just checked");
                Self::subtract(&mut self.totals, &e);
            } else {
                break;
            }
        }
        // Backstop: reap active entries whose guard never fired. `active` is
        // bounded by in-flight concurrency, so the scan is cheap.
        let mut totals = self.totals;
        self.active.retain(|e| {
            let leaked = e.at.elapsed() >= ACTIVE_RESERVATION_MAX_AGE;
            if leaked {
                warn!(
                    reservation = e.id,
                    "Nova Guard: reaping leaked active reservation"
                );
                Self::subtract(&mut totals, e);
            }
            !leaked
        });
        self.totals = totals;
        if self.active.is_empty() && self.completed.is_empty() {
            self.totals = PendingTotals::default(); // reset any float drift
        }
    }
}

impl PendingSpend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically reserve this request's estimated usage (cost, one request,
    /// estimated tokens) and return `(reservation_id, other_pending_totals)` —
    /// the totals of every *other* un-expired reservation, to be folded into
    /// the counters the policy engine evaluates.
    pub fn reserve(&self, cost_usd: f64, tokens: u64) -> (u64, PendingTotals) {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        inner.prune();
        let others = inner.totals;
        let id = inner.next_id;
        inner.next_id += 1;
        inner.active.push(PendingEntry {
            id,
            at: Instant::now(),
            cost_usd: cost_usd.max(0.0),
            tokens,
        });
        inner.totals.cost_usd += cost_usd.max(0.0);
        inner.totals.requests += 1;
        inner.totals.tokens += tokens;
        (id, others)
    }

    /// Release a reservation whose request was NOT forwarded (blocked): it
    /// will consume nothing, so it must stop counting immediately.
    pub fn release(&self, reservation: u64) {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        if let Some(idx) = inner.active.iter().position(|e| e.id == reservation) {
            let e = inner.active.swap_remove(idx);
            let mut t = inner.totals;
            PendingInner::subtract(&mut t, &e);
            inner.totals = t;
            if inner.active.is_empty() && inner.completed.is_empty() {
                inner.totals = PendingTotals::default();
            }
        }
    }

    /// Mark a forwarded request as completed: its reservation keeps counting
    /// for [`PENDING_SPEND_TTL`] from NOW (covering the usage-report + state
    /// ingestion lag), then expires.
    pub fn complete(&self, reservation: u64) {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        if let Some(idx) = inner.active.iter().position(|e| e.id == reservation) {
            let mut e = inner.active.swap_remove(idx);
            e.at = Instant::now();
            inner.completed.push_back(e);
        }
    }

    /// How many reservations are still ACTIVE (admitted, not yet completed or
    /// released). Anything left here after a request has ended is a leak that
    /// would keep counting against the cap until the 15-minute backstop, so
    /// cancellation tests assert on this rather than on the totals (a
    /// *completed* entry legitimately keeps counting for its short TTL).
    pub fn active_count(&self) -> usize {
        let inner = self.inner.lock().expect("pending-spend lock poisoned");
        inner.active.len()
    }

    /// Current un-expired pending totals (prunes expired entries).
    pub fn sum(&self) -> PendingTotals {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        inner.prune();
        inner.totals
    }
}

/// RAII completion guard for an admitted request's reservation.
///
/// Created the instant [`PendingSpend::reserve`] returns — *not* once a
/// response exists — and then moved along the request: held by the middleware
/// future while it awaits upstream response headers, and finally transferred
/// into the outgoing response body. Whenever and wherever it is dropped, the
/// reservation transitions from *active* to *completed* and starts its
/// post-completion TTL:
///
/// - normal completion: the response body finished streaming;
/// - client disconnect mid-body: the wrapped body stream is dropped;
/// - **cancellation before upstream headers**: the middleware future itself is
///   dropped, taking the guard with it. This is the case a response-only guard
///   missed — the entry stayed ACTIVE until the 15-minute leak backstop and
///   kept blocking a `maxRequests: 1` policy long after the client had gone.
///
/// A request that never reaches the provider (blocked at the input phase) calls
/// [`ReservationGuard::release`] instead, dropping the reservation outright.
pub struct ReservationGuard {
    ledger: Arc<PendingSpend>,
    id: u64,
    /// Cleared by [`release`](Self::release) so `Drop` becomes a no-op.
    armed: bool,
}

impl ReservationGuard {
    pub fn new(ledger: Arc<PendingSpend>, id: u64) -> Self {
        Self {
            ledger,
            id,
            armed: true,
        }
    }

    /// The guarded reservation id (diagnostics/tests).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Give the reservation back: the request was blocked before forwarding, so
    /// it will consume nothing and must stop counting immediately rather than
    /// linger for the post-completion TTL.
    pub fn release(mut self) {
        self.armed = false;
        self.ledger.release(self.id);
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.ledger.complete(self.id);
        }
    }
}

/// Result of a conditional `/state` fetch. The state is boxed to keep the
/// variants close in size (`LiveState` carries six maps).
enum StateFetch {
    NotModified,
    Modified {
        state: Box<LiveState>,
        etag: Option<String>,
        stale: bool,
    },
}

async fn fetch_state_conditional(
    cfg: &RemoteConfig,
    prev_etag: Option<&str>,
) -> Result<StateFetch, String> {
    let mut req = PLATFORM_CLIENT
        .get(cfg.state_url())
        .bearer_auth(&cfg.api_key);
    if let Some(tag) = prev_etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, tag);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(StateFetch::NotModified);
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    // Text first, then parse — a body may be non-JSON at any status (see
    // fetch_bundle_conditional).
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "state fetch returned {status}: {}",
            truncate_body(&body)
        ));
    }
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("invalid JSON ({status}): {e}: {}", truncate_body(&body)))?;
    let stale = json.get("stale").and_then(|s| s.as_bool()).unwrap_or(false);
    Ok(StateFetch::Modified {
        state: Box::new(platform::state_to_live_state(&json)),
        etag,
        stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full configuration matrix from the review's §4.5. `None` is an unset
    /// variable; `Some("")`/`Some("  ")` is one that is set but empty.
    #[test]
    fn remote_config_distinguishes_disabled_from_misconfigured() {
        // Neither set: platform-managed Nova Guard is off, and that is fine.
        assert_eq!(RemoteConfig::from_values(None, None, None), Ok(None));

        // Both set: configured.
        let cfg = RemoteConfig::from_values(Some("sk-test"), Some("proj_1"), None)
            .unwrap()
            .expect("both values present must configure the bridge");
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.project_id, "proj_1");
        assert_eq!(cfg.base_url, DEFAULT_API_URL);

        // Exactly one set: a half-applied config must NOT start pass-through.
        for (key, project, expect_names) in [
            (Some("sk-test"), None, API_KEY_VAR),
            (None, Some("proj_1"), PROJECT_ID_VAR),
        ] {
            let e = RemoteConfig::from_values(key, project, None)
                .expect_err("exactly one variable must be a configuration error");
            assert!(
                e.message.contains(expect_names) && e.message.contains("needs both"),
                "unhelpful message: {}",
                e.message
            );
        }

        // Set but empty/whitespace: a broken secret, not a choice.
        for (key, project, culprit) in [
            (Some(""), Some("proj_1"), API_KEY_VAR),
            (Some("   "), Some("proj_1"), API_KEY_VAR),
            (Some("sk-test"), Some(""), PROJECT_ID_VAR),
            (Some("sk-test"), Some("\t "), PROJECT_ID_VAR),
            (Some(""), Some(""), API_KEY_VAR),
            // Empty on one side and absent on the other is still explicit.
            (Some(""), None, API_KEY_VAR),
            (None, Some(""), PROJECT_ID_VAR),
        ] {
            let e = RemoteConfig::from_values(key, project, None)
                .expect_err("an empty value must be a configuration error");
            assert!(
                e.message.contains(culprit) && e.message.contains("empty"),
                "unhelpful message for ({key:?}, {project:?}): {}",
                e.message
            );
        }
    }

    #[test]
    fn remote_config_normalizes_values_and_base_url() {
        // Surrounding whitespace on a real value is trimmed, not treated as
        // part of the key (a common copy-paste / `echo` artifact in secrets).
        let cfg = RemoteConfig::from_values(
            Some(" sk-test\n"),
            Some(" proj_1 "),
            Some("  https://api.example.com/  "),
        )
        .unwrap()
        .unwrap();
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.project_id, "proj_1");
        // Trailing slashes are stripped so the URL builders can't double up.
        assert_eq!(cfg.base_url, "https://api.example.com");
        assert_eq!(
            cfg.policies_url(),
            "https://api.example.com/api/v1/projects/proj_1/policies/effective"
        );
        // An unset or blank base URL falls back to the default host.
        for base in [None, Some(""), Some("   ")] {
            let c = RemoteConfig::from_values(Some("k"), Some("p"), base)
                .unwrap()
                .unwrap();
            assert_eq!(c.base_url, DEFAULT_API_URL, "base={base:?}");
        }
    }

    #[test]
    fn reserve_returns_only_other_reservations() {
        let p = PendingSpend::new();
        let (_r1, others1) = p.reserve(0.01, 100);
        assert_eq!(others1, PendingTotals::default(), "first sees nothing");
        let (_r2, others2) = p.reserve(0.02, 50);
        assert!(
            (others2.cost_usd - 0.01).abs() < 1e-12,
            "second sees the first"
        );
        assert_eq!(others2.requests, 1);
        assert_eq!(others2.tokens, 100);
        let sum = p.sum();
        assert!((sum.cost_usd - 0.03).abs() < 1e-12);
        assert_eq!(sum.requests, 2);
        assert_eq!(sum.tokens, 150);
    }

    #[test]
    fn zero_cost_requests_still_reserve_request_and_token_capacity() {
        // An unpriced model can't reserve cost, but its request count and
        // tokens must still count toward rate limits.
        let p = PendingSpend::new();
        let (_r, _) = p.reserve(0.0, 42);
        let sum = p.sum();
        assert_eq!(sum.cost_usd, 0.0);
        assert_eq!(sum.requests, 1);
        assert_eq!(sum.tokens, 42);
    }

    #[test]
    fn release_removes_a_blocked_reservation() {
        let p = PendingSpend::new();
        let (r1, _) = p.reserve(0.01, 10);
        let (_r2, _) = p.reserve(0.02, 20);
        p.release(r1);
        let sum = p.sum();
        assert!((sum.cost_usd - 0.02).abs() < 1e-12);
        assert_eq!(sum.requests, 1);
        assert_eq!(sum.tokens, 20);
        // Releasing twice (or an unknown id) is a no-op.
        p.release(r1);
        p.release(999);
        assert_eq!(p.sum().requests, 1);
    }

    #[test]
    fn active_reservations_do_not_expire_and_complete_starts_the_ttl() {
        let p = PendingSpend::new();
        let (r, _) = p.reserve(0.01, 10);
        // Active entries never age out via the completed-prefix pruning; the
        // reservation is still counted regardless of admission age (the
        // 15-minute leak backstop is not reachable in a unit test).
        assert_eq!(p.sum().requests, 1);
        // Completing moves it to the TTL'd set — still counted immediately
        // after completion (the usage hasn't landed in /state yet).
        p.complete(r);
        assert_eq!(p.sum().requests, 1, "completed entries count until TTL");
        // Completing or releasing again is a no-op.
        p.complete(r);
        p.release(r);
        assert_eq!(p.sum().requests, 1);
    }

    // ---------------------------------------------------------------
    // Shared-gateway tenancy (§6.4 / NOV-117)
    // ---------------------------------------------------------------

    /// Shorthand: `from_values` with only the three interesting inputs.
    fn tenancy(
        mode: Option<&str>,
        key: Option<&str>,
        project: Option<&str>,
    ) -> Result<Option<GuardTenancy>, RemoteConfigError> {
        GuardTenancy::from_values(mode, key, project, None, None, None)
    }

    #[test]
    fn unset_tenancy_is_exactly_the_previous_behavior() {
        // The whole point of leaving the variable unset: dedicated deployments
        // that predate this change keep working, byte for byte.
        assert_eq!(tenancy(None, None, None), Ok(None));
        let cfg = tenancy(None, Some("sk-test"), Some("proj_1"))
            .unwrap()
            .expect("the credential pair still configures the bridge");
        assert_eq!(
            cfg,
            GuardTenancy::Dedicated(
                RemoteConfig::from_values(Some("sk-test"), Some("proj_1"), None)
                    .unwrap()
                    .unwrap()
            )
        );
        assert_eq!(cfg.dedicated().unwrap().project_id, "proj_1");
        // And a half-applied pair is still an error, with the same message.
        assert!(tenancy(None, Some("sk-test"), None)
            .unwrap_err()
            .message
            .contains("needs both"));
    }

    #[test]
    fn shared_mode_is_never_inferred_only_asked_for() {
        // There is no combination of the historical variables that produces
        // shared mode. It is entered by naming it, and only by naming it.
        for (key, project) in [
            (None, None),
            (Some("sk-test"), Some("proj_1")),
            (Some("sk-test"), Some("proj_2")),
        ] {
            let got = tenancy(None, key, project).unwrap();
            assert!(
                !matches!(got, Some(GuardTenancy::Shared(_))),
                "shared mode inferred from ({key:?}, {project:?})"
            );
        }
        let shared = tenancy(Some("shared"), None, None).unwrap().unwrap();
        assert!(matches!(shared, GuardTenancy::Shared(_)));
        assert_eq!(shared.base_url(), DEFAULT_API_URL);
        assert!(shared.dedicated().is_none());
        // Case and padding are operator typos, not different modes.
        for spelling in ["SHARED", " Shared ", "shared\n"] {
            assert!(matches!(
                tenancy(Some(spelling), None, None).unwrap().unwrap(),
                GuardTenancy::Shared(_)
            ));
        }
    }

    #[test]
    fn both_modes_at_once_is_a_startup_error() {
        // The single most dangerous misconfiguration: a shared public gateway
        // that also carries a process-wide project id. Whichever one "won"
        // would be a silent decision about whose budget pays for everyone.
        let e = tenancy(Some("shared"), None, Some("proj_1")).unwrap_err();
        assert!(
            e.message.contains("mutually exclusive") && e.message.contains(PROJECT_ID_VAR),
            "unhelpful message: {}",
            e.message
        );
        // A process-wide API key is the same hazard wearing a different hat: it
        // is one tenant's credential, and therefore one tenant's organization.
        let e = tenancy(Some("shared"), Some("sk-test"), None).unwrap_err();
        assert!(
            e.message.contains(API_KEY_VAR) && e.message.contains("CALLER"),
            "unhelpful message: {}",
            e.message
        );
        // Both at once still fails (on the project id, the more explicit clash).
        assert!(tenancy(Some("shared"), Some("sk-test"), Some("p")).is_err());
    }

    #[test]
    fn dedicated_mode_must_actually_be_configured() {
        // Asking for dedicated with no credentials is not "disabled": the
        // operator asked for enforcement and would get a silent pass-through.
        let e = tenancy(Some("dedicated"), None, None).unwrap_err();
        assert!(
            e.message.contains("neither") && e.message.contains(API_KEY_VAR),
            "unhelpful message: {}",
            e.message
        );
        // Half-applied is still the pre-existing error.
        assert!(tenancy(Some("dedicated"), None, Some("p")).is_err());
        // Fully applied works and is indistinguishable from the implicit form.
        assert_eq!(
            tenancy(Some("dedicated"), Some("k"), Some("p")).unwrap(),
            tenancy(None, Some("k"), Some("p")).unwrap()
        );
    }

    #[test]
    fn unknown_or_blank_tenancy_is_rejected_loudly() {
        for bad in ["multi", "per-tenant", "true", "1"] {
            let e = tenancy(Some(bad), None, None).unwrap_err();
            assert!(
                e.message.contains("not a deployment mode"),
                "unhelpful message for {bad:?}: {}",
                e.message
            );
        }
        // Present but blank = an unresolved template / stripped CI variable.
        for blank in ["", "   ", "\t"] {
            let e = tenancy(Some(blank), Some("k"), Some("p")).unwrap_err();
            assert!(e.message.contains("set but empty"), "{}", e.message);
        }
    }

    #[test]
    fn shared_tenancy_tuning_values_are_validated() {
        let cfg = GuardTenancy::from_values(
            Some("shared"),
            None,
            None,
            Some("https://api.example.com/"),
            Some("60"),
            Some("8"),
        )
        .unwrap()
        .unwrap();
        match cfg {
            GuardTenancy::Shared(s) => {
                assert_eq!(s.base_url, "https://api.example.com");
                assert_eq!(s.resolution_ttl, Duration::from_secs(60));
                assert_eq!(s.max_tenants, 8);
            }
            other => panic!("expected shared, got {other:?}"),
        }
        // Unusable tuning values are errors, not silent defaults: a "0 second"
        // resolution TTL would hammer the platform on every request.
        for (ttl, max) in [
            (Some("0"), None),
            (Some("-1"), None),
            (Some("abc"), None),
            (None, Some("0")),
            (None, Some("lots")),
        ] {
            assert!(
                GuardTenancy::from_values(Some("shared"), None, None, None, ttl, max).is_err(),
                "ttl={ttl:?} max={max:?} was accepted"
            );
        }
    }

    #[test]
    fn a_local_bundle_alongside_shared_mode_is_refused() {
        // It would be loaded and never consulted — and a local cost_cap would be
        // one counter shared by every tenant on the box.
        assert!(shared_mode_local_bundle_conflict(false, Some("/p.json"), None).is_none());
        assert!(shared_mode_local_bundle_conflict(true, None, None).is_none());
        assert!(shared_mode_local_bundle_conflict(true, Some("  "), Some("")).is_none());
        let e = shared_mode_local_bundle_conflict(true, Some("/p.json"), None).unwrap();
        assert!(e.contains("NOVEUM_GUARD_POLICIES_FILE is"), "{e}");
        let e = shared_mode_local_bundle_conflict(true, None, Some("{}")).unwrap();
        assert!(e.contains("NOVEUM_GUARD_POLICIES is"), "{e}");
        let e = shared_mode_local_bundle_conflict(true, Some("/p.json"), Some("{}")).unwrap();
        assert!(e.contains("and NOVEUM_GUARD_POLICIES are"), "{e}");
    }

    fn identity(org: &str, projects: &[&str]) -> ResolvedIdentity {
        ResolvedIdentity {
            organization_id: org.to_string(),
            projects: projects.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn a_routing_header_may_only_select_an_entitled_project() {
        let id = identity("org_a", &["proj_a1", "proj_a2"]);
        // Entitled → honored, and the tenant carries the DERIVED org, never a
        // header value.
        assert_eq!(
            select_tenant(&id, Some("proj_a2"), None).unwrap(),
            TenantId::new("org_a", "proj_a2")
        );
        // Not entitled → refused. This is the cross-tenant case: `proj_b1`
        // exists, it just isn't this credential's.
        assert_eq!(
            select_tenant(&id, Some("proj_b1"), None),
            Err(TenantRejection::ProjectNotEntitled {
                requested: "proj_b1".to_string()
            })
        );
        assert_eq!(
            select_tenant(&id, Some("proj_b1"), None)
                .unwrap_err()
                .status(),
            403
        );
        // A project that exists nowhere gets the SAME answer, so the status code
        // discloses nothing about other organizations' projects.
        assert_eq!(
            select_tenant(&id, Some("no_such_project"), None)
                .unwrap_err()
                .status(),
            403
        );
        // Whitespace-only headers are absent, not a project named "  ".
        assert_eq!(
            select_tenant(&identity("org_a", &["only"]), Some("   "), None).unwrap(),
            TenantId::new("org_a", "only")
        );
    }

    #[test]
    fn no_routing_header_resolves_to_the_credentials_own_project() {
        assert_eq!(
            select_tenant(&identity("org_a", &["proj_a1"]), None, None).unwrap(),
            TenantId::new("org_a", "proj_a1")
        );
        // Several entitlements and no selection: refused rather than guessed —
        // any guess would meter one project's spend against another's budget.
        let e = select_tenant(&identity("org_a", &["p1", "p2"]), None, None).unwrap_err();
        assert_eq!(e, TenantRejection::AmbiguousProject { candidates: 2 });
        assert_eq!(e.status(), 400);
        assert!(e.message().contains(ROUTING_PROJECT_HEADER), "{e}");
        // No projects at all: authenticated, but nothing to enforce against.
        let e = select_tenant(&identity("org_a", &[]), None, None).unwrap_err();
        assert_eq!(e, TenantRejection::NoProjects);
        assert_eq!(e.status(), 403);
    }

    #[test]
    fn an_organization_header_is_checked_never_trusted() {
        let id = identity("org_a", &["proj_a1"]);
        // Agreeing is fine (it is a no-op assertion by the caller).
        assert_eq!(
            select_tenant(&id, None, Some("org_a")).unwrap(),
            TenantId::new("org_a", "proj_a1")
        );
        // Disagreeing is refused — it can never be a harmless hint, because the
        // caller believes its traffic is being attributed elsewhere.
        let e = select_tenant(&id, Some("proj_a1"), Some("org_b")).unwrap_err();
        assert_eq!(
            e,
            TenantRejection::OrganizationMismatch {
                requested: "org_b".to_string()
            }
        );
        assert_eq!(e.status(), 403);
        // And it can never *widen* access: even paired with an entitled project.
        assert!(select_tenant(&id, Some("proj_a1"), Some("org_b")).is_err());
    }

    #[test]
    fn project_listing_parses_every_envelope_and_refuses_ambiguity() {
        let bare = serde_json::json!([
            {"id":"p1","organizationId":"org_a"},
            {"id":"p2","organizationId":"org_a"}
        ]);
        let parsed = parse_projects_payload(&bare).unwrap();
        assert_eq!(parsed, identity("org_a", &["p1", "p2"]));
        // Envelopes.
        for wrapped in [
            serde_json::json!({"projects": bare.clone()}),
            serde_json::json!({"data": bare.clone()}),
        ] {
            assert_eq!(parse_projects_payload(&wrapped).unwrap(), parsed);
        }
        // Duplicates collapse; ordering is normalized so the identity is stable.
        let dup = serde_json::json!([
            {"id":"p2","organizationId":"org_a"},
            {"id":"p1","organizationId":"org_a"},
            {"id":"p2","organizationId":"org_a"}
        ]);
        assert_eq!(parse_projects_payload(&dup).unwrap(), parsed);
        // No organizationId → an error, NOT an empty/blank org. Inventing one
        // would key every counter under "".
        let e = parse_projects_payload(&serde_json::json!([{"id":"p1"}])).unwrap_err();
        assert!(e.contains("organizationId"), "{e}");
        // Entries disagreeing about the org → refuse to pick one.
        let e = parse_projects_payload(&serde_json::json!([
            {"id":"p1","organizationId":"org_a"},
            {"id":"p2","organizationId":"org_b"}
        ]))
        .unwrap_err();
        assert!(e.contains("several organizations"), "{e}");
        // Not a list at all.
        assert!(parse_projects_payload(&serde_json::json!({"nope": 1})).is_err());
    }

    #[test]
    fn resolution_status_codes_fail_closed() {
        let ok = br#"[{"id":"p1","organizationId":"org_a"}]"#;
        assert_eq!(
            classify_resolution(200, ok).unwrap(),
            identity("org_a", &["p1"])
        );
        // An unknown/expired/revoked key: 401, and definitively so.
        assert_eq!(
            classify_resolution(401, b"{}"),
            Err(TenantRejection::InvalidCredential)
        );
        assert_eq!(classify_resolution(401, b"{}").unwrap_err().status(), 401);
        // Authenticated but under-permissioned.
        let e = classify_resolution(403, b"{}").unwrap_err();
        assert_eq!(e.status(), 403);
        assert!(e.message().contains("projects:read"), "{e}");
        // Everything else is UNAVAILABLE — a refusal, never a fallback tenant.
        for (status, body) in [
            (500u16, &b"boom"[..]),
            (502, b"<html>bad gateway</html>"),
            (404, b"{}"),
            (200, b"not json"),
            (200, br#"[{"id":"p1"}]"#),
        ] {
            let e = classify_resolution(status, body).unwrap_err();
            assert!(
                matches!(e, TenantRejection::Unavailable(_)),
                "status {status} produced {e:?}"
            );
            assert_eq!(e.status(), 503);
        }
    }

    #[test]
    fn missing_credential_is_401_and_never_a_default_tenant() {
        let e = TenantRejection::MissingCredential;
        assert_eq!(e.status(), 401);
        assert_eq!(e.error_type(), "authentication_error");
        assert!(e.message().contains(TENANT_CREDENTIAL_HEADER), "{e}");
        assert_eq!(
            TenantRejection::Unavailable("x".into()).error_type(),
            "gateway_error"
        );
        assert_eq!(
            TenantRejection::AmbiguousProject { candidates: 3 }.error_type(),
            "invalid_request_error"
        );
    }

    #[test]
    fn credentials_are_read_tolerantly_and_never_echoed() {
        assert_eq!(extract_credential(Some("nv_abc")), Some("nv_abc"));
        assert_eq!(extract_credential(Some("  nv_abc \n")), Some("nv_abc"));
        assert_eq!(extract_credential(Some("Bearer nv_abc")), Some("nv_abc"));
        assert_eq!(extract_credential(Some("bearer  nv_abc ")), Some("nv_abc"));
        // Present-but-empty is absent: a stripped secret authenticates no one.
        for blank in [
            None,
            Some(""),
            Some("   "),
            Some("Bearer "),
            Some("Bearer   "),
        ] {
            assert_eq!(extract_credential(blank), None, "{blank:?}");
        }
        // The fingerprint is short, stable, and does not contain the secret.
        let fp = credential_fingerprint("nv_supersecret");
        assert_eq!(fp.len(), 12);
        assert_eq!(fp, credential_fingerprint("nv_supersecret"));
        assert_ne!(fp, credential_fingerprint("nv_supersecrey"));
        assert!(!fp.contains("supersecret"));
    }

    #[tokio::test]
    async fn resolver_cache_never_aliases_credentials_with_the_same_log_fingerprint() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Precomputed birthday collision for the first 48 bits of SHA-256. The
        // short value is safe as a log label, but it is not an authorization
        // cache identity: the second key must still reach the platform and get
        // its own organization/project result.
        const FIRST: &str = "nv_collision_29875620495306";
        const SECOND: &str = "nv_collision_23185314331057";
        assert_ne!(FIRST, SECOND);
        assert_eq!(
            credential_fingerprint(FIRST),
            credential_fingerprint(SECOND)
        );

        let server = MockServer::start().await;
        for (key, org, project) in [
            (FIRST, "org_first", "project_first"),
            (SECOND, "org_second", "project_second"),
        ] {
            Mock::given(method("GET"))
                .and(path("/api/v1/projects/guardrails/resolve"))
                .and(header("authorization", format!("Bearer {key}")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                        "id": project,
                        "organizationId": org,
                    }])),
                )
                .mount(&server)
                .await;
        }

        let resolver = TenantResolver::with_timings(
            server.uri(),
            Duration::from_secs(300),
            Duration::from_secs(30),
            Duration::from_secs(2),
            64,
        );
        let first = resolver.resolve(FIRST).await.unwrap();
        let second = resolver.resolve(SECOND).await.unwrap();
        assert_eq!(first.organization_id, "org_first");
        assert_eq!(second.organization_id, "org_second");
        assert_eq!(second.projects, vec!["project_second"]);
    }

    #[test]
    fn tenant_cache_evicts_idle_first_then_least_recently_used() {
        let t = |n: &str| TenantId::new("org", n);
        let entries = vec![(t("a"), 1_000u64), (t("b"), 5_000), (t("c"), 9_000)];
        // Nothing idle, under the cap → nothing evicted.
        assert!(tenant_evictions(&entries, 9_500, 10_000, 8).is_empty());
        // `a` is idle beyond the TTL. This is also what makes a revoked
        // credential self-heal: its warm runtime goes away.
        assert_eq!(
            tenant_evictions(&entries, 9_500, 5_000, 8),
            vec![t("a")],
            "idle entries go first"
        );
        // Over the cap with nothing idle → least recently used first.
        assert_eq!(
            tenant_evictions(&entries, 9_500, 100_000, 1),
            vec![t("a"), t("b")]
        );
        // Ties break deterministically on the tenant id.
        let tied = vec![(t("z"), 1_000u64), (t("a"), 1_000)];
        assert_eq!(tenant_evictions(&tied, 1_100, 100_000, 1), vec![t("a")]);
    }

    #[test]
    fn tenant_ids_are_distinct_across_organizations() {
        // Two organizations that happen to name a project the same way must not
        // collide in any map keyed by tenant.
        let a = TenantId::new("org_a", "prod");
        let b = TenantId::new("org_b", "prod");
        assert_ne!(a, b);
        let mut m = std::collections::HashMap::new();
        m.insert(a.clone(), 1);
        m.insert(b.clone(), 2);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&a], 1);
        assert_eq!(a.to_string(), "org_a/prod");
    }

    #[test]
    fn a_tenant_config_is_built_from_the_derived_project_only() {
        let cfg = RemoteConfig::for_tenant("https://api.example.com/", "nv_key", "proj_a1");
        assert_eq!(cfg.base_url, "https://api.example.com");
        assert_eq!(cfg.project_id, "proj_a1");
        // Every platform URL therefore carries the derived project id — a
        // cross-tenant leak would be visible as a wrong path on the wire.
        assert!(cfg.policies_url().contains("/projects/proj_a1/"));
        assert!(cfg.state_url().contains("/projects/proj_a1/"));
        assert!(cfg.usage_url().contains("/projects/proj_a1/"));
    }

    /// An edge/proxy that answers a policy fetch with an HTML login or challenge
    /// page and a `200` is the same opacity problem as the `403` HTML body: the
    /// parse error alone ("expected value at line 1") names neither the status
    /// nor what actually came back. Both must survive into the error.
    #[tokio::test]
    async fn a_200_with_a_non_json_body_surfaces_the_status_and_the_body() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let html = "<html><body>Sign in to continue</body></html>";
        Mock::given(method("GET"))
            .and(path_regex(r".*/policies/effective$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(html.as_bytes().to_vec(), "text/html"),
            )
            .mount(&server)
            .await;

        let cfg = RemoteConfig::for_tenant(&server.uri(), "nv_key", "proj_1");
        let err = match fetch_bundle_conditional(&cfg, None).await {
            Err(e) => e,
            Ok(_) => panic!("a 200 carrying HTML is not a usable policy bundle"),
        };

        assert!(err.contains("200"), "status missing from {err}");
        assert!(
            err.contains("Sign in to continue"),
            "raw body missing from {err}"
        );
    }

    /// The same 200-with-HTML shape on the *state* endpoint.
    #[tokio::test]
    async fn a_200_state_fetch_with_a_non_json_body_surfaces_the_body_too() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r".*/policies/state$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(b"<html>challenge</html>".to_vec(), "text/html"),
            )
            .mount(&server)
            .await;

        let cfg = RemoteConfig::for_tenant(&server.uri(), "nv_key", "proj_1");
        let err = match fetch_state_conditional(&cfg, None).await {
            Err(e) => e,
            Ok(_) => panic!("a 200 carrying HTML is not a usable state document"),
        };

        assert!(err.contains("200"), "status missing from {err}");
        assert!(err.contains("challenge"), "raw body missing from {err}");
    }

    /// A body longer than `truncate_body`'s 512-byte bound is capped, not dumped
    /// whole, and the cap is announced with the real length.
    #[tokio::test]
    async fn a_huge_non_json_200_body_is_truncated_in_the_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let huge = "x".repeat(5000);
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(huge.as_bytes().to_vec(), "text/html"),
            )
            .mount(&server)
            .await;

        let cfg = RemoteConfig::for_tenant(&server.uri(), "nv_key", "proj_1");
        let err = match fetch_bundle_conditional(&cfg, None).await {
            Err(e) => e,
            Ok(_) => panic!("a 200 carrying 5 KB of HTML is not a policy bundle"),
        };

        assert!(err.contains("(5000 bytes)"), "no length note in {err}");
        assert!(
            err.len() < 1024,
            "error was not truncated: {} bytes",
            err.len()
        );
    }

    #[test]
    fn reservation_guard_completes_on_drop() {
        let ledger = Arc::new(PendingSpend::new());
        let (r, _) = ledger.reserve(0.01, 10);
        {
            let _guard = ReservationGuard::new(ledger.clone(), r);
        } // dropped here → complete()
          // Entry moved to completed (still counted within TTL) — and is no
          // longer releasable, proving it left the active set.
        ledger.release(r);
        assert_eq!(ledger.sum().requests, 1);
    }
}

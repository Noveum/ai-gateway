//! Nova Guard — in-process policy enforcement for the gateway.
//!
//! This module is the gateway-side implementation of the Nova Guard policy
//! layer. It is deliberately self-contained: the deterministic policy types
//! (regex, banned substrings, model allow/deny, token caps, JSON schema, PII,
//! secrets) are enforced entirely in-process with no network dependency, so the
//! gateway delivers guardrails standalone (BYOK mode) from a local policy bundle.
//!
//! Architecture:
//! * [`config`] — the `nova-guard.json` bundle schema (shared with the SDK).
//! * [`decision`] — uniform decision/severity/action types + telemetry mapping.
//! * [`rules`] — the extensible [`rules::PolicyRule`] trait + one module per
//!   deterministic policy type.
//! * [`metering`] — SSE frame reassembly + response/stream usage parsing, shared
//!   by the native telemetry path, the Anthropic stream translator, and the
//!   Worker bridge (pure/sync, wasm-safe).
//! * [`pricing`] — model cost table for per-request cost estimation.
//! * [`engine`] — orchestration: ordering, mode semantics, transform composition.
//!   (`cost_cap`/`rate_limit` accept an optional live-state injection seam; with
//!   no state backend configured they fail open — see [`engine`].)
//! * [`synthetic`] — provider-shaped block responses.
//! * [`source`] — local bundle loading (file/env).
//! * [`middleware`] — the Tower layer that wires the engine into the request path.

pub mod config;
pub mod decision;
pub mod engine;
/// Shared metering primitives: SSE frame reassembly + response/stream usage
/// parsing. Pure + sync, so it compiles for BOTH the native server and the
/// wasm32 Worker and is testable without a runtime.
pub mod metering;
/// Translation layer for Noveum platform NovaGuard policies + live state. Pure +
/// shared; the native HTTP fetch is in `crate::policy::remote`.
pub mod platform;
pub mod pricing;
/// Hand-maintained rate rows, matching `pricing/catalog.json`.
pub mod pricing_catalog;
pub mod rules;
pub mod synthetic;

// Local bundle loading reads the filesystem (`tokio::fs`) — native only. The
// Worker builds the engine from an in-memory bundle (env var / KV) instead.
#[cfg(not(target_arch = "wasm32"))]
pub mod source;

// Native HTTP bridge to the Noveum platform NovaGuard API (reqwest) — fetches
// policies + live cost/rate state.
#[cfg(not(target_arch = "wasm32"))]
pub mod remote;

/// The `/admit` + reservation-settlement WIRE CONTRACT: request/settlement
/// bodies, response classification and URL shaping. Pure and wasm-safe, so the
/// native client and the Worker client share one definition instead of two that
/// can drift.
pub mod admission_wire;

/// Cross-replica atomic admission (POST .../policies/admit + reservation
/// settlement). Strict-mode `cost_cap` enforcement calls this instead of the
/// per-process `remote::PendingSpend` ledger. Native-only (reqwest).
#[cfg(not(target_arch = "wasm32"))]
pub mod admission;

// Best-effort usage reporting (POST .../policies/usage) — advances the platform's
// cost/rate counters (ALLOWED) and records limit blocks (BLOCKED). Native-only.
#[cfg(not(target_arch = "wasm32"))]
pub mod usage;

// The Tower/Axum guard middleware is native-only; the Cloudflare Worker wires the
// engine into `worker_rt` instead.
#[cfg(not(target_arch = "wasm32"))]
pub mod middleware;

/// Cloudflare Worker control-plane client: the `wasm32` counterpart to
/// [`remote`] + [`admission`], speaking the same wire contract over
/// `worker::Fetch` instead of `reqwest`.
///
/// The module is compiled on BOTH targets on purpose. Only its four `Fetch`
/// calls are `cfg(target_arch = "wasm32")`; everything that *decides* something
/// — the credential matrix, URL shaping, `/admit` classification (block is HTTP
/// 200, 503 is never an allow), the settlement choice and the request-body cap —
/// is pure and therefore covered by the ordinary `cargo test` run. Gating the
/// whole module on wasm32 would move exactly that logic into a build this
/// repository cannot execute.
pub mod worker_remote;

/// Optional Workers KV policy bundle for the edge runtime (no platform bridge).
pub mod worker_kv;

pub use config::{Policy, PolicyBundle, PolicyType};
pub use decision::{Phase, PolicyAction, PolicyDecision, PolicyMode, Severity};
pub use engine::{EngineOptions, EvaluationResult, PolicyEngine};
pub use synthetic::BlockResponseMode;

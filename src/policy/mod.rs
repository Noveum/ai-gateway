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
pub mod middleware;
pub mod pricing;
pub mod rules;
pub mod source;
pub mod synthetic;

pub use config::{Policy, PolicyBundle, PolicyType};
pub use decision::{Phase, PolicyAction, PolicyDecision, PolicyMode, Severity};
pub use engine::{EngineOptions, EvaluationResult, PolicyEngine};
pub use synthetic::BlockResponseMode;

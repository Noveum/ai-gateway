//! Nova Guard — in-process policy enforcement for the gateway.
//!
//! This module is the gateway-side implementation of the Nova Guard policy
//! layer. It is deliberately self-contained: the deterministic policy types
//! (regex, banned substrings, model allow/deny, token caps, JSON schema, PII,
//! secrets) are enforced entirely in-process with no network dependency, so the
//! gateway delivers guardrails standalone (BYOK mode) as well as when wired to
//! the Noveum control plane for hosted policies and atomic budget reservation.
//!
//! Architecture:
//! * [`config`] — the `nova-guard.json` bundle schema (shared with the SDK).
//! * [`decision`] — uniform decision/severity/action types + telemetry mapping.
//! * [`rules`] — the extensible [`rules::PolicyRule`] trait + one module per
//!   deterministic policy type.
//! * [`pricing`] — model cost table for cost estimation / reservation.
//! * [`engine`] — orchestration: ordering, mode semantics, transform composition,
//!   cost-cap / rate-limit evaluation against live state.
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

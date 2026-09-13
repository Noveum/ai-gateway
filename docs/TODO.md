# Roadmap after v2.0

This is a list of known product gaps, not a commitment or release schedule.
Open an issue before implementation so the behavior and compatibility contract
can be agreed first.

- Add a distinct readiness endpoint that can represent configured control-plane
  dependencies without making liveness depend on third-party provider health.
- Add configurable CORS and a documented gateway-auth integration contract.
  v2.0.x remains permissive and relies on an external access-control layer.
- Add persistent Worker-native telemetry (Workers Logs/Analytics Engine/Queues)
  with prompt-data minimization and explicit retention controls.
- Generate Rust and platform pricing tables from `pricing/catalog.json` instead
  of maintaining hand-written mirrors.
- ~~Add Workers KV or another reviewed source for globally distributed stateless
  policy bundles~~ — implemented via optional `NOVEUM_GUARD_POLICIES_KV` binding
  (see [Cloudflare Worker operations](CLOUDFLARE_WORKER.md)).
- Add Bedrock tool/multimodal conversion and source-Region-aware strict pricing
  before expanding strict admission beyond the documented Claude surface.
- Design streaming output-phase policy enforcement without losing backpressure
  or sending content before a terminal block decision.
- Add OpenAPI generation and a hosted API reference. `/docs` and
  `/openapi.json` intentionally remain 404 in v2.0.1.
- Automate authorized edge/provider smoke tests separately from hermetic pull
  request CI, with low budgets and reliable cleanup.

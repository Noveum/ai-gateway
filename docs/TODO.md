Things to do ->

Improve /health path. Add a readiness check (e.g. verify configured upstreams /
registered exporters are healthy) returning a non-200 on failure. Useful for
kubernetes deployments.

Future: optional Noveum platform integration — export per-request traces to the
Noveum trace API, and hosted Nova Guard policy distribution + budget reservation.
Deferred for now; the gateway runs self-contained with local policies.

The gateway reports these errors after is has been running idle for sometime. Is there a way to optimize this, in senses less errors.


Things to do ->

Improve /health path. Add a readiness check there (e.g. verify the Noveum trace
endpoint is reachable when ENABLE_NOVEUM_TRACES is set) returning a non-200 on
failure. Useful for kubernetes deployments.

The gateway reports these errors after is has been running idle for sometime. Is there a way to optimize this, in senses less errors.


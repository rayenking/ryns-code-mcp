# ryns-code-mcp safety sidecar

Newline-delimited JSON bridge between the Rust MCP server and the
`cc-safety-net` policy library (`checkCommand`). Not run directly: the server
spawns `node scripts/safety-sidecar/index.mjs` lazily on the first gated tool
call (paths: `RYNS_SIDECAR_SCRIPT`, default `/scripts/safety-sidecar/index.mjs`;
node binary: `RYNS_NODE_BIN`, default `node`).

Protocol (one JSON per line, stdin = requests, stdout = responses):

- request:  `{"v":1,"id":<u64>,"op":"checkCommand","command":"…","cwd":"…"}`
- response: `{"v":1,"id":<n>,"kind":"allow"}`
            `{"v":1,"id":<n>,"kind":"deny","reason":"…","ruleId":"…"}`
            `{"v":1,"id":<n>,"kind":"error","message":"…"}` (protocol-level)

npm install of `cc-safety-net` is done at deploy time (pin: `^2.7.2` — the
QA-verified version whose `checkCommand` import works; `1.x` lacks the `./api`
export). This folder only defines the dependency (`package.json`).

Contract notes (plan §3, frozen):
- the sidecar NEVER inherits `RYNS_AUTH_TOKEN` (server uses its `child_env`);
- a `checkCommand` THROW (bad input, missing cwd, policy parse error) maps to
  `kind:"deny", ruleId:"custom.sidecar-error"` — fail closed;
- per-call 5 s budget is enforced Rust-side (transport failure/timeout →
  enforce: deny, observe: allow + warn); process death → one respawn per call,
  then per-mode failure handling.

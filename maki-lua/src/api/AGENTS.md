Mirror Neovim's Lua API namespaces (maki.uv = vim.uv, maki.fs = vim.fs, maki.treesitter = vim.treesitter).
Keep function signatures identical so plugins can be copy-pasted between Neovim and maki.
Only exception is the UI API, neovim's has baggage.

## Design

Our goal is to let plugin authors have as much freedom as possible, that's why desiging the APIs should be looked at as simple primitives you combine together.

## Error convention

Fallible runtime operations return the pair (value, err) and never throw.
Throwing is reserved for programmer errors, like passing a number where a string belongs.

## Tool ctx

One `LuaCtx` userdata type (util/ctx.rs) serves handler, `start`, and restore
invocations, built by `LuaCtx::handler/start/restore`. Capabilities a kind
lacks are `None` fields; their methods still exist and return
`(nil, "<method> not available in <kind> ctx")` instead of throwing, so
callers probe without pcall. Dispatch (`maki.agent.*`) is structurally
handler-only: the `agent` field is `None` elsewhere.

`maki.agent.*` follows the strict pair rule: wrong argument types throw;
every value or runtime failure (unknown prompt_id/audience, bad spec,
missing capability) returns `(nil, err)`.

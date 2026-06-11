# RestCraft

REST Client for [Zed](https://zed.dev) — send HTTP requests directly from
`.http` / `.rest` files. A port of
[vscode-restclient](https://github.com/Huachao/vscode-restclient) semantics.

## Architecture

Zed extensions are sandboxed WASM with no custom UI surface, so RestCraft is
split in two:

```
┌─ Zed extension (this repo root, WASM, thin) ─────────────────┐
│ • registers the HTTP language (.http/.rest)                  │
│ • tree-sitter-http grammar + highlight/injection/outline     │
│ • wires up the restcraft-lsp language server binary          │
└──────────────────────────────────────────────────────────────┘
┌─ restcraft-lsp (lsp/, native Rust binary, all the logic) ────┐
│ • parses ### request blocks, file variables, metadata        │
│ • variable substitution (file / environment / system vars)   │
│ • sends requests with reqwest (timeouts, redirects, cookies) │
│ • exposes "Send Request" as code lens + code action          │
│ • writes the response to a stable temp file and opens it as  │
│   a Zed tab via the `zed` CLI (resends refresh in place)     │
└──────────────────────────────────────────────────────────────┘
```

The two crates are deliberately independent: the root crate builds for
`wasm32-wasip1`, while `lsp/` is a plain native binary (it has its own
`[workspace]` root).

## Development setup

1. Install the language server (native toolchain required):

   ```sh
   cargo install --path lsp
   ```

   Make sure `~/.cargo/bin` is on your `PATH` so Zed can find `restcraft-lsp`.

2. Install the extension in Zed: open the command palette, run
   `zed: install dev extension`, and pick this repository's root directory.
   (Building the extension requires the `wasm32-wasip1` Rust target.)

3. Enable code lenses (optional but recommended) — Zed ships them disabled.
   In your Zed `settings.json`:

   ```json
   {
     "code_lens": "on"
   }
   ```

   Without code lenses you can still trigger everything through code actions
   on a request line ("Send Request", "Switch Environment: ...").

4. Install the `zed` CLI if you haven't (`zed: install cli` in the command
   palette) — RestCraft uses it to open response tabs.

You can also point Zed at a specific server binary instead of relying on
`PATH`:

```json
{
  "lsp": {
    "restcraft-lsp": {
      "binary": { "path": "/path/to/restcraft-lsp" }
    }
  }
}
```

## License

MIT — see [LICENSE](LICENSE). Includes work derived from MIT-licensed
projects; see [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

# RestCraft

REST client for [Zed](https://zed.dev) — write requests in `.http` / `.rest`
files and send them without leaving the editor. RestCraft is a Rust port of
[vscode-restclient](https://github.com/Huachao/vscode-restclient) semantics,
adapted to Zed's extension model.

```http
### Get a user
# @name getUser
GET {{host}}/users/1
Accept: application/json

### Chain off the previous response
POST {{host}}/audit
Content-Type: application/json

{ "userId": {{getUser.response.body.$.id}}, "traceId": "{{$guid}}" }
```

## Features

- **Send requests from the editor** — one "Send Request" code lens per `###`
  block (plus code actions), responses open in a syntax-highlighted tab.
- **Variables** — file variables (`@name = value`), environment variables,
  and system variables: `{{$guid}}`, `{{$randomInt min max}}`,
  `{{$timestamp [offset unit]}}`, `{{$datetime fmt}}`, `{{$localDatetime fmt}}`,
  `{{$processEnv NAME}}`, `{{$dotenv NAME}}`. Resolution order matches
  vscode-restclient: system → request → file → environment.
- **Request chaining** — name a request with `# @name foo`, then reference its
  exchange from later requests:
  `{{foo.response.body.$.token}}` (JSONPath), XPath for XML bodies, and
  `{{foo.response.headers.Content-Type}}`.
- **Environments** — JetBrains-style `http-client.env.json` (with `$shared`),
  discovered upward from the `.http` file. Switch via the
  "Switch Environment: …" code action; the active environment shows as a code
  lens and persists across restarts.
- **Authentication** — Basic (raw `user:pass` and `user pass` forms are
  base64-encoded for you) and Digest (MD5 / MD5-sess challenge-response).
- **Cookies** — persistent cookie jar shared across requests; opt out per
  request with `# @no-cookie-jar`. Redirect following is controlled globally
  and per request with `# @no-redirect`.
- **GraphQL** — `X-Request-Type: GraphQL` requests wrap the query (and an
  optional variables JSON after a blank line) into the proper payload.
- **File bodies** — `< ./payload.json` sends a file as the request body,
  resolved against the `.http` file's directory.
- **curl import & export** — paste a `curl …` command as a request block and
  send it as-is; "Copy Request As cURL" exports any block to the clipboard.
- **Request history** — every send is recorded (newest first, capped at 50);
  "View Request History" opens the history as a plain `.http` buffer where
  each entry can be resent with the regular code lens.
- **Editor smarts** — completion (methods, header names, MIME types,
  Authorization schemes, variables, dot-by-dot request-variable paths), hover
  (resolved variable values), and diagnostics for unresolved `{{refs}}`.
- **Highlighted responses** — a dedicated "HTTP Response" language colors the
  status line by class (2xx/3xx/4xx/5xx) and injects the body's language based
  on the `Content-Type` header (JSON, HTML, XML, CSS, JavaScript).

## Installation

RestCraft is not yet listed in the Zed extension registry — until it is,
install it as a **dev extension**:

1. Clone this repository (with the `wasm32-wasip2` Rust target installed,
   `rustup target add wasm32-wasip2`).

2. Install the language server (native Rust toolchain required):

   ```sh
   cargo install --path lsp
   ```

   Make sure `~/.cargo/bin` is on your `PATH` so Zed can find `restcraft-lsp`,
   or point Zed at the binary explicitly (see [Configuration](#configuration)).

3. In Zed, open the command palette, run `zed: install dev extension`, and
   pick this repository's root directory.

4. Enable code lenses — Zed ships them disabled. In your Zed `settings.json`:

   ```json
   {
     "code_lens": "on"
   }
   ```

   Without code lenses you can still trigger everything through code actions
   on a request line ("Send Request", "Switch Environment: …").

5. Install the `zed` CLI if you haven't (`zed: install cli` in the command
   palette) — RestCraft uses it to open response tabs.

## Usage

Create a file with the `.http` (or `.rest`) extension:

```http
@host = https://api.example.com

### List users
GET {{host}}/users?page=1
Accept: application/json

### Create a user
# @name createUser
POST {{host}}/users
Content-Type: application/json
Authorization: Basic admin secret

{ "name": "Ada", "joined": "{{$datetime iso8601}}" }

### Use the created id in a follow-up request
GET {{host}}/users/{{createUser.response.body.$.id}}

### Or paste a curl command — it sends as-is
curl -X DELETE {{host}}/users/42 -H 'X-Reason: cleanup'
```

Click the **Send Request** lens above a block (or use the code action on the
request line). The response opens as an `.http-response` tab; resending the
same request refreshes the tab in place.

### Environments

Put an `http-client.env.json` next to your `.http` files (or in any parent
directory up to the worktree root):

```json
{
  "$shared": { "version": "v1" },
  "dev":     { "host": "http://localhost:3000" },
  "prod":    { "host": "https://api.example.com" }
}
```

`$shared` values apply to every environment; the selected environment wins on
conflicts. Switch with the "Switch Environment: dev" code action — the choice
is persisted in `~/.restcraft/environment.json` and shown as an
"Environment: dev" code lens.

### Responses: the "HTTP Response" language

Responses open as `.http-response` files in a dedicated language backed by
[tree-sitter-http-response](https://github.com/jidohyun/tree-sitter-http-response),
so the exchange view is highlighted instead of plain text:

```
# get-user | 12ms | 344B          <- request name | elapsed | size

HTTP/1.1 200 OK                   <- status line (code colored by class)
Content-Type: application/json    <- headers
                                  <- blank line
{ "id": 1 }                       <- body
```

The body is injected with the language named by the `Content-Type` subtype —
`application/json` → JSON, `text/html` → HTML, `image/svg+xml` /
`application/xml` → XML (RFC 6839 `+suffix` aware), plus CSS and JavaScript.
JSON/JS/CSS highlighting works out of the box; HTML and XML need their Zed
extensions installed. Unknown subtypes degrade gracefully to no injection.

## Configuration

All knobs live under `lsp.restcraft-lsp` in Zed's `settings.json`:

```json
{
  "lsp": {
    "restcraft-lsp": {
      "binary": { "path": "/path/to/restcraft-lsp" },
      "settings": {
        "timeoutMs": 0,
        "followRedirects": true,
        "maxRedirects": 10
      }
    }
  }
}
```

| Setting           | Default | Meaning                                              |
| ----------------- | ------- | ---------------------------------------------------- |
| `timeoutMs`       | `0`     | Request timeout in milliseconds; `0` = no timeout    |
| `followRedirects` | `true`  | Follow redirects (`# @no-redirect` overrides per request) |
| `maxRedirects`    | `10`    | Redirect limit when following                        |

`binary.path` is optional — without it, `restcraft-lsp` is looked up on
`PATH`. State (environment selection, cookie jar, history) lives in
`~/.restcraft/`, created owner-only (0700).

## Differences from vscode-restclient

Zed extensions cannot create custom UI (no webview, status bar, or input
boxes), so some surfaces are reshaped rather than removed:

| vscode-restclient                  | RestCraft                                                |
| ---------------------------------- | -------------------------------------------------------- |
| Webview response preview           | Highlighted `.http-response` text tab (via the `zed` CLI) |
| Command palette / keybindings      | Code lenses (opt-in) + code actions                       |
| Environments in VS Code `settings.json` | JetBrains-style `http-client.env.json`              |
| Status-bar environment indicator   | "Environment: …" code lens + switch code action           |
| History QuickPick                  | "View Request History" renders a resendable `.http` buffer |
| `jsonpath-plus` dialect            | RFC 9535 JSONPath (`serde_json_path`) — no `^`, `~`, type selectors, or JS filters |
| `xmldom` XPath                     | XPath 1.0 (`sxd-xpath`) — no namespace-prefixed queries   |

Not supported (yet):

- AWS Signature v4, Azure AD (`$aadToken` / `$aadV2Token`), OIDC
  (`$oidcAccessToken`), and client certificate authentication
- `# @prompt` / `# @note` (parsed and ignored — no input UI in Zed)
- Code snippet generation beyond curl export (no httpsnippet equivalent)
- Swagger/OpenAPI import
- In-editor HTML or image response preview (responses are text)
- Proxy settings, custom default headers, and the other vscode-restclient
  `rest-client.*` options not listed in [Configuration](#configuration)
- Sending requests from Markdown code blocks
- TLS verification toggle — like vscode-restclient's default
  (`rejectUnauthorized: false`), certificate errors are currently ignored;
  don't rely on RestCraft for certificate validation

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
`wasm32-wasip2`, while `lsp/` is a plain native binary (it has its own
`[workspace]` root). Run the server's tests with `cargo test` inside `lsp/`.

## License

MIT — see [LICENSE](LICENSE). Includes work derived from MIT-licensed
projects ([vscode-restclient](https://github.com/Huachao/vscode-restclient),
[rest-nvim/tree-sitter-http](https://github.com/rest-nvim/tree-sitter-http));
see [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

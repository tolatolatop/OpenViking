# [Bug]: `ov chat` ignores api_key/account/user from ovcli.conf causing 401 Unauthorized

## Description

The `ov chat` command fails with `401 Unauthorized: Missing API Key when resolving identity` even when `api_key`, `account`, and `user` are correctly configured in `~/.openviking/ovcli.conf`.

## Reproduction

### Steps to Reproduce

1. **Configure server with API_KEY auth mode** (auto-detected when `root_api_key` is set)
   ```json
   // /etc/openviking/ov.conf (server-side)
   {
     "server": {
       "host": "0.0.0.0",
       "port": 1933,
       "root_api_key": "root-api-key-change-me"
     }
   }
   ```

2. **Configure client with valid credentials**
   ```bash
   mkdir -p ~/.openviking
   cat > ~/.openviking/ovcli.conf << 'EOF'
   {
     "url": "http://localhost:1933",
     "api_key": "root-api-key-change-me",
     "account": "default",
     "user": "user1"
   }
   EOF
   ```

3. **Test other commands work (verify config is valid)**
   ```bash
   $ ov ls
   Type  Name  Size  Modified
   ==========================
   dir   viking://
   ```

4. **Run ov chat - fails with 401**
   ```bash
   $ ov chat -m "hello"
   Error: API error: Request failed (401 Unauthorized): {"status":"error","result":null,"error":{"code":"UNAUTHENTICATED","message":"Missing API Key when resolving identity.","details":{}},"telemetry":null}
   ```

### Expected Behavior

`ov chat` should:
1. Read `api_key`, `account`, `user` from `ovcli.conf` (matching `ov ls`, `ov find`, etc.)
2. Send `X-API-Key`, `X-OpenViking-Account`, `X-OpenViking-User` headers to the `/chat` endpoint

### Actual Behavior

| Command | HTTP Headers Sent | Result |
|---------|------------------|--------|
| `ov ls` | `X-API-Key`, `X-OpenViking-Account`, `X-OpenViking-User` | ✅ 200 |
| `ov find "test"` | `X-API-Key`, `X-OpenViking-Account`, `X-OpenViking-User` | ✅ 200 |
| `ov chat -m "hi"` | (none) | ❌ 401 |
| `ov chat -m "hi" --api-key "xxx"` | `X-API-Key` only | ❌ 401 |
| `curl -H "X-API-Key: xxx" -H "X-OpenViking-Account: default" -H "X-OpenViking-User: user1"` | All headers | ✅ 200 |

## Key Logs

### Server Logs (continuous 401 errors)
```
2026-04-19 04:00:01,045 - openviking.server.routers.system - WARNING - Failed to resolve identity: Missing API Key when resolving identity.
2026-04-19 04:00:31,094 - openviking.server.routers.system - WARNING - Failed to resolve identity: Missing API Key when resolving identity.
2026-04-19 04:01:01,149 - openviking.server.routers.system - WARNING - Failed to resolve identity: Missing API Key when resolving identity.
```

### Source of Error
```python
# openviking/server/auth.py:124-125
if not api_key:
    raise UnauthenticatedError("Missing API Key when resolving identity.")
```

Called from `resolve_identity()` at line 69-76:
```python
async def resolve_identity(
    request: Request,
    x_api_key: Optional[str] = Header(None),
    authorization: Optional[str] = Header(None),
    x_openviking_account: Optional[str] = Header(None, alias="X-OpenViking-Account"),
    x_openviking_user: Optional[str] = Header(None, alias="X-OpenViking-User"),
    ...
) -> ResolvedIdentity:
```

## Root Cause Analysis

Three independent bugs compound in `crates/ov_cli/src/commands/chat.rs`:

### Bug 1: API Key Not Loaded from Config

`ChatCommand` only accepts `api_key` from CLI args (`--api-key`) or env var, **never from ovcli.conf**:

```rust
#[derive(Debug, Parser)]
pub struct ChatCommand {
    #[arg(short, long, env = "VIKINGBOT_API_KEY")]
    pub api_key: Option<String>,  // ❌ Never reads from Config
    ...
}
```

Compare with other commands that use `CliContext.get_client()`:
```rust
// crates/ov_cli/src/main.rs:66-79
pub fn get_client(&self) -> client::HttpClient {
    client::HttpClient::new(
        &self.config.url,
        self.config.api_key.clone(),      // ✅ Uses config
        self.config.agent_id.clone(),
        self.config.account.clone(),      // ✅ Uses config
        self.config.user.clone(),         // ✅ Uses config
        ...
    )
}
```

### Bug 2: Missing Required HTTP Headers

When using `root_api_key` (API_KEY auth mode), `auth.py:155-175` requires `X-OpenViking-Account` and `X-OpenViking-User` headers for tenant-scoped APIs:

```python
# openviking/server/auth.py:167-175
if (
    auth_mode == AuthMode.API_KEY
    and api_key_manager is not None
    and identity.role == Role.ROOT
    and _root_request_requires_explicit_tenant(path)
):
    if not account_header or not user_header:
        raise InvalidArgumentError(
            "ROOT requests to tenant-scoped APIs must include X-OpenViking-Account "
            "and X-OpenViking-User headers."
        )
```

Current chat code only sends `X-API-Key`:
```rust
// crates/ov_cli/src/commands/chat.rs:136-140 (BEFORE FIX)
let mut req_builder = client.post(&url).json(&request);
if let Some(api_key) = &self.api_key {
    req_builder = req_builder.header("X-API-Key", api_key);
}
// ❌ Missing X-OpenViking-Account
// ❌ Missing X-OpenViking-User
```

### Bug 3: Config Field Name Mismatch

User configs may use `account_id`/`user_id` (Python SDK convention) while Rust code expects `account`/`user`. Other parts support both via serde aliases; chat doesn't use config at all.

## Workaround

Until fixed, use direct HTTP request with all headers:

```bash
curl -X POST http://localhost:1933/bot/v1/chat \
  -H "Content-Type: application/json" \
  -H "X-API-Key: root-api-key-change-me" \
  -H "X-OpenViking-Account: default" \
  -H "X-OpenViking-User: user1" \
  -d '{"message":"hello","user_id":"user1","stream":false}'
```

## Environment

- **OpenViking Version:** 0.2.6
- **OS:** Ubuntu 24.04
- **Server Config:** `auth_mode=API_KEY` (auto-detected via `root_api_key`)
- **Deployment:** Docker (`openviking:fixed`)

## Files to Modify

| File | Change |
|------|--------|
| `crates/ov_cli/src/config.rs` | Add `#[serde(alias = "account_id")]` and `#[serde(alias = "user_id")]` to support both field names |
| `crates/ov_cli/src/commands/chat.rs` | Add `account`/`user` fields; add `resolve_auth()` to load from config; add `apply_auth_headers()` to send all headers |
| `crates/ov_cli/src/main.rs` | Pass `account`/`user` from `ctx.config` to `ChatCommand` |

## Proposed Fix

See PR description for full implementation.

---

## AI Disclosure / 技术溯源声明

This issue was auto-discovered and root-caused by an AI assistant (Hermes Agent with Codex tooling).

**Investigation process:**
1. Used `ov chat -m "hello"` to reproduce the 401 error
2. Examined server logs showing "Missing API Key when resolving identity" warnings every 30 seconds
3. Traced the error to `openviking/server/auth.py:124-125`
4. Used Codex to analyze `crates/ov_cli/src/commands/chat.rs` and identified missing config/auth headers
5. Implemented the fix (see PR) that loads auth from Config and sends all required headers

The complete analysis and patching workflow was performed autonomously with code-level verification.

**Labels:** bug, cli, authentication

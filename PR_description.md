## Summary

Fixes #1574

This PR fixes `ov chat` command failing with 401 Unauthorized even when valid credentials are configured in `ovcli.conf`.

## Problem

`ov chat` was not reading `api_key`, `account`, and `user` from the configuration file, and was not sending the required `X-OpenViking-Account` and `X-OpenViking-User` headers for API_KEY auth mode.

## Changes

### 1. `crates/ov_cli/src/commands/chat.rs`

- Added `account` and `user` CLI arguments to `ChatCommand`
- Added `ChatAuth` struct to hold resolved authentication info
- Added `resolve_auth()` method to load auth from Config when not provided via CLI
- Added `apply_auth_headers()` helper to send all required headers
- Updated all `send_message*` methods to use the new auth flow

### 2. `crates/ov_cli/src/config.rs`

- Added `#[serde(alias = "account_id")]` on `account` field
- Added `#[serde(alias = "user_id")]` on `user` field
- Added test for deserializing `account_id`/`user_id` aliases

### 3. `crates/ov_cli/src/main.rs`

- Pass `account` and `user` from `ctx.config` to `ChatCommand` construction

## Testing

Before:
```bash
$ ov chat -m "hello"
Error: API error: Request failed (401 Unauthorized): Missing API Key...
```

After:
```bash
$ ov chat -m "hello"
Bot:
Hello! I'm VikingBot, an AI assistant built on the OpenViking...
```

## Checklist

- [x] Code compiles (`cargo build --release -p ov_cli`)
- [x] Fix verified by testing `ov chat` command
- [x] Backward compatible (CLI args override config values)
- [x] Supports both `account`/`user` and `account_id`/`user_id` field names

---

**AI Assistant Note:** This fix was identified through code analysis by Hermes Agent with Codex tooling. See #1574 for full investigation details.

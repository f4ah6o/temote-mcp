# Session automatic restart policy

Date: 2026-08-25

## Context

Session lifecycle supervision now records `starting`, `active`, `stopping`, `stopped`, and `crashed`, persists crash reasons, reconciles stale state after supervisor restart, and supports manual `temote-mcp session restart <id>`.

The first implementation intentionally uses restart policy `never` so that a deterministic crash loop cannot consume host resources or repeatedly trigger side effects.

## Goal

Add an explicit optional restart policy:

```text
never
on-failure
```

`never` remains the safe default.

## Requirements

- persist the configured restart policy with lifecycle metadata
- restart only unexpected failures; never restart an explicit graceful stop
- use bounded exponential backoff and/or a restart-rate window
- prevent infinite crash loops with a terminal `crashed` state after the limit is exceeded
- persist enough restart history to explain why automatic restart stopped
- surface restart count, most recent restart time, and next/backoff state in `session info`
- keep one session's restart loop isolated from other sessions
- do not weaken sandbox, approval, named-root, 1Password, or kintone bridge boundaries
- define supervisor-restart behavior for sessions configured as `on-failure`

## Suggested acceptance tests

- immediate crash is restarted once when policy is `on-failure`
- graceful stop is never restarted
- repeated crashes hit the rate limit and settle as `crashed`
- backoff grows within a configured maximum
- supervisor restart restores policy without bypassing the rate limit
- one crashing/restarting session does not affect another session
- default policy remains `never`

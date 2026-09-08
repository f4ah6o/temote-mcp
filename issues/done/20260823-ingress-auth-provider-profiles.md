# Cloudflare / Tailscale / OpenAI Secure MCP Tunnel を使い分ける connection provider architecture を導入する

Status: Done
Closed by triage: 2026-09-08

## Resolution

The connection profile architecture and repository-local implementation are complete.

Implemented profiles include Cloudflare Tunnel + Access Managed OAuth, Tailscale Funnel + Temote local OAuth, and OpenAI Secure MCP Tunnel. Provider-neutral connection identity/endpoint boundaries, profile-aware lifecycle/doctor behavior, local OAuth security checks, OpenAI setup/lifecycle support, and regression coverage are implemented. Tailscale external live acceptance is already recorded in the original issue.

The original detailed architecture, security model, implementation phases, and evidence remain available in repository history before this triage archival change.

## Remaining live evidence

Cloudflare external revalidation, supported-product OpenAI Secure MCP Tunnel acceptance, and three-profile live boundary parity are consolidated into `issues/open/20260908-live-acceptance-matrix.md`.

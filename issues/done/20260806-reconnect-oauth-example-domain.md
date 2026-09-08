# temote-mcp の公開 OAuth 接続を temotemcp.example.com で再構築する

Status: Done
Closed by triage: 2026-09-08

## Resolution

Repository-local work and the unauthenticated discovery/401 probes are complete. The original issue had already been reduced to external authenticated connector verification only.

Completed evidence includes the Cloudflare Tunnel/Access configuration path, private `public.env` handling, successful `just env-check`, expected unauthenticated `401 + WWW-Authenticate`, and successful OAuth authorization/protected-resource discovery responses.

The original detailed procedure and evidence remain available in repository history before this triage archival change.

## Remaining live evidence

Authenticated external MCP/ChatGPT connection evidence is consolidated into `issues/open/20260908-live-acceptance-matrix.md`.

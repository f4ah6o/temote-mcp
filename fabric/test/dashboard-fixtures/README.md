# Dashboard browser fixture

Start the deterministic, loopback-only dashboard fixture from the repository root:

```sh
node fabric/test/dashboard-fixtures/server.mjs
```

Open `http://127.0.0.1:4173/dash/`. The fixture serves the real dashboard assets and safe sample API responses. It binds only to `127.0.0.1` and does not connect to Fabric, Cloudflare Access, D1, or a Host. It deliberately bypasses authentication for local UI checks and must not be used as a production server.

Change API responses while the page is open, then press **Refresh**:

```sh
curl -X POST http://127.0.0.1:4173/__fixture/state \
  -H 'content-type: application/json' \
  -d '{"hostsUnavailable":true,"tasksUnavailable":true,"contextUnavailable":true}'
```

Restore each changed flag to `false` to reset a scenario. Other fields include `livenessUnavailable`, `hostOffline`, `codexUnavailable`, `timelineUnavailable`, `pendingState`, `taskRevision`, `summaryRevision`, and `producerEpoch`, `membershipMissing`, and `replicaUnavailable`. The fixture includes a hostile repository label to verify that browser rendering treats untrusted values as text.

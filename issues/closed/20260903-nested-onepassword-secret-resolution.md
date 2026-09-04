# Nested 1Password secret resolution without exposing the service-account token

Date: 2026-09-03
Status: closed
Updated: 2026-09-03
Branch: feat/nested-onepassword-secret-resolution

## Background

`temote-mcp` deliberately keeps the 1Password Service Account token inside the supervisor boundary.

`onepassword_service_account_run` can execute a command through `op run`, but `OP_SERVICE_ACCOUNT_TOKEN` is removed from the target command environment. This is the correct default security property: arbitrary child processes must not be able to read or exfiltrate the raw Service Account token.

A runtime gap appears when the launched application itself contains an approved secret resolver and needs to resolve additional `op://` references after startup.

Observed on 2026-09-03 while running the `obara-integration-platform` NextSet client-certificate lifecycle audit:

```text
Temote supervisor
  └─ onepassword_service_account_run
       └─ NextSet / Kintone integration job
            └─ ServiceAccountOnePasswordReader
                 └─ op read op://...
```

The outer Temote invocation can resolve explicitly supplied NextSet references, but the child process later needs to resolve the Kintone credential registry references itself. Because the Service Account token is intentionally removed from the child environment and there is no systemd credential file inside the development invocation, the nested resolver fails with the equivalent of:

```text
Kintone OAuth credential resolution failed
```

The underlying locators are valid and readable by the Temote Service Account. The failure is therefore not a missing secret; it is a missing capability boundary for nested secret resolution.

This pattern is not NextSet-specific. It applies to development or maintenance commands whose runtime adapters resolve logical 1Password references internally, including Kintone OAuth and potentially Box, Salesforce, Gulliver, or other integration runtimes.

## Problem statement

Temote needs a safe way for an approved child process to resolve permitted `op://` references after startup without exposing `OP_SERVICE_ACCOUNT_TOKEN` or persisting it to disk.

The desired property is:

> A child process may request specifically authorized secret values through a supervisor-owned capability, while the raw 1Password Service Account credential remains inaccessible to the child and its descendants.

Do **not** solve this by simply preserving `OP_SERVICE_ACCOUNT_TOKEN` in the target environment.

## Security requirements

The implementation must preserve the current security boundary:

- raw `OP_SERVICE_ACCOUNT_TOKEN` is never exported to the launched process;
- the token is never written into the worktree, `.env`, Dagu sandbox state, lifecycle metadata, logs, argv, or normal child-readable temporary files;
- secret values are not logged by Temote;
- locator failures are fail-closed;
- authorization is bounded to the current Temote invocation/session rather than becoming a host-wide unauthenticated secret service;
- the child cannot use the resolver to exceed the Service Account scope configured for the Temote supervisor;
- preferably, the child is further restricted to an explicit locator allowlist or approved resolver policy for that invocation;
- capability lifetime ends when the command/session ends;
- descendants must not gain a reusable bearer credential equivalent to the Service Account token.

## Proposed model

Prefer a supervisor-owned local credential broker rather than token forwarding.

Conceptually:

```text
Temote supervisor
  ├─ OP_SERVICE_ACCOUNT_TOKEN      # supervisor only
  │
  └─ per-invocation secret broker
       ├─ local IPC / inherited capability
       ├─ exact locator/policy scope
       └─ op read performed by supervisor boundary
            ↑
            │ resolve(op://...)
            │
      child SecretReader
```

The child receives only a short-lived local capability endpoint/descriptor. It sends an `op://` locator request and receives the resolved value only when that locator is permitted by the invocation policy.

The implementation may use a Unix-domain socket, inherited file descriptor, or another local authenticated IPC primitive. The exact transport is secondary to keeping the Service Account token outside the child boundary.

## Locator authorization

Do not make the broker an unrestricted `op read` proxy by accident.

Initial implementation should support an explicit policy such as one or more of:

```text
exact locator allowlist
approved env-file locator set
approved vault/item prefix
named secret-resolution profile
```

Exact locators are preferred when practical.

For repository runtimes that already use a logical credential registry, a future integration may allow Temote to load a reviewed non-secret locator manifest and expose only those locators for the command.

The policy itself may contain `op://` references because locators are not secret values, but it must not contain resolved credentials.

## CLI / tool surface

The existing `onepassword_service_account_run` semantics should remain backward-compatible.

Possible extension:

```text
temote-mcp ... run-with-secrets \
  --allow-locator op://vault/item/field \
  --allow-locator op://vault/other/field \
  -- command ...
```

or an internal MCP equivalent where the caller supplies the permitted locator set/profile.

Temote can then inject a non-secret indicator such as:

```text
TEMOTE_SECRET_RESOLVER=<local capability reference>
```

A repository-specific `SecretReader` may use that resolver in development/Temote mode while retaining its existing production backend.

Avoid forcing applications to depend on Temote-specific APIs in their business logic. The integration should stay behind a small secret-reader abstraction.

## Production compatibility

This issue is primarily about Temote/development execution.

Production runtimes may continue using their existing hardened mechanism, for example:

```text
systemd
  └─ CREDENTIALS_DIRECTORY/op_service_account_token
       └─ ServiceAccountOnePasswordReader
```

Temote should not require production applications to replace a working systemd credential boundary.

A typical application abstraction can remain:

```text
SecretReader
  ├─ systemd/service-account reader   # production
  └─ Temote capability reader         # development / operator execution
```

The logical `op://` references and credential registries remain unchanged.

## Required behavior

### 1. Capability creation

When nested resolution is requested, Temote creates a per-invocation resolver capability before starting the target command.

The capability must be bound to:

- the owning Temote supervisor/session;
- the target invocation lifetime;
- the approved locator policy;
- the local host/user boundary.

### 2. Resolution

For each child request:

1. validate the capability;
2. validate and normalize the requested `op://` locator;
3. reject locators outside the approved policy before invoking 1Password;
4. perform the 1Password read inside the supervisor boundary;
5. return only the requested secret value;
6. avoid logging the secret value or raw token.

### 3. Failure semantics

Fail closed for:

- malformed locator;
- locator outside policy;
- expired/closed capability;
- Service Account unavailable;
- `op read` failure;
- ambiguous broker identity/ownership;
- broker transport failure.

Do not silently fall back to plaintext env files or interactive user credentials.

### 4. Cleanup

On command/session completion:

- close the broker/capability;
- remove local socket/metadata if applicable;
- ensure subsequent requests fail;
- leave no secret values or Service Account token on disk.

### 5. Observability

Audit only non-secret metadata, for example:

```text
resolver invocation id
session id
locator policy identifier
requested locator hash or redacted locator identity
success/failure
```

Do not log resolved values or the Service Account token.

## Reproduction / acceptance scenario

The original reproduction is a useful integration test shape:

1. Temote supervisor has a valid 1Password Service Account.
2. Start an integration job through the Temote secret execution boundary.
3. The job receives no `OP_SERVICE_ACCOUNT_TOKEN`.
4. The job's own `SecretReader` requests Kintone OAuth client ID, client secret, and refresh token through the nested resolver.
5. The requests succeed when the locators are allowed.
6. An unlisted locator fails before 1Password access.
7. The job proceeds to its read-only API operation.
8. After the job exits, the same capability can no longer resolve anything.

## Non-goals

- exporting `OP_SERVICE_ACCOUNT_TOKEN` to arbitrary children;
- replacing 1Password authorization policy;
- creating a general host-wide secret daemon;
- persisting secret values for retry convenience;
- weakening normal Temote sandbox/network/approval boundaries;
- changing production systemd credential handling when it already satisfies the runtime contract;
- adding repository-specific Kintone/NextSet special cases to Temote core.

## Acceptance criteria

- [x] a Temote-launched child can resolve an explicitly authorized `op://` locator after startup
- [x] `OP_SERVICE_ACCOUNT_TOKEN` remains absent from the child environment
- [x] the child cannot read the raw Service Account token through the capability
- [x] unapproved locators fail closed before `op read`
- [x] capability scope is per invocation/session and becomes unusable after cleanup
- [x] resolved values are not persisted in worktree/runtime/lifecycle files
- [x] secret values and raw tokens are absent from logs and error messages
- [x] concurrent invocations cannot use each other's resolver capability
- [x] normal `onepassword_service_account_run` behavior remains backward-compatible
- [x] existing approval/yolo semantics remain unchanged
- [x] Linux tests cover success, denied locator, dead capability, concurrent isolation, and child-token absence
- [x] macOS uses an equivalent secure local capability or explicitly fails closed until supported
- [x] documentation explains when to use direct environment substitution versus nested resolution

## Required tests

1. allowed exact locator -> resolved value reaches only the requesting child
2. disallowed locator -> rejected without calling `op read`
3. malformed locator -> rejected
4. Service Account unavailable -> fail closed
5. child inspects environment -> no `OP_SERVICE_ACCOUNT_TOKEN`
6. child attempts to reuse capability after command exit -> rejected
7. two concurrent commands -> capability A cannot access capability B
8. broker crash/disconnect -> child receives a deterministic failure, no plaintext fallback
9. logs/errors contain locator-safe metadata but no resolved value/token
10. existing non-nested `onepassword_service_account_run` tests remain unchanged and passing

## Implementation note

The current behavior of stripping `OP_SERVICE_ACCOUNT_TOKEN` from the target is a security feature and should remain the default. This issue should add a narrower secret-resolution capability rather than weakening that boundary.


## Implementation result

- `onepassword_service_account_run` に optional `allowed_locators` を追加し、既存の `environment` / `env_files` path を変更せず nested resolution を opt-in にした。
- service-account token を持つ `op run` / `op read` は **untrusted target 起動前に完了**させる。direct environment substitution は supervisor memory に capture し、nested `allowed_locators` は exact locator-to-value map として事前解決する。Linux supervisor は startup token を検出すると command parse 前に `PR_SET_DUMPABLE=0` を適用し、upgrade / re-exec 時は raw token を startup environment に再注入せず mode 000 の sealed anonymous FD で引き継ぐ。raw-token CLI は root-owned / non-writable / setgid で、setgid group が Temote user から利用不能な `op` executable の場合だけ許可し、`fs.suid_dumpable=1` は fail closed にする。さらに service-account target は private PID namespace + private `/proc` で起動する。これにより process-local lock を credential boundary とせず、独立 supervisor 間でも target から raw token-bearing host process を観測できない OS-level boundary にした。
- Linux では invocation ごとに owner-only directory + Unix-domain socket を作成する。broker は事前解決済み map だけを返し、target 起動後に 1Password API / `op read` を呼ばない。
- broker authorization は random capability token + peer UID に加え、`SO_PEERCRED` PID が bind 済み target root PID またはその descendant であることを `/proc/<pid>/stat` の parent chain / start time で検証する。same-UID の別 invocation が token/socket を知っていても process identity 外なら deny する。
- broker connection concurrency は invocation ごとに bounded とし、shutdown 時には active connection task を deterministic に cancel する。target 起動後の resolver/API call 自体を廃止したため、unbounded `op read` / resolver timeout surface も除去した。
- locator validation は control character に加えて bidi override/isolate、zero-width 等の operator-visible identity を壊す Unicode format character を reject する。通常の日本語 locator は許可する。
- capability token が存在する invocation では、resolved secret がまだ使われていなくても captured output が truncation された場合は stdout/stderr を fail-closed に全面抑止する。direct/nested の解決済み secret と raw token も output から redaction する。`env_files` は value に `op://` を含む assignment だけを secret-bearing として登録し、曖昧な assignment syntax は fail closed にするため、`MODE=production` や `RETRY=1` のような非secret値を誤って全面 redaction しない。
- process-boundary regression は fabricated token を実際の `temote-mcp` startup environment に入れ、actual binary の `/proc/<pid>/environ` から取得できないことを確認する。さらに独立 supervisor/process fixture を使い、target A から protected supervisor B と exec-time credential boundary を持つ token-bearing helper の environment を取得できないことを host integration で確認する。正しい broker token/socket を知る process-tree 外の same-UID process も deny される。
- process-local RW lock を削除したため、long-running service-account target 実行中でも別の status / pre-resolution operation を実行できる。
- macOS など Linux 以外は `allowed_locators` 使用時に明示的に fail closed とし、従来の non-nested service-account execution は維持する。
- Rust MCP schema / gateway routed-tool contract / English/Japanese docs / Temote Agent Skill を同期した。

## Verification

- `cargo fmt --all -- --check`: pass
- `cargo test`: pass
- `cargo clippy --all-targets -- -D warnings`: pass
- `cargo check --no-default-features --all-targets`: pass（既存 feature-off dead-code warnings のみ）
- `(cd gateway && npm test)`: 46/46 pass
- `git diff --check`: pass

## Completion

上記 security boundary、nested resolution、cleanup、gateway parity、documentation、Linux process-boundary regression tests を実装・検証したため close する。

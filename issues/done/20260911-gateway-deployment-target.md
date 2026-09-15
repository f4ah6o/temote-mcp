# Gateway の deployment target を明示・文書化する

Status: done
Model: gpt-5.6-sol
Created: 2026-09-11
Updated: 2026-09-15
Branch: codex/20260915-completion-baseline

## Resolution

Repository-local Slices A and B are complete. The English/Japanese operator guides define the custom-domain versus existing-DNS Worker-route choice, `workers_dev = false`, `--keep-vars`, secret boundaries, deploy verification, and rollback. The deterministic preflight and its CLI distinguish `target_missing`, `target_mismatch`, and `remote_unknown`, reject unsafe workers.dev configuration and simultaneous route/custom-domain flags, and perform no Cloudflare mutation. Credential-dependent live deployment evidence remains in `20260908-live-acceptance-matrix.md`.

## 提案する方針

Gateway target の選択と local preflight を repository-local に限定し、Cloudflare route/domain の mutation と live verification は別の acceptance matrix で管理する。
Priority: P1 operational correctness

## 概要

`workers_dev = false` の Gateway Worker について、custom domain と既存 DNS hostname への Worker route の選択、deploy 時の target 指定、target の検証方法を docs と必要最小限の運用 tooling で明確にする。

この issue は **Gateway の routing/protocol 実装ではなく deployment correctness** を扱う。最初の実装スライスでは実 Cloudflare 設定を変更しない。

## 背景

Gateway の基本設定と host-agent 要件は [`docs/gateway.md`](../../docs/gateway.md) と [`docs/gateway.ja.md`](../../docs/gateway.ja.md) に記録されている。現在の設定は `workers_dev = false` で、Worker の route/domain を `gateway/wrangler.toml` に宣言していない。

既存 DNS を維持したまま hostname を Worker に割り当てる場合は、Wrangler deploy に route target を明示する必要がある。一方、custom domain を作成する場合は Cloudflare 側の domain binding と Access application を別途構成する必要がある。

## 問題

target を設定ファイルまたは deploy 手順で明示しないまま `wrangler deploy` を実行すると、Worker version は upload されても `No targets deployed` となり、期待する公開 hostname は direct origin または未割り当てのままになる。

custom domain と Worker route の違い、既存 DNS record を削除せずに route を上書きする手順、`--keep-vars` が必要な dashboard-managed variables の扱い、deploy 後の route verification が一つの手順として整理されていない。

## 目標

Gateway の公開 target を再現可能な形で選択・deploy・検証できるようにし、target 未設定や意図しない `workers.dev` 公開を operator が早期に検出できるようにする。

## 対象外

- Cloudflare Access policy の自動作成・自動変更
- DNS record の削除または自動移行
- Gateway の MCP protocol、Durable Object、host-agent routing semantics の変更
- `workers_dev = true` への変更
- Gateway federation readiness の end-to-end 診断（`20260911-gateway-doctor-readiness.md` で扱う）
- 実 Cloudflare account の route/domain を repository test から自動変更すること

## 運用契約

Gateway を公開する operator は、deploy 前に次のどちらか一方を明示的に選ぶ。

### A. Custom domain binding

- Cloudflare Worker custom domain を明示的に作成する。
- 対象 hostname 全体を Access で保護する。
- MCP client 用 Managed OAuth と host-agent 用 Access service-token policy を混同しない。
- deploy 後に domain binding が intended Worker/version を指すことを read-only で確認する。

### B. Existing DNS + Worker route

- 既存 DNS record を削除しない。
- exact hostname/pattern を `wrangler deploy --routes '<hostname>/*'` などの明示 target として渡す。
- 対象 hostname 全体を Access で保護する。
- deploy 後に route が intended Worker/version を指すことを read-only で確認する。

どちらの方式でも `workers_dev = false` を維持し、target 未指定の deploy を成功扱いしない。

## Secret / variable boundary

- dashboard-managed non-secret variables を保持する deploy では `--keep-vars` を使う。
- Worker secrets は `wrangler secret` / Cloudflare の secret 管理に残し、issue/docs/log に値を保存しない。
- Access service-token credential と Gateway host token は別の credential boundary として扱う。
- verification は secret の「存在/認証結果」を扱っても値を表示しない。

## Executable implementation slices

### Slice A — docs + deterministic local checks

- `docs/gateway.md` / `docs/gateway.ja.md` に custom domain と existing DNS + Worker route の decision table を追加する。
- `workers_dev = false` の前提、Access/OAuth/service-token 境界、`--keep-vars`、secret 管理、rollback を明記する。
- `No targets deployed` を成功扱いしない deploy checklist を追加する。
- exact hostname/pattern 以外を変更しない rollback 手順を書く。
- docs/example の決定論的 check を追加する。
- **実 Cloudflare route/domain は変更しない。**

**Done when:** operator が docs だけで target を選択し、dry-run/deploy/verify/rollback の順序を再現できる。

### Slice B — optional target preflight

Slice A の後、手作業だけでは target 欠落を十分検出できない場合にのみ実装する。

- repository script または narrow CLI preflight として実装する。
- `workers_dev=false` かつ target 未指定を明示 failure にする。
- intended gateway hostname と configured route/domain の不一致を non-secret に報告する。
- Cloudflare credential がない場合は remote verification を `unknown/not checked` とし、ready と偽装しない。
- DNS/Access/route の mutation は行わない。

**Done when:** target 未指定・hostname 不一致・remote 未確認を distinct status で返せる。

### Slice C — live deployment acceptance

- 実 account で選択した方式を一度適用する。
- deploy output と route/domain の read-only verification を記録する。
- public hostname の疎通、Access、host-agent の成立確認は live acceptance matrix に evidence を残す。
- secret 値は保存しない。

**Done when:** repository-local implementation issue と credential-dependent live evidence が分離されている。

## 受け入れ条件

- [x] custom domain と既存 DNS + Worker route の違い、選択条件、必要な Access 設定が英日 docs に記載される。
- [x] `workers_dev = false` で target 未設定の場合に、deploy 手順または preflight が `No targets deployed` を見逃さない。
- [x] 既存 DNS record を削除せずに route を割り当てる実行例と、deploy 後の route/domain verification が記載される。
- [x] `--keep-vars`、Worker secrets、Access service token、host token の管理境界が混同されない。
- [x] 意図しない workers.dev 公開を有効化しない。
- [x] docs または tooling の変更に対する決定論的な test/check が追加される。
- [x] 実環境でしか確認できない項目は `20260908-live-acceptance-matrix.md` に evidence として分離される。

## テスト計画

Repository-local:

- docs/example の deterministic check
- preflight を追加した場合は fake wrangler/config を使った target-present / target-missing / mismatch / remote-unknown tests
- `git diff --check`

Live acceptance（実装完了の repository-local gate とは分離）:

- `npx wrangler deploy --dry-run --keep-vars --routes '<hostname>/*'`
- 実 Cloudflare account で target 一覧を read-only 確認し、対象 hostname が intended Worker に割り当てられていることを確認する
- 選択した deployment 方式について deploy output と公開 hostname の疎通を確認する

## リスク

同一 hostname に direct Tunnel と Worker route が共存すると、route の追加・削除によって接続先が切り替わる。変更対象を exact hostname/pattern に限定し、DNS・Access・Tunnel の各状態を個別に確認できる rollback 手順を docs に含める必要がある。target 検証用 API credential は秘密値をログや issue に保存しない。

## 変更履歴

`CHANGES.md` impact: yes

項目案：

- Gateway Worker の deployment target 選択、既存 DNS route、custom domain の運用手順を明確化。

## 注記

- 2026-09-11: `localmcp.obr-grp.com/*` への Worker route は direct Temote 復旧のため削除した。DNS record、Access application、Tunnel は削除していない。
- 2026-09-11: `workers_dev = false` の route/domain 未設定 deploy で `No targets deployed` が発生したため、再発防止の issue として記録した。
- 2026-09-15: CLI completion slice is fully specified and implemented; advance through the repository state workflow before recording completion.
- 2026-09-15: Verified implementation commit and review evidence are being recorded for completion.
- 2026-09-15: Repository-local deployment-target CLI acceptance is complete in 04ddd76; deterministic and full gates passed, Astra approved, and live Cloudflare evidence remains in the acceptance matrix.

## Remaining external evidence

Cloudflare route/domain verification is credential-dependent live acceptance and remains in `issues/open/20260908-live-acceptance-matrix.md`. It does not block this repository-local implementation issue.

## Implementation notes (2026-09-11)

Slice A landed on main:

- `docs/gateway.md` / `docs/gateway.ja.md` now contain the custom-domain vs existing-DNS + Worker-route decision table, the `workers_dev = false` requirement, the `No targets deployed` failure rule, `--keep-vars` and secret-boundary guidance, read-only post-deploy verification (`wrangler deployments status`, `/healthz` service identity), and exact-pattern rollback that leaves DNS, Access, and Tunnel untouched.
- `tests/gateway_deployment_docs.rs` deterministically checks both documents for the required operator contract and rejects any `workers_dev = true` guidance.

No Cloudflare route, DNS, Access, or Tunnel state was changed.

## Implementation notes (2026-09-12)

Slice B implemented:

- `gateway/scripts/deployment-preflight.mjs` checks local `workers_dev = false` configuration and an explicitly supplied route or custom-domain target.
- It distinguishes `target_missing`, `target_mismatch`, and `remote_unknown`; it never uses Cloudflare credentials or claims remote readiness.
- `gateway/test/deployment-preflight.test.mjs` covers target-present, missing, mismatch, unsafe workers.dev configuration, and non-disclosure of target values.
- `npm run deploy:preflight -- --hostname <host> --route '<host>/*'` is documented in both gateway operator guides.

Cloudflare route/domain verification remains Slice C live acceptance.

## Completion notes (2026-09-15)

- Commit `04ddd76` fixes the command-line parser so `--hostname` remains required, simultaneous `--route` and `--custom-domain` remains a usage error with exit code 2, and omitting both target flags reaches the evaluator and returns `target_missing` with exit code 1.
- The command-level regression starts the actual Node subprocess and verifies exit code 1, parseable `target_missing` JSON on stdout, and empty stderr. A second subprocess assertion preserves the simultaneous-target usage error.
- Verification passed: focused deployment-preflight tests 7/7, gateway tests 70/70, full `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, `cargo fmt --all -- --check`, and `git diff --check`. The no-default-features check emitted five pre-existing dead-code warnings and completed successfully.
- `gpt-6-astra` reviewed immutable commit `04ddd76`, reran all seven deployment-preflight tests, and approved with no findings.
- `CHANGES.md` impact is yes because the operator-facing preflight now reports the intended missing-target status from the CLI. This repository contains no `CHANGES.md` file at the verified commit, and the authorized completion-baseline scope excludes creating or editing shared changelog documentation.
- No Cloudflare route, custom domain, DNS record, Access policy, Tunnel, or credential was changed. Live route/domain verification remains exclusively in `issues/open/20260908-live-acceptance-matrix.md`; no repository-local implementation work remains in this issue.

## Triage note

- 2026-09-14: The S04 review found that `node gateway/scripts/deployment-preflight.mjs --hostname example.com --config gateway/wrangler.toml` exits with usage code 2 before `target_missing` can be emitted, while the existing five tests call `evaluateDeploymentPreflight` directly. The prior triage move to `done/` was therefore reverted to `open/`; the CLI fix and command-level regression test are required before completion. `CHANGES.md` remains unchanged during this triage because the repository-local implementation is not complete.

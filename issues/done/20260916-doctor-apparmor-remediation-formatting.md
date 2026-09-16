# `doctor` の AppArmor remediation command が連結表示され copy/paste 不能

Status: done
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P2 developer workflow friction
Type: diagnostics / UX

## Observed

normal Temote session から `temote-mcp doctor` を実行し、Ubuntu/AppArmor の unprivileged user namespace restriction を検出した際、remediation guidance が次のように連結表示された。

```text
[WARN] AppArmor userns policy: unprivileged user namespaces are restricted (1)
       Ubuntu may be blocking unprivileged user namespaces. Try:sudo apt updatesudo apt install apparmor-profiles apparmor-utilssudo install -m 0644 /usr/share/apparmor/extra-profiles/bwrap-userns-restrict /etc/apparmor.d/bwrap-userns-restrictsudo apparmor_parser -r /etc/apparmor.d/bwrap-userns-restrict
```

`sudo apt update`, `sudo apt install ...`, `sudo install ...`, `sudo apparmor_parser ...` の境界に改行・separator がなく、human がそのまま実行できない。

sandbox failure の診断は安全境界に関わるため、remediation text が曖昧だと誤った host 操作や unnecessary yolo workaround を誘発する。

## Goal

`doctor` の remediation guidance を、readable・copy/paste可能・bounded・platform-specific に表示する。

## Proposed direction

- remediation を単一連結 string ではなく structured steps / lines として表現する。
- terminal renderer は各 command を明確な改行と indentation で出す。
- human prose と command argv/string を区別する。
- JSON/structured doctor output がある場合は command list を array として保持する。
- sudo を伴う host mutation は自動実行せず、doctor は診断と guidance のみに留める。

## Implemented

AppArmor remediation commandを `APPARMOR_PROFILE_COMMANDS` の4 stepとして保持し、`apparmor_profile_hint()` が human prose の後に各commandを明示的な `\n` で連結するようにした。Rust sourceのline-continuation `\\` によるnewline消失を使わない。

同じremediationは `AppArmor userns policy` warningだけでなく、bwrap network namespace probe / sandbox execution が同じpermission errorを検出した場合にも共通helperを使う。doctorは引き続きhintを表示するだけで、sudo commandを実行しない。

## Acceptance criteria

- [x] AppArmor warning の各 remediation command が独立した行として表示される。
- [x] command 間の whitespace/newline が snapshot test で固定される。
- [x] narrow terminal width / plain text rendering でも command が連結しない。rendererはhintのlogical linesをそのままindented lineとして出力し、terminal width依存のjoinをしない。
- [x] doctor は remediation を自動実行しない。
- [x] Linux non-AppArmor / macOS / Cloudflare 等、既存 doctor rendering を回帰させない。

## Verification

- `doctor::tests::apparmor_remediation_keeps_commands_on_independent_lines`: PASS。prose + 4 commandsのexact multiline snapshotを固定。
- `cargo test --bin temote-mcp --all-features --locked doctor::tests::`: **27/27 PASS**。
- `cargo fmt --all -- --check`: PASS。
- `cargo clippy --all-targets -- -D warnings`: PASS。
- `cargo check --no-default-features --all-targets --locked`: PASS（既存dead-code warningのみ）。
- `just sandboxed-check`: exit 0。lib 110/110、activity job 5/5、activity coverage 3/3、upgrade transaction 40/40、upgrade coordinator 6/6、gateway 2/2、`git diff --check` PASS。
- Linux nested sandbox runtime / full binary-local Unix-socket integration / ignored supervisor-process-boundary E2E: **NOT RUN (host/CI gate)**。

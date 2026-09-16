# `just` recipe が normal Temote session で `/etc/bash.bashrc` の nounset 警告を毎回出す

Status: done
Model: GPT-5.6 Sol
Created: 2026-09-16
Priority: P2 developer workflow friction
Type: development / shell runtime

## Observed

normal Temote session で `just sandboxed-check` を実行すると、各 recipe の開始時に次が stderr へ出た。

```text
/etc/bash.bashrc: line 7: PS1: unbound variable
```

最小再現も同じだった。

```text
bash -eu -o pipefail -c true
```

`/etc/bash.bashrc` は interactive 判定として `[ -z "$PS1" ] && return` を実行するが、Bash startup 時点から `-u` が有効なため、non-interactive shell の未定義 `PS1` 参照が警告になる。recipe 自体は exit 0 のため gate failure ではないが、長い check で同じ警告が繰り返され、実際の stderr を見つけにくくする。

## Resolution

Just の child shell は startup 時には `-u` を付けず、`bash -e -o pipefail -c 'set -u; eval "$0"' <recipe>` で startup 完了直後に nounset を有効化する。

これにより system/user bashrc は変更せず、recipe 実行時の `errexit` / `nounset` / `pipefail` は維持する。

## Verification

```text
bash -e -o pipefail -c 'set -u; eval "$0"' 'echo nounset=$-; test -n "$PWD"'
# exit 0, nounset option `u` present, no /etc/bash.bashrc warning
```

`just sandboxed-check` を変更後に再実行し、startup警告が消えたことと deterministic gate が引き続き PASS することを確認する。

## Acceptance criteria

- [x] system `/etc/bash.bashrc` と user dotfiles を変更しない。
- [x] recipe 実行時は `-e`, `-u`, `pipefail` が有効。
- [x] non-interactive startup の `PS1` nounset warning を出さない。
- [x] `just sandboxed-check` が変更後も PASS。

変更後の実測では `just sandboxed-check` は exit 0。library deterministic subset 100/100、activity job 5/5、activity coverage 3/3、upgrade transaction 40/40、upgrade coordinator 6/6、gateway 2/2 が PASSし、startup の `PS1` nounset warning は0件だった。

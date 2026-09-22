# GitHub pull-request broker

The broker provides bounded list, get, and close operations for the repository selected by a managed session, `cwd`, and Git remote.

List returns at most 50 open pull requests sorted by `updated_at` descending. Its wrapper metadata contains `repository`, `limit`, and `possibly_truncated`; `limit` is always 50, and `possibly_truncated` is true exactly when 50 items were returned. This is conservative and does not prove that more items exist.

Each pull-request summary contains exactly **number, title, state, draft, head_ref, updated_at, html_url**.

Get and close accept `number` as a canonical positive decimal string:

```json
{ "session_id":"temo", "number":"7" }
```

This example applies to GET/CLOSE, not list.

Close performs the fixed `PATCH` transition `state=closed` after local-user approval. It does not require the pull request to be already closed, and cannot merge, delete a branch, add comments or reviews, invoke an arbitrary API, or target another repository.

Before approval, the broker pins the worktree and its private/common Git metadata directories. It reuses the same file descriptors afterward. Credentials come from repository-local `gh-git` configuration; it never switches the global account.

The contract does not expose tokens or arbitrary API request bodies.

Source and build checks plus a rebuilt-runtime canary are required before claiming that a runtime exposes these tools. The fixture-backed close test remains pending until recorded evidence exists.

日本語: [GitHub PR ブローカー](ja/github-pr-broker.md)

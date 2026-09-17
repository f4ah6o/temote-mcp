# Git broker queue must not trust agent-writable filesystem entries

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`
Depends on: none

## Current code and contract

The build/response queue lives under the agent state root, which is a writable local-agent sandbox
root, so the agent can replace entries and directories. Review-time `serve`/`request_broker`
(`src/agent_git.rs:298`, `:413`) use plain `std::fs::read`/`write`:

- reads follow symlinks and can block forever on a FIFO/device (the poll loop stops serving);
- the request size is checked only after the whole file is read into memory;
- `std::fs::write(&response_path, payload)` follows a symlink planted at the response path, which
  is a parent-user write outside any sandbox;
- request enumeration and per-tick processing are unbounded;
- a directory replaced by a symlink redirects the next path-based operation.

## Reproduction

- `ln -s /etc/hostname git-broker/requests/x.json`: broker reads the outside file (parse failure
  path).
- `mkfifo git-broker/requests/x.json`: broker blocks in `read` forever.
- `ln -s ~/victim git-broker/responses/<id>.json` then request `<id>`: broker writes through the
  symlink to the victim path.
- `git-broker/responses` renamed and replaced by a symlink to another directory.

## The one responsibility to change

Make the broker (and the shim side of the same queue) hold directory handles and operate only on
validated regular files:

- open `requests/` and `responses/` once at start with `O_DIRECTORY|O_NOFOLLOW`, verify
  directory/owner/mode (0700, current euid), and keep the descriptors for the broker lifetime;
- enumerate with bounded `readdir` on a duplicated descriptor, cap entries scanned and requests
  processed per tick;
- read entries with `openat(..., O_RDONLY|O_NOFOLLOW|O_NONBLOCK)`, verify regular file and size
  before a bounded read (`take(max+1)`), reject symlinks/special files/directories;
- publish responses into a `O_CREAT|O_EXCL|O_NOFOLLOW` temp entry and `renameat` it to the final
  name so an existing entry (including a symlink) is replaced atomically, never followed;
- unlink by name through the held request descriptor; malformed entries never stop the loop;
- fixed error payloads only, no file content or path leakage.

## Not changing

- Transport stays the private request/response directory; no socket/transport replacement.
- Existing request/response schemas and size limits stay.
- `GitBroker` lifetime still matches one `local_agent::run`.

## Focused tests

- FIFO and symlinked request entries are rejected without blocking or reading the target.
- oversized request entry rejected without unbounded allocation.
- symlinked response target is not followed; sentinel outside the queue is unchanged.
- renamed/replaced `requests` directory does not redirect a held broker.
- queue with an entry flood still drains in bounded per-tick batches.
- malformed entry does not stall the next valid request.
- sentinel files outside the queue are byte-identical after all rejection tests.

## Host / CI / provider verification

- Linux host/CI focused tests; macOS host execution NOT RUN unless a macOS runner is used.

## Completion condition

Queue processing is symlink/special-file/race resistant with bounded memory and progress; focused
tests and `just sandboxed-check` pass.

## Implementation notes (2026-09-17)

Changes in `src/agent_git.rs` (`BrokerQueue` + helpers):

- Queue creation opens the root, `requests/`, and `responses/` with
  `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC` and validates directory/owner/mode (current euid, no group or
  other bits); subdirectories are created with `mkdirat` + `openat` relative to the held root.
- Request/response entries are opened with `openat(O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC)` and
  must be regular files within the size limit before a `take(max+1)` bounded read.
- Responses are published with an exclusive temp entry plus `renameat`; a planted symlink at the
  final name is replaced, never followed.
- Enumeration uses a fresh `openat` + `fdopendir` per tick (a `dup` would share the directory
  offset and miss new entries), capped at 4096 scanned entries and 16 processed entries per tick.
- The serve loop stops if the held request directory loses its last link, and malformed names are
  unlinked without a response.

Focused tests (PASS):

- `queue_rejects_symlinked_and_special_entries_without_touching_sentinels` — symlink and FIFO
  request entries rejected without blocking, oversized entry rejected, symlinked response target
  not followed, outside sentinel unchanged, final response is a regular file.
- `broker_survives_malformed_and_special_queue_entries` — FIFO + malformed + oversized entries
  processed first; a valid later request still completes.
- `queue_enumerates_a_bounded_number_of_entries` — 4104 entries yield a bounded list.
- Host-only `sandbox::linux_tests::linux_local_agent_git_shim_executes_from_state_and_uses_the_private_queue`
  PASS in the full `--all-features` run (real sandboxed shim against the new queue).

NOT RUN: macOS host execution; adversarial concurrent directory-swap execution beyond the
link-count stop and held-descriptor model.

## 2026-09-17 correction

The single queue root became two roots: `requests` stays under the agent-writable state root and
`responses` moved to a parent-owned read-only root. The symlink/special-file/bounded-read/atomic
rename protections apply to both roots; see
`issues/done/20260917-git-broker-response-authority.md`.

## 2026-09-17 completion

Repository-local acceptance is met and the repair round above passed independent review; the
remaining NOT RUN rows above stay host/CI or live-matrix gates.

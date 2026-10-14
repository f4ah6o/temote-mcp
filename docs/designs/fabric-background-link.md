# Managed background Fabric Link

The owner CLI can enroll in its current terminal, explicitly approve the account,
Host and root scope, then hand an approved browser Link to a detached child. This
does not install a login item, launchd/systemd unit, or change Supervisor ownership.

## Commands and scope

- `temote fabric connect --background`: keep the existing foreground flow for
  any necessary browser interaction and approval, then return after a managed
  child is ready. A previously approved valid grant can be reused.
- `temote fabric status`: include the local managed process state independently
  of authentication, grant, transport connectivity and root verification.
- `temote fabric disconnect`: stop only the managed child for this public
  profile. It never stops the Supervisor, sessions or revokes the Host grant.
- Existing `connect`/`link` foreground and static mode remain compatible. Static
  background mode is outside this change.

## Boundaries

1. Acquire the profile Link lifetime lock before enrollment or process spawning.
   A foreground or background owner prevents a duplicate attempt.
2. Keep account/Host/root approvals in the parent. The child is resume-only: a
   noninteractive secure-store read, pinned options, active unexpired grant,
   verified owner and remote scope, and current read-only Supervisor root identity
   are mandatory. No child browser launch, reserve, activate, re-enrollment,
   root changes or grant revocation is permitted.
3. Re-exec the current binary with null stdio in a new session. An anonymous
   inherited Unix socket carries public settings and startup acknowledgements.
   An inherited locked descriptor transfers lock ownership without a release gap.
   The parent disables explicit unlock before spawn; the child enables it after commit. EOF before commit stops the child, while EOF after commit detaches it. Ready follows the first remote connection and local receipt, before RPC polling begins.
4. Credentials remain in the OS store and process memory. No credentials in
   arguments, environment, ordinary files or logs. Child stdout/stderr are null;
   state files contain a closed set of public lifecycle fields and reason codes.
5. macOS background refresh uses an existing-item update with authentication UI
   forbidden and verified read-back. Other stores without a no-prompt update
   fail before token refresh and require a foreground recovery command.
6. Authentication rejection, uncertain refresh, inaccessible secure store,
   grant expiry/change, root identity change or Supervisor epoch change stops the
   child. Transient transport failures can retry under the same approved grant. Verification read failures pause new admission and retry for at most 60 seconds; a refresh ambiguity never retries the old token.
7. Local status/stop use an owner-only Unix socket with peer UID and instance
   checks. Never signal a PID loaded from a file. Stale state is not proof of a
   running process. Cleanup cannot remove a successor's socket or state.

## Isolated verification

Use mocks and private subprocess/PTY fixtures with cleared environment, private
HOME/runtime/namespace and no production OAuth endpoints or OS credentials.
Cover initial cancellation, absent/expired/pending grant, changed owner/roots,
foreground/background duplicate ownership, handoff parent failure, graceful stop,
stale records/PID reuse, authentication failure, read/write UI prohibition and
secret sentinels absent from all public output and state files.

## Small follow-up units

1. Validate the included older-Supervisor readiness wording and multi-machine
   setup guide with first-time users.
2. Add a Linux Secret Service backend that never performs a prompt during atomic
   refresh persistence; do not weaken the secure-store requirement.
3. Separately review an opt-in login/service installer, uninstall and recovery
   flow. Registration and automatic startup need explicit user approval.

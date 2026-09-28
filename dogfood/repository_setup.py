"""Observe repository setup through MCP only; filesystem work stays delegated."""
from __future__ import annotations

import time
import json
import uuid

from .protocol import Recorder, TERMINAL, metrics, snapshot
from .runner import AdapterError, FakeAdapter


def final_report(content):
    """Only final assistant output is evidence; a sentinel in the prompt is not."""
    try:
        thread = json.loads(content).get('thread', {})
        messages = [item['text'] for turn in thread.get('turns', [])
                    for item in turn.get('items', [])
                    if item.get('type') == 'agentMessage' and item.get('phase') in {'final', 'final_answer'}]
    except (ValueError, TypeError, KeyError):
        raise AdapterError('FINAL_REPORT_INVALID')
    if not messages:
        raise AdapterError('FINAL_REPORT_MISSING')
    return messages[-1]


class SetupFixture(FakeAdapter):
    def __init__(self):
        super().__init__()
        self.destinations = {}

    def call(self, tool, arguments):
        if tool == 'repository_clone_bare':
            destination = arguments['destination']
            if destination.startswith(('/', '../')) or arguments['session_id'].startswith('worktree-'):
                raise AdapterError('REPOSITORY_SETUP_REJECTED')
            if destination in self.destinations and self.destinations[destination] != arguments['operation_id']:
                raise AdapterError('DESTINATION_EXISTS')
            self.destinations[destination] = arguments['operation_id']
            return super().call('codex_task_start', arguments)
        if tool == 'session_start':
            return {'session_id': arguments['session_id'], 'status': 'active', 'yolo': False}
        if tool == 'session_info':
            return {'session_id': arguments['session_id'], 'status': 'active',
                    'permission_mode': 'agent', 'yolo': False, 'server_contract_fingerprint': 'f' * 64}
        if tool == 'evidence_read':
            return {'content': 'TEMOTE_SETUP_VERIFIED'}
        return super().call(tool, arguments)


def execute_setup(scenario_data, phase, adapter, *, session_id, repository_head, binary_identity,
                  backend='codex', model=None, effort=None, max_polls=200, poll_interval=1,
                  root='src', source='src/temote-mcp-df', destination=None, gates=None, **unused):
    if not 1 <= max_polls <= 200 or poll_interval < 0:
        raise ValueError('invalid bounded wait')
    if backend != 'codex':
        raise ValueError('repository setup currently requires the local Codex backend')
    if type(adapter) is FakeAdapter:
        adapter = SetupFixture()
    run_id = str(uuid.uuid4())
    destination = destination or 'temote-dogfood-' + run_id + '.git'
    worktree_session = 'worktree-' + run_id
    recorder = Recorder(run_id, scenario_data, phase)
    assertions = {key: 'not_run' for key in scenario_data['assertions']}
    assertions['identity_is_separate'] = 'pass' if repository_head != binary_identity else 'fail'
    contract = 'f' * 64 if isinstance(adapter, FakeAdapter) else 'unknown'
    permission_mode = None
    last_state = 'unknown'
    outcome = 'blocked'
    failure_code = None

    def observed(operation, tool, args, recovery=False):
        nonlocal last_state
        start = time.monotonic()
        try:
            response = adapter.call(tool, args)
        except AdapterError as error:
            last_state = 'unavailable' if error.code.endswith('UNAVAILABLE') else 'error'
            recorder.call(operation, tool, args, {}, state=last_state, error_code=error.code,
                          retryable=error.retryable, next_action='reconcile' if error.retryable else 'inspect_failure',
                          duration_ms=int((time.monotonic() - start) * 1000), recovery=recovery)
            raise
        last_state = response.get('status', 'ok')
        recorder.call(operation, tool, args, response, state=last_state,
                      next_action='read_evidence' if last_state in TERMINAL else 'poll_or_inspect',
                      duration_ms=int((time.monotonic() - start) * 1000), recovery=recovery)
        return response

    def finish(operation, view, owner_session):
        task_id = view.get('task_id')
        if not task_id:
            raise AdapterError('TASK_ID_MISSING')
        for index in range(max_polls):
            if view.get('status') in TERMINAL:
                break
            if index:
                time.sleep(poll_interval)
            view = observed(operation, 'codex_task_get', {'session_id': owner_session, 'task_id': task_id})
        if view.get('status') not in TERMINAL:
            raise AdapterError('POLL_LIMIT_REACHED', retryable=True)
        if view['status'] != 'completed':
            raise AdapterError('TASK_NOT_COMPLETED')
        evidence_id = (view.get('evidence') or {}).get('evidence_id')
        if not evidence_id and not isinstance(adapter, FakeAdapter):
            if len(adapter.owners) > 1:
                adapter.release_prior_owners()
            view = observed(operation, 'codex_task_get',
                            {'session_id': owner_session, 'task_id': task_id}, recovery=True)
            evidence_id = (view.get('evidence') or {}).get('evidence_id')
        if not evidence_id:
            raise AdapterError('TERMINAL_EVIDENCE_MISSING')
        chunks = []
        offset = 0
        while True:
            evidence = observed('read_terminal_result', 'evidence_read',
                                {'session_id': owner_session, 'evidence_id': evidence_id,
                                 'offset_bytes': offset, 'max_bytes': 16384})
            chunks.append(evidence.get('content', ''))
            next_offset = evidence.get('next_offset_bytes')
            if next_offset is None:
                break
            if next_offset <= offset or next_offset > 1048576:
                raise AdapterError('EVIDENCE_BOUND_EXCEEDED')
            offset = next_offset
        assertions['bounded_result'] = 'pass'
        content = ''.join(chunks)
        if isinstance(adapter, FakeAdapter):
            return content
        return final_report(content)

    def task(operation, text, owner_session=session_id):
        view = observed(operation, 'codex_task_start',
                        {'session_id': owner_session, 'operation_id': str(uuid.uuid4()),
                         'model': model, 'effort': effort, 'task': text})
        return finish(operation, view, owner_session)

    def refused(arguments):
        try:
            observed('verify_repository_setup', 'repository_clone_bare', arguments)
        except AdapterError as error:
            if error.retryable:
                raise
            return
        raise AdapterError('HOST_ADMISSION_BYPASSED')

    clone_args = {'session_id': session_id, 'operation_id': str(uuid.uuid4()), 'root': root,
                  'source': source, 'destination': destination, 'model': model, 'effort': effort}
    try:
        info = observed('inspect_session', 'session_info', {'session_id': session_id})
        contract = info['server_contract_fingerprint']
        permission_mode = info.get('permission_mode')
        if info.get('status') != 'active' or info.get('permission_mode') not in {'agent', 'ask'} or info.get('yolo'):
            raise AdapterError('PREPARATION_SESSION_UNSUITABLE')
        try:
            view = observed('clone_bare', 'repository_clone_bare', clone_args)
        except AdapterError as error:
            if not error.retryable:
                raise
            if not isinstance(adapter, FakeAdapter):
                adapter.reconnect()
            view = observed('retry_clone', 'repository_clone_bare', clone_args, recovery=True)
        finish('wait_until_terminal', view, session_id)
        assertions['bare_clone_completed'] = 'pass'
        # New transport, same operation UUID. Do not invent another ID after an uncertain acceptance.
        if not isinstance(adapter, FakeAdapter):
            adapter.reconnect()
        replay = observed('retry_clone', 'repository_clone_bare', clone_args, recovery=True)
        if replay.get('task_id') != view.get('task_id'):
            raise AdapterError('DUPLICATE_CLONE_ACCEPTANCE')
        branch = 'codex/dogfood-' + run_id
        task('create_worktree',
             f'Within this session root, verify {destination!r} is a bare Git repository, then create exactly '
             f'one worktree at {destination + "/.wt/development"!r} with new branch {branch!r} from HEAD. '
             'Use git worktree add. Do not overwrite anything, remove files, change credentials, or use network. '
             'Report failure truthfully; do not fix a missing clone. Do not inspect unrelated repositories.')
        assertions['worktree_created'] = 'pass'
        start = observed('start_worktree_session', 'session_start',
                         {'session_id': worktree_session, 'path': root + '/' + destination + '/.wt/development'})
        info = observed('start_worktree_session', 'session_info', {'session_id': start['session_id']})
        if info.get('status') != 'active' or info.get('permission_mode') != 'agent' or info.get('yolo'):
            raise AdapterError('WORKTREE_SESSION_UNSUITABLE')
        assertions['normal_session_started'] = 'pass'
        # Select the normal development session through a fresh MCP transport.
        # Keep preparation owners alive until their accepted work is terminal.
        if not isinstance(adapter, FakeAdapter):
            adapter.reconnect()
        observed('develop_with_jj', 'session_info', {'session_id': start['session_id']})
        task('develop_with_jj',
             'Prepare jj using a Git backing repository wholly inside this worktree, keeping its existing linked .git pointer intact. '
             'Run git init --bare .jj-backing.git; git --git-dir .jj-backing.git fetch ../.. HEAD:refs/heads/seed; '
             'git --git-dir .jj-backing.git symbolic-ref HEAD refs/heads/seed. '
             'Run jj --config snapshot.auto-track=\"none()\" git init --git-repo .jj-backing.git. '
             'Pass --config snapshot.auto-track=\"none()\" as one literal argument to EVERY jj command. '
             'Do not run jj config set/edit/path --repo or --workspace, create config-id files, or write host user configuration. '
             'Pass these as literal argv values with proper shell quoting if a shell is used. Do not initialize colocation '
             'in this linked Git worktree, and do not write the backing bare repository outside this session cwd. '
             'Using jj, create a change containing a new DOGFOOD_JJ.txt with the text "Temote MCP bare clone dogfood". '
             'Explicitly track only DOGFOOD_JJ.txt with jj file track; keep .jj-backing.git untracked. Describe the change "test: prove repository setup through jj". Run jj status and jj diff --git '
             'and ensure the file is tracked in the jj working copy. Do not push, release, merge, or alter unrelated files. '
             'Report the commands and actual result truthfully.', start['session_id'])
        assertions['jj_development_completed'] = 'pass'
        before = observed('verify_repository_setup', 'task_list', {'session_id': session_id, 'limit': 128})
        for invalid in ('../escape.git', '/tmp/escape.git'):
            refused({**clone_args, 'operation_id': str(uuid.uuid4()), 'destination': invalid})
        refused({**clone_args, 'operation_id': str(uuid.uuid4()), 'destination': destination})
        refused({**clone_args, 'operation_id': str(uuid.uuid4()), 'session_id': start['session_id'],
                 'destination': 'must-not-create-' + run_id + '.git'})
        after = observed('verify_repository_setup', 'task_list', {'session_id': session_id, 'limit': 128})
        for listing in (before, after):
            if listing.get('truncated') or any(v.get('status') == 'unavailable'
                                              for v in listing.get('backends', {}).values()):
                raise AdapterError('TASK_LIST_INCOMPLETE')
        if {t['task_id'] for t in before.get('tasks', [])} != {t['task_id'] for t in after.get('tasks', [])}:
            raise AdapterError('REJECTED_CLONE_WAS_DELEGATED')
        assertions['host_admission_enforced'] = 'pass'
        evidence = task('verify_repository_setup',
                        f'Read-only verification: inspect only {destination!r} and its .wt/development worktree. '
                        'Verify Git reports a bare repository at the destination, exactly one linked development '
                        'worktree exists, the linked Git common directory belongs to that bare repository, '
                        'jj --ignore-working-copy --config snapshot.auto-track=\"none()\" status/log/diff show '
                        'the described jj change and DOGFOOD_JJ.txt with expected text. Use that CLI config argument '
                        'for every jj invocation; no repo/workspace config-id should exist, and no host config writes are allowed. Verify '
                        'and no duplicate clone or alternate clone directory was created. Do not change or repair '
                        'anything. Output TEMOTE_SETUP_VERIFIED only if every check actually passes; otherwise '
                        'output TEMOTE_SETUP_FAILED with the exact failing check. Put the result sentinel alone on a line '
                        'in your final report, not only in commentary.')
        # Evidence bodies remain in memory. Persist only the outcome derived from the sentinel.
        if not any(line.strip() == 'TEMOTE_SETUP_VERIFIED' for line in evidence.splitlines()) or 'TEMOTE_SETUP_FAILED' in evidence:
            raise AdapterError('INDEPENDENT_SETUP_VERIFICATION_FAILED')
        assertions['no_duplicate_clone'] = 'pass'
        outcome = 'pass'
    except AdapterError as error:
        failure_code = error.code
        outcome = 'blocked' if error.retryable or error.code.endswith('UNAVAILABLE') else 'fail'
        if assertions['bare_clone_completed'] == 'not_run':
            assertions['bare_clone_completed'] = 'blocked' if outcome == 'blocked' else 'fail'
    return {'schema_version': 1, 'run_id': run_id, 'phase': phase,
            'scenario_id': scenario_data['id'], 'scenario_revision': scenario_data['revision'],
            'scenario_fingerprint': scenario_data['fingerprint'],
            'snapshot': snapshot(repository_head, binary_identity, contract,
                                 {'backend': backend, 'model': model, 'effort': effort,
                                  'source_kind': 'named-root-local' if not source.startswith('https://') else 'https',
                                  'terminal_read_strategy': 'reuse', 'permission_mode': permission_mode,
                                  'max_polls': max_polls, 'poll_interval': poll_interval,
                                  'lifecycle_transport': 'http' if getattr(adapter, 'lifecycle_url', None) else 'stdio'}),
            'outcome': outcome, 'failure_code': failure_code, 'assertions': assertions, 'events': recorder.events,
            'metrics': metrics(recorder.events), 'last_state': last_state, 'gates': gates or {}}

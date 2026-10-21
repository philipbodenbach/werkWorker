#!/usr/bin/env python3
"""Real PTY + authenticated Werk fixture: setup, retry, persistence and reconnect.

No live model or real credential is used. Run with a built ww binary.
"""
import fcntl
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import select
import sqlite3
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from tui_support import Screen

KEY = 'setup-fixture-sensitive-key'
requests = []
discovery_requests = []

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def respond(self, data, status=200, control=False):
        if control:
            data = {'protocol': {'major': 1, 'minor': 0},
                    'request_id': 'req_test', 'data': data}
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        if control:
            self.send_header('x-werk-protocol-version', '1.0')
            self.send_header('x-werk-request-id', 'req_test')
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        discovery_requests.append(self.path)
        if self.headers.get('Authorization') != 'Bearer ' + KEY:
            self.respond({'error': 'unauthorized'}, 401)
            return
        routes = {
            '/werk/v1/info': {'service': 'werk1112', 'protocol': {'major': 1, 'minor': 0}, 'active_backend': 'fixture'},
            '/werk/v1/capabilities': {'capabilities': [
                {'id': 'runtime.state.persistence', 'status': 'unsupported',
                 'detail': 'fixture backend does not expose this operation'}]},
            '/v1/capabilities': {'models': [{'id': 'fixture-model', 'available_tasks': ['text_generation']}]},
            '/v1/models': {'data': [{'id': 'fixture-model'}]},
            '/v1/parameters?task=text-generation': {'parameters': []},
        }
        if self.path in routes:
            self.respond(routes[self.path], control=self.path.startswith('/werk/'))
        else:
            self.respond({}, 404)

    def do_POST(self):
        assert self.headers.get('Authorization') == 'Bearer ' + KEY
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(body)
        assert KEY not in json.dumps(body)
        assert body['max_completion_tokens'] == 4096
        assert body['model'] == 'fixture-model'
        assert 'fixture project instruction' in body['messages'][0]['content']
        if not any(m['role'] == 'tool' for m in body['messages']):
            self.respond({'choices': [{'message': {'role': 'assistant', 'content': 'Read approved fixture', 'tool_calls': [{'id': 'read1', 'type': 'function', 'function': {'name': 'files.read', 'arguments': json.dumps({'path': 'notes.md'})}}]}, 'finish_reason': 'tool_calls'}]})
            return
        assert 'notes fixture content' in json.dumps(body)
        self.respond({'choices': [{'message': {'role': 'assistant', 'content': 'Setup connected successfully'}, 'finish_reason': 'stop'}], 'usage': {'prompt_tokens': 100, 'completion_tokens': 4}})

binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='ww-setup-test-') as temp:
    root = Path(temp)
    workspace = root / 'workspace'
    workspace.mkdir()
    (workspace / "AGENTS.md").write_text("fixture project instruction")
    (workspace / "notes.md").write_text("notes fixture content")
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f'http://127.0.0.1:{server.server_port}'
    env = dict(os.environ, TERM='xterm-256color', XDG_CONFIG_HOME=str(root / 'config'),
               XDG_STATE_HOME=str(root / 'state'))
    env.pop('NO_COLOR', None)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 36, 130, 0, 0))
    before = termios.tcgetattr(slave)
    capture = bytearray()
    screen = Screen()
    proc = None

    def start(*extra, cwd=None):
        args = [str(binary), '--store', str(root / 'state.sqlite')]
        if cwd is None:
            args += ['--workspace', str(workspace)]
        return subprocess.Popen(args + list(extra), cwd=cwd,
                                stdin=slave, stdout=slave, stderr=slave, env=env)

    def wait_for(token, timeout=15, present=True):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if (token.encode() in screen.text()) == present:
                return
            if select.select([master], [], [], .1)[0]:
                data = os.read(master, 65536)
                capture.extend(data)
                screen.feed(data)
            if proc.poll() is not None:
                break
        raise AssertionError((token, screen.text().decode(), proc.poll()))

    def send(value):
        os.write(master, value.encode())

    def exited(timeout=10):
        # Keep consuming terminal output while waiting: a partial frame can
        # otherwise fill the PTY and block the UI before it processes Ctrl-Q.
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if select.select([master], [], [], .05)[0]:
                data = os.read(master, 65536)
                capture.extend(data)
                screen.feed(data)
            if proc.poll() is not None:
                return proc.returncode
        raise AssertionError(('UI did not exit', screen.text().decode()))

    try:
        proc = start('--setup')
        wait_for('Choose how to connect')
        send('\r')
        wait_for('Server and credentials')
        send('\x15' + origin + '\t\twrong-key\t\r')
        wait_for('Authentication failed')
        send('\x1b[Z\x15\x1b[200~' + KEY + '\x1b[201~\t\r')
        wait_for('Model and agent settings')
        wait_for('fixture-model')
        # Model(0), context(1), output(2), profile(3), access(4), tools(5), stream(6).
        send('\t32768' + '\t' * 5 + '\x1b[C' + '\t' * 3 + '\r')
        wait_for('Ready')
        config = root / 'config/werkworker/config.json'
        settings = json.loads(config.read_text())
        assert settings['werk']['base_url'] == origin
        assert settings['werk']['model'] == 'fixture-model'
        assert settings['limits']['reserve_output_tokens'] == 4096
        assert settings['werk']['capabilities']['context_tokens'] == 32768
        assert settings['secret_env'] is None
        assert KEY not in config.read_text()
        assert config.stat().st_mode & 0o777 == 0o600
        assert not requests, 'Opening setup must not trigger inference'
        send('Hello from configured TUI\r')
        wait_for('Approval')
        send('\x19')
        wait_for('Setup connected successfully')
        wait_for('Completed')
        assert len(requests) == 2
        send('/setup\r')
        wait_for('Choose how to connect')
        send('\r')
        wait_for('Server and credentials')
        wait_for(origin)
        assert b'********' not in screen.text(), 'Session API key must be entered again'
        send('\x03')
        wait_for('Press Ctrl-C again to exit setup')
        send('\x03')
        assert exited() == 0
        assert termios.tcgetattr(slave) == before
        assert KEY.encode() not in capture
        assert KEY.encode() not in (root / 'state.sqlite').read_bytes()

        # A new process reloads the same config and can use an environment reference.
        screen = Screen()
        env['WW_SETUP_FIXTURE_KEY'] = KEY
        proc = start('--setup')
        wait_for('Choose how to connect')
        send('\r')
        wait_for(origin)
        send('\t\x1b[C\t\x15WW_SETUP_FIXTURE_KEY\t\r')
        wait_for('Model and agent settings')
        wait_for('32768')
        send('\t' * 9 + '\r')
        wait_for('Ready')
        settings = json.loads(config.read_text())
        assert settings['secret_env'] == 'WW_SETUP_FIXTURE_KEY'
        assert KEY not in config.read_text()
        send('\x11')
        assert exited() == 0
        assert termios.tcgetattr(slave) == before
        assert KEY.encode() not in capture
        assert len(requests) == 2
        # Project preferences override defaults, but their requested access never does.
        project_dir = workspace / '.werkworker'
        project_dir.mkdir()
        project_file = project_dir / 'config.json'
        project_file.write_text(json.dumps({'version': 1, 'model': 'fixture-model',
            'output_tokens': 2048, 'requested_access': 'process',
            'test_commands': {'check': ['never-run-this-command']}}))
        global_before = config.read_bytes()
        screen = Screen()
        proc = start('--setup')
        wait_for('Choose how to connect')
        send('\r')
        wait_for(origin)
        send('\t' * 3 + '\r')
        wait_for('Model and agent settings')
        wait_for('2048')
        # Change output, keep default read-only policy, save only project settings.
        send('\t' * 2 + '\x154096' + '\t' * 7 + '\r')
        wait_for('Ready')
        assert config.read_bytes() == global_before
        project_settings = json.loads(project_file.read_text())
        assert project_settings['output_tokens'] == 4096
        assert project_settings['test_commands']['check'] == ['never-run-this-command']
        assert 'base_url' not in project_settings and 'secret_env' not in project_settings
        send('\x11')
        assert exited() == 0
        with closing(sqlite3.connect(root / 'state.sqlite')) as db:
            sessions = [json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')]
        assert len(sessions) == 3
        assert all(data['policy']['request_approvals'] for data in sessions)
        assert all(data['policy']['workspace_writable'] for data in sessions)
        assert all('process.run' in data['profile']['tools'] and 'files.write' in data['profile']['tools'] for data in sessions)
        assert all(all(not g['preapproved'] and g['tool'].startswith('files.') for g in data['policy']['grants']) for data in sessions)
        assert all('fixture project instruction' in data['profile']['prompt'] for data in sessions)
        assert len(requests) == 2, 'Setup and suggested test commands must not run inference/processes'
        assert termios.tcgetattr(slave) == before
        assert KEY.encode() not in capture and KEY not in json.dumps(sessions)
        # Bare ww discovers connection + credentials in its current workspace.
        local_connection = project_dir / 'werk-config.json'
        local_connection.write_text(json.dumps(settings['werk']))
        local_secrets = project_dir / 'secrets.json'
        local_secrets.write_text(json.dumps({'api_key': KEY}))
        local_secrets.chmod(0o600)
        settings['werk']['base_url'] = 'http://127.0.0.1:1'
        settings['secret_env'] = 'MISSING_GLOBAL_FIXTURE_KEY'
        config.write_text(json.dumps(settings))
        global_before = config.read_bytes()
        screen = Screen()
        proc = start('--setup', cwd=workspace)
        wait_for('Choose how to connect')
        send('\r')
        wait_for(origin)
        wait_for('********')
        send('\t' * 3 + '\r')
        wait_for('Model and agent settings')
        wait_for('fixture-model')
        send('\t' * 9 + '\r')
        wait_for('Ready')
        send('\x11')
        assert exited() == 0
        assert config.read_bytes() == global_before
        assert KEY not in project_file.read_text()
        assert KEY.encode() not in capture
        assert KEY.encode() not in (root / 'state.sqlite').read_bytes()
        assert len(requests) == 2
        assert termios.tcgetattr(slave) == before
        # --workspace has the same discovery behavior as the working directory.
        screen = Screen()
        proc = start('--setup')
        wait_for('Choose how to connect')
        send('\r')
        wait_for(origin)
        wait_for('********')
        send('\x11')
        assert exited() == 0
        # Ordinary restart offers the remembered service and durable root chats.
        with closing(sqlite3.connect(root / 'state.sqlite')) as db:
            old_sessions = [json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')]
        original = next(data for data in old_sessions if data['runs'])
        # Project connection defaults can be incomplete; Continue must retain the
        # effective capability choices validated during the successful setup.
        defaults = json.loads(local_connection.read_text())
        defaults['capabilities']['tool_calling'] = 'unknown'
        local_connection.write_text(json.dumps(defaults))
        other_workspace = root / 'other-workspace'
        other_workspace.mkdir()
        subprocess.run([str(binary), '--workspace', str(other_workspace),
                        '--store', str(root / 'state.sqlite'), 'run', '--demo',
                        'Hidden other workspace conversation'], env=env, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        screen = Screen()
        proc = start(cwd=workspace)
        wait_for('Continue previous')
        wait_for('Connect new')
        assert len(requests) == 2
        send('\r')
        wait_for('New / Previous sessions')
        wait_for('Hello from configured TUI')
        assert b'Hidden other workspace' not in screen.text()
        # Session management also lists the empty conversations created during
        # setup. Select the intended history by ID instead of assuming it is
        # the newest row after "New conversation".
        send('/' + original['id'] + '\r')
        wait_for('1 shown')
        send('\x1b[B\r')
        wait_for('Opened saved session')
        wait_for('Setup connected successfully')
        assert len(requests) == 2, 'Opening completed history must not repeat inference or tools'
        send('Follow up in the original conversation\r')
        deadline = time.monotonic() + 15
        while len(requests) < 3 and time.monotonic() < deadline:
            time.sleep(.05)
        assert len(requests) == 3
        assert any(m['role'] == 'user' and m['content'] == 'Hello from configured TUI'
                   for m in requests[-1]['messages'])
        assert any(m['role'] == 'assistant' and m.get('content') == 'Setup connected successfully'
                   for m in requests[-1]['messages'])
        # Wait for the committed follow-up instead of a previous frame's Completed label.
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            with closing(sqlite3.connect(root / 'state.sqlite')) as db:
                restored = next(json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')
                                if json.loads(row[0])['id'] == original['id'])
            if len(restored['runs']) == 2 and restored['runs'][-1]['status'] == 'completed':
                break
            if select.select([master], [], [], .05)[0]:
                data = os.read(master, 65536)
                capture.extend(data)
                screen.feed(data)
        assert len(restored['runs']) == 2 and restored['runs'][-1]['status'] == 'completed', (
            [(r['status'], r.get('error')) for r in restored['runs']], screen.text().decode())
        assert restored['policy'] == original['policy']
        assert restored['profile'] == original['profile']
        send('/sessions\r')
        wait_for('New / Previous sessions')
        send('\r')
        # Ready may still be visible behind the session picker. Wait for the
        # new chat frame before sending a key across the terminal transition.
        wait_for('New / Previous sessions', present=False)
        wait_for('Ready')
        send('\x11')
        assert exited() == 0
        assert termios.tcgetattr(slave) == before
        assert len(requests) == 3
        # --session alone opens a completed conversation without requiring a run ID.
        screen = Screen()
        proc = start('tui', '--session', original['id'])
        wait_for('Continue previous')
        send('\r')
        wait_for('Opened saved session')
        wait_for('Follow up in the original conversation')
        send('\x11')
        assert exited() == 0
        # Abrupt exit while awaiting approval: reopen the same run and request ID.
        screen = Screen()
        proc = start('tui', 'Recover the pending read')
        wait_for('Continue previous')
        send('\r')
        wait_for('Approval')
        with closing(sqlite3.connect(root / 'state.sqlite')) as db:
            pending = next(json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')
                           if any(r['input'] == 'Recover the pending read' for r in json.loads(row[0])['runs']))
        pending_run = pending['runs'][-1]
        request_id = next(i['approval']['request_id'] for i in pending_run['invocations'] if i.get('approval'))
        proc.kill()
        proc.wait(timeout=10)
        termios.tcsetattr(slave, termios.TCSANOW, before)
        screen = Screen()
        proc = start()
        wait_for('Continue previous')
        send('\r')
        wait_for('New / Previous sessions')
        send('\x1b[B\r')
        wait_for('Resume interrupted run')
        send('\x1b[B\r')
        wait_for('Opened saved session')
        send('Do not start another run over unresolved tools\r')
        wait_for('Resume the interrupted run via /sessions')
        assert len(requests) == 4
        send('\x15/sessions\r')
        wait_for('New / Previous sessions')
        send('\x1b[B\r')
        wait_for('Resume interrupted run')
        send('\r')
        wait_for('Approval ·')
        wait_for('Allow once')
        assert len(requests) == 4, 'Resume must not replay the already committed model tool call'
        send('\x19')
        wait_for('Setup connected successfully')
        wait_for('Completed')
        send('\x11')
        assert exited() == 0
        with closing(sqlite3.connect(root / 'state.sqlite')) as db:
            recovered = next(json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')
                             if json.loads(row[0])['id'] == pending['id'])
        assert len(recovered['runs']) == 1
        recovered_run = recovered['runs'][0]
        assert recovered_run['id'] == pending_run['id'] and recovered_run['status'] == 'completed'
        assert [i['approval']['request_id'] for i in recovered_run['invocations'] if i.get('approval')] == [request_id]
        assert len(requests) == 5
        assert termios.tcgetattr(slave) == before
        # Failure keeps the previous service available for retry or explicit new setup.
        server.shutdown()
        server.server_close()
        screen = Screen()
        proc = start()
        wait_for('Continue previous')
        send('\r')
        wait_for('Connection failed')
        wait_for('Connect new')
        send('\x1b[B\r')
        wait_for('Choose how to connect')
        # Exercise CI's failed chat -> setup -> connection transition repeatedly.
        # No sleeps or retry keystrokes mask a lost Enter after /setup.
        for transition in range(20):
            send('\x1b[C\x1b[C\r')  # Explicit offline demo; no server is needed.
            if transition == 0:
                wait_for('New / Previous sessions')
                send('\r')
            wait_for('Ready')
            # A PTY read can end after the new chat header but before the old
            # setup body is erased. Do not mistake that stale body for the
            # subsequent /setup screen and send its Enter to the old screen.
            wait_for('Choose how to connect', present=False)
            send('/setup\r')
            wait_for('Choose how to connect')
            wait_for('Ready', present=False)
            send('\r')
            wait_for('Server and credentials')
            send('\x1b')
            wait_for('Choose how to connect')
            wait_for('Server and credentials', present=False)
        send('\x11')
        assert exited() == 0
        assert KEY.encode() not in capture
        remembered = list((root / 'config/werkworker/connections').glob('*.json'))
        assert len(remembered) == 1
        assert all(KEY not in p.read_text() and p.stat().st_mode & 0o777 == 0o600 for p in remembered)
        assert '/v1/capabilities' not in discovery_requests
        assert not any(path.startswith('/v1/parameters') for path in discovery_requests)
        print('PASS: fast discovery omits global backend probes; 20 immediate chat/setup/connection transitions retain keyboard input.')
        print('PASS: remembered connection and capabilities, workspace-filtered sessions, same-session follow-up with model history, --session without run-id, interrupted approval recovery, retry/connect-new after failure; no replay or leaked credentials.')
        print('PASS: PTY setup, masked key paste, auth retry, model discovery, budgets, authenticated chat and approval, saved global/project configs (0600), project rules and permission separation, reconnect, environment reference, double Ctrl-C and terminal restoration.')
        print('PASS: automatic workspace connection and masked local credentials for bare ww and --workspace; no token in settings, terminal capture or sessions.')
    finally:
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()
        server.server_close()

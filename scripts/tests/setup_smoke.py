#!/usr/bin/env python3
"""Real PTY + authenticated Werk fixture: setup, retry, persistence and reconnect.

No live model or real credential is used. Run with a built ww binary.
"""
import fcntl
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
        if self.headers.get('Authorization') != 'Bearer ' + KEY:
            self.respond({'error': 'unauthorized'}, 401)
            return
        routes = {
            '/werk/v1/info': {'service': 'werk1112', 'protocol': {'major': 1, 'minor': 0}, 'active_backend': 'fixture'},
            '/werk/v1/capabilities': {'capabilities': []},
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

    def start():
        return subprocess.Popen([str(binary), '--workspace', str(workspace), '--store', str(root / 'state.sqlite')],
                                stdin=slave, stdout=slave, stderr=slave, env=env)

    def wait_for(token, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if token.encode() in screen.text():
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

    try:
        proc = start()
        wait_for('Choose how to connect')
        send('\r')
        wait_for('Server and credentials')
        send('\x15' + origin + '\t\twrong-key\t\r')
        wait_for('Authentication failed')
        send('\x1b[Z\x15\x1b[200~' + KEY + '\x1b[201~\t\r')
        wait_for('Model and agent settings')
        wait_for('fixture-model')
        # Model(0), context(1), output(2), profile(3), access(4), tools(5), stream(6).
        send('\t32768' + '\t' * 5 + '\x1b[C' + '\t' * 2 + '\r')
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
        assert proc.wait(timeout=10) == 0
        assert termios.tcgetattr(slave) == before
        assert KEY.encode() not in capture
        assert KEY.encode() not in (root / 'state.sqlite').read_bytes()

        # A new process reloads the same config and can use an environment reference.
        screen = Screen()
        env['WW_SETUP_FIXTURE_KEY'] = KEY
        proc = start()
        wait_for('Choose how to connect')
        send('\r')
        wait_for(origin)
        send('\t\x1b[C\t\x15WW_SETUP_FIXTURE_KEY\t\r')
        wait_for('Model and agent settings')
        wait_for('32768')
        send('\t' * 8 + '\r')
        wait_for('Ready')
        settings = json.loads(config.read_text())
        assert settings['secret_env'] == 'WW_SETUP_FIXTURE_KEY'
        assert KEY not in config.read_text()
        send('\x11')
        assert proc.wait(timeout=10) == 0
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
        proc = start()
        wait_for('Choose how to connect')
        send('\r')
        wait_for(origin)
        send('\t' * 3 + '\r')
        wait_for('Model and agent settings')
        wait_for('2048')
        # Change output, keep default read-only policy, save only project settings.
        send('\t' * 2 + '\x154096' + '\t' * 6 + '\r')
        wait_for('Ready')
        assert config.read_bytes() == global_before
        project_settings = json.loads(project_file.read_text())
        assert project_settings['output_tokens'] == 4096
        assert project_settings['test_commands']['check'] == ['never-run-this-command']
        assert 'base_url' not in project_settings and 'secret_env' not in project_settings
        send('\x11')
        assert proc.wait(timeout=10) == 0
        with sqlite3.connect(root / 'state.sqlite') as db:
            sessions = [json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')]
        assert len(sessions) == 3
        assert all(not data['policy']['workspace_writable'] for data in sessions)
        assert all(all(not g['preapproved'] and g['tool'].startswith('files.') for g in data['policy']['grants']) for data in sessions)
        assert all('fixture project instruction' in data['profile']['prompt'] for data in sessions)
        assert len(requests) == 2, 'Setup and suggested test commands must not run inference/processes'
        assert termios.tcgetattr(slave) == before
        assert KEY.encode() not in capture and KEY not in json.dumps(sessions)
        print('PASS: PTY setup, masked key paste, auth retry, model discovery, budgets, authenticated chat and approval, saved global/project configs (0600), project rules and permission separation, reconnect, environment reference, double Ctrl-C and terminal restoration.')
    finally:
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()
        server.server_close()

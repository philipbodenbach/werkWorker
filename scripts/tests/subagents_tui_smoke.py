#!/usr/bin/env python3
"""PTY regression: inspect two children and address their approvals/questions."""
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


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def respond(self, value, control=False, status=200):
        if control:
            value = {'protocol': {'major': 1, 'minor': 0}, 'request_id': 'req_fixture', 'data': value}
        raw = json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        routes = {
            '/werk/v1/info': {'service': 'werk1112', 'protocol': {'major': 1, 'minor': 0}},
            '/werk/v1/capabilities': {'capabilities': []},
            '/v1/models': {'data': [{'id': 'fixture'}]},
            '/v1/capabilities': {'models': []},
            '/v1/parameters?task=text-generation': {'parameters': []},
        }
        self.respond(routes.get(self.path, {}), control=self.path.startswith('/werk/'),
                     status=200 if self.path in routes else 404)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        messages = request['messages']
        child = any(m['role'] == 'system' and 'delegated worker' in m['content'] for m in messages)
        results = [m for m in messages if m['role'] == 'tool']
        call = None
        if not child and not results:
            text = 'Delegating two fixture tasks'
            call = ('delegate_task', {'tasks': [{'goal': 'alpha'}, {'goal': 'beta'}]})
        elif child:
            name = 'alpha' if any(m['role'] == 'user' and 'Goal: alpha' in m['content'] for m in messages) else 'beta'
            if not results:
                text = f'{name} is preparing a write'
                call = ('files.write', {'path': f'{name}.txt', 'content': name})
            elif len(results) == 1:
                text = f'{name} wrote its file'
                call = ('interaction.ask', {'question': f'{name} audience?', 'choices': ['team', 'public']})
            else:
                text = f'{name} finished for team'
        else:
            text = 'All fixture children finished'
        message = {'role': 'assistant', 'content': text}
        if call:
            message['tool_calls'] = [{'id': 'fixture_call', 'type': 'function', 'function': {'name': call[0], 'arguments': json.dumps(call[1])}}]
        self.respond({'choices': [{'message': message, 'finish_reason': 'tool_calls' if call else 'stop'}]})


binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='ww-child-ui-') as temporary:
    root = Path(temporary)
    workspace = root / 'workspace'
    workspace.mkdir()
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config = json.loads((Path(__file__).resolve().parents[2] / 'examples/werk-config.json').read_text())
    config.update(base_url=f'http://127.0.0.1:{server.server_port}', model='fixture', stream=False)
    config['capabilities'].update(context_tokens=32768, tool_calling='supported')
    (root / 'connection.json').write_text(json.dumps(config))
    # Isolate child approval/interaction UI from coding acceptance tests.
    harness = root / 'harness.json'
    harness.write_text(json.dumps({'coding_checks': False}))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 36, 130, 0, 0))
    before = termios.tcgetattr(slave)
    env = dict(os.environ, TERM='xterm-256color', WW_CHILD_FIXTURE_KEY='fixture-key', XDG_CONFIG_HOME=str(root / 'config'))
    screen = Screen()
    proc = subprocess.Popen([str(binary), '--harness-config', str(harness), '--workspace', str(workspace), '--store', str(root / 'state.sqlite'),
        '--werk-config', str(root / 'connection.json'), '--secret-env', 'WW_CHILD_FIXTURE_KEY',
        'tui', 'Run both fixture tasks'],
        stdin=slave, stdout=slave, stderr=slave, env=env)

    def send(value):
        os.write(master, value.encode())

    def wait_for(predicate, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            text = screen.text().decode()
            if predicate(text):
                return text
            if select.select([master], [], [], .1)[0]:
                screen.feed(os.read(master, 65536))
            if proc.poll() is not None:
                break
        raise AssertionError(screen.text().decode())

    def token(value):
        return wait_for(lambda text: value in text)

    try:
        token('Approval')
        send('\x1bOR')  # F3
        token('Agent details')
        token('Inspector ')
        token('alpha')
        token('beta')
        send('\x1b[B')
        token('Goal: beta')
        send('\x19')  # Hidden approvals must not be granted from the inspector.
        time.sleep(.3)
        assert not list(workspace.glob('*.txt'))
        send('\x1b')
        wait_for(lambda text: 'Inspector ' not in text)
        token('Approval')
        # Resolve whichever addressed child request the UI currently displays.
        seen = set()
        while len(seen) < 4:
            def pending(text):
                for name in ['alpha', 'beta']:
                    if f'Agent: {name}' in text:
                        kind = 'approval' if 'Approval ·' in text else 'question' if f'{name} audience?' in text else None
                        if kind and (name, kind) not in seen:
                            return True
                return False
            text = wait_for(pending)
            name = 'alpha' if 'Agent: alpha' in text else 'beta'
            if 'Approval ·' in text:
                seen.add((name, 'approval'))
                send('ja\r')
            else:
                seen.add((name, 'question'))
                send('/permissions\r')
                token('Host grants:')
                token('Inspector ')
                send('\x1b')
                wait_for(lambda text: 'Inspector ' not in text)
                token(f'{name} audience?')
                send('team\r')
        token('[Completed]')
        send('/agents\r')
        token('Agent details')
        send('\x1b[A')
        token('alpha finished for team')
        send('\x1b[B')
        token('beta finished for team')
        assert (workspace / 'alpha.txt').read_text() == 'alpha'
        assert (workspace / 'beta.txt').read_text() == 'beta'
        send('\x1b')
        wait_for(lambda text: 'Inspector ' not in text)
        send('/sessions\r')
        token('New / Previous sessions')
        with sqlite3.connect(root / 'state.sqlite') as db:
            saved = [json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')]
        parent = next(data for data in saved if not data.get('parent'))
        token(parent['id'])
        assert all(data['id'].encode() not in screen.text() for data in saved if data.get('parent'))
        send('\x1b[B\r')
        token('Opened saved session')
        send('/agents\r')
        token('Agent details')
        token('alpha finished for team')
        send('\x1b[B')
        token('beta finished for team')
        send('\x11')
        assert proc.wait(timeout=10) == 0
        assert termios.tcgetattr(slave) == before
        with sqlite3.connect(root / 'state.sqlite') as db:
            children = [json.loads(row[0]) for row in db.execute('SELECT data FROM sessions') if json.loads(row[0]).get('parent')]
        assert len(children) == 2
        for child in children:
            assert child['policy']['request_approvals']
            assert child['policy']['grants'] == []
            run = child['runs'][0]
            assert run['status'] == 'completed'
            assert [i['approval']['decision'] for i in run['invocations'] if i.get('approval')] == ['allow_once']
            assert [i['interaction']['answer'] for i in run['invocations'] if i.get('interaction')] == ['team']
        print('PASS: default all-tool availability, inherited child approval requests, typed ja executes real child writes, inspector, completed results and terminal restoration.')
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()
        server.server_close()

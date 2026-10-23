#!/usr/bin/env python3
"""PTY regression for direct, protocol-backed reasoning controls and persistence."""
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
from urllib.parse import parse_qs, urlparse
from tui_support import Screen


chats, counts = [], []
generation_started, release_generation = threading.Event(), threading.Event()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, data, control=False, status=200):
        if control:
            data = {'protocol': {'major': 1, 'minor': 0}, 'request_id': 'fixture', 'data': data}
        raw = json.dumps(data).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        url = urlparse(self.path)
        if url.path == '/werk/v1/chat/reasoning':
            output = int(parse_qs(url.query)['max_output_tokens'][0])
            self.reply({'schema_version': 1, 'model': 'fixture', 'supported': True,
                        'default_tier': 'low', 'default_adjusted_to_limits': False,
                        'limits': {'context_tokens': 32768, 'output_tokens': output,
                                   'reasoning_tokens': output - 512, 'answer_reserve_tokens': 512},
                        'tiers': [{'id': name, 'label': label, 'reasoning_tokens': tokens,
                                   'minimum_output_tokens': tokens + 512, 'available': tokens + 512 <= output}
                                  for name, label, tokens in [('low', 'Low', 512), ('high', 'High', 2048),
                                                              ('ultra', 'Ultra', 8192), ('custom-v2', 'Custom V2', 1024)]]}, True)
            return
        routes = {
            '/werk/v1/info': {'service': 'werk1112', 'protocol': {'major': 1, 'minor': 0}},
            '/werk/v1/capabilities': {'capabilities': [
                {'id': 'api.chat.reasoning', 'status': 'supported', 'detail': 'fixture', 'operations': ['profile', 'tiers']},
                {'id': 'api.chat.token_count', 'status': 'supported', 'detail': 'fixture', 'operations': ['count_tokens']}]},
            '/v1/models': {'data': [{'id': 'fixture'}]},
            '/v1/capabilities': {'models': []},
            '/v1/parameters?task=text-generation': {'parameters': []},
        }
        self.reply(routes.get(self.path, {}), self.path.startswith('/werk/'), 200 if self.path in routes else 404)

    def do_POST(self):
        data = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if self.path.endswith('/count_tokens'):
            counts.append(data)
            self.reply({'model': 'fixture', 'input_tokens': 64}, True)
        else:
            chats.append(data)
            if len(chats) == 5:
                generation_started.set()
                assert release_generation.wait(15), 'Fixture generation was not released'
            self.reply({'choices': [{'message': {'role': 'assistant', 'content': f'fixture response {len(chats)}'}, 'finish_reason': 'stop'}]})


binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='ww-effort-ui-') as temporary:
    root = Path(temporary)
    workspace = root / 'workspace'
    workspace.mkdir()
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config = json.loads((Path(__file__).resolve().parents[2] / 'examples/werk-config.json').read_text())
    config.update(base_url=f'http://127.0.0.1:{server.server_port}', model='fixture', stream=False)
    config['options'] = {}
    config['capabilities'].update(context_tokens=32768, tool_calling='supported')
    (root / 'connection.json').write_text(json.dumps(config))
    (root / 'limits.json').write_text(json.dumps({'reserve_output_tokens': 4096, 'continuous': True}))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 36, 140, 0, 0))
    before = termios.tcgetattr(slave)
    screen = Screen()
    proc = subprocess.Popen([str(binary), '--workspace', str(workspace), '--store', str(root / 'state.sqlite'),
                             '--werk-config', str(root / 'connection.json'), '--secret-env', 'WW_EFFORT_FIXTURE_KEY',
                             '--limits', str(root / 'limits.json'), 'tui', 'first fixture task'],
                            stdin=slave, stdout=slave, stderr=slave,
                            env=dict(os.environ, TERM='xterm-256color', XDG_CONFIG_HOME=str(root / 'config'), WW_EFFORT_FIXTURE_KEY='fixture-key'))

    def send(text):
        os.write(master, text.encode())

    def wait_for(text):
        until = time.monotonic() + 15
        while time.monotonic() < until:
            if text.encode() in screen.text():
                return
            if select.select([master], [], [], .1)[0]:
                screen.feed(os.read(master, 65536))
            if proc.poll() is not None:
                break
        raise AssertionError((text, screen.text().decode(errors='replace')[-5000:]))

    def submit_task(number, tier):
        send(f'fixture task {number}\r')
        wait_for(f'fixture response {number}')
        assert chats[-1]['werk']['reasoning']['tier'] == tier, chats[-1]
        assert 'reasoning_effort' not in chats[-1]
        assert any(c['messages'] == chats[-1]['messages'] and c['werk'] == chats[-1]['werk'] for c in counts)

    try:
        wait_for('fixture response 1')
        assert chats[-1]['werk']['reasoning']['tier'] == 'default'
        send('/')
        wait_for('/effort')
        wait_for('/reasoning')
        send('\x15/effort\r')
        wait_for('Reasoning effort · WerkProtocol')
        wait_for('Current: Server default: Low')
        wait_for('Custom V2')
        wait_for('←/→ select')
        assert b'Ultra' not in screen.text(), 'Unavailable tiers must not be slider stops'
        assert b'fixture response 1' in screen.text(), 'Mini picker must keep the conversation visible'
        send('\x1b[C\x1b[C\x1b[C')
        wait_for('custom-v2 · 1024 reasoning tokens')
        send('\x1b[D\x1b')
        wait_for('Message · Enter send')
        assert chats[-1]['werk']['reasoning']['tier'] == 'default'
        send('/effort\r')
        wait_for('Current: Server default: Low')
        wait_for('←/→ select')
        send('\x1b[C\x1b[C\r')
        wait_for('Reasoning effort set to High')
        submit_task(2, 'high')
        send('/reasoning custom-v2\r')
        wait_for('Reasoning effort set to Custom V2')
        submit_task(3, 'custom-v2')
        send('/effort ultra\r')
        wait_for("Reasoning tier 'ultra' is unavailable")
        send('/effort imaginary\r')
        wait_for("Reasoning tier 'imaginary' is unavailable")
        send('/reasoning default\r')
        wait_for('Reasoning effort set to Server default: Low')
        submit_task(4, 'default')
        send('hold this generation\r')
        assert generation_started.wait(10)
        send('and put it all in one file\r')
        wait_for('steering · Queued for the next model request')
        release_generation.set()
        wait_for('steering · Included in this model request')
        wait_for('fixture response 6')
        assert any(m['role'] == 'user' and m['content'] == 'and put it all in one file' for m in chats[-1]['messages'])
        with sqlite3.connect(root / 'state.sqlite') as db:
            saved = [json.loads(row[0]) for row in db.execute('select data from sessions')]
        assert len(saved) == 1, 'Changing effort must retain the same session'
        assert saved[0]['metadata']['_werkworker_reasoning']['tier'] == 'default'
        assert all(not m['content'].startswith(('/effort', '/reasoning')) for m in saved[0]['messages'] if m['role'] == 'user')
        send('/quit\r')
        proc.wait(timeout=10)
        assert proc.returncode == 0
        assert termios.tcgetattr(slave) == before
        print('PASS: compact effort slider, arrows/cancel/apply, protocol tiers, request/count payloads, persistence, steering delivery and terminal restoration.')
    finally:
        release_generation.set()
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()

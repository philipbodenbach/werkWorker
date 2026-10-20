#!/usr/bin/env python3
"""Real PTY test: streaming, addressed approval, answer, cancellation and restore."""
import fcntl
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
import time

from tui_support import Screen

binary = Path(sys.argv[1]).resolve()
def turn(name=None, args=None, text=''):
    deltas = [{'type':'text','text':text}]
    if name: deltas.append({'type':'tool','index':0,'id':'call','name':name,'arguments':json.dumps(args)})
    deltas += [{'type':'finish','reason':'tool_calls' if name else 'stop'},{'type':'done'}]
    return {'deltas':deltas,'error':None}
with tempfile.TemporaryDirectory(prefix='ww-tui-test-') as temp:
    root=Path(temp); workspace=root/'workspace'; workspace.mkdir()
    script=root/'script.json'; script.write_text(json.dumps([
        turn('files.write',{'path':'approved.txt','content':'approved'},'Prepared write'),
        turn('interaction.ask',{'question':'Which audience?','choices':['team','public']}),
        turn('interaction.ask',{'question':'Choose visibility','choices':['private','public']}),
        turn(text='\n'.join(f'History row {i:03}' for i in range(80))
             + '\n```rust\nfn main() {\n    let message = "hello"; // checked code\n}\n```\nFinished for the team'),
        {'deltas': [], 'error': {'code': 'fixture_failure', 'message': 'Fixture model request failed.',
                                'retryable': False, 'retry_after_ms': None}},
        turn(text='Follow-up recovered in the same session'),
        dict(turn(text='slow response'),delay_ms=10_000),
    ]))
    grants=root/'grants.json'; grants.write_text(json.dumps({'mode':'ask','workspace_writable':True,'grants':[{'tool':'files.write','version':'1','resource':'.','arguments_hash':None,'expires_at_ms':None,'preapproved':False}]}))
    master,slave=pty.openpty(); fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',36,130,0,0))
    before=termios.tcgetattr(slave)
    terminal_env=dict(os.environ, TERM='xterm-256color', COLORTERM='truecolor', XDG_CONFIG_HOME=str(root/'config'))
    terminal_env.pop('NO_COLOR', None)
    # Isolate clipboard tests from the developer's real desktop clipboard.
    helpers=root/'helpers'; helpers.mkdir()
    clipboard=root/'clipboard'; clipboard.write_text('Grüße 世界\nsecond line\n')
    reads=root/'clipboard-reads'
    for names, body in [
        (['wl-copy', 'pbcopy'], "Path(os.environ['TEST_CLIPBOARD']).write_bytes(sys.stdin.buffer.read())"),
        (['wl-paste', 'pbpaste'], "p=Path(os.environ['TEST_CLIPBOARD_READS']); p.write_text(p.read_text()+'r' if p.exists() else 'r'); sys.stdout.buffer.write(Path(os.environ['TEST_CLIPBOARD']).read_bytes())"),
    ]:
        for name in names:
            helper=helpers/name
            helper.write_text(f'#!{sys.executable}\nimport os, sys\nfrom pathlib import Path\n{body}\n')
            helper.chmod(0o755)
    for name in ['WSL_DISTRO_NAME', 'WSL_INTEROP', 'DISPLAY']:
        terminal_env.pop(name, None)
    terminal_env.update(PATH=str(helpers)+os.pathsep+os.environ['PATH'],
                        WAYLAND_DISPLAY='ww-test', TEST_CLIPBOARD=str(clipboard),
                        TEST_CLIPBOARD_READS=str(reads))
    proc=subprocess.Popen([str(binary),'--workspace',str(workspace),'--store',str(root/'state.sqlite'),'--script',str(script),'--grants',str(grants),'tui','initial task','--profile','coding-basic'],stdin=slave,stdout=slave,stderr=slave,env=terminal_env)
    capture=bytearray(); screen=Screen()
    def wait_for(token,timeout=15,present=True):
        deadline=time.monotonic()+timeout
        while time.monotonic()<deadline:
            if (token in screen.text()) == present: return
            if select.select([master],[],[],0.1)[0]:
                data=os.read(master,65536); capture.extend(data); screen.feed(data)
            if proc.poll() is not None: break
        raise AssertionError((token,screen.text()[-2000:]))
    try:
        wait_for(b'Approval')
        wait_for(b'Compact')
        wait_for(b'Prepared write')
        assert b' Session ' not in screen.text()
        if os.environ.get('WERK_WORKER_TUI_CAPTURE'):
            Path(os.environ['WERK_WORKER_TUI_CAPTURE']).write_bytes(screen.text())
        assert not (workspace/'approved.txt').exists()
        assert b'\x1b[?2004h' in capture, 'Bracketed paste was not enabled'
        os.write(master,b'\x1b[200~ja\r\n/quit\r\n\x1b[201~')
        wait_for(b'/quit')
        time.sleep(.2)
        assert proc.poll() is None and not (workspace/'approved.txt').exists()
        os.write(master,b'\x15')  # Pasted approval/command text must stay in the draft.
        os.write(master,b'/permissions\r')
        wait_for(b'Host grants:')
        wait_for(b'Inspector ')
        wait_for(b'files.write')
        os.write(master,b'\x19')  # Inspector cannot approve a hidden request.
        time.sleep(.3)
        assert not (workspace/'approved.txt').exists()
        os.write(master,b'\r')
        wait_for(b'Enable tool approval requests in this session?')
        wait_for('› Keep current permissions'.encode())
        os.write(master,b'\r')  # Default is to retain the existing scope.
        wait_for(b'Inspector ',present=False)
        assert not (workspace/'approved.txt').exists()
        os.write(master,b'/permissions\r')
        wait_for(b'Host grants:')
        os.write(master,b'\r')
        wait_for(b'Enable tool approval requests in this session?')
        os.write(master,b'\x1b[A\r')  # Explicitly enable requests, not execution.
        wait_for(b'Permissions updated:')
        wait_for(b'Inspector ',present=False)
        wait_for(b'Approval')
        assert not (workspace/'approved.txt').exists()
        os.write(master,b'ja\r')  # The typed approval must execute the prepared write.
        wait_for(b'Which audience?')
        assert (workspace/'approved.txt').read_text()=='approved'
        os.write(master,b'/agents\r')  # Commands also work while answering a child/root question.
        wait_for(b'No subagents in this run yet.')
        wait_for(b'Inspector ')
        os.write(master,b'\x1b')
        wait_for(b'Inspector ',present=False)
        wait_for(b'Which audience?')
        os.write(master,b'\x1b[B\x1b[B\r')  # Select Write my own answer.
        wait_for(b'Your answer \xc2\xb7 type below')
        os.write(master,b'te\x1bOS')  # F4 opens the layout menu while preserving a draft answer.
        wait_for(b'Choose your layout')
        wait_for(b'Inspector ')
        os.write(master,b'\x1b[B\r')
        wait_for(b'Inspector ',present=False)
        wait_for(b' Session ')
        wait_for(b'__        __')
        wait_for('› te'.encode())
        assert json.loads((root/'config/werkworker/ui.json').read_text())['layout']=='cockpit'
        os.write(master,b'am\r')
        wait_for(b'Choose visibility')
        wait_for(b'Write my own answer')
        os.write(master,b'\x1b[B\r')  # Choose public with arrows and Enter.
        wait_for(b'Finished for the team')
        wait_for(b'Completed')
        os.write(master,b'\x16')  # Ctrl-V reads the host clipboard, preserving Unicode/newlines.
        wait_for('Grüße'.encode())
        wait_for('世'.encode())  # The minimal screen emulator retains wide-cell placeholders.
        wait_for('界'.encode())
        wait_for(b'second line')
        assert reads.read_text()=='r'
        os.write(master,b'\x15')
        wait_for(b'second line',present=False)
        # Select only the visible answer, copy with Ctrl-C, and keep the session alive.
        text='Finished for the team'
        y=next(i for i,row in enumerate(screen.rows) if text in ''.join(row))
        x=''.join(screen.rows[y]).index(text)
        os.write(master, f'\x1b[<0;{x+1};{y+1}M\x1b[<32;{x+len(text)};{y+1}M\x1b[<0;{x+len(text)};{y+1}m'.encode())
        time.sleep(.1)
        os.write(master,b'\x03')
        deadline=time.monotonic()+5
        while clipboard.read_text()!=text and time.monotonic()<deadline:
            time.sleep(.05)
        assert clipboard.read_text()==text, clipboard.read_text()
        assert proc.poll() is None
        assert b'Press Ctrl-C again' not in screen.text()
        # A single right click pastes; releasing the button must not paste again.
        os.write(master,b'\x1b[<2;10;32M\x1b[<2;10;32m')
        wait_for(('› '+text).encode())
        time.sleep(.2)
        assert reads.read_text()=='rr', reads.read_text()
        os.write(master,b'\x15')
        wait_for(('› '+text).encode(),present=False)
        # A single right click also copies a selection, without pasting it.
        clipboard.write_text('copy sentinel')
        y=next(i for i,row in enumerate(screen.rows) if text in ''.join(row))
        x=''.join(screen.rows[y]).index(text)
        os.write(master, f'\x1b[<0;{x+1};{y+1}M\x1b[<32;{x+len(text)};{y+1}M\x1b[<0;{x+len(text)};{y+1}m\x1b[<2;10;32M\x1b[<2;10;32m'.encode())
        deadline=time.monotonic()+5
        while clipboard.read_text()!=text and time.monotonic()<deadline:
            time.sleep(.05)
        assert clipboard.read_text()==text
        assert reads.read_text()=='rr', 'Right-click copied and pasted in one action'
        os.write(master,b'\x1b[17~')  # F6 restores native terminal selection.
        wait_for(b'Terminal selection:')
        assert b'\x1b[?1006l' in capture
        os.write(master,b'\x1b[17~')
        wait_for(b'Terminal selection:',present=False)
        assert b'48;2;29;32;30' in capture, 'Code block background was not rendered'
        assert b'\x1b[?1006h' in capture, 'Mouse capture was not enabled'
        os.write(master,b'\x1b[H')  # Home: oldest history.
        wait_for(b'you')
        wait_for(b'initial task')
        wait_for(b'End: latest')
        os.write(master,b'\x1b[6~')  # Page Down.
        wait_for(b'History row')
        os.write(master,b'\x1b[F')  # End: follow the latest output.
        wait_for(b'Finished for the team')
        wait_for(b'End: latest',present=False)
        os.write(master,b'\x1b[<64;10;15M')  # SGR mouse wheel over the chat.
        wait_for(b'End: latest')
        os.write(master,b'\x1b[F')
        wait_for(b'End: latest',present=False)
        os.write(master,b'trigger failure\r')
        wait_for(b'Fixture model request failed.')
        wait_for(b'The session is still open.')
        assert proc.poll() is None
        os.write(master,b'continue after failure\r')
        wait_for(b'Follow-up recovered in the same session')
        wait_for(b'Completed')
        os.write(master,b'/')
        wait_for(b'Commands ')
        wait_for(b'/clear')
        os.write(master,b'\x1b[B\t')  # Select /clear and complete it.
        wait_for('› /clear'.encode())
        os.write(master,b'\r')
        wait_for(b'Finished for the team',present=False)
        # Help also works without involving the model or consuming a scripted turn.
        os.write(master,b'/he\t\r')
        wait_for(b'Alt-Up/Down scroll request details')
        os.write(master,b'\x1b[H')  # The expanded help spans more than one viewport.
        wait_for(b'/help: Show keyboard shortcuts')
        # PTY reads can split a frame after the help heading but before old
        # conversation rows have been erased. Wait for the actual clear too.
        wait_for(b'Finished for the team',present=False)
        assert b'Finished for the team' not in screen.text(), screen.text().decode()
        os.write(master,b'\x03')
        wait_for(b'Press Ctrl-C again to quit')
        assert proc.poll() is None
        colors_seen = b'38;2;0;220;255' in capture
        # Typing resets the first Ctrl-C; the next Ctrl-C must only cancel.
        capture.clear(); os.write(master,b'next task\r'); wait_for(b'Running')
        os.write(master,b'\x03'); wait_for(b'Cancelled')
        assert proc.poll() is None
        os.write(master,b'\x03'); assert proc.wait(timeout=10)==0
        while select.select([master],[],[],0)[0]:
            data=os.read(master,65536)
            if not data: break
            capture.extend(data); screen.feed(data)
        assert termios.tcgetattr(slave)==before, 'Terminal raw mode was not restored'
        assert colors_seen
        assert b'\x1b[?1006l' in capture, 'Mouse capture was not restored'
        assert b'\x1b[?2004l' in capture, 'Bracketed paste was not restored'
        with sqlite3.connect(root/'state.sqlite') as db:
            sessions=[json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')]
        answers=[i['interaction']['answer'] for s in sessions for r in s['runs'] for i in r['invocations'] if i.get('interaction')]
        assert answers==['team','public'],answers
        assert len(sessions)==1, 'Follow-up created a different session'
        assert sessions[0]['policy']['request_approvals']
        assert 'http.fetch' in sessions[0]['profile']['tools']
        assert [r['status'] for r in sessions[0]['runs']]==['completed','failed','completed','cancelled']
        screen=Screen()
        proc=subprocess.Popen([str(binary),'--workspace',str(workspace),'--store',str(root/'restart.sqlite'),'tui','--demo'],stdin=slave,stdout=slave,stderr=slave,env=terminal_env)
        wait_for(b' Session ')
        wait_for(b'__        __')
        os.write(master,b'/view\r')
        wait_for(b'Choose your layout')
        wait_for(b'Inspector ')
        os.write(master,b'\x1b[A\r')
        wait_for(b'Inspector ',present=False)
        wait_for(b'Compact')
        wait_for(b' Session ',present=False)
        os.write(master,b'\x1b[15~')  # F5 edits profile/access/tools/skills in the current session.
        wait_for(b'Draft profile: general-readonly')
        os.write(master,b'\x1b[Z')  # Settings opens on Permissions; Shift-Tab selects Profiles.
        os.write(master,b'\x1b[B ')
        wait_for(b'Draft profile: coding-basic')
        os.write(master,b'\t'+b'\x1b[B'*6+b' ')
        wait_for(b'Ask before each action; workspace writes enabled')
        os.write(master,b'\t')
        wait_for('› [x] agents.cancel'.encode())
        os.write(master,b' ')
        wait_for('› [ ] agents.cancel'.encode())
        os.write(master,b'\t')
        wait_for('› [x] coding-basic'.encode())
        os.write(master,b' ')
        wait_for('› [ ] coding-basic'.encode())
        os.write(master,b'\x13')  # Ctrl-S saves both the current session and project defaults.
        wait_for(b'Saved: current session and project defaults.')
        config=json.loads((workspace/'.werkworker/config.json').read_text())
        assert config['profile']=='coding-basic'
        assert 'agents.cancel' not in config['tools']
        assert config['skills']==['task-execution']
        access_files=list((root/'config/werkworker/workspace-access').glob('*.json'))
        assert len(access_files)==1
        assert json.loads(access_files[0].read_text())['policy']['request_approvals']
        os.write(master,b'\x1b')
        wait_for(b'Inspector ',present=False)
        os.write(master,b'/sessions\r')
        wait_for(b'New / Previous sessions')
        os.write(master,b'\r')
        wait_for(b'New / Previous sessions',present=False)
        wait_for(b'Compact')
        with sqlite3.connect(root/'restart.sqlite') as db:
            saved=[json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')]
        assert len(saved)==2, len(saved)
        for state in saved:
            assert state['profile']['name']=='coding-basic'
            assert 'agents.cancel' not in state['profile']['tools']
            assert state['profile']['skills']==['task-execution']
            assert state['policy']['request_approvals']
        os.write(master,b'\x11')
        assert proc.wait(timeout=10)==0
        assert termios.tcgetattr(slave)==before
        screen=Screen()
        proc=subprocess.Popen([str(binary),'--workspace',str(workspace),'--store',str(root/'defaults.sqlite'),'tui','--demo'],stdin=slave,stdout=slave,stderr=slave,env=terminal_env)
        wait_for(b'Compact')
        os.write(master,b'\x1bOQ')  # F2 verifies persisted defaults after restarting the process.
        wait_for(b'Profile: coding-basic')
        with sqlite3.connect(root/'defaults.sqlite') as db:
            state=json.loads(db.execute('SELECT data FROM sessions').fetchone()[0])
        assert state['profile']['skills']==['task-execution']
        assert 'agents.cancel' not in state['profile']['tools']
        assert state['policy']['request_approvals']
        os.write(master,b'\x11')
        assert proc.wait(timeout=10)==0
        assert termios.tcgetattr(slave)==before
        print('PTY: clipboard shortcuts, single right-click copy/paste, bracketed paste, settings and project defaults across new sessions/restart, approvals, answers, layouts, scrolling, cancellation and terminal restoration passed.')
    finally:
        if proc.poll() is None: proc.kill(); proc.wait()
        os.close(master); os.close(slave)

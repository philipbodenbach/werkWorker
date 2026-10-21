#!/usr/bin/env python3
"""Real PTY session management: rename, filter, cancel/delete and reopen."""
import fcntl, json, os, pty, select, sqlite3, struct, subprocess, sys, tempfile, termios, time
from pathlib import Path
from tui_support import Screen

binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='ww-sessions-') as tmp:
    root = Path(tmp)
    workspace = root / 'workspace'
    workspace.mkdir()
    sentinel = workspace / 'keep.txt'
    sentinel.write_text('Keep workspace files')
    store = root / 'sessions.sqlite'
    script = root / 'script.json'
    script.write_text(json.dumps([{'deltas':[{'type':'text','text':'Saved response'}, {'type':'finish','reason':'stop'}, {'type':'done'}], 'error':None}]))
    args = [str(binary), '--workspace', str(workspace), '--store', str(store), '--script', str(script)]
    env = dict(os.environ, TERM='xterm-256color', XDG_CONFIG_HOME=str(root/'config'))
    for prompt in ['Older conversation', 'Latest conversation']:
        subprocess.run(args + ['run', prompt], env=env, check=True, capture_output=True)
    def sessions():
        with sqlite3.connect(store) as db:
            return [json.loads(row[0]) for row in db.execute('SELECT data FROM sessions')]
    original = sessions()
    latest_id = next(s['id'] for s in original if s['runs'][0]['input'] == 'Latest conversation')
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH',36,130,0,0))
    before = termios.tcgetattr(slave)
    proc = subprocess.Popen(args + ['tui'], stdin=slave, stdout=slave, stderr=slave, env=env)
    screen = Screen()
    def wait(token, present=True, timeout=10):
        token = token.encode()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if (token in screen.text()) == present: return
            if select.select([master],[],[],.02)[0]: screen.feed(os.read(master,65536))
            if proc.poll() is not None: break
        raise AssertionError((token,screen.text().decode()))
    try:
        wait('Session management')
        wait('Latest conversation')
        os.write(master,b'\x1b[Br')
        wait('Rename session')
        os.write(master,b'\x15Renamed reference\r')
        wait('Session renamed.')
        assert next(s for s in sessions() if s['id']==latest_id)['title'] == 'Renamed reference'
        os.write(master,b'/Renamed\r')
        wait('1 shown')
        wait('Older conversation',False)
        os.write(master,b'\x1b[Bd')
        wait('Delete permanently?')
        os.write(master,b'\r')
        wait('Deletion cancelled.')
        assert len(sessions())==2
        os.write(master,b'd')
        wait('Delete permanently?')
        os.write(master,b'\x1b[B\r')
        wait('Deleted conversation')
        wait('0 shown')
        assert len(sessions())==1 and all(s['id']!=latest_id for s in sessions())
        os.write(master,b'\x1b')
        wait('Older conversation')
        os.write(master,b'\x1b[B\r')
        wait('Opened saved session')
        os.write(master,b'/sessions\r')
        wait('Session management')
        os.write(master,b'\x1b[Br')
        wait('Rename session')
        os.write(master,b'\x15Survivor\r')
        wait('Session renamed.')
        os.write(master,b'\x1b')
        wait('Opened saved session')
        # Delete the currently open conversation, then start a new one. An old
        # handle must never bring the deleted history back on Esc or restart.
        os.write(master,b'/sessions\r')
        wait('Survivor')
        os.write(master,b'\x1b[Bd')
        wait('Delete permanently?')
        os.write(master,b'\x1b[B\r')
        wait('0 shown')
        assert sessions()==[]
        os.write(master,b'\r')
        wait('Message')
        os.write(master,b'New history\r')
        wait('Saved response')
        wait('Completed')
        os.write(master,b'\x11')
        proc.wait(timeout=5)
        assert termios.tcgetattr(slave)==before
        rows=sessions()
        assert len(rows)==1 and rows[0]['runs'][0]['input']=='New history'
        assert sentinel.read_text()=='Keep workspace files'
        screen=Screen()
        proc=subprocess.Popen(args+['tui'],stdin=slave,stdout=slave,stderr=slave,env=env)
        wait('New history')
        assert b'Survivor' not in screen.text() and b'Renamed reference' not in screen.text()
        os.write(master,b'\x1b')
        proc.wait(timeout=5)
        print('PTY: rename, search, default cancel, delete, current-session deletion, new session, restart and workspace preservation passed.')
    finally:
        if proc.poll() is None:
            proc.terminate()
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired: proc.kill();proc.wait()
        os.close(master);os.close(slave)

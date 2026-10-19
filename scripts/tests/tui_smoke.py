#!/usr/bin/env python3
"""Real PTY test: streaming, addressed approval, answer, cancellation and restore."""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
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
        turn(text='Finished for the team'),
        dict(turn(text='slow response'),delay_ms=10_000),
    ]))
    grants=root/'grants.json'; grants.write_text(json.dumps({'mode':'ask','workspace_writable':True,'grants':[{'tool':'files.write','version':'1','resource':'.','arguments_hash':None,'expires_at_ms':None,'preapproved':False}]}))
    master,slave=pty.openpty(); fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',36,130,0,0))
    before=termios.tcgetattr(slave)
    terminal_env=dict(os.environ, TERM='xterm-256color', COLORTERM='truecolor')
    terminal_env.pop('NO_COLOR', None)
    proc=subprocess.Popen([str(binary),'--workspace',str(workspace),'--store',str(root/'state.sqlite'),'--script',str(script),'--grants',str(grants),'tui','initial task','--profile','coding-basic'],stdin=slave,stdout=slave,stderr=slave,env=terminal_env)
    capture=bytearray(); screen=Screen()
    def wait_for(token,timeout=15):
        deadline=time.monotonic()+timeout
        while time.monotonic()<deadline:
            if token in screen.text(): return
            if select.select([master],[],[],0.1)[0]:
                data=os.read(master,65536); capture.extend(data); screen.feed(data)
            if proc.poll() is not None: break
        raise AssertionError((token,screen.text()[-2000:]))
    try:
        wait_for(b'Approval')
        assert b'werkWorker' in screen.text() and b'__        __' in screen.text()
        if os.environ.get('WERK_WORKER_TUI_CAPTURE'):
            Path(os.environ['WERK_WORKER_TUI_CAPTURE']).write_bytes(screen.text())
        assert not (workspace/'approved.txt').exists()
        os.write(master,b'\x19')  # Ctrl-Y explicitly approves the prepared write.
        wait_for(b'Which audience?')
        assert (workspace/'approved.txt').read_text()=='approved'
        os.write(master,b'team\r')
        wait_for(b'Finished for the team')
        wait_for(b'Completed')
        os.write(master,b'/')
        wait_for(b'Commands ')
        wait_for(b'/clear')
        os.write(master,b'\x1b[B\t')  # Select /clear and complete it.
        wait_for('› /clear'.encode())
        os.write(master,b'\r')
        # Help also works without involving the model or consuming a scripted turn.
        os.write(master,b'/he\t\r')
        wait_for(b'/help: Show keyboard shortcuts')
        assert b'Finished for the team' not in screen.text()
        os.write(master,b'\x03')
        wait_for(b'Press Ctrl-C again to quit')
        assert proc.poll() is None
        colors_seen = b'38;2;0;220;255' in capture
        # Typing resets the first Ctrl-C; the next Ctrl-C must only cancel.
        capture.clear(); os.write(master,b'next task\r'); wait_for(b'Running')
        os.write(master,b'\x03'); wait_for(b'Cancelled')
        assert proc.poll() is None
        os.write(master,b'\x03'); assert proc.wait(timeout=10)==0
        assert termios.tcgetattr(slave)==before, 'Terminal raw mode was not restored'
        assert colors_seen
        print('PTY: approval, answer, slash preview/completion/commands, Ctrl-C reset, double Ctrl-C exit and terminal restoration passed.')
    finally:
        if proc.poll() is None: proc.kill(); proc.wait()
        os.close(master); os.close(slave)

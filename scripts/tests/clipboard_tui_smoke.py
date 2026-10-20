#!/usr/bin/env python3
"""Actual PTY: burst typing, nonblocking clipboard, PNG attachment round-trip."""
import base64,fcntl,json,os,pty,select,sqlite3,struct,subprocess,sys,tempfile,termios,threading,time,zlib
from pathlib import Path
from tui_support import Screen
binary=Path(sys.argv[1]).resolve()
typing_only='--typing-only' in sys.argv
with tempfile.TemporaryDirectory(prefix='ww-clipboard-test-') as tmp:
 root=Path(tmp);workspace=root/'workspace';workspace.mkdir();helpers=root/'helpers';helpers.mkdir()
 def chunk(kind,data):return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data)&0xffffffff)
 png=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',2,2,8,2,0,0,0))+chunk(b'IDAT',zlib.compress(b'\0'+b'\xff\0\0'*2+b'\0'+b'\0\xff\0'*2))+chunk(b'IEND',b'')
 (root/'clipboard').write_bytes(b' clipboard text')
 (root/'delay').write_text('0')
 helper=helpers/'wl-paste';helper.write_text(f'#!{sys.executable}\nimport time,os,sys\nfrom pathlib import Path\nr=Path(os.environ["CLIP_TEST_ROOT"])\ntime.sleep(float((r/"delay").read_text()))\nsys.stdout.buffer.write((r/"clipboard").read_bytes())\n');helper.chmod(0o755)
 script=root/'turns.json';script.write_text(json.dumps([{'deltas':[{'type':'text','text':text},{'type':'finish','reason':'stop'},{'type':'done'}],'error':None} for text in ['Image received by model']]))
 master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',36,130,0,0))
 env=dict(os.environ,TERM='xterm-256color',WAYLAND_DISPLAY='test',CLIP_TEST_ROOT=str(root),XDG_CONFIG_HOME=str(root/'config'),PATH=str(helpers)+os.pathsep+os.environ['PATH'])
 for key in ['WSL_DISTRO_NAME','WSL_INTEROP','DISPLAY']:env.pop(key,None)
 proc=subprocess.Popen([str(binary),'--workspace',str(workspace),'--store',str(root/'state.sqlite'),'--script',str(script),'tui','--profile','general-readonly'],stdin=slave,stdout=slave,stderr=slave,env=env)
 screen=Screen()
 def wait(token,timeout=10,present=True):
  deadline=time.monotonic()+timeout
  while time.monotonic()<deadline:
   if (token in screen.text())==present:return
   if select.select([master],[],[],.02)[0]:
    screen.feed(os.read(master,65536))
   if proc.poll() is not None:break
  raise AssertionError((token,screen.text().decode()))
 try:
  wait(b'Message')
  payload=b'x'*32000+b'END_OF_BURST'
  def write_all():
   rest=memoryview(payload)
   while rest:rest=rest[os.write(master,rest):]
  start=time.monotonic();writer=threading.Thread(target=write_all,daemon=True);writer.start()
  wait(b'END_OF_BURST',4);elapsed=time.monotonic()-start
  print(f'32 KiB unbracketed typing visible in {elapsed:.3f}s',flush=True)
  writer.join(timeout=1)
  if typing_only:
   os.write(master,b'\x15');wait(b'END_OF_BURST',present=False)
  else:
   os.write(master,b'\x15');wait(b'END_OF_BURST',present=False)
   start=time.monotonic();os.write(master,b'\x1b[200~'+b'y'*32000+b'\x1b[201~');wait(b'[Pasted text #1',1)
   print(f'32 KiB bracketed paste visible in {time.monotonic()-start:.3f}s',flush=True)
   os.write(master,b'\x15');wait(b'[Pasted text #1',present=False)
   (root/'delay').write_text('1.5');os.write(master,b'\x16');wait(b'Reading clipboard')
   start=time.monotonic();os.write(master,b'during');wait(b'during',.7)
   print(f'Typing during slow clipboard visible in {time.monotonic()-start:.3f}s',flush=True)
   os.write(master,b'\r');wait(b'wait before sending',.7)
   wait(b'during clipboard text');os.write(master,b'\x15');wait(b'during clipboard text',present=False)
   (root/'delay').write_text('0');(root/'clipboard').write_bytes(png)
   os.write(master,b'Describe \x16');wait(b'[Image #2')
   os.write(master,b'\x7f');wait(b'[Image #2',present=False)
   os.write(master,b'\x16');wait(b'[Image #3');os.write(master,b'\r');wait(b'Image received by model')
   db=sqlite3.connect(root/'state.sqlite');state=json.loads(db.execute('SELECT data FROM sessions').fetchone()[0]);db.close()
   user=next(m for m in reversed(state['messages']) if m['role']=='user')
   assert user['content']=='Describe ',user['content']
   assert len(user['images'])==1 and user['images'][0]['mime_type']=='image/jpeg'
   assert user['images'][0]['width']==2
   assert base64.b64decode(user['images'][0]['data_base64']).startswith(b'\xff\xd8')
   print('PNG paste, atomic removal, image+text submission and durable image payload: passed',flush=True)
 finally:
  proc.terminate()
  try:proc.wait(timeout=5)
  except subprocess.TimeoutExpired:proc.kill();proc.wait()
  os.close(master);os.close(slave)

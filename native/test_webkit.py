from pathlib import Path
import os,json,struct,subprocess,threading,queue,time,sys,tempfile,uuid
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
helper=Path(sys.argv[1]).resolve(); token=uuid.uuid4().hex
work=tempfile.TemporaryDirectory();root=Path(work.name)
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/api/auth/session':
            page=json.dumps({'authenticated':self.headers.get('Cookie') == 'z3_test='+token}).encode()
            self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(page)));self.end_headers();self.wfile.write(page);return
        page=b'''<!doctype html><style>body{background:#141414;color:white;margin:0}button{position:absolute;left:40px;top:40px;width:200px;height:70px}</style><button onclick="parent.postMessage('Z3_CLICKED','*')">Interactive button</button><script>try{parent.document.body;parent.postMessage('ESCAPED','*')}catch(e){parent.postMessage('SANDBOXED','*')}</script>'''
        self.send_response(200);self.send_header('Content-Type','text/html');self.send_header('Content-Length',str(len(page)));self.end_headers();self.wfile.write(page)
    def log_message(self,*args):pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);origin=f'http://127.0.0.1:{server.server_port}';threading.Thread(target=server.serve_forever,daemon=True).start()
env=os.environ.copy();env.update(GDK_BACKEND='x11');env.pop('WAYLAND_DISPLAY',None)
log=(root/'z3-webkit-check.log').open('w');os.chmod(log.name,0o600)
process=subprocess.Popen([str(helper)],env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,start_new_session=True)
frames=queue.Queue()
def read():
    try:
        while header:=process.stdout.read(9):
            assert len(header)==9
            kind=chr(header[0]);ident,size=struct.unpack('<II',header[1:]);payload=process.stdout.read(size)
            if kind=='J': frames.put(json.loads(payload))
    except Exception:frames.put(None)
threading.Thread(target=read,daemon=True).start()
def send(cmd,**fields):
    payload=json.dumps(dict(id=1,cmd=cmd,**fields)).encode();process.stdin.write(struct.pack('<I',len(payload))+payload);process.stdin.flush()
def evaluate(script):
    send('eval',script=script);return frames.get(timeout=10)
def wait_for(script,predicate):
    for _ in range(30):
        value=evaluate(script)
        if predicate(value):return value
        time.sleep(.2)
    raise AssertionError('Native browser did not reach the expected state')
try:
    send('create');send('resize',width=500,height=300,scale=1);send('visible',value=1)
    send('appearance',dark=True)
    send('load-document',url=origin+'/visual')
    wait_for("Boolean(document.querySelector('iframe'))",bool)
    assert wait_for("matchMedia('(prefers-color-scheme: dark)').matches",lambda value:value is True) is True
    evaluate("window.z3Messages=[];addEventListener('message',e=>z3Messages.push(e.data));true")
    # Reload the frame after attaching the test-only parent listener.
    evaluate("document.querySelector('iframe').src="+json.dumps(origin+'/visual?test=1')+";true")
    value=wait_for('window.z3Messages',lambda value:isinstance(value,list) and 'SANDBOXED' in value)
    assert 'ESCAPED' not in value
    send('down',x=100,y=75,button=1,mods=0);send('up',x=100,y=75,button=1,mods=0)
    wait_for('window.z3Messages',lambda value:isinstance(value,list) and 'Z3_CLICKED' in value)
    send('load-session',origin=origin,url=origin+'/api/auth/session',cookieName='z3_test',accessToken=token)
    value=wait_for("(()=>{try{return JSON.parse(document.body.innerText).authenticated}catch{return false}})()",lambda value:value is True)
    assert evaluate('document.cookie')==''
    print('Embedded WebKit: sandbox isolation, real pointer interaction, authenticated session cookie and HttpOnly protection passed.')
finally:
    try:send('close')
    except BrokenPipeError:pass
    process.stdin.close()
    try:process.wait(timeout=10)
    except subprocess.TimeoutExpired:os.killpg(process.pid,15);process.wait(timeout=10)
    server.shutdown();server.server_close();work.cleanup()

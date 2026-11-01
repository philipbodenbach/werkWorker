"""Trusted Playwright driver inside an empty, network-isolated WW sandbox.

JSONL on stdio is private to the Rust broker. Page code never gets a binding to
this channel. HTTP requests are fulfilled by the host after origin/DNS checks.
"""
import base64
import json
import os
import re
import sys
import http.server
import ssl
import socket
import subprocess
import tempfile
import threading
from pathlib import Path

from playwright.sync_api import sync_playwright


def emit(value):
    print(json.dumps(value, ensure_ascii=True), flush=True)


class Proxy:
    """No socket to an origin is ever opened here, only to the local browser.

    CONNECT terminates TLS in this private namespace. The Rust broker verifies
    real upstream HTTPS certificates; browser-side trust is only for this
    ephemeral proxy certificate. Redirects use the same broker at every hop.
    """
    def __init__(self, driver):
        self.driver = driver
        self.files = tempfile.TemporaryDirectory(prefix="ww-proxy-")
        root = Path(self.files.name)
        subprocess.run(["/usr/bin/openssl", "req", "-x509", "-newkey", "rsa:2048",
                        "-nodes", "-keyout", str(root / "key.pem"),
                        "-out", str(root / "cert.pem"), "-days", "1",
                        "-subj", "/CN=werkworker-browser", "-config", "/dev/null"],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.tls.load_cert_chain(root / "cert.pem", root / "key.pem")
        proxy = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            authority = None
            scheme = "https"

            def log_message(self, *_):
                pass

            def setup(self):
                self.request.settimeout(20)
                super().setup()

            def do_CONNECT(self):
                # CONNECT only opens TLS to this local proxy, never to the host.
                self.send_response(200)
                self.end_headers()
                self.wfile.flush()
                # Playwright's request context tunnels plain HTTP with CONNECT
                # too. Both forms still terminate here; neither opens a socket
                # to the destination. The broker validates scheme/origin/DNS.
                if self.connection.recv(1, socket.MSG_PEEK) == b'\x16':
                    self.connection = proxy.tls.wrap_socket(self.connection, server_side=True)
                    self.rfile = self.connection.makefile("rb")
                    self.wfile = self.connection.makefile("wb")
                    self.scheme = "https"
                else:
                    self.scheme = "http"
                self.authority = self.path
                self.close_connection = False

            def forward(self):
                epoch = driver.epoch
                if not driver.active:
                    self.send_error(403, "No browser action is active")
                    self.close_connection = True
                    return
                try:
                    length = int(self.headers.get("content-length", "0"))
                    if length < 0 or length > 1048576 or self.headers.get("transfer-encoding") or self.headers.get("upgrade"):
                        raise ValueError("Unsupported streaming request")
                    body = self.rfile.read(length) if length else b""
                    url = self.scheme + "://" + self.authority + self.path if self.authority else self.path
                    with driver.lock:
                        if not driver.active or driver.epoch != epoch:
                            self.send_error(403, "Browser action finished")
                            self.close_connection = True
                            return
                        emit({"kind": "http", "url": url, "method": self.command,
                              "headers": dict(self.headers.items()),
                              "body": base64.b64encode(body).decode("ascii") if body else None})
                        line = sys.stdin.readline()
                        if not line:
                            raise EOFError("Broker closed")
                        reply = json.loads(line)
                    if "error" in reply:
                        self.send_error(403, reply["error"])
                    else:
                        payload = base64.b64decode(reply["body"])
                        self.send_response(reply["status"])
                        for name, value in reply["headers"]:
                            self.send_header(name, value)
                        self.send_header("Content-Length", str(len(payload)))
                        self.send_header("Connection", "close")
                        self.end_headers()
                        if self.command != "HEAD":
                            self.wfile.write(payload)
                    self.wfile.flush()
                except (ValueError, EOFError, OSError):
                    pass
                finally:
                    self.close_connection = True

            do_GET = do_HEAD = do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = forward

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.origin = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()


class Driver:
    def __init__(self, playwright):
        self.pw = playwright
        self.browser = None
        self.context = None
        self.page = None
        self.pages = {}
        self.next_page = 1
        self.active = False
        self.pointer = None
        self.references = {}
        self.secrets = set()
        self.ref_sequence = 0
        self.epoch = 0
        self.lock = threading.Lock()
        self.proxy = Proxy(self)

    def add_page(self, page):
        if len(self.pages) >= 12:
            page.close()
            return
        self.pages[self.next_page] = page
        self.next_page += 1
        page.set_default_timeout(5000)
        page.set_default_navigation_timeout(15000)
        page.on("dialog", lambda dialog: dialog.dismiss())
        page.on("download", lambda download: download.cancel())
        page.on("filechooser", lambda chooser: chooser.set_files([]))

    def open(self, args):
        kind = args["browser"]
        launch = {"headless": True, "proxy": {"server": self.proxy.origin}}
        if kind in ("firefox", "webkit"):
            engine = getattr(self.pw, kind)
            if kind == "firefox":
                launch["firefox_user_prefs"] = {"network.proxy.allow_hijacking_localhost": True, "network.proxy.no_proxies_on": ""}
            else:
                # The pinned WebKit bundle's wrapper overwrites LD_LIBRARY_PATH.
                # Preserve optional private host-installed platform libraries.
                bundle = Path(engine.executable_path).parent / "minibrowser-wpe"
                if (bundle / "bin/MiniBrowser").is_file():
                    launch["executable_path"] = str(bundle / "bin/MiniBrowser")
                    launch["env"] = dict(os.environ, WEBKIT_EXEC_PATH=str(bundle / "bin"),
                        WEBKIT_INJECTED_BUNDLE_PATH=str(bundle / "lib"),
                        WEBKIT_INSPECTOR_RESOURCES_PATH=str(bundle / "share"),
                        WEBKIT_FORCE_COMPLEX_TEXT="1",
                        LD_LIBRARY_PATH=f"{bundle}/lib:{bundle}/sys/lib:" + os.environ.get("LD_LIBRARY_PATH", ""))
        else:
            engine = self.pw.chromium
            launch["args"] = ["--disable-gpu", "--disable-dev-shm-usage",
                              "--renderer-process-limit=2", "--num-raster-threads=2",
                              "--proxy-bypass-list=<-loopback>"]
            if kind in ("chrome", "msedge"):
                launch["channel"] = kind
            elif kind in ("brave", "opera", "vivaldi"):
                candidates = {
                    "brave": ["/opt/brave.com/brave/brave", "/usr/lib/brave-browser/brave"],
                    "opera": ["/usr/lib/x86_64-linux-gnu/opera/opera", "/usr/lib/opera/opera", "/usr/lib64/opera/opera"],
                    "vivaldi": ["/opt/vivaldi/vivaldi-bin", "/opt/vivaldi/vivaldi"],
                }[kind]
                launch["executable_path"] = next((path for path in candidates if Path(path).is_file()), candidates[0])
        self.browser = engine.launch(**launch)
        self.context = self.browser.new_context(
            viewport={"width": 1024, "height": 768},
            service_workers="block", accept_downloads=False, ignore_https_errors=True)
        self.context.route_web_socket("**/*", lambda ws: ws.close())
        self.context.on("page", self.add_page)
        self.page = self.context.new_page()
        self.page.goto(args["url"], wait_until="domcontentloaded")

    def target(self, spec):
        if "ref" in spec:
            target = self.references.get(spec["ref"])
            if target is None:
                raise ValueError("Stale element reference; observe the page again")
            return target
        scope = self.page.frame_locator(spec["frame"]) if "frame" in spec else self.page
        if "selector" in spec:
            return scope.locator(spec["selector"])
        if "label" in spec:
            return scope.get_by_label(spec["label"], exact=True)
        return scope.get_by_role(spec["role"], name=spec["name"], exact=True)

    @staticmethod
    def info(element):
        meta = element.evaluate("""e => ({tag:e.tagName.toLowerCase(),type:e.type||'',role:e.getAttribute('role')||({a:'link',button:'button',input:e.type==='checkbox'?'checkbox':'textbox',textarea:'textbox',select:'combobox'}[e.tagName.toLowerCase()]||''),
          name:(e.getAttribute('aria-label') || (e.labels&&Array.from(e.labels).map(x=>x.innerText).join(' ')) || e.innerText || e.name || '').slice(0,160),
          id:(e.id||'').slice(0,256),autocomplete:(e.autocomplete||'').slice(0,128),href:(e.href||'').slice(0,2048),download:e.hasAttribute('download'),
          submits:!!e.form && (e.type==='submit'||e.type==='image'),form_action:e.form?e.form.action.slice(0,2048):'',
          bounds:(()=>{let r=e.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height}})()})""")
        description = ' '.join(str(meta.get(k,'')) for k in ('name','id','autocomplete','type'))
        meta['protected'] = (meta['type'] in ('password','file') or bool(re.search(
            r'password|passwort|passcode|one.time.code|einmalcode|current-password|new-password|cc-[a-z]+|card.?number|cardholder|expiration|expiry|kreditkarte|cvc|cvv|security.?code|verification.?code|auth.?code|\botp\b|\bpin\b',description,re.I)))
        meta['consequential'] = bool(re.search(r'\b(sign.?in|log.?in|submit|send|buy|purchase|pay|delete|remove|confirm|publish|save|anmelden|senden|kaufen|bezahlen|löschen|bestaetigen|bestätigen|speichern)\b',meta['name'],re.I))
        return meta

    def protected_values(self):
        values=[]; masks=[]
        if not self.page:
            return values,masks
        for frame in self.page.frames[:16]:
            fields=frame.locator('input,textarea,[contenteditable=true]')
            for i in range(min(fields.count(),100)):
                field=fields.nth(i)
                try:
                    if self.info(field)['protected']:
                        value=field.evaluate("e => e.value || e.innerText || ''")
                        if value: values.append(value)
                        masks.append(field)
                except Exception:
                    continue
        return values,masks

    def focused_element(self):
        frame=self.page.main_frame
        for _ in range(16):
            element=frame.evaluate_handle('document.activeElement').as_element()
            if element is None: return None
            # Keyboard events are delivered into the focused child frame.
            child=element.content_frame()
            if child is None: return element
            frame=child
        raise ValueError('Too many nested focus frames')

    def review_action(self,args):
        if args.get('human'):
            if args.get('text'):
                field=self.target(args['target']) if 'target' in args else self.focused_element()
                try:
                    if self.info(field)['protected'] and len(self.secrets)<128: self.secrets.add(args['text'])
                except Exception:
                    pass
            return
        kind=args['action']; element=None
        if 'target' in args:
            element=self.target(args['target'])
        elif kind=='press':
            element=self.focused_element()
        if element:
            meta=self.info(element)
            if meta['protected'] and (kind in ('fill','select') or kind=='press' and args['key'] not in ('Enter','Tab','Escape','ArrowLeft','ArrowRight','ArrowUp','ArrowDown')):
                raise ValueError('Protected password, payment, OTP or file field: use Take over in the browser panel and enter it yourself')
            if args.get('browser_category')=='browsing' and kind in ('click','fill','select'):
                safe = kind=='fill' and not meta['protected'] or kind=='click' and meta['tag']=='a' and meta['href'] and not meta['submits'] and not meta['consequential'] and not meta['download']
                if meta['tag']=='a' and not meta['href'].startswith(('http://','https://')): safe=False
                if not safe: raise ValueError('Control changed since review; observe again before acting')

    def action(self, args):
        kind = args["action"]
        self.review_action(args)
        if kind in ("click", "fill", "select", "hover"):
            target = self.target(args["target"])
            if kind == "fill":
                target.fill(args["text"])
            elif kind == "select":
                target.select_option(args["value"])
            else:
                getattr(target, kind)()
        elif kind == "type" and args.get("human"):
            self.page.keyboard.insert_text(args["text"])
        elif kind == "press":
            target = self.target(args["target"]) if "target" in args else self.page.keyboard
            target.press(args["key"])
        elif kind in ("click_at", "move"):
            self.pointer = {"x": args["x"], "y": args["y"]}
            if kind == "click_at":
                self.page.mouse.click(args["x"], args["y"], button=args.get("button", "left"), click_count=args.get("click_count", 1))
            else:
                self.page.mouse.move(args["x"], args["y"])
        elif kind == "drag":
            points = args["path"]
            self.page.mouse.move(points[0]["x"], points[0]["y"])
            button = args.get("button", "left")
            self.page.mouse.down(button=button)
            try:
                for point in points[1:]:
                    self.page.mouse.move(point["x"], point["y"], steps=4)
                self.pointer = points[-1]
            finally:
                self.page.mouse.up(button=button)
        elif kind == "scroll":
            self.page.mouse.wheel(args["x"], args["y"])
        elif kind in ("back", "forward", "reload"):
            method = {"back": "go_back", "forward": "go_forward", "reload": "reload"}[kind]
            getattr(self.page, method)(wait_until="domcontentloaded")
        elif kind == "wait":
            self.page.wait_for_timeout(args["ms"])
        elif kind == "tab_new":
            if len(self.pages) >= 12:
                raise ValueError("At most 12 tabs are supported")
            self.page = self.context.new_page()
        elif kind in ("tab_select", "tab_close"):
            page = self.pages.get(args["tab_id"])
            if page is None or page.is_closed():
                raise ValueError("Tab is no longer available; observe current tabs")
            if kind == "tab_select":
                self.page = page
            else:
                page.close()
                if page == self.page:
                    self.page = next((p for p in self.context.pages if not p.is_closed()), None)
                    if self.page is None:
                        self.page = self.context.new_page()

    def snapshot(self, screenshot=False):
        self.pages = {key: page for key, page in self.pages.items() if not page.is_closed()}
        if self.page is None or self.page.is_closed():
            self.page = next(iter(self.pages.values()), None)
        result = {"url": self.page.url if self.page else "about:blank", "tabs": [], "viewport": {"width": 1024, "height": 768}, "pointer": self.pointer}
        for key, page in self.pages.items():
            result["tabs"].append({"tab_id": key, "url": page.url[:512], "active": page == self.page})
        if self.page:
            self.references = {}
            result["controls"] = []
            controls_size=0
            for element in self.page.query_selector_all('a,button,input,select,textarea,[role=button],[contenteditable=true],canvas')[:100]:
                try:
                    if not element.is_visible(): continue
                    meta=self.info(element)
                    controls_size+=len(json.dumps(meta,ensure_ascii=True))
                    if controls_size>10000: break
                    self.ref_sequence += 1
                    ref=f'e{self.ref_sequence}'
                    self.references[ref]=element
                    meta['ref']=ref
                    result['controls'].append(meta)
                    if len(result['controls'])>=40: break
                except Exception:
                    continue
            result["title"] = self.page.title()[:512]
            body = self.page.locator("body")
            if body.count():
                result["text"] = body.inner_text(timeout=2000)[:6000]
                result["accessibility"] = body.aria_snapshot(timeout=2000)[:8000]
            secret_values,masks=self.protected_values()
            self.secrets.update(secret_values[:max(0,128-len(self.secrets))])
            def redact(value):
                if isinstance(value,str):
                    for secret in self.secrets: value=value.replace(secret,'[private]')
                elif isinstance(value,list): value=[redact(item) for item in value]
                elif isinstance(value,dict): value={k:redact(v) for k,v in value.items()}
                return value
            for key in ("text","accessibility","title","controls"):
                if key in result: result[key]=redact(result[key])
            if screenshot:
                result["screenshot_base64"] = base64.b64encode(
                    self.page.screenshot(type="jpeg", quality=65, timeout=5000, mask=masks)).decode("ascii")
        return result

    def run(self, tool, args):
        # Drain queued browser events with networking disabled before authorizing
        # this operation; there is never an ambient host network connection.
        if self.page and not self.page.is_closed():
            self.page.wait_for_timeout(1)
        if tool not in ("browser.open", "browser.observe") and args.get("expected_url") != self.page.url:
            return {"url": self.page.url, "error": "Page changed since approval; observe before acting"}
        with self.lock:
            self.epoch += 1
            self.active = True
        result = {}
        try:
            if tool == "browser.open":
                self.open(args)
            elif tool == "browser.navigate":
                self.page.goto(args["url"], wait_until="domcontentloaded")
            elif tool == "browser.action":
                self.action(args)
            elif tool == "browser.evaluate":
                value=self.page.evaluate(args['script'])
                encoded=json.dumps(value,ensure_ascii=True)
                result['script_result']=value if len(encoded)<12000 else {'truncated':True,'text':encoded[:12000]}
            elif tool == "browser.download":
                response=self.context.request.get(args['url'],timeout=20000)
                if not response.ok: raise ValueError('Download failed with HTTP '+str(response.status))
                body=response.body()
                if len(body)>4*1024*1024: raise ValueError('Download exceeds 4 MiB')
                result['download_base64']=base64.b64encode(body).decode('ascii')
                result['content_type']=response.headers.get('content-type','application/octet-stream')
            # Include immediate event handlers/popups in this action's result.
            if self.page and not self.page.is_closed():
                self.page.wait_for_timeout(50)
        except Exception as exc:
            result["error"] = str(exc)[:1500]
            for secret in self.secrets: result['error']=result['error'].replace(secret,'[private]')
        try:
            # Leave transport headroom for a maximum-sized download; the
            # existing panel frame remains valid for this nonvisual operation.
            result.update(self.snapshot("download_base64" not in result))
        except Exception as exc:
            result.setdefault("error", str(exc)[:1500])
            result.setdefault("url", self.page.url if self.page else "about:blank")
        finally:
            with self.lock:
                self.active = False
        if args.get('human') and args.get('text') and 'error' in result:
            result['error']=result['error'].replace(args['text'],'[private]')
        return result


def main():
    with sync_playwright() as pw:
        driver = Driver(pw)
        for line in sys.stdin:
            command = json.loads(line)
            emit({"kind": "result", "data": driver.run(command["tool"], command["args"])})
        if driver.browser:
            driver.browser.close()


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        emit({"kind": "result", "data": {"error": "Browser runtime failed: " + str(exc)[:1500]}})

"""One bounded local browser check, executed only inside WW's sandbox."""
import base64
import functools
import http.server
import json
from pathlib import Path
import sys
import threading
from urllib.parse import quote

from playwright.sync_api import sync_playwright

args = json.loads(sys.argv[1])
root = Path('/workspace').resolve()


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send_head(self):
        path = Path(self.translate_path(self.path)).resolve()
        if not path.is_relative_to(root):
            self.send_error(403)
            return None
        return super().send_head()


server = http.server.HTTPServer(('127.0.0.1', 0), functools.partial(Handler, directory=str(root)))
threading.Thread(target=server.serve_forever, daemon=True).start()
origin = f'http://127.0.0.1:{server.server_port}'
result = {'actions': [], 'assertions': 0, 'errors': [], 'console': [], 'passed': False}

try:
    with sync_playwright() as pw:
        browser = pw.chromium.launch(headless=True, args=['--disable-gpu', '--disable-dev-shm-usage', '--renderer-process-limit=2', '--num-raster-threads=2'])
        context = browser.new_context(viewport={'width': 1024, 'height': 768}, service_workers='block', accept_downloads=False)
        # All browser traffic stays inside this check's local origin. The OS
        # network namespace also excludes host services and the public network.
        context.route('**/*', lambda route: route.continue_() if route.request.url.startswith(origin + '/') else route.abort())
        page = context.new_page()
        page.set_default_timeout(3000)
        page.on('pageerror', lambda error: result['errors'].append(str(error)[:1000]) if len(result['errors']) < 16 else None)
        def record_console(msg):
            if len(result['console']) < 12:
                result['console'].append({'type': msg.type, 'text': msg.text[:500]})
            if msg.type == 'error' and len(result['errors']) < 16:
                result['errors'].append(msg.text[:500])
        page.on('console', record_console)
        page.on('dialog', lambda dialog: dialog.dismiss())
        try:
            response = page.goto(origin + '/' + quote(args['path'], safe='/'), wait_until='load', timeout=10000)
            if response is None or response.status >= 400:
                raise RuntimeError('Local page failed to load')
            for action in args.get('actions', []):
                kind = action['type']
                if kind == 'click':
                    page.locator(action['selector']).click()
                elif kind == 'press':
                    page.keyboard.press(action['key'])
                elif kind == 'fill':
                    page.locator(action['selector']).fill(action['text'])
                elif kind == 'wait':
                    page.wait_for_timeout(min(action['ms'], 2000))
                elif kind == 'expect_visible':
                    page.locator(action['selector']).wait_for(state='visible')
                    result['assertions'] += 1
                elif kind == 'expect_text':
                    from playwright.sync_api import expect
                    expect(page.locator(action['selector'])).to_contain_text(action['text'], timeout=3000)
                    result['assertions'] += 1
                elif kind == 'expect':
                    # This predicate runs in the page, never as a host expression.
                    page.wait_for_function(action['expression'], timeout=3000)
                    result['assertions'] += 1
                result['actions'].append({'type': kind, 'passed': True})
            result['title'] = page.title()[:256]
            result['text'] = page.locator('body').inner_text()[:2000]
            result['passed'] = result['assertions'] > 0 and not result['errors'] and not any(m['type'] == 'error' for m in result['console'])
        except Exception as error:
            result['errors'].append(str(error)[:2000])
        if args.get('screenshot', True):
            result['screenshot_base64'] = base64.b64encode(page.screenshot(type='jpeg', quality=60)).decode('ascii')
        browser.close()
except Exception as error:
    result['passed'] = False
    result['errors'].append(str(error)[:2000])
finally:
    server.shutdown()
print(json.dumps(result, ensure_ascii=True))

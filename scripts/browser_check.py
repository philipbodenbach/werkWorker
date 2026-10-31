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


def action_details(action, index):
    # Do not echo filled input values. Indices in diagnostics are one-based.
    details = {'index': index, 'type': action['type']}
    for key in ('selector', 'key', 'expression'):
        if key in action:
            details[key] = action[key][:1024]
    if action['type'] == 'expect_text':
        details['text'] = action['text'][:1024]
    return details


def target_state(page, selector):
    target = page.locator(selector)
    count = target.count()
    state = {'count': count}
    if count:
        first = target.first
        state.update(visible=first.is_visible(), hidden=first.get_attribute('hidden') is not None,
                     text=(first.text_content() or '')[:240])
    return state


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
        current_action = None
        last_interaction = None
        try:
            response = page.goto(origin + '/' + quote(args['path'], safe='/'), wait_until='load', timeout=10000)
            if response is None or response.status >= 400:
                raise RuntimeError('Local page failed to load')
            for index, action in enumerate(args.get('actions', []), start=1):
                current_action = action_details(action, index)
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
                else:
                    raise ValueError(f'Unsupported browser action: {kind}')
                result['actions'].append({'type': kind, 'passed': True})
                if kind in ('click', 'press', 'fill'):
                    last_interaction = current_action
                current_action = None
            result['passed'] = result['assertions'] > 0 and not result['errors'] and not any(m['type'] == 'error' for m in result['console'])
        except Exception as error:
            result['errors'].append(str(error)[:2000])
            if current_action is not None:
                result['failed_action'] = current_action
                if last_interaction is not None:
                    result['failed_action']['last_interaction'] = last_interaction
                if 'selector' in current_action:
                    try:
                        result['failed_action']['target'] = target_state(page, current_action['selector'])
                    except Exception as observation_error:
                        result['failed_action']['observation_error'] = str(observation_error)[:200]
        # Preserve the failure state as well as the successful action prefix.
        # Never rerun a predicate/action for diagnostics: it may have side effects.
        try:
            result['title'] = page.title()[:256]
            result['text'] = page.locator('body').inner_text(timeout=500)[:1000]
            if 'failed_action' in result:
                result['controls'] = page.locator('button, [role="button"], [role="tab"], select, input[type="submit"]').evaluate_all('''elements =>
                    elements.filter(el => el.getClientRects().length && getComputedStyle(el).visibility !== 'hidden').slice(0, 12).map(el => ({
                        id: el.id.slice(0, 80),
                        name: (el.getAttribute('aria-label') || el.textContent || '').trim().slice(0, 80),
                        selected: el.getAttribute('aria-selected'),
                        disabled: !!el.disabled
                    }))''')
        except Exception as observation_error:
            result['observation_error'] = str(observation_error)[:200]
        if args.get('screenshot', True):
            result['screenshot_base64'] = base64.b64encode(page.screenshot(type='jpeg', quality=60)).decode('ascii')
        browser.close()
except Exception as error:
    result['passed'] = False
    result['errors'].append(str(error)[:2000])
finally:
    server.shutdown()
print(json.dumps(result, ensure_ascii=True))

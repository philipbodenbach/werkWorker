"""One bounded local browser check, executed only inside WW's sandbox."""
import base64
import functools
import http.server
import json
from pathlib import Path
import sys
import threading
from urllib.parse import quote, unquote, urlsplit

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


class InvalidCheck(Exception):
    pass


# Catch exceptions INSIDE the predicate so they never become uncaught page/CDP
# errors. Return an error sentinel to stop polling without evaluating it again.
PREDICATE = """async expression => {
    try {
        const candidate = (0, eval)('(' + expression + '\\n)');
        const value = await (typeof candidate === 'function' ? candidate() : candidate);
        if (typeof value !== 'boolean') {
            return {check_error: 'Expected boolean true/false, received ' +
                (value === null ? 'null' : typeof value) +
                '. Use an explicit comparison or expect_text; a returned value is not an assertion.'};
        }
        return value;
    } catch (error) {
        return {check_error: 'Predicate threw: ' + String(error).slice(0, 600) +
            '. Verify the expression and selectors against the observed DOM before changing application code.'};
    }
}"""


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


def record_exception(event):
    # CDP carries locations for parse errors even when pageerror has no stack.
    errors = result.setdefault('page_errors', [])
    if len(errors) >= 2:
        return
    details = event['exceptionDetails']
    exception = details.get('exception', {})
    diagnostic = {'message': exception.get('description', details.get('text', 'JavaScript error')).split('\n')[0][:300]}
    location = details
    if not location.get('url'):
        location = next(iter(details.get('stackTrace', {}).get('callFrames', [])), details)
    url = location.get('url', '')
    if url.startswith(origin + '/'):
        path = (root / unquote(urlsplit(url).path).lstrip('/')).resolve()
        if path.is_relative_to(root):
            line = location.get('lineNumber', 0) + 1
            column = location.get('columnNumber', 0) + 1
            diagnostic.update(path=str(path.relative_to(root))[:240], line=line, column=column)
            try:
                with path.open('rb') as source:
                    lines = source.read(1_048_576).decode('utf-8', errors='replace').splitlines()
                if 0 < line <= len(lines):
                    start = max(0, column - 121)
                    diagnostic['source_line'] = lines[line - 1][start:start + 300]
                    diagnostic['source_column_start'] = start + 1
            except OSError:
                pass
    errors.append(diagnostic)
    # Keep the location in the normal errors as well: repair summaries retain it.
    location_text = f"{diagnostic['path']}:{diagnostic['line']}:{diagnostic['column']}: " if 'path' in diagnostic else ''
    if len(result['errors']) < 16:
        result['errors'].append(location_text + diagnostic['message'])


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
        diagnostics = context.new_cdp_session(page)
        diagnostics.on('Runtime.exceptionThrown', record_exception)
        diagnostics.send('Runtime.enable')
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
            if result['errors']:
                result['failure_phase'] = 'startup'
                result['failure_origin'] = 'application'
                raise RuntimeError('Page startup failed; actions were not run. Fix the reported page error before checking interactions.')
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
                    handle = page.wait_for_function(PREDICATE, arg=action['expression'], timeout=3000)
                    try:
                        value = handle.json_value()
                    finally:
                        handle.dispose()
                    if value is not True:
                        raise InvalidCheck(value['check_error'])
                    result['assertions'] += 1
                else:
                    raise ValueError(f'Unsupported browser action: {kind}')
                result['actions'].append({'type': kind, 'passed': True})
                if result['errors']:
                    result['failure_origin'] = 'application'
                    raise RuntimeError('Application error during this action; see page_errors/console.')
                if kind in ('click', 'press', 'fill'):
                    last_interaction = current_action
                current_action = None
            result['passed'] = result['assertions'] > 0 and not result['errors'] and not any(m['type'] == 'error' for m in result['console'])
        except Exception as error:
            application_failed = bool(result['errors'])
            result['errors'].append(str(error)[:2000])
            if current_action is not None:
                # An assertion exception is a check failure, unless the page
                # independently emitted an application error during the action.
                origin = 'application' if application_failed else ('check' if isinstance(error, InvalidCheck) else 'assertion_or_action')
                result['failure_origin'] = origin
                current_action['failure_origin'] = origin
                if origin == 'check':
                    current_action['repair_hint'] = 'Correct this check using the observed element_ids and verification.replaces. This predicate error alone does not justify application edits.'
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
                result['element_ids'] = page.locator('[id]').evaluate_all('elements => elements.slice(0, 32).map(el => el.id.slice(0, 64))')
                result['controls'] = page.locator('button, [role="button"], [role="tab"], select, input[type="submit"]').evaluate_all('''elements =>
                    elements.filter(el => el.getClientRects().length && getComputedStyle(el).visibility !== 'hidden')
                    .sort((a,b) => Number(!!b.id) - Number(!!a.id)).slice(0, 12).map(el => ({
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
        result['passed'] = result['passed'] and not result['errors']
except Exception as error:
    result['passed'] = False
    result['errors'].append(str(error)[:2000])
finally:
    server.shutdown()
print(json.dumps(result, ensure_ascii=True))

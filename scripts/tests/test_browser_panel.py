"""Panel permission controls in an isolated Chromium page; all requests are mocked."""
import json
import os
from pathlib import Path
import unittest

REQUIRE_BROWSER = os.environ.get('REQUIRE_BROWSER') == '1'
if REQUIRE_BROWSER:
    from playwright.sync_api import sync_playwright


@unittest.skipUnless(REQUIRE_BROWSER, 'Set REQUIRE_BROWSER=1 with the installed browser runtime')
class PanelPermissions(unittest.TestCase):
    def test_combined_controls_target_the_pending_site_and_exclude_scripts(self):
        html = (Path(__file__).parents[2] / 'src/browser_view.html').read_text()
        host = {'enabled': True, 'policy': {}, 'approvals': []}
        commands = []

        def request(category, tool='browser.action'):
            host['approvals'] = [{
                'request_id': category,
                'run_id': 'fixture',
                'description': {'tool': tool, 'arguments': {
                    'origin': 'https://jspaint.app', 'browser_category': category,
                }},
            }]

        def route(r):
            path = r.request.url.removeprefix('https://panel.invalid/')
            if path == 'state':
                r.fulfill(json={'host': host})
            elif path == 'control':
                commands.append(json.loads(r.request.post_data))
                r.fulfill(json={'ok': True})
            elif path == '':
                r.fulfill(content_type='text/html', body=html)
            else:
                r.abort()

        with sync_playwright() as pw:
            browser = pw.chromium.launch(headless=True, args=['--disable-dev-shm-usage'])
            try:
                page = browser.new_page()
                page.route('**/*', route)
                request('actions')
                page.goto('https://panel.invalid/')
                for category in ['actions', 'unlabelled']:
                    request(category)
                    page.wait_for_function('(c) => pending?.request_id === c', arg=category)
                    self.assertEqual(page.locator('#request-scope').inner_text(),
                                     f'https://jspaint.app · {category}')
                    page.locator('#allow-controls').click()
                    page.wait_for_function('!busy')
                    self.assertEqual(commands[-1], {
                        'op': 'rule', 'site': 'https://jspaint.app',
                        'category': 'controls', 'permission': 'allow',
                    })
                self.assertEqual(len(commands), 2)
                request('scripts', 'browser.evaluate')
                page.wait_for_function("pending?.request_id === 'scripts'")
                self.assertTrue(page.locator('#allow-controls').is_hidden())
                self.assertTrue(page.locator('#site').is_hidden())
                page.locator('#settings').click()
                page.locator('#site-input').fill('https://example.com')
                page.locator('#category').select_option('controls')
                page.locator('#permission').select_option('allow')
                page.locator('#save-rule').click()
                page.wait_for_function('!busy')
                self.assertEqual(commands[-1], {
                    'op': 'rule', 'site': 'https://example.com',
                    'category': 'controls', 'permission': 'allow',
                })
            finally:
                browser.close()


if __name__ == '__main__':
    unittest.main()

"""Real Chromium observation faults, with no external page/network access.
Run with REQUIRE_BROWSER=1 and the installed WW runtime on PYTHONPATH/
PLAYWRIGHT_BROWSERS_PATH. Explicit runs fail if that runtime is unavailable.
"""
import importlib.util
import os
import signal
from pathlib import Path
import threading
import unittest

REQUIRE_BROWSER = os.environ.get('REQUIRE_BROWSER') == '1'
if REQUIRE_BROWSER:
    from playwright.sync_api import sync_playwright
    spec=importlib.util.spec_from_file_location('driver',Path(__file__).parents[1]/'browser_control.py')
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)

class FaultyBody:
    def inner_text(self, **kwargs): raise RuntimeError('isolated text failure')
    def aria_snapshot(self, **kwargs): raise RuntimeError('isolated accessibility failure')
class FaultyPage:
    def __init__(self,page): self.page=page
    def __getattr__(self,name): return getattr(self.page,name)
    def locator(self,selector): return FaultyBody() if selector=='body' else self.page.locator(selector)

@unittest.skipUnless(REQUIRE_BROWSER, 'Set REQUIRE_BROWSER=1 with the installed browser runtime')
class Observations(unittest.TestCase):
    def setUp(self):
        self.pw=sync_playwright().start()
        self.browser=self.pw.chromium.launch(headless=True,args=['--disable-dev-shm-usage'])
        self.page=self.browser.new_page(viewport={'width':1024,'height':768})
        self.page.context.set_offline(True)
        self.page.set_content('<input type="password" value="PrivateSecret"><button onclick="window.clicks=(window.clicks||0)+1">Count</button>')
        self.driver=module.Driver.__new__(module.Driver)
        self.driver.page=self.page;self.driver.pages={};self.driver.next_page=1;self.driver.crashed_pages=set()
        self.driver.add_page(self.page)
        self.driver.pointer=None;self.driver.references={};self.driver.ref_sequence=0
        self.driver.secrets=set();self.driver.transport_blocks=[];self.driver.lock=threading.Lock();self.driver.epoch=0;self.driver.active=False
    def tearDown(self): self.browser.close();self.pw.stop()
    def test_extraction_fault_preserves_masked_frame_and_single_action(self):
        self.driver.page=FaultyPage(self.page);self.driver.pages={1:self.driver.page}
        result=self.driver.run('browser.action',{'expected_url':'about:blank','action':'click','target':{'selector':'button'},'browser_category':'actions'})
        self.assertTrue(result['action_executed'])
        self.assertNotIn('error',result)
        self.assertEqual(self.page.evaluate('window.clicks'),1)
        self.assertIn('screenshot_base64',result)
        self.assertEqual({e['part'] for e in result['observation_errors']},{'text','accessibility'})
        self.assertNotIn('PrivateSecret',str({k:v for k,v in result.items() if k!='screenshot_base64'}))
        self.driver.run('browser.observe',{})
        self.assertEqual(self.page.evaluate('window.clicks'),1)
    def test_protection_fault_suppresses_frame_and_text(self):
        def fail(): raise RuntimeError('protection failed')
        self.driver.protected_values=fail
        result=self.driver.snapshot(True)
        self.assertNotIn('screenshot_base64',result)
        self.assertNotIn('text',result)
        self.assertEqual(result['observation_errors'][0]['reason'],'masking_unavailable')
    def test_crashed_tab_is_failed_and_does_not_repeat_actions(self):
        # Only terminate renderer children reported by this test's own browser.
        # chrome://crash can leave headless Chromium waiting for a crash handler.
        session=self.browser.new_browser_cdp_session()
        renderers=[p['id'] for p in session.send('SystemInfo.getProcessInfo')['processInfo'] if p['type']=='renderer']
        self.assertTrue(renderers)
        with self.page.expect_event('crash',timeout=10000):
            for pid in renderers: os.kill(pid,signal.SIGKILL)
        for tool,args in [('browser.observe',{}),('browser.navigate',{'url':'https://example.com'})]:
            result=self.driver.run(tool,args)
            self.assertEqual(result['browser_state'],'crashed')
            self.assertFalse(result['action_executed'])
            self.assertIn('Close this browser session',result['error'])
            self.assertNotIn('screenshot_base64',result)
            self.assertNotIn('text',result)
            self.assertNotIn('masking_unavailable',str(result))
    def test_keyboard_text_checks_focused_fields_including_frames(self):
        self.page.set_content('<input id="normal"><input id="secret" type="password"><iframe srcdoc="&lt;input type=password&gt;"></iframe>')
        self.page.locator('#normal').focus()
        self.driver.action({'action':'type','text':'Grüße 世界'})
        self.assertEqual(self.page.locator('#normal').input_value(),'Grüße 世界')
        for field in [self.page.locator('#secret'), self.page.frame_locator('iframe').locator('input')]:
            field.focus()
            with self.assertRaisesRegex(ValueError,'Protected'):
                self.driver.action({'action':'type','text':'must-not-enter'})
            self.assertEqual(field.input_value(),'')
    def test_semantic_observation_does_not_reload_failed_images(self):
        requests=[]
        def failed_image(route):
            requests.append(route.request.url)
            route.fulfill(status=404,body='missing')
        self.page.route('https://assets.invalid/missing.png',failed_image)
        self.page.set_content('<img src="https://assets.invalid/missing.png"><p>Stable editor</p><span aria-live="polite">Incidental</span>')
        for _ in range(3):
            self.assertEqual(self.driver.semantic(),'Stable editor')
            self.page.wait_for_timeout(100)
        self.assertEqual(len(requests),1,'Observing page text must not reload its images')

    def test_icon_controls_have_clickable_named_refs_not_decorative_canvases(self):
        self.page.set_content('''<style>.tool {position:relative;width:32px;height:32px}.overlay {position:absolute;inset:0}</style>
            <div class="tool" title="Fill with Color" tabindex="0" onclick="window.selected='fill'">
            <canvas width="32" height="32"></canvas><span class="overlay"></span></div>
            <div tabindex="0"><canvas id="drawing" width="400" height="300"></canvas></div>''')
        observation=self.driver.snapshot(True)
        controls=observation['controls']
        fill=next(c for c in controls if c['name']=='Fill with Color')
        self.assertEqual(fill['tag'],'div')
        self.assertEqual([c['id'] for c in controls if c['tag']=='canvas'],['drawing'])
        result=self.driver.run('browser.action',{'expected_url':'about:blank','action':'click',
            'target':{'ref':fill['ref']},'browser_category':'actions'})
        self.assertNotIn('error',result)
        self.assertTrue(result['action_executed'])
        self.assertEqual(self.page.evaluate('window.selected'),'fill')

if __name__=='__main__': unittest.main()

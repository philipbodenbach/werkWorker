"""Real Chromium observation faults, with no external page/network access.
Run with the installed WW browser runtime on PYTHONPATH/PLAYWRIGHT_BROWSERS_PATH.
"""
import importlib.util
from pathlib import Path
import threading
import unittest
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

class Observations(unittest.TestCase):
    def setUp(self):
        self.pw=sync_playwright().start()
        self.browser=self.pw.chromium.launch(headless=True,args=['--disable-dev-shm-usage'])
        self.page=self.browser.new_page(viewport={'width':1024,'height':768})
        self.page.context.set_offline(True)
        self.page.set_content('<input type="password" value="PrivateSecret"><button onclick="window.clicks=(window.clicks||0)+1">Count</button>')
        self.driver=module.Driver.__new__(module.Driver)
        self.driver.page=FaultyPage(self.page);self.driver.pages={1:self.driver.page}
        self.driver.pointer=None;self.driver.references={};self.driver.ref_sequence=0
        self.driver.secrets=set();self.driver.transport_blocks=[];self.driver.lock=threading.Lock();self.driver.epoch=0;self.driver.active=False
    def tearDown(self): self.browser.close();self.pw.stop()
    def test_extraction_fault_preserves_masked_frame_and_single_action(self):
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

if __name__=='__main__': unittest.main()

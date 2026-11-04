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
    def test_action_target_is_redacted_without_modifying_image_bytes(self):
        self.page.set_content('<input type="password" value="a"><button title="a">a</button>')
        self.driver.snapshot()
        result=self.driver.run('browser.action',{'expected_url':'about:blank','action':'click',
            'target':{'selector':'button'},'browser_category':'actions'})
        self.assertNotIn('error',result)
        self.assertEqual(result['action_target']['name'],'[private]')
        self.assertNotIn('[private]',result['screenshot_base64'])
        self.assertTrue(module.base64.b64decode(result['screenshot_base64'],validate=True).startswith(b'\xff\xd8'))
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
        fill=next(c for c in controls if c.get('name')=='Fill with Color')
        self.assertEqual(fill['tag'],'div')
        self.assertEqual([c.get('id') for c in controls if c['tag']=='canvas'],['drawing'])
        result=self.driver.run('browser.action',{'expected_url':'about:blank','action':'click',
            'target':{'ref':fill['ref'],'name':'Informational name only'},'browser_category':'actions'})
        self.assertNotIn('error',result)
        self.assertTrue(result['action_executed'])
        self.assertEqual(result['action_target']['name'],'Fill with Color')
        self.assertEqual(self.page.evaluate('window.selected'),'fill')

    def test_hidden_menus_do_not_consume_visible_control_budget(self):
        self.page.set_content('<div hidden>'+''.join(
            f'<div role="menuitem">Hidden item {i}</div>' for i in range(150))+'</div>'+
            '<button onclick="window.selected=\'pencil\'">Pencil</button>'+
            '<canvas id="drawing" width="400" height="300"></canvas>')
        result=self.driver.snapshot(True)
        controls=result['controls']
        pencil=next((c for c in controls if c.get('name')=='Pencil'),None)
        self.assertIsNotNone(pencil,'Hidden menu items must not hide visible toolbar controls')
        self.assertTrue(any(c.get('id')=='drawing' for c in controls))
        self.assertFalse(any(c.get('name','').startswith('Hidden item') for c in controls))
        self.driver.action({'action':'click','target':{'ref':pencil['ref']},'browser_category':'actions'})
        self.assertEqual(self.page.evaluate('window.selected'),'pencil')

    def test_custom_palette_reports_clickable_colors_and_selected_tools_within_budget(self):
        self.page.set_content('''<style>.swatch{display:inline-block;width:13px;height:13px}.swatch canvas{pointer-events:none}</style>'''+
            ''.join(f'<button>Tool {i}</button>' for i in range(23))+
            '<div title="Pencil" class="selected">Pencil</div><canvas id="drawing" width="400" height="300"></canvas>'+
            '<canvas id="helper" width="400" height="300" style="pointer-events:none"></canvas>'+
            ''.join(f'<div class="swatch" data-color="rgb({i},0,255)" onclick="window.chosen=this.dataset.color"><canvas width="13" height="13"></canvas></div>' for i in range(28)))
        observation=self.driver.snapshot(True,stable_text=self.driver.semantic())
        colors=[c for c in observation['controls'] if c.get('color')]
        self.assertEqual(len(colors),28,'The palette must survive the bounded control budget')
        self.assertTrue(all(c['tag']=='div' for c in colors))
        self.assertTrue(any(c.get('id')=='drawing' for c in observation['controls']))
        self.assertFalse(any(c.get('id')=='helper' for c in observation['controls']))
        self.assertTrue(next(c for c in observation['controls'] if c.get('name')=='Pencil')['selected'])
        self.assertTrue(any(c.get('color')=='rgb(27,0,255)' for c in observation['semantic_state']['controls']))
        result=self.driver.run('browser.action',{'expected_url':'about:blank','action':'click',
            'target':{'ref':colors[-1]['ref']},'browser_category':'actions'})
        self.assertNotIn('error',result)
        self.assertEqual(self.page.evaluate('window.chosen'),'rgb(27,0,255)')
        self.assertEqual(result['action_target']['color'],'rgb(27,0,255)')
        self.assertEqual(result['action_target']['tag'],'div')

    def test_overlapping_palette_preview_does_not_advertise_an_intercepted_ref(self):
        self.page.set_content('''<style>.swatch{position:absolute;width:16px;height:16px;top:10px}
            .swatch canvas{pointer-events:none}</style>
            <div class="swatch" style="left:18px" data-color="white"><canvas width="16" height="16"></canvas></div>
            <div class="swatch" style="left:11px" data-color="blue" onclick="window.chosen='blue'"><canvas width="16" height="16"></canvas></div>''')
        observed=self.driver.snapshot(True)
        colors=[c for c in observed['controls'] if c.get('color')]
        self.assertEqual([c['color'] for c in colors],['blue'])
        result=self.driver.run('browser.action',{'expected_url':'about:blank','action':'click',
            'target':{'ref':colors[0]['ref']},'browser_category':'actions'})
        self.assertNotIn('error',result)
        self.assertEqual(self.page.evaluate('window.chosen'),'blue')

    def test_compact_link_metadata_preserves_explicit_permission_classification(self):
        self.page.set_content('<a href="https://example.com">Documentation</a><a href="https://example.com" download>Export</a>')
        links=[c for c in self.driver.snapshot()['controls'] if c.get('tag')=='a']
        self.assertEqual(len(links),2)
        self.assertIs(links[0]['consequential'],False)
        self.assertIs(links[0]['download'],False)
        self.assertIs(links[1]['download'],True)

    def test_general_controls_combine_labels_icon_hints_roles_and_states(self):
        self.page.set_content('''<span id="report-label" hidden>Open report</span>
            <button aria-labelledby="report-label" aria-label="Ignored fallback" onclick="window.clicked='report'"><svg width="24" height="24"><title>Document icon</title></svg></button>
            <button onclick="window.clicked='refresh'"><img alt="Refresh results" width="24" height="24"></button>
            <button onclick="window.clicked='expand'"><svg width="24" height="24"><title>Expand details</title></svg></button>
            <input type="button" value="Run report" onclick="window.clicked='run'">
            <div onclick="window.clicked='custom'">Custom control</div>
            <div role="switch" aria-label="Notifications" aria-checked="true">Enabled</div>
            <div role="tab" aria-selected="true">Overview</div>
            <div role="menuitemcheckbox" aria-checked="false">Optional column</div>
            <div role="checkbox" aria-checked="mixed">All rows</div>
            <div role="button" aria-disabled="true">Unavailable action</div>
            <button style="width:24px;height:24px;background:blue" onclick="window.clicked='unlabelled'"></button>''')
        observed=self.driver.snapshot(True)
        self.assertTrue(observed['visual_surface'],'Auto needs an image for unlabelled controls on ordinary HTML pages too')
        by_name={c.get('name'):c for c in observed['controls']}
        self.assertIs(by_name['Notifications']['checked'],True)
        self.assertIs(by_name['Overview']['selected'],True)
        self.assertIs(by_name['Optional column']['checked'],False)
        self.assertEqual(by_name['All rows']['checked'],'mixed')
        self.assertIs(by_name['Unavailable action']['disabled'],True)
        self.assertEqual(by_name['Run report']['role'],'button')
        for name,event in [('Open report','report'),('Refresh results','refresh'),('Expand details','expand'),('Run report','run'),('Custom control','custom')]:
            target=next(c for c in observed['controls'] if c.get('name')==name)
            observed=self.driver.run('browser.action',{'expected_url':'about:blank','action':'click',
                'target':{'ref':target['ref']},'browser_category':'actions'})
            self.assertNotIn('error',observed)
            self.assertEqual(observed['action_target']['name'],name)
            self.assertEqual(self.page.evaluate('window.clicked'),event)

    def test_modal_overlay_exposes_the_receiving_button(self):
        self.page.set_content('''<button id="behind" style="position:absolute;left:10px;top:10px;width:120px;height:40px" onclick="window.clicked='behind'">Behind modal</button>
            <div role="dialog" style="position:absolute;left:10px;top:10px;z-index:2;background:white">
            <button style="width:120px;height:40px" onclick="window.clicked='confirm'">Confirm choice</button></div>''')
        observed=self.driver.snapshot(True)
        self.assertFalse(any(c.get('id')=='behind' for c in observed['controls']))
        target=next(c for c in observed['controls'] if c.get('name')=='Confirm choice')
        result=self.driver.run('browser.action',{'expected_url':'about:blank','action':'click',
            'target':{'ref':target['ref']},'browser_category':'actions'})
        self.assertNotIn('error',result)
        self.assertEqual(self.page.evaluate('window.clicked'),'confirm')

    def test_nested_web_components_keep_actionable_labels_and_hit_targets(self):
        self.page.set_content('<div id="host"></div>')
        self.page.evaluate('''()=>{
            const outer=document.querySelector('#host').attachShadow({mode:'open'});
            outer.innerHTML='<div id="inner"></div>';
            const inner=outer.querySelector('#inner').attachShadow({mode:'open'});
            inner.innerHTML='<span id="label" hidden>Open settings</span><button aria-labelledby="label" onclick="window.clicks=(window.clicks||0)+1">⚙</button>';
        }''')
        observed=self.driver.snapshot(True)
        target=next(c for c in observed['controls'] if c.get('name')=='Open settings')
        for action in [
            {'action':'click','target':{'ref':target['ref']}},
            {'action':'click_at','x':round(target['bounds']['x']+target['bounds']['width']/2),
                'y':round(target['bounds']['y']+target['bounds']['height']/2)}
        ]:
            result=self.driver.run('browser.action',{'expected_url':'about:blank','browser_category':'actions',**action})
            self.assertNotIn('error',result)
            self.assertEqual(result['action_target']['name'],'Open settings')
        self.assertEqual(self.page.evaluate('window.clicks'),2)

if __name__=='__main__': unittest.main()

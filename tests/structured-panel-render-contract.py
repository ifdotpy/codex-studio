#!/usr/bin/env python3
"""Real isolated Electron render of structured panels. No workspace or inference."""
import base64
import json
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_panel_render import render_panel, PanelRenderError


def panel(spec, callbacks=None):
    return {'format': 'json-render', 'spec': spec, 'callbacks': callbacks or [], 'html': '', 'css': '', 'version': 3}


class StructuredPanelRenderContract(unittest.TestCase):
    def test_bundled_example_all_tabs_and_png(self):
        example = json.loads((ROOT / '.agents/skills/codex-workspace/assets/panel-progress.json').read_text())
        result = render_panel(panel(example['spec'], example['callbacks']))
        self.assertTrue(result['layout']['fits'])
        self.assertEqual([v['width'] for v in result['layout']['viewports']], [320, 640, 1000])
        self.assertEqual((result['width'], result['height'], result['version']), (1000, 150, 3))
        self.assertTrue(base64.b64decode(result['data_url'].split(',')[1]).startswith(b'\x89PNG'))

    def test_dynamic_option_that_appears_after_toggle_is_measured(self):
        choices = lambda values: [{'label': value, 'value': value} for value in values]
        spec = {'root': 'root', 'state': {'expanded': False, 'view': 'a'}, 'elements': {
            'root': {'type': 'Stack', 'props': {'gap': 'xs'}, 'children': ['controls', 'text']},
            'controls': {'type': 'Stack', 'props': {'direction': 'row'}, 'children': ['toggle', 'select']},
            'toggle': {'type': 'Toggle', 'props': {'name': 'expanded', 'label': 'More views', 'checked': {'$bindState': '/expanded'}}},
            'select': {'type': 'Select', 'props': {'name': 'view', 'label': 'View', 'value': {'$bindState': '/view'}, 'options': {'$cond': {'$state': '/expanded'}, '$then': choices(['a', 'b', 'c']), '$else': choices(['a', 'b'])}}},
            'text': {'type': 'Text', 'props': {'text': {'$cond': {'$state': '/view', 'eq': 'c'}, '$then': ('A long visible line of evidence.\n' * 15), '$else': 'Fits'}}},
        }}
        with self.assertRaisesRegex(PanelRenderError, 'local state.*does not fit'):
            render_panel(panel(spec))
        spec['elements']['text']['props']['text']['$then'] = 'Third view fits'
        self.assertTrue(render_panel(panel(spec))['layout']['fits'])

    def test_invalid_spec_does_not_become_a_blank_success_image(self):
        cases = [
            {'root': 'x', 'elements': {'x': {'type': 'Unknown', 'props': {}}}},
            {'root': 'x', 'elements': {'x': {'type': 'Text', 'props': {'text': 'Hello', 'style': {'background': 'red'}}}}},
            {'root': 'x', 'elements': {'x': {'type': 'Stack', 'props': {}, 'children': ['x']}}},
            {'root': 'x', 'elements': {'x': {'type': 'Button', 'props': {'label': 'Send', 'callback': 'undeclared'}}}},
        ]
        for spec in cases:
            with self.subTest(spec=spec), self.assertRaises(PanelRenderError):
                render_panel(panel(spec))


if __name__ == '__main__':
    unittest.main()

# /// script
# dependencies = []
# ///
import importlib.util
from pathlib import Path
import unittest
import sys
sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location('center', Path(__file__).with_name('lm-center-hyprland.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class CenterTests(unittest.TestCase):
    def test_omarchy_4k_at_2x(self):
        self.assertEqual(module.center([dict(width=3840, height=2160, scale=2, x=0, y=0)]), (960, 540))

    def test_focused_rotated_monitor_and_negative_origin(self):
        monitors = [dict(width=1920, height=1080, scale=1, x=0, y=0),
                    dict(width=3840, height=2160, scale=2, x=-1080, y=-200, transform=1, focused=True)]
        self.assertEqual(module.center(monitors), (-540, 760))

    def test_sleeping_monitors_are_excluded(self):
        with self.assertRaises(ValueError):
            module.center([dict(dpmsStatus=False)])

unittest.main()

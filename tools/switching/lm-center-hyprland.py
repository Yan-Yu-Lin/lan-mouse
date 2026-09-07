#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Center on the focused Hyprland monitor using logical, transformed coordinates."""
import json
import subprocess
import sys


def center(monitors):
    active = [m for m in monitors if not m.get('disabled') and m.get('dpmsStatus', True)]
    if not active:
        raise ValueError('No active monitor')
    monitor = next((m for m in active if m.get('focused')), active[0])
    width, height = monitor['width'], monitor['height']
    if monitor.get('transform', 0) % 2:
        width, height = height, width
    return round(monitor['x'] + width / monitor['scale'] / 2), round(monitor['y'] + height / monitor['scale'] / 2)


def main():
    monitors = json.loads(subprocess.check_output(['hyprctl', '-j', 'monitors'], timeout=0.4))
    x, y = center(monitors)
    if '--dry-run' in sys.argv:
        print(json.dumps({'x': x, 'y': y}))
        return
    result = subprocess.run(['hyprctl', 'dispatch', f'hl.dsp.cursor.move({{ x = {x}, y = {y} }})'], capture_output=True, text=True, timeout=0.4, check=True)
    if result.stdout.strip() != 'ok':
        raise RuntimeError(result.stdout + result.stderr)


if __name__ == '__main__':
    main()

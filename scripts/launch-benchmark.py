#!/usr/bin/env python3
"""Measure launcher Enter -> spawn/commit/map/submit in a private nested session."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import statistics
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('output', type=Path)
parser.add_argument('--runs', type=int, default=10)
parser.add_argument('--screenshots', action='store_true', help='Capture the first mapped window of each app after timing it')
parser.add_argument('--binary', type=Path, default=Path(os.environ.get('CARGO_TARGET_DIR', 'target')) / 'debug/gavno')
args = parser.parse_args()
binary = args.binary.resolve()
out = args.output.resolve()
out.mkdir(parents=True, exist_ok=True)
for name in ['home', 'runtime', 'config/gavno', 'data/applications']:
    (out / name).mkdir(parents=True, exist_ok=True)
(out / 'runtime').chmod(0o700)
(out / 'config/gavno/config.toml').write_text('''mod_key = "super"
polkit_agent = ""
[modules]
overview = false
[keyboard]
layout = "us"
options = ""
[lock]
after_idle_secs = 0
[classic]
desktop_home = false
desktop_trash = false
''')
for app, app_id, command in [('Zenity', 'zenity', 'zenity --info --no-wrap --text=Launch-benchmark'),
                              ('Konsole', 'org.kde.konsole', 'konsole --separate --nofork')]:
    (out / 'data/applications' / f'{app_id}.desktop').write_text(
        f'[Desktop Entry]\nType=Application\nName={app}\nExec={command}\n')
wayland = os.environ['WAYLAND_DISPLAY']
if not wayland.startswith('/'):
    wayland = str(Path(os.environ['XDG_RUNTIME_DIR']) / wayland)
socket = str(out / 'runtime/ipc.sock')
env = dict(PATH=os.environ['PATH'], HOME=str(out / 'home'), USER=os.environ.get('USER', ''),
           LANG='C.UTF-8', XDG_RUNTIME_DIR=str(out / 'runtime'), XDG_CONFIG_HOME=str(out / 'config'),
           XDG_DATA_HOME=str(out / 'data'), WAYLAND_DISPLAY=wayland,
           GDK_BACKEND='wayland', QT_QPA_PLATFORM='wayland', GAVNO_SOCKET=socket,
           GAVNO_DEBUG_INPUT='1', RUST_LOG='info', NO_COLOR='1', TERM='dumb')
client_env = dict(os.environ, GAVNO_SOCKET=socket)

def msg(method, params=None):
    command = [str(binary), 'msg', method]
    if params is not None:
        command.append(json.dumps(params))
    result = subprocess.run(command, env=client_env, capture_output=True, text=True, check=True)
    return json.loads(result.stdout)['result']

def key(name):
    msg('inject_key', dict(key=name, state='tap'))

def until(check, description, timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            value = check()
            if value:
                return value
        except (subprocess.CalledProcessError, FileNotFoundError):
            pass
        if process.poll() is not None:
            raise RuntimeError(f'Compositor exited: see {out / "compositor.log"}')
        time.sleep(.002)
    raise TimeoutError(description)

results = []
with (out / 'compositor.log').open('w') as log:
    process = subprocess.Popen(['dbus-run-session', '--', str(binary), '--winit'], env=env,
                               stdout=log, stderr=log, start_new_session=True)
    try:
        until(lambda: msg('state'), 'IPC startup')
        # The same warm asset-cache protocol is used before and after.
        key('super')
        time.sleep(.5)
        key('escape')
        time.sleep(.6)
        for app in ['zenity', 'konsole']:
            for run in range(args.runs):
                before = msg('state').get('focused')
                key('super')
                for char in app:
                    key(char)
                start = time.monotonic()
                key('return')
                def focused():
                    state = msg('state')
                    focus = state.get('focused_window', state.get('focused'))
                    return focus if focus and focus != before else None
                focus = until(focused, f'{app} focus')
                focused_ms = (time.monotonic() - start) * 1000
                def record():
                    text = re.sub(r'\x1b\[[0-9;]*m', '', (out / 'compositor.log').read_text())
                    lines = [line for line in text.splitlines() if 'app launch' in line and f"'{app}'" in line]
                    return lines[run] if len(lines) > run else None
                line = until(record, f'{app} timing log')
                metrics = {name: float(value) for name, value in re.findall(r'\b(\w+_ms)=([\d.]+)', line)}
                metrics.update(app=app, run=run + 1, focus_poll_ms=focused_ms, focus=focus)
                results.append(metrics)
                print(json.dumps(metrics), flush=True)
                if args.screenshots and run == 0:
                    msg('screenshot', dict(path=str(out / f'{app}.png')))
                # Close through the regular compositor keybinding (Super+Q).
                msg('inject_key', dict(key='super', state='press'))
                key('q')
                msg('inject_key', dict(key='super', state='release'))
                until(lambda: not msg('state').get('focused'), 'window close')
                time.sleep(.6)
    finally:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=10)
(out / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
for app in ['zenity', 'konsole']:
    subset = [row for row in results if row['app'] == app]
    for name in ['dispatch_ms', 'spawn_ms', 'commit_ms', 'mapped_ms', 'presented_ms', 'commit_to_present_ms', 'focus_poll_ms']:
        if not all(name in row for row in subset):
            continue
        values = [row[name] for row in subset]
        print(f'{app} {name}: median={statistics.median(values):.3f} worst={max(values):.3f}')

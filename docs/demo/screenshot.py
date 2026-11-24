"""Screenshot cleaner's GUI on a private Xvfb display.
Usage: screenshot.py OUT.png [PLAN_ID]   (env: HOME = seeded demo home, PATH has cleaner)"""

import os
import signal
import subprocess
import sys
import time

out, plan = sys.argv[1], sys.argv[2:]
disp = ":97"
xvfb = subprocess.Popen(["Xvfb", disp, "-screen", "0", "900x700x24", "-nolisten", "tcp"],
                        start_new_session=True)
time.sleep(1.5)
env = dict(os.environ, DISPLAY=disp, GDK_BACKEND="x11", GSK_RENDERER="cairo")
env.pop("WAYLAND_DISPLAY", None)
gui = subprocess.Popen(["dbus-run-session", "--", "cleaner", "gui", *plan], env=env,
                       start_new_session=True)
try:
    time.sleep(6)
    subprocess.run(["import", "-display", disp, "-window", "root", out], check=True, timeout=20)
finally:
    for p in (gui, xvfb):
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        p.wait()

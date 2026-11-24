"""Plays the README demo inside `asciinema rec`: types each command, runs it
in a pty and answers apply's confirmation like a person would."""

import os
import pty
import sys
import time


def out(s):
    sys.stdout.write(s)
    sys.stdout.flush()


def type_cmd(cmd):
    out("$ ")
    time.sleep(0.6)
    for ch in cmd:
        out(ch)
        time.sleep(0.06)
    time.sleep(0.4)
    out("\r\n")


def run(cmd, answer=None):
    type_cmd(" ".join(cmd))
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(cmd[0], cmd)
    seen = b""
    while True:
        try:
            data = os.read(fd, 4096)
        except OSError:  # EIO once the child exits
            break
        if not data:
            break
        os.write(1, data)
        seen += data
        if answer and b"[y/N]" in seen:
            time.sleep(1.2)
            os.write(fd, answer.encode())
            time.sleep(0.3)
            os.write(fd, b"\n")
            answer = None
    os.waitpid(pid, 0)


def clear():
    out("\x1b[2J\x1b[H")


run(["cleaner", "plan"])
time.sleep(3.5)
clear()
run(["cleaner", "show"])
time.sleep(7)
clear()
run(["cleaner", "apply"], answer="y")
time.sleep(5)

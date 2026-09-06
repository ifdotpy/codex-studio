#!/usr/bin/env python3
"""Acquire the controlling PTY before replacing this helper with the user's shell."""

import fcntl
import os
import sys
import termios

fcntl.ioctl(0, termios.TIOCSCTTY, 0)
shell = sys.argv[1]
os.execvpe(shell, [shell, "-l"], os.environ)

#!/usr/bin/env python3
"""Run repository desktop npm/Tauri commands without ambient signing authority."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys


def local_build_environment(inherited):
    env = {name: value for name, value in inherited.items()
           if not name.startswith(('APPLE_', 'TAURI_SIGNING_'))}
    env['APPLE_SIGNING_IDENTITY'] = '-'
    return env


def command(action, extra):
    commands = {
        'install': ['ci'], 'frontend-build': ['run', 'build'], 'frontend-test': ['run', 'test'],
        'build': ['exec', 'tauri', 'build', '--', '--locked', '--no-bundle'],
        'bundle': ['exec', 'tauri', 'bundle', '--'],
    }
    if action not in commands:
        raise ValueError('unsupported desktop action')
    if action not in {'build', 'bundle'} and extra:
        raise ValueError('frontend actions do not accept extra arguments')
    return [shutil.which('npm') or 'npm', *commands[action], *extra]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--desktop', type=Path, required=True)
    parser.add_argument('action', choices=['install', 'frontend-build', 'frontend-test', 'build', 'bundle'])
    parser.add_argument('extra', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    extra = args.extra[1:] if args.extra[:1] == ['--'] else args.extra
    result = subprocess.run(command(args.action, extra), cwd=args.desktop,
                            env=local_build_environment(os.environ), check=False)
    raise SystemExit(result.returncode)


if __name__ == '__main__':
    main()

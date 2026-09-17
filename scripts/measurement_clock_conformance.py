#!/usr/bin/env python3
"""Read-only explicit installed clock diagnostic; no GUI/catalog/worker workload."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time


def executable_binding(path):
    def identity():
        value = path.stat()
        return {key: getattr(value, key) for key in ('st_dev', 'st_ino', 'st_size', 'st_mtime_ns', 'st_ctime_ns')}
    before = identity()
    with path.open('rb') as source:
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
    if identity() != before:
        raise ValueError('Executable changed during hash')
    return {'identity': before, 'sha256': digest}


class BoundedResponseError(ValueError):
    def __init__(self, message, partial):
        super().__init__(message)
        self.partial = bytes(partial[:4096])


def read_bounded_line(stream, seconds=5):
    deadline = time.monotonic() + seconds
    line = bytearray()
    while len(line) < 4096:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([stream], [], [], remaining)[0]:
            raise BoundedResponseError('Clock diagnostic response timeout', line)
        chunk = os.read(stream.fileno(), 4096 - len(line))
        if not chunk:
            raise BoundedResponseError('Clock diagnostic closed before response', line)
        line.extend(chunk)
        if b'\n' in line:
            if not line.endswith(b'\n') or line.count(b'\n') != 1:
                raise BoundedResponseError('Unrequested clock diagnostic output', line)
            return bytes(line)
    raise BoundedResponseError('Clock diagnostic response exceeds bound', line)


def diagnose(executable):
    rows, process = [], None
    clock = time.get_clock_info('monotonic')
    result = {'verdict': 'FAILED_CLOCK_CONFORMANCE', 'executable': str(executable), 'rows': rows,
              'python': sys.version, 'platform': sys.platform, 'clock': vars(clock)}
    try:
        if sys.platform != 'darwin' or clock.implementation != 'mach_absolute_time()':
            raise ValueError('Requires qualified macOS Python monotonic clock')
        result['executable_before'] = executable_binding(executable)
        process = subprocess.Popen([str(executable), '--s12-clock-diagnostic'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        session = None
        for index in range(1, 17):
            before = time.monotonic_ns()
            process.stdin.write(b'anchor\n'); process.stdin.flush()
            raw = read_bounded_line(process.stdout)
            after = time.monotonic_ns()
            try:
                native = json.loads(raw)
            except (ValueError, UnicodeError) as error:
                raise BoundedResponseError(f"Invalid diagnostic JSON: {error}", raw) from error
            rows.append({'before_ns': str(before), 'native': native, 'after_ns': str(after)})
            if not isinstance(native['monotonic_ns'], str) or not native['monotonic_ns'].isdigit():
                raise ValueError('Native clock timestamp must be lossless decimal string')
            ns = int(native['monotonic_ns'])
            if not (native['clock'] == 'macos_mach_absolute_ns' and native['native_pid'] == process.pid
                    and native['anchor_id'] == index and native['run_id'] == 'conformance' and before <= ns <= after
                    and native['session_id'] and (session is None or session == native['session_id'])):
                raise ValueError('Native clock identity or Python causal bracket mismatch')
            session = native['session_id']
        if process.wait(timeout=5) != 0:
            raise ValueError('Clock diagnostic failed')
        result['executable_after'] = executable_binding(executable)
        if result['executable_after'] != result['executable_before']:
            raise ValueError('Executable changed during diagnostic')
        result['verdict'] = 'PASS_CLOCK_CONFORMANCE_ONLY'
    except Exception as error:
        result['error'] = f'{type(error).__name__}: {error}'
        if isinstance(error, BoundedResponseError):
            result['failed_response'] = {'encoding': 'hex', 'bytes': len(error.partial), 'data': error.partial.hex(), 'limit_bytes': 4096}
    finally:
        if process is not None:
            if process.poll() is None:
                process.kill(); process.wait(timeout=5)
            result['child_exit'] = process.returncode
            for stream in (process.stdin, process.stdout):
                stream.close()
        if 'executable_before' in result and 'executable_after' not in result:
            try:
                result['executable_after'] = executable_binding(executable)
            except Exception as error:
                result['executable_after_error'] = f'{type(error).__name__}: {error}'
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--executable', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    with args.output.open('x') as output:
        result = diagnose(args.executable.absolute())
        json.dump(result, output, indent=2); output.write('\n')
    raise SystemExit(0 if result['verdict'] == 'PASS_CLOCK_CONFORMANCE_ONLY' else 2)

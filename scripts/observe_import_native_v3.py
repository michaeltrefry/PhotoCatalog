#!/usr/bin/env python3
"""Read-only, create-new native import-lock evidence; never starts an import."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import stat
import subprocess
import sys
import time
import uuid

from observe_export_native_v2 import (CLOCK, exception_evidence,
                                     lock_contended, same_birth, sha256_path, psutil)

ROLES = {'desktop': '--catalog-desktop-worker', 'filesystem': '--catalog-filesystem-worker'}
BINDINGS = ('root_pid', 'root_birth_unix_s', 'desktop_pid', 'desktop_birth_unix_s',
            'filesystem_pid', 'filesystem_birth_unix_s', 'executable', 'executable_sha256',
            'executable_identity', 'catalog', 'catalog_identity', 'lock_identity', 'import_id', 'source_blake3')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def file_identity(path, directory=False):
    path = Path(path)
    require(path.is_absolute() and path.resolve(strict=True) == path, 'Noncanonical/symlink path')
    value = path.stat(follow_symlinks=False)
    require(stat.S_ISDIR(value.st_mode) if directory else stat.S_ISREG(value.st_mode), 'Wrong file type')
    return [value.st_dev, value.st_ino]


def executable_identity(path):
    file_identity(path)
    value = Path(path).stat(follow_symlinks=False)
    return {name: getattr(value, name) for name in ('st_dev', 'st_ino', 'st_size', 'st_mtime_ns', 'st_ctime_ns', 'st_mode')}


def declaration_digest(binding):
    return hashlib.sha256(json.dumps(binding, sort_keys=True, separators=(',', ':'), ensure_ascii=True, allow_nan=False).encode('ascii')).hexdigest()


def validate_declaration(value):
    require(str(uuid.UUID(value['import_id'])) == value['import_id'], 'Noncanonical import UUID')
    for field in ('executable_sha256', 'source_blake3'):
        require(isinstance(value[field], str) and len(value[field]) == 64
                and all(c in '0123456789abcdef' for c in value[field]), 'Invalid digest')
    require(len({value[f'{role}_pid'] for role in ('root', 'desktop', 'filesystem')}) == 3, 'Distinct process IDs required')
    for role in ('root', 'desktop', 'filesystem'):
        require(type(value[f'{role}_pid']) is int and value[f'{role}_pid'] > 0
                and type(value[f'{role}_birth_unix_s']) in (int, float)
                and 0 < value[f'{role}_birth_unix_s'] < 1e11, 'Invalid process binding')
    for name in ('catalog_identity', 'lock_identity'):
        require(isinstance(value[name], list) and len(value[name]) == 2
                and all(type(v) is int and v >= 0 for v in value[name]), 'Invalid file identity')


def process_proof(binding, role):
    pid, birth = binding[f'{role}_pid'], binding[f'{role}_birth_unix_s']
    process = psutil.Process(pid)
    require(same_birth(pid, birth), f'{role} process birth/lifecycle changed')
    require(Path(process.exe()) == Path(binding['executable']), f'{role} executable path changed')
    require(executable_identity(process.exe()) == binding['executable_identity'], f'{role} executable object changed')
    command = process.cmdline()
    require(bool(command) and Path(command[0]) == Path(binding['executable']), f'{role} argv executable changed')
    if role != 'root':
        require(command[1:] == [ROLES[role]], f'{role} role arguments changed')
        require(process.ppid() == binding['root_pid'], f'{role} is not a direct GUI child')
    else:
        require(not any(arg in ROLES.values() for arg in command[1:]), 'Root is a worker')
    return {'pid': pid, 'birth_unix_s': birth, 'parent_pid': process.ppid(), 'argv': command,
            'executable_identity': executable_identity(process.exe())}


def import_lock_holder(pid, path, identity):
    started = time.monotonic_ns()
    result = subprocess.run(['/usr/sbin/lsof', '-nP', '-FpfinDl', '--', str(path)],
                            capture_output=True, text=True, timeout=1, check=False)
    elapsed = time.monotonic_ns() - started
    require(result.returncode == 0 and not result.stderr and len(result.stdout) <= 16384
            and 0 <= elapsed <= 1_000_000_000, 'lsof failed/overran/output overflow')
    records, current, owner = [], None, None
    for line in result.stdout.splitlines():
        require(bool(line) and line[0] in 'pfinDl', 'Unexpected lsof field')
        key, value = line[0], line[1:]
        if key == 'p':
            owner = int(value)
        elif key == 'f':
            current = {'pid': owner, 'fd': value}
            records.append(current)
        else:
            require(current is not None and key not in current, 'Unbound/duplicate lsof field')
            current[key] = value
    # Darwin reports a blank lock field even for an exclusive flock. Do not
    # invent a kernel owner marker: require F to be the sole visible open owner
    # of this exact inode, plus independent exclusive contention around lsof.
    require(0 < len(records) <= 128 and all(r.get('pid') == pid and r.get('n') == str(path)
            and r.get('i') == str(identity[1]) and int(r.get('D', '-1'), 16) == identity[0]
            and r.get('fd', '').isdigit() and r.get('l') in (' ', 'W') for r in records),
            'Exact filesystem worker is not sole visible lock-file owner')
    require(len({r['fd'] for r in records}) == len(records), 'Duplicate lsof descriptor')
    return {'pid': pid, 'path': str(path), 'device_inode': identity,
            'sole_visible_owner': True, 'descriptors': [{'fd': r['fd'], 'lock_field': r['l']} for r in records],
            'elapsed_ns': elapsed}


def unique_role_owners(binding):
    """A fresh one-catalog GUI must have exactly one C and one F owner."""
    found = {role: [] for role in ROLES}
    for process in psutil.Process(binding['root_pid']).children(recursive=False):
        command = process.cmdline()
        for role, flag in ROLES.items():
            if flag in command:
                require(command == [binding['executable'], flag], 'Ambiguous role command')
                require(process.ppid() == binding['root_pid'], 'Role owner reparented')
                found[role].append({'pid': process.pid, 'birth_unix_s': process.create_time()})
    expected = {role: [{'pid': binding[f'{role}_pid'], 'birth_unix_s': binding[f'{role}_birth_unix_s']}] for role in ROLES}
    require(found == expected, 'Ambiguous/missing GUI catalog-worker owners')
    return found


def positive_probe(binding):
    """All evidence is rechecked around lsof; no cached holder/ancestry claim."""
    before = time.monotonic_ns()
    require(executable_identity(binding['executable']) == binding['executable_identity'], 'Executable changed')
    require(file_identity(binding['catalog'], True) == binding['catalog_identity'], 'Catalog replaced')
    lock = Path(binding['catalog']) / 'import.lock'
    require(file_identity(lock) == binding['lock_identity'], 'Import lock replaced')
    processes = {role: process_proof(binding, role) for role in ('root', 'desktop', 'filesystem')}
    owners = unique_role_owners(binding)
    if not lock_contended(lock, tuple(binding['lock_identity'])):
        return None
    holder = import_lock_holder(binding['filesystem_pid'], lock, binding['lock_identity'])
    # Recheck all identities after lsof so a replacement/exit during the external query is never positive.
    require(file_identity(binding['catalog'], True) == binding['catalog_identity'], 'Catalog changed after lsof')
    require(file_identity(lock) == binding['lock_identity'], 'Import lock changed after lsof')
    require(executable_identity(binding['executable']) == binding['executable_identity'], 'Executable changed after lsof')
    for role in ('root', 'desktop', 'filesystem'):
        require(process_proof(binding, role) == processes[role], 'Process identity changed after lsof')
    require(unique_role_owners(binding) == owners, 'GUI role ownership changed after lsof')
    if not lock_contended(lock, tuple(binding['lock_identity'])):
        return None
    return {'positive_before_monotonic_ns': before, 'positive_after_monotonic_ns': time.monotonic_ns(),
            'processes': processes, 'role_owners': owners, 'lock_identity': binding['lock_identity'], 'holder': holder,
            'lock_contended_before': True, 'lock_contended_after': True}


def observe(binding, emit, seconds=600, interval=.2):
    """One retained F lease. First gap retires it; no segment reopening."""
    validate_declaration(binding)
    require(0 < seconds <= 600 and .05 <= interval <= 1, 'Observer bounds')
    start = time.monotonic_ns()
    deadline = start + round(seconds * 1e9)
    emit({'kind': 'identity', 'protocol': 3, 'clock': CLOCK, 'profile': 'import_v1',
          **{key: binding[key] for key in BINDINGS}, 'monotonic_ns': start,
          'seconds': seconds, 'interval': interval, 'python_version': sys.version,
          'psutil_version': psutil.__version__, 'declaration_sha256': declaration_digest(binding), 'clock_implementation': time.get_clock_info('monotonic').implementation})
    segments, fatal, gaps, positive = [], 0, 0, 0
    current = None

    def close(reason, at):
        nonlocal current
        if current is not None:
            current.update(closed_reason=reason, closed_monotonic_ns=at)
            emit({'kind': 'segment_closed', 'segment': 1, 'reason': reason, 'monotonic_ns': at})
            current = None

    try:
        require(sys.platform == 'darwin' and time.get_clock_info('monotonic').implementation == 'mach_absolute_time()', 'Unqualified native clock')
        require(sha256_path(Path(binding['executable'])) == binding['executable_sha256'], 'Executable SHA mismatch')
        while time.monotonic_ns() < deadline:
            # Reserve lsof's entire timeout rather than deliberately starting a
            # probe too late to qualify. This only shortens the admitted window.
            if deadline - time.monotonic_ns() <= 1_100_000_000:
                break
            proof = positive_probe(binding)
            if proof is None:
                now = time.monotonic_ns()
                if current is not None:
                    close('lock_not_contended', now)
                    emit({'kind': 'observation_gap', 'reason': 'lock_not_contended', 'monotonic_ns': now})
                    gaps += 1
                    break
                emit({'kind': 'waiting', 'reason': 'lock_not_contended', 'monotonic_ns': now})
            else:
                # Late external/system calls cannot extend the declared window.
                if proof['positive_after_monotonic_ns'] > deadline:
                    raise RuntimeError('Positive probe exceeded observer deadline')
                positive += 1
                if current is None:
                    current = {'segment': 1, 'first_positive_after_monotonic_ns': proof['positive_after_monotonic_ns'],
                               'last_positive_before_monotonic_ns': None, 'positive_observations': 1}
                    segments.append(current)
                    kind = 'segment_admitted'
                else:
                    current['last_positive_before_monotonic_ns'] = proof['positive_before_monotonic_ns']
                    current['positive_observations'] += 1
                    kind = 'active'
                emit({'kind': kind, 'segment': 1, **proof})
            remaining = deadline - time.monotonic_ns()
            if remaining > 0:
                time.sleep(min(interval, remaining / 1e9))
    except Exception as error:
        # Retire before diagnostics. Permission and unknown extension errors are
        # fatal here: retained F is long-lived; an error is never exit evidence.
        now = time.monotonic_ns()
        close('error', now)
        fatal += 1
        emit({'kind': 'error', 'operation': 'observation', 'monotonic_ns': now,
              'error': f'{type(error).__name__}: {error}', 'exception_chain': exception_evidence(error)})
    end = time.monotonic_ns()
    close('observer_end', end)
    unchanged, root_same = False, False
    try:
        unchanged = (executable_identity(binding['executable']) == binding['executable_identity']
                     and sha256_path(Path(binding['executable'])) == binding['executable_sha256'])
        root_same = same_birth(binding['root_pid'], binding['root_birth_unix_s'])
        require(unchanged and root_same, 'Final executable/root identity changed')
    except Exception as error:
        fatal += 1
        emit({'kind': 'error', 'operation': 'final_identity', 'monotonic_ns': time.monotonic_ns(),
              'error': f'{type(error).__name__}: {error}', 'exception_chain': exception_evidence(error)})
    summary = {'kind': 'summary', 'protocol': 3, 'clock': CLOCK, 'fatal_errors': fatal,
               'executable_unchanged': unchanged, 'root_same_birth': root_same,
               'measurement_end_monotonic_ns': end, 'segments': segments,
               'positive_observations': positive, 'observation_gaps': gaps,
               'usable_segments': sum(s['last_positive_before_monotonic_ns'] is not None for s in segments)}
    emit(summary)
    return 0 if fatal == 0 and summary['usable_segments'] else 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--declaration', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--seconds', type=float, default=600)
    parser.add_argument('--interval', type=float, default=.2)
    args = parser.parse_args()
    binding = json.loads(args.declaration.read_text())
    with args.output.open('x') as output:
        def emit(row):
            output.write(json.dumps(row, separators=(',', ':'), sort_keys=True) + '\n')
            output.flush()
        try:
            return observe(binding, emit, args.seconds, args.interval)
        except Exception as error:
            emit({'kind': 'error', 'operation': 'preflight', 'monotonic_ns': time.monotonic_ns(),
                  'error': f'{type(error).__name__}: {error}', 'exception_chain': exception_evidence(error)})
            emit({'kind': 'summary', 'protocol': 3, 'clock': CLOCK, 'fatal_errors': 1, 'usable_segments': 0})
            return 2


if __name__ == '__main__':
    raise SystemExit(main())

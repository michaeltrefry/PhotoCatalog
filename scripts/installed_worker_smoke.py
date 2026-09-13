#!/usr/bin/env python3
"""Run the synthetic service probe outside the checkout with a clean loader env."""
import argparse
import json
import os
from pathlib import Path
import tempfile
import package_desktop as package


def environment(inherited, platform):
    env = {k: v for k, v in inherited.items()
           if not k.startswith(('LD_', 'DYLD_', 'APPLE_', 'TAURI_SIGNING_'))
           and k not in {'LIBRARY_PATH', 'CPATH', 'PKG_CONFIG_PATH', 'VCPKG_ROOT'}}
    if platform == 'windows':
        system = Path(env['SystemRoot'])
        env['PATH'] = os.pathsep.join(str(system/p) for p in ('System32', ''))
    else:
        env['PATH'] = '/usr/bin:/bin:/usr/sbin:/sbin'
    return env


def run(probe, executable, output, platform):
    probe, executable = Path(probe).resolve(strict=True), Path(executable).resolve(strict=True)
    output = Path(output).absolute()
    output.mkdir(parents=True, exist_ok=False)
    pins = {'probe': package.sha256(probe), 'executable': package.sha256(executable)}
    # Reuse the repository's reviewed process-tree owner and bounded captures.
    from edit_campaign import invoke
    inherited, previous_cwd = dict(os.environ), Path.cwd()
    try:
        with tempfile.TemporaryDirectory(prefix='photocatalog-installed-cwd-') as cwd:
            os.environ.clear()
            os.environ.update(environment(inherited, platform))
            os.chdir(cwd)
            owned = invoke([str(probe), '--worker-executable', str(executable)], output/'invoke',
                {'deadline_seconds': 120, 'process_rss_bytes': 1024**3,
                 'group_rss_bytes': 2*1024**3, 'free_reserve_bytes': 64*1024**2}, output)
    finally:
        os.chdir(previous_cwd)
        os.environ.clear()
        os.environ.update(inherited)
    stdout = (output/'invoke/stdout.log').read_bytes()
    package.require(len(stdout) <= 65536, 'probe output bounds')
    value = json.loads(stdout)
    package.require(value['status'] == 'PASS_INSTALLED_PREVIEW_AND_EXPORT_WORKERS', 'probe did not pass')
    package.require(Path(value['worker_executable']) == executable and value['temporary_state_removed'] is True,
                    'probe executable/cleanup association')
    package.require(pins == {'probe': package.sha256(probe), 'executable': package.sha256(executable)},
                    'executables changed during probe')
    result = {'status': 'PASS_INSTALLED_WORKERS_ONLY', 'pins': pins, 'ownership': owned['ownership'],
              'loader_environment_sanitized': True, 'cwd_outside_checkout': True,
              'gui_tested': False, 'observation': value}
    package.write_json(output/'result.json', result)
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe', required=True, type=Path)
    parser.add_argument('--executable', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--platform', required=True, choices=['macos', 'linux', 'windows'])
    args = parser.parse_args()
    run(args.probe, args.executable, args.output, args.platform)

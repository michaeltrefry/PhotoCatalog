#!/usr/bin/env python3
"""Run the synthetic service probe outside the checkout with a clean loader env."""
import argparse
import json
import os
from pathlib import Path, PureWindowsPath
import tempfile
import package_desktop as package


def environment(inherited, platform):
    # Windows os.environ is case-insensitive; its plain-dict snapshot is not.
    if platform == 'windows':
        inherited = {key.upper(): value for key, value in inherited.items()}
    env = {k: v for k, v in inherited.items()
           if not k.startswith(('LD_', 'DYLD_', 'APPLE_', 'TAURI_SIGNING_'))
           and k not in {'LIBRARY_PATH', 'CPATH', 'PKG_CONFIG_PATH', 'VCPKG_ROOT'}}
    if platform == 'windows':
        package.require(env.get('SYSTEMROOT', '').strip(), 'Windows smoke environment missing SYSTEMROOT')
        system = PureWindowsPath(env['SYSTEMROOT'])
        env['PATH'] = ';'.join(str(system/p) for p in ('System32', ''))
    else:
        env['PATH'] = '/usr/bin:/bin:/usr/sbin:/sbin'
    return env


def run(probe, executable, output, platform):
    probe, executable = Path(probe).resolve(strict=True), Path(executable).resolve(strict=True)
    output = Path(output).absolute()
    output.mkdir(parents=True, exist_ok=False)
    output = output.resolve(strict=True)
    pins = {'probe': package.sha256(probe), 'executable': package.sha256(executable)}
    # Reuse the repository's reviewed process-tree owner and bounded captures.
    from edit_campaign import invoke,storage_identity
    inherited, previous_cwd = dict(os.environ), Path.cwd()
    with tempfile.TemporaryDirectory(prefix='photocatalog-installed-cwd-') as cwd:
        try:
            os.environ.clear()
            os.environ.update(environment(inherited, platform))
            os.chdir(cwd)
            owned = invoke([str(probe), '--worker-executable', str(executable)], output/'invoke',
                {'deadline_seconds': 120, 'process_rss_bytes': 1024**3,
                 'group_rss_bytes': 2*1024**3,
                 'storage': {'artifact': {'root': str(output), **storage_identity(output),
                                          'reserve_bytes': 64*1024**2}}}, {'artifact': output})
        finally:
            # Windows cannot remove a process's current directory. Restore it
            # before TemporaryDirectory cleanup, including failed child runs.
            os.chdir(previous_cwd)
            os.environ.clear()
            os.environ.update(inherited)
    stdout = (output/'invoke/stdout.log').read_bytes()
    package.require(len(stdout) <= 65536, 'probe output bounds')
    value = json.loads(stdout)
    package.require(value['status'] == 'PASS_INSTALLED_PREVIEW_AND_EXPORT_WORKERS', 'probe did not pass')
    # Rust canonicalize may report a Windows verbatim path (\\?\...) while
    # Python resolved the same file without that prefix. Compare the file object,
    # preserving the executable hashes below, rather than comparing spellings.
    reported = value.get('worker_executable')
    package.require(isinstance(reported, str) and Path(reported).is_absolute(),
                    'probe executable association requires an absolute path')
    try:
        same_executable = Path(reported).samefile(executable)
    except (OSError, ValueError) as error:
        raise package.PackageError('probe executable association could not be verified') from error
    package.require(same_executable, 'probe executable association')
    package.require(value['temporary_state_removed'] is True, 'probe temporary state cleanup')
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

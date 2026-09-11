"""Supplementary one-attempt full/paths/packets owner; frozen runner unchanged.

Explicit recipe and grant required. No implicit next phase, selection, migration,
initialization, adoption, failed retry, or pixel work. macOS ownership primitives
are loaded only from their exact independently reviewed source bytes.
"""
import argparse
import ast
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import signal
import subprocess
import sys
import threading
import time
import types
import uuid

C = None
MIB = 1024**2
SUPERVISOR_SHA = 'cd4ebbda6eaab270d33e5a7b310966762bbde3044745e58514b0789388dffc14'
MAX_COMMANDS = 20000
MAX_METADATA = 16*MIB
MAX_PHASES = 4096
MAX_KNOWN = 20000
MAX_ACTIVE = 8
LOG_CAP = 4*MIB


def bootstrap(ref):
    # Verify before executing any non-stdlib helper, including the contract file.
    if set(ref) != {'path', 'sha256'}:
        raise ValueError('unresolved source descriptor')
    path = Path(ref['path'])
    if not path.is_absolute() or any(p.is_symlink() for p in [path, *path.parents]):
        raise ValueError('source path alias')
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        meta = os.fstat(stream.fileno())
        if not stat.S_ISREG(meta.st_mode) or meta.st_size > 16*MIB:
            raise ValueError('source size/type bound')
        value = stream.read(16*MIB+1)
    if len(value) > 16*MIB or hashlib.sha256(value).hexdigest() != ref['sha256']:
        raise ValueError('source hash differs')
    return value


def module(ref, name):
    value = bootstrap(ref)
    result = types.ModuleType(name); result.__file__ = ref['path']
    exec(compile(value, ref['path'], 'exec'), result.__dict__)
    return result


def save(path, value, replace=False):
    data = C.encoded(value)
    if len(data) > MAX_METADATA:
        raise ValueError('control document bound')
    pending = path.with_name(path.name+'.pending-'+uuid.uuid4().hex)
    with pending.open('xb') as stream:
        stream.write(data); stream.flush(); os.fsync(stream.fileno())
    if replace:
        os.replace(pending, path)
    else:
        os.link(pending, path); pending.unlink()
    sync(path.parent)


def sync(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try: os.fsync(fd)
    finally: os.close(fd)


def read(path):
    return json.loads(C.raw(path))


def validate_temp_storage(recipe):
    """Sample the bound temp destination; this is not filesystem confinement."""
    if 'temp_storage' not in recipe:
        return
    storage = recipe['temp_storage']
    if not isinstance(storage, dict) or set(storage) != {'directory', 'device', 'inode', 'environment'}:
        raise ValueError('invalid temp_storage descriptor')
    directory = C.absolute(storage['directory'])
    if str(directory) != storage['directory']:
        raise ValueError('canonical temp_storage directory required')
    expected = {name: str(directory) for name in ['SQLITE_TMPDIR', 'TMPDIR']}
    if storage['environment'] != expected or any(os.environ.get(k) != v for k, v in expected.items()):
        raise ValueError('temp_storage environment missing or different')
    C.integer(storage['device'], 0); C.integer(storage['inode'], 1)
    meta = directory.lstat()
    if (not stat.S_ISDIR(meta.st_mode) or (meta.st_dev, meta.st_ino) != (storage['device'], storage['inode'])
            or not os.access(directory, os.W_OK | os.X_OK)):
        raise ValueError('temp_storage directory identity/type/access changed')
    if C.absolute(recipe['run']).stat().st_dev != meta.st_dev:
        raise ValueError('temp_storage and run must share a device')


CANONICAL_FUNCTIONS = ['_small_canonical_budget', 'update_canonical_hash']
REPLAY_FUNCTIONS = CANONICAL_FUNCTIONS + ['read_json']
LEGACY_FULL_CONTROLLER_SHA = '969eeaadb4e4693ddb12a6dbcd03c36c0d38991e7697a1994cd7210d38a865bd'


def canonical_hash_profile(recipe):
    """Validate a separately reviewed optimization, never change the base binding."""
    ref = recipe.get('canonical_hash_profile')
    if ref is None:
        if 'canonical_hash_profile' in recipe or 'canonical_hash_transition' in recipe:
            raise ValueError('unresolved canonical profile/transition')
        return None
    if recipe['phase'] not in {'full', 'paths', 'packets'}:
        raise ValueError('canonical profile phase unsupported')
    profile = C.document(ref)
    fields = {'protocol', 'kind', 'base_driver', 'helper', 'functions', 'equivalence_review', 'runtime'}
    expected_functions = CANONICAL_FUNCTIONS if profile.get('protocol') == 1 else REPLAY_FUNCTIONS
    expected_kind = 'canonical_hash_override' if profile.get('protocol') == 1 else 'canonical_hash_and_replay_json_override'
    if (set(profile) != fields or type(profile['protocol']) is not int or profile['protocol'] not in {1,2} or profile['kind'] != expected_kind
            or profile['base_driver'] != recipe['code']['runner'] or profile['runtime'] != recipe['code']['python']
            or profile['functions'] != expected_functions):
        raise ValueError('canonical hash profile identity differs')
    raw = bootstrap(profile['helper'])
    if len(raw) > 128*1024:
        raise ValueError('canonical helper source bound')
    review = C.document(profile['equivalence_review'])
    assertions = {'canonical_bytes_equal': True, 'bounded_fast_path': True, 'fallback_preserved': True}
    if profile['protocol'] == 2:
        assertions.update(read_json_equivalent=True, raw_bytes_released_before_parse=True)
    if (review.get('status') != 'PASS' or review.get('base_driver') != profile['base_driver']
            or review.get('helper') != profile['helper'] or review.get('runtime') != profile['runtime']
            or review.get('assertions') != assertions
            or not isinstance(review.get('author'), str) or not 1 <= len(review['author'].encode()) <= 256):
        raise ValueError('canonical helper equivalence/resource review missing or mismatched')
    canonical_functions(raw, profile['helper']['path'], expected_functions)  # Reject executable module scaffolding before admission.
    return profile


def canonical_functions(raw, filename, expected_functions=None):
    """Compile only the reviewed function roster with explicit json globals."""
    if expected_functions is None: expected_functions = CANONICAL_FUNCTIONS
    if expected_functions not in (CANONICAL_FUNCTIONS, REPLAY_FUNCTIONS): raise ValueError('unreviewed helper roster')
    tree = ast.parse(raw, filename)
    definitions = []
    for node in tree.body:
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant) and isinstance(node.value.value, str):
            continue
        if isinstance(node, ast.Import) and len(node.names) == 1 and node.names[0].name == 'json' and node.names[0].asname is None:
            continue
        if not isinstance(node, ast.FunctionDef) or node.name not in expected_functions or node.decorator_list:
            raise ValueError('canonical helper contains unapproved module statements')
        for value in [*node.args.defaults, *[v for v in node.args.kw_defaults if v is not None]]:
            try: ast.literal_eval(value)
            except (ValueError, TypeError): raise ValueError('canonical helper executable default') from None
        if node.returns is not None or any(v.annotation is not None for v in [*node.args.posonlyargs, *node.args.args, *node.args.kwonlyargs, *([node.args.vararg] if node.args.vararg else []), *([node.args.kwarg] if node.args.kwarg else [])]):
            raise ValueError('canonical helper annotations are not admitted')
        definitions.append(node)
    if sorted(n.name for n in definitions) != sorted(expected_functions):
        raise ValueError('canonical helper definition roster differs')
    scope = {'json': json}
    exec(compile(ast.Module(body=definitions, type_ignores=[]), filename, 'exec'), scope)
    return scope


def canonical_invariants(recipe):
    # The only source change permitted at the first transition is the controller
    # plus its separately attributed pure encoder profile. Everything else stays.
    omitted = {'attempt_id', 'previous', 'journal', 'expected_next_command', 'pause', 'grant',
               'canonical_hash_profile', 'canonical_hash_transition'}
    value = {k: v for k, v in recipe.items() if k not in omitted}
    value['code'] = {k: v for k, v in recipe['code'].items() if k != 'controller'}
    return value


def admit_canonical_previous(recipe, result, review):
    profile = canonical_hash_profile(recipe)
    if profile is None:
        if 'execution_profile' in result or 'execution_profile' in review:
            raise ValueError('canonical execution profile cannot be silently removed')
        return
    old_attempt = Path(recipe['previous']['result']['path']).parent
    old_recipe_ref = C.reference(old_attempt/'recipe.json'); old_recipe = C.document(old_recipe_ref)
    old_fixed = canonical_invariants(old_recipe); new_fixed = canonical_invariants(recipe)
    if old_recipe.get('canonical_hash_profile') is not None and old_recipe['phase'] != recipe['phase']:
        # Existing admit_previous verifies the exact successful phase/output;
        # validate_recipe separately checks new phase inputs and funding.
        for key in ['phase', 'input', 'baseline', 'paths_review', 'funding']:
            old_fixed.pop(key, None); new_fixed.pop(key, None)
    if old_fixed != new_fixed:
        raise ValueError('canonical transition changed fixed input/funding/runtime/temp scope')
    if old_recipe.get('canonical_hash_profile') is not None:
        if ('canonical_hash_transition' in recipe or old_recipe['canonical_hash_profile'] != recipe['canonical_hash_profile']
                or old_recipe['code']['controller'] != recipe['code']['controller']):
            raise ValueError('canonical continuation profile/controller differs')
        proof_ref = result.get('execution_profile')
        if proof_ref is None or review.get('execution_profile') != proof_ref:
            raise ValueError('prior canonical execution profile review missing')
        validate_execution_profile(old_recipe, old_attempt, result)
    else:
        if (recipe['phase'] != 'full' or old_recipe['phase'] != 'full'
                or 'canonical_hash_profile' in old_recipe or 'execution_profile' in result or 'execution_profile' in review
                or old_recipe['code']['controller']['sha256'] != LEGACY_FULL_CONTROLLER_SHA
                or result['status'] != 'paused_at_command_boundary' or result.get('cleanup') is not None):
            raise ValueError('canonical transition requires exact clean legacy FULL pause')
        if profile['protocol'] != 1:
            raise ValueError('legacy transition admits only protocol1; protocol2 requires a separately reviewed execution owner')
        transition = C.document(recipe['canonical_hash_transition'])
        expected = {'status': 'PASS', 'kind': 'legacy_full_pause_to_canonical_hash_profile',
                    'profile': recipe['canonical_hash_profile'], 'previous': recipe['previous'],
                    'previous_recipe': old_recipe_ref, 'base_binding': recipe['binding'],
                    'journal': recipe['journal'], 'expected_next_command': recipe['expected_next_command'],
                    'invariants_sha256': C.sha(C.encoded(canonical_invariants(recipe)))}
        if transition != expected:
            raise ValueError('exact independent canonical transition review required')


def execution_profile_value(recipe, attempt):
    return {'protocol': 1, 'status': 'AUTHORIZED_BEFORE_DISPATCH', 'kind': 'effective_python_execution',
            'base_binding': recipe['binding'], 'base_driver': recipe['code']['runner'],
            'canonical_hash_profile': recipe['canonical_hash_profile'], 'controller': recipe['code']['controller'],
            'runtime': recipe['code']['python'], 'recipe': C.reference(attempt/'recipe.json'),
            'previous': recipe['previous'], 'phase': recipe['phase'], 'input': recipe['input'],
            'baseline': recipe['baseline'], 'first_command': recipe['expected_next_command'],
            'attempt_id': recipe['attempt_id']}


def validate_execution_profile(recipe, attempt, result=None):
    profile = canonical_hash_profile(recipe)
    if profile is None:
        return None
    expected = execution_profile_value(recipe, attempt)
    ref = C.reference(attempt/'execution-profile.json')
    if C.document(ref) != expected:
        raise ValueError('effective execution authorization differs')
    if result is not None:
        consumed_ref = C.reference(attempt/'execution-profile-consumed.json')
        consumed = C.document(consumed_ref); process = read(attempt/'process.json')
        if (result.get('execution_profile') != ref or result.get('execution_profile_consumed') != consumed_ref
                or consumed != {'execution_profile': ref, 'helper': profile['helper'], 'pid': process['pid']}
                or result.get('next_command', 0) < recipe['expected_next_command']):
            raise ValueError('effective execution consumption/result differs')
    return ref


def install_canonical_profile(recipe, attempt, frozen):
    profile = canonical_hash_profile(recipe)
    if profile is None:
        return
    ref = validate_execution_profile(recipe, attempt)
    # Keep frozen.__file__, encoded(), Runner and all native/replay guards intact.
    # New behavior is explicitly attributed by the separate effective profile.
    scope = canonical_functions(bootstrap(profile['helper']), profile['helper']['path'], profile['functions'])
    frozen.update_canonical_hash = scope['update_canonical_hash']
    if profile['protocol'] == 2:
        frozen.read_json = scope['read_json']
    save(attempt/'execution-profile-consumed.json', {'execution_profile': ref, 'helper': profile['helper'], 'pid': os.getpid()})


def validate_recipe(recipe):
    fields = {'protocol', 'phase', 'run', 'control', 'attempt_id', 'input', 'baseline', 'paths_review',
              'code', 'config', 'binding', 'journal', 'expected_next_command', 'previous', 'pause',
              'funding', 'memory', 'grant'}
    optional = {'temp_storage', 'canonical_hash_profile', 'canonical_hash_transition'}
    if not fields <= set(recipe) or set(recipe)-fields-optional or recipe['protocol'] != 1 or any(v is None for v in recipe.values()):
        raise ValueError('unresolved/unknown recipe fields')
    if str(uuid.UUID(recipe['attempt_id'])) != recipe['attempt_id']:
        raise ValueError('invalid attempt UUID')
    for name in ['run', 'control']: C.absolute(recipe[name])
    validate_temp_storage(recipe)
    C.integer(recipe['expected_next_command'], 1)
    if set(recipe['memory']) != {'python_process_rss_bytes', 'native_process_rss_bytes', 'combined_owned_rss_bytes'}:
        raise ValueError('explicit sampled memory ceilings required')
    for v in recipe['memory'].values(): C.integer(v, 1)
    if set(recipe['code']) != {'controller', 'contract', 'runner', 'helper', 'funding_guard', 'supervisor', 'python'}:
        raise ValueError('incomplete source/runtime bindings')
    run = Path(recipe['run']); code = recipe['code']
    if code['controller']['path'] != str(Path(__file__).resolve()):
        raise ValueError('executing controller differs')
    if Path(code['helper']['path']) != Path(code['runner']['path']).with_name('lightroom_generation.py'):
        raise ValueError('frozen helper location differs')
    if code['supervisor']['sha256'] != SUPERVISOR_SHA:
        raise ValueError('unreviewed ownership primitive')
    for ref in code.values(): C.checked(ref)
    if recipe['config']['path'] != str(run/'config.json') or recipe['binding']['path'] != str(run/'binding.json'):
        raise ValueError('wrong run binding paths')
    config = C.document(recipe['config']); binding = C.document(recipe['binding'])
    if (config['exclusive_output'] != str(run) or config['automatic_choose'] is not False or config['automatic_migration'] is not False
            or config['closed_application_evidence'] is not None or binding['driver_sha256'] != code['runner']['sha256']
            or binding['generation_driver_sha256'] != code['helper']['sha256']
            or binding['config_sha256'] != C.sha(C.encoded(config))
            or binding['source'] != config['source_commit'] or binding['binary_sha256'] != config['tested_binary_sha256']):
        raise ValueError('frozen config/native/source binding differs')
    # Native bytes are validated by the frozen Runner before any command. This
    # control never copies/substitutes that executable or guesses a new identity.
    if recipe['journal']['path'] != str(run/'journal.json'):
        raise ValueError('wrong journal path')
    canonical_hash_profile(recipe)
    ctx = C.context(recipe)
    budget = C.funding(recipe['funding'], recipe['phase'], recipe['input']['sha256'])
    grant = C.document(recipe['grant'])
    unsigned = {k: v for k, v in recipe.items() if k != 'grant'}
    if grant != {'status': 'EXECUTION_GRANTED', 'recipe_body_sha256': C.sha(C.encoded(unsigned)),
                 'scope': recipe['phase'], 'attempt_id': recipe['attempt_id']}:
        raise ValueError('exact separate coordinator grant required')
    return ctx, budget, binding, config


LEGACY_MAIN_V3_SHA = '3aa8d1222d8323f491fb9a176fa61416eee790f7b1eca2d05514ea7019e55f7b'


def admit_legacy_main_owner(proof, old, old_binding):
    """Adapt only the pinned v3 owner, without adding fields to its raw result."""
    owner = proof.get('old_owner')
    if not isinstance(owner, dict) or set(owner) != {'kind', 'wrapper', 'started', 'process', 'recipe', 'outer_wait', 'review'}:
        raise ValueError('missing explicit legacy owner reaping evidence')
    if (owner['kind'] != 'legacy_main_v3' or owner['wrapper']['sha256'] != LEGACY_MAIN_V3_SHA
            or 'root_reaped' in old or old.get('cleanup', 'missing') is not None or 'failure' in old):
        raise ValueError('unsupported/abnormal legacy owner result shape')
    # Read/parse the exact independently reviewed source, never execute it.
    import ast
    source = ast.parse(C.checked(owner['wrapper']))
    constants = {node.targets[0].id: ast.literal_eval(node.value) for node in source.body
                 if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name)
                 and node.targets[0].id in {'CORE', 'BINARY_SHA', 'RUNNER_SHA', 'HELPER_SHA', 'CONFIG_SHA', 'PYTHON', 'SUPERVISOR_SHA'}}
    expected = {k: constants[v] for k, v in {'source': 'CORE', 'binary_sha256': 'BINARY_SHA',
                'driver_sha256': 'RUNNER_SHA', 'generation_driver_sha256': 'HELPER_SHA', 'config_sha256': 'CONFIG_SHA'}.items()}
    if any(old_binding.get(k) != v for k, v in expected.items()):
        raise ValueError('legacy source binding differs from actual owner constants')
    attempt = Path(proof['old_result']['path']).parent
    if proof['old_result']['path'] != str(attempt/'result.json') or any(owner[n]['path'] != str(attempt/(n+'.json')) for n in ['started', 'process']):
        raise ValueError('legacy attempt receipt locations differ')
    started = C.document(owner['started']); process = C.document(owner['process']); recipe = C.document(owner['recipe'])
    if (started.get('recipe') != owner['recipe'] or started.get('wrapper_sha256') != LEGACY_MAIN_V3_SHA
            or started.get('supervisor_sha256') != constants['SUPERVISOR_SHA']
            or recipe.get('binding') != proof['old_binding'] or recipe.get('run') != proof['old_run']
            or Path(recipe['control'])/'attempts'/recipe['attempt_id'] != attempt
            or old.get('funding_policy') != recipe['funding_policy']):
        raise ValueError('legacy started/recipe/result provenance differs')
    funding = C.document(recipe['funding_policy'])
    if (funding.get('binding') != proof['old_binding'] or funding.get('run') != proof['old_run']
            or funding.get('attempt') != str(attempt)
            or process.get('argv') != [constants['PYTHON'], funding['adapter']['path'], recipe['funding_policy']['path'], recipe['funding_policy']['sha256']]
            or type(process.get('pid')) is not int or process['pid'] <= 1
            or process.get('started_unix') != old.get('started_unix')
            or not started['started_unix'] <= old['started_unix'] <= old['finished_unix']):
        raise ValueError('legacy actual funding child provenance differs')
    terminal = C.document(owner['outer_wait']); review = C.document(owner['review'])
    required = {name: owner[name] for name in ['wrapper', 'started', 'process', 'recipe', 'outer_wait']}
    required.update(status='PASS', kind='legacy_main_v3_outer_wait', result=proof['old_result'], binding=proof['old_binding'])
    if (any(review.get(k) != v for k, v in required.items()) or type(review.get('session_id')) is not int
            or review['session_id'] <= 0 or type(terminal.get('exit_code')) is not int or terminal['exit_code'] != 0
            or terminal.get('session_id') is not None):
        raise ValueError('legacy actual outer terminal wait/review differs')
    # The raw terminal response may have empty output: the actual legacy launch
    # redirects stdout/stderr to a retained log. The independently reviewed
    # session/start/recipe/result association is the proof, not repeated stdout.


def admit_imported_main(recipe, ctx):
    """Read-only provenance admission, never manufacture a main execution."""
    run = Path(recipe['run']); control = Path(recipe['control']); prev = recipe['previous']
    if (set(prev) != {'kind', 'bootstrap', 'review'} or recipe['phase'] != 'full'
            or recipe['expected_next_command'] != 1 or recipe['pause'] != {'kind': 'absent'}):
        raise ValueError('imported main is only an initial pristine FULL predecessor')
    if os.path.lexists(control/'current.json') or ((control/'attempts').exists() and any((control/'attempts').iterdir())):
        raise ValueError('imported main cannot bypass a preexisting attempt')
    if C.document(recipe['journal']) != {'next_command': 1}:
        raise ValueError('imported main requires pristine journal one')
    for name in ['commands', 'steps', 'plans', 'captures']:
        if any((run/name).iterdir()): raise ValueError('imported main requires empty '+name)
    if any((run/'reports').glob('phase-*.json')) or os.path.lexists(run/'pause-request'):
        raise ValueError('imported main cannot fabricate current phase history')
    proof = C.document(prev['bootstrap']); reviewed = C.document(prev['review'])
    if (reviewed.get('status') != 'PASS' or reviewed.get('bootstrap') != prev['bootstrap']
            or reviewed.get('new_binding') != recipe['binding']):
        raise ValueError('imported main bootstrap independent review differs')
    required = {'protocol', 'status', 'old_run', 'old_binding', 'old_main', 'old_result', 'old_review',
                'new_binding', 'new_main', 'initialization', 'captures'}
    if (set(proof) not in (required, required | {'old_owner'})
            or proof['protocol'] != 1 or proof['status'] != 'IMPORTED_MAIN_NOT_EXECUTED'
            or proof['new_binding'] != recipe['binding'] or proof['new_main'] != recipe['baseline']):
        raise ValueError('imported main bootstrap binding differs')
    old_run = C.absolute(proof['old_run'])
    old_binding = C.document(proof['old_binding']); new_binding = C.document(recipe['binding'])
    if (old_run == run or old_run in run.parents or run in old_run.parents
            or proof['old_binding']['path'] != str(old_run/'binding.json')
            or old_binding == new_binding or old_binding.get('source') == new_binding.get('source')
            or proof['old_main']['path'] != str(old_run/'reports/main-review.json')):
        raise ValueError('imported main requires truthful distinct old/new source bindings')
    if C.checked(proof['old_main']) != C.checked(recipe['baseline']):
        raise ValueError('imported main baseline copy bytes differ')
    old = C.document(proof['old_result']); review = C.document(proof['old_review'])
    if (old.get('status') != 'main_review_returned_not_acceptance' or type(old.get('exit_code')) is not int or old.get('exit_code') != 0
            or old.get('ownership_status') != 'observed_owned_processes_reaped'
            or old.get('new_command_failures') != [] or review.get('status') != 'PASS'
            or review.get('result') != proof['old_result'] or review.get('binding') != proof['old_binding']
            or review.get('output') != proof['old_main']):
        raise ValueError('legacy main is not a reviewed terminal owned success')
    if 'old_owner' in proof:
        admit_legacy_main_owner(proof, old, old_binding)
    elif old.get('root_reaped') is not True:
        raise ValueError('explicit old root reaping proof required')
    phase = C.document(old['phase'])
    if (phase.get('phase') != 'main' or phase.get('binding') != old_binding
            or phase.get('status') != 'review_artifact_returned_not_acceptance'
            or phase.get('output') != proof['old_main']['path']):
        raise ValueError('legacy main phase provenance differs')
    config = C.document(recipe['config']); init = proof['initialization']
    if (set(init) != {'result', 'pointer', 'review', 'config', 'argv'}
            or config.get('generation_request') is not None or config.get('seed_request') is not None):
        raise ValueError('imported main must not activate old command replay/adoption')
    init_config = C.document(init['config'])
    if C.encoded(init_config) != C.encoded(config):
        raise ValueError('actual initialization config differs')
    argv = [recipe['code']['python']['path'], '-I', '-B', recipe['code']['runner']['path'], 'init', init['config']['path']]
    init_result = C.pointer(C.document(init['result']), init['pointer']); init_review = C.document(init['review'])
    if (init['argv'] != argv or init_result.get('argv') != argv or init_result.get('exit_code') != 0
            or init_result.get('failure') is not None or init_result.get('root_reaped') is not True
            or init_result.get('ownership_status') != 'observed_owned_processes_reaped'
            or init_review.get('status') != 'PASS' or init_review.get('result') != init['result']
            or init_review.get('binding') != recipe['binding']):
        raise ValueError('actual initialization execution/review differs')
    # Full will use every successful prior capture as fallback or re-inspection.
    # Read manifest metadata only; raw capture content is independently checked
    # by native add, not asserted rehashed by this bootstrap.
    expected = {k: v for k, v in ctx['all_outcomes'].items() if (v.get('capture') or {}).get('state') == 'captured'}
    captures = proof['captures']
    if not isinstance(captures, list) or len(captures) > 256 or len(captures) != len(expected):
        raise ValueError('complete bounded reused capture roster required')
    seen = set(); total = 0
    for item in captures:
        if set(item) != {'candidate_key', 'capture_path', 'manifest'} or item['candidate_key'] in seen:
            raise ValueError('duplicate/invalid reused capture reference')
        key = item['candidate_key']; seen.add(key)
        prior = expected.get(key)
        if prior is None or item['capture_path'] != prior['capture_path'] or item['manifest']['path'] != str(C.absolute(prior['capture_path'])/'manifest.json'):
            raise ValueError('reused capture mapping differs')
        data = C.checked(item['manifest']); total += len(data)
        if total > 128*C.MIB: raise ValueError('capture manifest metadata admission exceeded')
        manifest = json.loads(data)
        if (manifest.get('state') != 'captured' or manifest.get('revision_id') != prior['capture']['revision_id']
                or manifest.get('request', {}).get('source') != prior['candidate']['path']
                or (prior.get('inspection') and prior['inspection']['revision'] != manifest['revision_id'])):
            raise ValueError('reused capture manifest provenance differs')
    if seen != set(expected): raise ValueError('reused capture roster differs')


def admit_previous(recipe, ctx):
    run = Path(recipe['run']); control = Path(recipe['control']); prev = recipe['previous']
    if prev.get('kind') == 'imported_main':
        if 'canonical_hash_profile' in recipe:
            raise ValueError('canonical profile requires an existing clean FULL pause')
        return admit_imported_main(recipe, ctx)
    if set(prev) != {'result', 'review'}:
        raise ValueError('completed predecessor and independent review required')
    result = C.document(prev['result']); review = C.document(prev['review'])
    if review.get('status') != 'PASS' or review.get('result') != prev['result'] or review.get('binding') != recipe['binding']:
        raise ValueError('predecessor review binding differs')
    if (result.get('ownership_status') != 'observed_owned_processes_reaped' or result.get('new_command_failures') != []
            or type(result.get('exit_code')) is not int or result.get('root_reaped') is not True):
        raise ValueError('failed/unknown predecessor cannot resume')
    if result.get('journal') != recipe['journal'] or result.get('next_command') != recipe['expected_next_command']:
        raise ValueError('predecessor journal checkpoint differs')
    current = control/'current.json'
    if os.path.lexists(current):
        pointer = read(current)
        expected = control/'attempts'/pointer['attempt_id']/'result.json'
        if prev['result']['path'] != str(expected):
            raise ValueError('current failed/orphan attempt cannot be bypassed')
    phase = C.document(result['phase'])
    if phase['binding'] != C.document(recipe['binding']):
        raise ValueError('predecessor phase source binding differs')
    if result['status'] == 'paused_at_command_boundary':
        if (not os.path.lexists(current) or result.get('phase_input') != recipe['input']
                or result.get('phase_name') != recipe['phase'] or phase['status'] != 'paused'):
            raise ValueError('paused predecessor is another phase/input')
    elif result.get('phase_name') == recipe['phase'] and result.get('phase_input') == recipe['input']:
        if (result['status'] != 'review_returned_not_acceptance' or phase.get('output') != ctx['output']
                or review.get('output') != result.get('output')):
            raise ValueError('completed replay provenance differs')
        C.validate_output(C.document(result['output']), ctx)
    else:
        expected_phase = {'full': 'main', 'paths': 'full', 'packets': 'paths'}[recipe['phase']]
        expected_output = recipe['baseline'] if recipe['phase'] != 'packets' else recipe['paths_review']
        if (result['status'] not in {'main_review_returned_not_acceptance', 'review_returned_not_acceptance'}
                or phase['phase'] != expected_phase or phase.get('output') != expected_output['path']
                or phase['status'] != 'review_artifact_returned_not_acceptance'
                or review.get('output') != expected_output):
            raise ValueError('unknown/wrong successful predecessor')
        C.checked(expected_output)
    if C.document(recipe['journal']) != {'next_command': recipe['expected_next_command']}:
        raise ValueError('journal changed before admission')
    admit_canonical_previous(recipe, result, review)


def remove_pause(recipe, attempt):
    pause = recipe['pause']; path = Path(recipe['run'])/'pause-request'
    if pause == {'kind': 'absent'}:
        if os.path.lexists(path): raise ValueError('unadmitted pause exists')
        return
    if set(pause) != {'kind', 'reference', 'identity', 'owner'} or pause['kind'] != 'owned' or pause['reference']['path'] != str(path):
        raise ValueError('invalid exact owned pause')
    value = C.document(pause['reference']); meta = path.lstat()
    identity = [meta.st_dev, meta.st_ino, meta.st_size, meta.st_mtime_ns, meta.st_ctime_ns]
    if value.get('owner') != pause['owner'] or identity != pause['identity']:
        raise ValueError('changed/foreign pause')
    save(attempt/'pause-before.json', pause)
    capture_parent = attempt
    if path.parent.stat().st_dev != attempt.stat().st_dev:
        capture_parent = path.parent/('.pause-capture-'+str(uuid.UUID(recipe['attempt_id'])))
        capture_parent.mkdir(mode=0o700)  # Exclusive; existing directories/links fail.
        sync(path.parent)
    C.absolute(capture_parent)
    captured = capture_parent/'pause-captured'
    if os.path.lexists(captured):
        raise ValueError('pause capture destination already exists')
    os.rename(path, captured)
    try:
        after = captured.lstat()
        if ([after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns] != identity[:4]
                or C.sha(C.raw(captured)) != pause['reference']['sha256'] or os.path.lexists(path)):
            raise ValueError('pause capture mismatch/raced replacement')
    except BaseException:
        with contextlib.suppress(OSError): os.link(captured, path, follow_symlinks=False)
        # Keep both captured and any independently installed pause; never unlink
        # a raced pathname to make this attempt appear admissible.
        sync(capture_parent); sync(path.parent); sync(attempt)
        raise
    sync(capture_parent); sync(path.parent); sync(attempt)
    save(attempt/'pause-capture.json', {'original': pause, 'captured': C.reference(captured)})


def phase_files(run):
    values = set()
    for path in (run/'reports').glob('phase-*.json'):
        if len(values) >= MAX_PHASES: raise ValueError('phase metadata bound')
        values.add(path)
    return values


def compare_full_inventory(ctx, inventory):
    candidates = inventory.get('candidates', [])
    actual = {C.sha(C.encoded(item['path'])): item for item in candidates}
    expected = {key: row['candidate'] for key, row in ctx['all_outcomes'].items()}
    if inventory.get('complete') is not True or len(actual) != len(candidates) or actual != expected:
        raise ValueError('fresh full inventory changed/incomplete; no new full work admitted')


def fresh_full_admission(runner, recipe, ctx, attempt, pause_type):
    key = ['phase-control', 'full', recipe['attempt_id'], 'discover']
    save(attempt/'inventory-admission-started.json', {'key': key, 'baseline': recipe['baseline'],
         'binding': recipe['binding'], 'expected_next_command': recipe['expected_next_command']})
    try:
        value = runner.require(key, ['discover', runner.config['catalog_root']])
        compare_full_inventory(ctx, value['value'])
        record = value['record']
        proof = {'status': 'complete', 'key': key, 'command': record['sequence'],
                 'baseline': recipe['baseline'], 'binding': recipe['binding'],
                 'stdout': {'path': str(Path(recipe['run'])/record['stdout']['path']),
                            'sha256': record['stdout']['sha256']}}
    except pause_type:
        proof = {'status': 'paused_before_discovery', 'key': key, 'baseline': recipe['baseline'],
                 'binding': recipe['binding'], 'next_command': read(Path(recipe['run'])/'journal.json')['next_command']}
        save(attempt/'inventory-admission.json', proof)
        raise
    except BaseException as error:
        save(attempt/'inventory-admission.json', {'status': 'failed_or_unresolved', 'key': key,
             'baseline': recipe['baseline'], 'binding': recipe['binding'], 'error': repr(error)})
        raise
    save(attempt/'inventory-admission.json', proof)


def validate_full_admission(recipe, ctx, binding, end, paused):
    attempt = Path(recipe['control'])/'attempts'/recipe['attempt_id']
    proof_ref = C.reference(attempt/'inventory-admission.json'); proof = C.document(proof_ref)
    key = ['phase-control', 'full', recipe['attempt_id'], 'discover']
    if proof.get('baseline') != recipe['baseline'] or proof.get('binding') != recipe['binding'] or proof.get('key') != key:
        raise ValueError('full admission provenance differs')
    if proof['status'] == 'paused_before_discovery':
        step = Path(recipe['run'])/'steps'/(C.sha(C.encoded(key))+'.json')
        if not paused or proof['next_command'] != end or end != recipe['expected_next_command'] or os.path.lexists(step):
            raise ValueError('paused full admission reserved work')
    elif proof['status'] == 'complete':
        n = proof['command']
        if type(n) is not int or not recipe['expected_next_command'] <= n < end:
            raise ValueError('fresh full discovery is outside current attempt')
        record = command_record(recipe, ctx, n, binding)
        if record['key'] != key or record['exit_code'] != 0 or record['failure'] or record['log_errors']:
            raise ValueError('fresh full discovery failed')
        expected = {'path': str(Path(recipe['run'])/record['stdout']['path']), 'sha256': record['stdout']['sha256']}
        if proof['stdout'] != expected:
            raise ValueError('fresh full inventory stdout provenance differs')
        compare_full_inventory(ctx, C.document(expected))
    else:
        raise ValueError('failed/unresolved full admission')
    return proof_ref


def command_record(recipe, ctx, number, binding):
    run = Path(recipe['run']); path = run/'commands'/f'{number:09d}'/'result.json'
    record = json.loads(C.raw(path, 65536)); args = record['requested_arguments']; key = record['key']
    if (record['sequence'] != number or record['source_binding'] != binding or not args
            or record['argv'] != [str(run/'lightroom_inspect'), *[str(run/'captures'/f'{number:09d}') if x == '@CAPTURE@' else x for x in args]]):
        raise ValueError('native command identity differs')
    step = run/'steps'/(C.sha(C.encoded(key))+'.json')
    if read(step) != {'sequence': number, 'record': str(path.relative_to(run))}:
        raise ValueError('command step differs')
    scope = key
    while isinstance(scope, list) and scope and isinstance(scope[0], list):
        scope = scope[0]
    if ctx['phase'] == 'full':
        admission = key == ['phase-control', 'full', recipe['attempt_id'], 'discover'] and args[0] == 'discover'
        if not admission and (not isinstance(scope, list) or scope[:2] != ctx['tag']):
            raise ValueError('wrong full command scope')
        allowed = {'discover', 'capture', 'create', 'register-inventory', 'add', 'resume', 'report', 'rows', 'issues', 'packets', 'metadata-conflicts', 'families'}
    else:
        if scope != ctx['tag']:
            raise ValueError('wrong path/packet command scope')
        allowed = {'discover', 'check-paths', 'paths', 'packets', 'metadata-conflicts', 'issues', 'report', 'families'}
    if args[0] not in allowed:
        raise ValueError('prohibited phase command')
    if args[0] == 'discover':
        if args != ['discover', C.document(recipe['config'])['catalog_root']]: raise ValueError('wrong inventory root')
    elif args[0] == 'capture':
        if len(args) != 3 or args[1] not in ctx['sources'] or args[2] != '@CAPTURE@':
            raise ValueError('non-requested/main-only capture')
    else:
        if len(args) < 2 or args[1] != ctx['plan']: raise ValueError('wrong derived plan')
        if args[0] in {'create', 'families'} and len(args) != 2:
            raise ValueError('unexpected plan command options')
        if args[0] == 'add':
            prior = {v.get('capture_path') for v in ctx.get('all_outcomes', {}).values()}
            capture = Path(args[2]) if len(args) == 3 else Path('.')
            if str(capture) not in prior and (capture.parent != run/'captures' or not capture.name.isdecimal()):
                raise ValueError('unrelated capture evidence')
        if args[0] == 'report' and len(args) != 3:
            raise ValueError('unexpected report arguments')
        if args[0] == 'resume' and (len(args) != 5 or args[3:] != ['--max-rows', str(C.document(recipe['config'])['resume_rows_per_call'])]):
            raise ValueError('resume batch differs')
        if args[0] in {'rows', 'paths', 'packets', 'issues', 'metadata-conflicts'}:
            if (len(args) != 7 or args[3] != '--after' or not args[4].isdecimal()
                    or args[5:] != ['--limit', str(C.document(recipe['config'])['page_limit'])]):
                raise ValueError('page cursor/limit differs')
        if args[0] == 'register-inventory' and (len(args) != 3 or Path(args[2]).parent.parent != run/'commands' or Path(args[2]).name != 'stdout'):
            raise ValueError('unrelated inventory evidence')
        if args[0] == 'check-paths':
            limit = str(C.document(recipe['config'])['page_limit'])
            expected = ['check-paths', ctx['plan'], args[2], '--limit', limit]
            if ctx['phase'] == 'packets': expected.append('--packets')
            if args != expected or args[2] not in {v['inspection']['revision'] for v in ctx['requested'].values()}:
                raise ValueError('wrong direct-I/O mode/revision')
    return record


def serial_tail(recipe, ctx, binding):
    start = recipe['expected_next_command']; end = C.integer(read(Path(recipe['run'])/'journal.json')['next_command'], start)
    if end-start > MAX_COMMANDS: raise ValueError('new command count bound')
    if end == start: return {'next_command': end, 'terminal': True}
    record = command_record(recipe, ctx, end-1, binding)
    return {'next_command': end, 'terminal': type(record.get('exit_code')) is int}


def classify(recipe, ctx, binding, before, started, code, pause):
    run = Path(recipe['run']); added = phase_files(run)-before
    if len(added) != 1: raise ValueError('missing/ambiguous phase receipt')
    phase_path = added.pop(); phase = read(phase_path)
    if (phase['phase'] != recipe['phase'] or phase['input'] != recipe['input']['path']
            or phase['binding'] != binding or phase['started_unix'] < started):
        raise ValueError('phase receipt identity differs')
    tail = serial_tail(recipe, ctx, binding)
    if not tail['terminal']: raise ValueError('unresolved native tail')
    used = 0; failures = []
    for n in range(recipe['expected_next_command'], tail['next_command']):
        used += (run/'commands'/f'{n:09d}'/'result.json').stat().st_size
        if used > MAX_METADATA: raise ValueError('new result metadata bound')
        record = command_record(recipe, ctx, n, binding)
        if record['exit_code'] != 0 or record['failure'] or record['log_errors']: failures.append(n)
    if failures: raise ValueError('native failure; no automatic pause/retry acceptance: '+str(failures[:32]))
    output = None
    fresh = None
    if recipe['phase'] == 'full':
        fresh = validate_full_admission(recipe, ctx, binding, tail['next_command'], code != 0 and phase['status'] == 'paused' and pause is not None)
    if code != 0 and phase['status'] == 'paused' and pause is not None:
        C.checked(pause)
        status = 'paused_at_command_boundary'
    elif code == 0 and phase['status'] == 'review_artifact_returned_not_acceptance' and phase['output'] == ctx['output']:
        output = C.reference(ctx['output']); C.validate_output(C.document(output), ctx)
        status = 'review_returned_not_acceptance'
    else:
        raise ValueError('exit/phase/output classification differs')
    return {'status': status, 'phase': C.reference(phase_path), 'phase_name': recipe['phase'], 'phase_input': recipe['input'],
            'output': output, 'journal': C.reference(run/'journal.json'), 'next_command': tail['next_command'],
            'new_command_failures': failures, 'pause': pause, 'fresh_full_inventory': fresh, 'ownership_status': 'observed_owned_processes_reaped',
            **canonical_result_fields(recipe)}


def canonical_result_fields(recipe):
    if 'canonical_hash_profile' not in recipe:
        return {}
    attempt = Path(recipe['control'])/'attempts'/recipe['attempt_id']
    fields = {'execution_profile': C.reference(attempt/'execution-profile.json'),
              'execution_profile_consumed': C.reference(attempt/'execution-profile-consumed.json')}
    validate_execution_profile(recipe, attempt, dict(fields, next_command=read(Path(recipe['run'])/'journal.json')['next_command']))
    return fields


class Drain:
    def __init__(self, pipe, path):
        self.pipe = pipe; self.path = path; self.excess = threading.Event(); self.error = None
        self.seen = 0; self.kept = 0
        self.thread = threading.Thread(target=self.run, daemon=True)
    def run(self):
        try:
            with self.path.open('xb') as stream:
                while chunk := self.pipe.read(65536):
                    keep = chunk[:max(0, LOG_CAP-self.kept)]; stream.write(keep)
                    self.seen += len(chunk); self.kept += len(keep)
                    if self.seen > LOG_CAP: self.excess.set()
                stream.flush(); os.fsync(stream.fileno())
        except BaseException as error:
            self.error = repr(error); self.excess.set()
        finally:
            self.pipe.close()


def supervise(recipe, attempt, ctx, budget, binding, supervisor, funding_module):
    run = Path(recipe['run']); before = phase_files(run); known = {}; peaks = {}; maximum = 0
    started = time.time(); tick = time.monotonic(); child = None; drains = []; pause = None
    cleanup = None; result = {'status': 'failed_or_unknown', 'ownership_status': 'unknown_requires_review'}
    policy = dict(budget, run=str(run), attempt=str(attempt))
    monitor = funding_module.FundingMonitor(policy, 'outer')
    grace = {'interrupt_grace_seconds': 30, 'kill_grace_seconds': 10}
    recipe_ref = C.reference(attempt/'recipe.json')
    argv = [recipe['code']['python']['path'], '-I', '-B', recipe['code']['controller']['path'], '--child', recipe_ref['path'], recipe_ref['sha256']]
    previous_term = signal.getsignal(signal.SIGTERM)
    def interrupted(signum, frame):
        raise InterruptedError('phase owner received SIGTERM')
    signal.signal(signal.SIGTERM, interrupted)
    try:
        validate_temp_storage(recipe)
        child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        save(attempt/'process.json', {'pid': child.pid, 'argv': argv, 'started_unix': started})
        for stream, name in [(child.stdout, 'stdout'), (child.stderr, 'stderr')]:
            drain = Drain(stream, attempt/name); drains.append(drain); drain.thread.start()
        while True:
            live = supervisor.observe(child, known, peaks)
            validate_temp_storage(recipe)
            if len(known) > MAX_KNOWN or len(live) > MAX_ACTIVE: raise RuntimeError('observed process admission bound')
            combined = sum(x['rss_bytes'] for x in live.values()); maximum = max(maximum, combined)
            elapsed = time.monotonic()-tick
            if elapsed >= 4800: raise TimeoutError('sampled4800s emergency stop, separate cleanup graces')
            memory = recipe['memory']
            if combined > memory['combined_owned_rss_bytes'] or any(x['rss_bytes'] > memory['python_process_rss_bytes' if p == child.pid else 'native_process_rss_bytes'] for p, x in live.items()):
                raise RuntimeError('sampled RSS admission exceeded')
            if any(x.excess.is_set() for x in drains): raise RuntimeError('bounded output overflow/write failure')
            monitor.outer()
            if elapsed >= 600 and not os.path.lexists(run/'pause-request'):
                try: save(run/'pause-request', {'owner': str(attempt), 'reason': '600s cooperative phase slice', 'requested_unix': time.time()})
                except FileExistsError: pass
            if pause is None and os.path.lexists(run/'pause-request'):
                pause = C.reference(run/'pause-request')
                if not C.document(pause).get('owner'): raise ValueError('pause owner unknown')
                save(attempt/'pause-observed.json', pause)
            if child.poll() is not None:
                if supervisor.observe(child, known, peaks): raise RuntimeError('observed descendants survive root exit')
                validate_recipe(recipe)  # Immutable input/source drift fails before a returned review.
                result.update(classify(recipe, ctx, binding, before, started, child.returncode, pause)); break
            time.sleep(1)
    except BaseException as error:
        # A repeated TERM cannot interrupt the owned-root cleanup sequence.
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        result['failure'] = repr(error)
        if child is not None:
            try:
                cleanup = supervisor.halt(child, known, peaks, repr(error), attempt, grace, None, started)
            except BaseException as failure:
                cleanup = {'root_reaped': False, 'root_signals': [], 'errors': [repr(failure)]}
            finally:
                if child.poll() is None: supervisor.stop_root(child, grace, cleanup)
            try: result['serial_tail'] = serial_tail(recipe, ctx, binding)
            except BaseException as failure: result['serial_tail'] = {'terminal': False, 'error': repr(failure)}
        # Never promote abnormal completion from a successful final command alone.
        result['status'] = 'failed_or_unknown'; result['ownership_status'] = 'unknown_requires_review'
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        for drain in drains: drain.thread.join(timeout=5)
        if any(x.thread.is_alive() or x.excess.is_set() for x in drains):
            result.update(status='failed_or_unknown', ownership_status='unknown_requires_review', pipe_failure=True)
        try: monitor.finish()
        except BaseException as error:
            result.update(status='failed_or_unknown', ownership_status='unknown_requires_review', funding_receipt_error=repr(error))
    result.update(started_unix=started, finished_unix=time.time(), exit_code=None if child is None else child.poll(), cleanup=cleanup,
                  root_reaped=child is not None and child.poll() is not None, observed_process_lifetimes=list(known.values()),
                  sampled_observed_peak_rss_per_pid=peaks, sampled_observed_peak_combined_rss_bytes=maximum,
                  rss_meaning='one-second ps samples, native microsecond birth ownership; not HWM/quota or all-descendant coverage',
                  soft_slice_seconds=600, sampled_emergency_seconds=4800,
                  logs={x.path.name: {'observed_bytes': x.seen, 'retained_bytes': x.kept, 'truncated': x.seen > x.kept,
                                     'error': x.error, 'complete': not x.thread.is_alive(),
                                     'reference': C.reference(x.path) if x.path.exists() and not x.thread.is_alive() else None} for x in drains})
    signal.signal(signal.SIGTERM, previous_term)
    return result


def child_main(recipe):
    ctx, budget, binding, config = validate_recipe(recipe)
    attempt = Path(recipe['control'])/'attempts'/recipe['attempt_id']
    if read(Path(recipe['control'])/'current.json') != {'attempt_id': recipe['attempt_id']} or read(attempt/'started.json')['binding'] != recipe['binding']:
        raise ValueError('child has no persistent owning attempt')
    frozen = module(recipe['code']['runner'], 'bound_frozen_phase_runner')
    install_canonical_profile(recipe, attempt, frozen)
    funding_module = module(recipe['code']['funding_guard'], 'bound_funding')
    monitor = funding_module.FundingMonitor(dict(budget, run=recipe['run'], attempt=str(attempt)), 'adapter')
    base = funding_module.guarded_type(frozen.Runner, monitor, frozen.PauseRequested)
    class AdmittedRunner(base):
        def __init__(self, root):
            super().__init__(root)
            if C.document(recipe['journal']) != {'next_command': recipe['expected_next_command']}:
                self.close(); raise ValueError('journal changed before child acquired runner lock')
        def full_phase(self, request_path):
            fresh_full_admission(self, recipe, ctx, attempt, frozen.PauseRequested)
            return super().full_phase(request_path)
        def space(self, minimum=None):
            validate_temp_storage(recipe)
            if read(Path(recipe['run'])/'journal.json')['next_command'] >= recipe['expected_next_command']+MAX_COMMANDS:
                monitor.pause('new command metadata count admission')
                raise frozen.PauseRequested('bounded command count reached')
            return super().space(minimum)
        def call(self, key, arguments):
            validate_temp_storage(recipe)
            return super().call(key, arguments)
    frozen.Runner = AdmittedRunner
    previous = sys.argv
    try:
        sys.argv = [recipe['code']['runner']['path'], recipe['phase'], recipe['run'], recipe['input']['path']]
        frozen.main()
    finally:
        sys.argv = previous; monitor.finish()


def run(recipe):
    ctx, budget, binding, config = validate_recipe(recipe)
    control = Path(recipe['control']); run_root = Path(recipe['run'])
    if control == run_root or control in run_root.parents or run_root in control.parents:
        raise ValueError('separate external phase control directory required')
    control.mkdir(mode=0o700, exist_ok=True)
    with (control/'owner.lock').open('a+b') as owner:
        fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            with (run_root/'runner.lock').open('a+b') as runner_lock:
                fcntl.flock(runner_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                try:
                    admit_previous(recipe, ctx)
                    volume = os.statvfs(run_root); free = volume.f_bavail*volume.f_frsize
                    if free < budget['initial_minimum_bytes']: raise ValueError('full phase funding unavailable')
                    attempts = control/'attempts'; attempts.mkdir(mode=0o700, exist_ok=True)
                    attempt = attempts/recipe['attempt_id']; attempt.mkdir(mode=0o700); sync(attempts)
                    save(attempt/'recipe.json', recipe)
                    save(attempt/'started.json', {'binding': recipe['binding'], 'phase': recipe['phase'], 'input': recipe['input'], 'started_unix': time.time(), 'available_bytes': free, 'funding': budget})
                    save(control/'current.json', {'attempt_id': recipe['attempt_id']}, replace=True)
                    if 'canonical_hash_profile' in recipe:
                        save(attempt/'execution-profile.json', execution_profile_value(recipe, attempt))
                    remove_pause(recipe, attempt)
                finally: fcntl.flock(runner_lock, fcntl.LOCK_UN)
            result = supervise(recipe, attempt, ctx, budget, binding,
                               module(recipe['code']['supervisor'], 'bound_phase_supervisor'),
                               module(recipe['code']['funding_guard'], 'bound_phase_funding'))
            save(attempt/'result.json', result)
            print(json.dumps({'status': result['status'], 'receipt': str(attempt/'result.json')}), flush=True)
            return 0 if result['status'] in {'paused_at_command_boundary', 'review_returned_not_acceptance'} and result['ownership_status'] == 'observed_owned_processes_reaped' else 1
        finally: fcntl.flock(owner, fcntl.LOCK_UN)


def main():
    global C
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--child', action='store_true'); parser.add_argument('recipe'); parser.add_argument('sha256')
    args = parser.parse_args()
    recipe = json.loads(bootstrap({'path': args.recipe, 'sha256': args.sha256}))
    C = module(recipe['code']['contract'], 'bound_phase_contract')
    return child_main(recipe) if args.child else run(recipe)


if __name__ == '__main__':
    sys.exit(main())

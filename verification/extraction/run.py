#!/usr/bin/env python3
"""Source-preserving extraction probe; generated external models remain unproved."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
SANDBOX = Path('/tmp/blend-verification-tools/extraction')
TOOLS = SANDBOX / 'tools'
sys.path.insert(0, str(ROOT / 'scripts'))
from kani_release import require_directory, require_owned_path, unique_object
from verify_local import OUTPUT, contained

METHODS = (
    'to_asset_from_b_token', 'to_asset_from_d_token',
    'to_b_token_down', 'to_b_token_up', 'to_d_token_down', 'to_d_token_up',
)
EXPECTED = {
    'aeneas': 'd968af5e01e50cef84add48a75cf2af8f80751458cab931066d05f42098d934c',
    'charon': '813d8ce502b7fe1b4bbb5e149ce7b403a031ce17c2cfd6fab831342ed397dc74',
    'charon-driver': '0bbfebd1e1cc5715e9b3b85cea8a321c13fb07d218847b692243c9cad054a96d',
}
TARGETS = ('wasm32v1-none', 'x86_64-unknown-linux-gnu')


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def sources():
    paths = {ROOT / 'Cargo.toml', *ROOT.glob('*/Cargo.toml'),
             *ROOT.glob('mocks/*/Cargo.toml')}
    for directory in ('pool/src', 'shared', 'mocks/moderc3156-iface'):
        paths.update((ROOT / directory).rglob('*.rs'))
    paths.update(ROOT / name for name in ('Cargo.lock', 'justfile', 'rust-toolchain.toml'))
    return {str(p.relative_to(ROOT)): digest(p) for p in sorted(paths) if p.is_file()}


def dependency_sources():
    result = {}
    registry = SANDBOX / 'cargo-home/registry/src'
    for package in ('soroban-fixed-point-math-1.5.0', 'soroban-sdk-25.3.0'):
        for source in sorted(registry.glob(f'*/{package}/src/**/*.rs')):
            result[str(source.relative_to(registry))] = digest(source)
    return result


def checked_toolchain():
    require_directory(SANDBOX.parent, 'Extraction tool parent', recursive=False)
    require_directory(SANDBOX, 'Extraction sandbox', recursive=False)
    # Bundled Lean/source trees contain legitimate, unused symlinks. Check the
    # executable inputs and compiler/cache trees rather than traversing those.
    require_directory(TOOLS, 'Extraction tool directory', recursive=False)
    for name in EXPECTED:
        require_owned_path(TOOLS / name, 'Extraction executable')
    require_directory(SANDBOX / 'rustup', 'Extraction compiler')
    require_directory(SANDBOX / 'cargo-home', 'Extraction Cargo home')
    for name, expected in EXPECTED.items():
        if digest(TOOLS / name) != expected:
            raise ValueError(f'Pinned tool hash mismatch: {name}')
    pin = json.loads(Path(__file__).with_name('evaluation.json').read_text())['toolchain']['rust']
    rust_bin = SANDBOX / pin['toolchain_directory'] / 'bin'
    if digest(rust_bin / 'rustc') != pin['compiler_sha256']:
        raise ValueError('Pinned extraction compiler hash mismatch')
    rustup = shutil.which('rustup')
    if rustup is None:
        raise ValueError('rustup launcher is required; automatic installation is disabled')
    env = os.environ.copy()
    env.update(RUSTUP_HOME=str(SANDBOX / 'rustup'), CARGO_HOME=str(SANDBOX / 'cargo-home'),
               RUSTUP_AUTO_INSTALL='0')
    version = subprocess.run(
        [rustup, 'run', pin['channel'], 'rustc', '--version', '--verbose'],
        env=env, capture_output=True, text=True, check=True, timeout=15,
    )
    if f"commit-hash: {pin['revision']}" not in version.stdout:
        raise ValueError('Pinned extraction compiler identity mismatch')
    return rust_bin, version.stdout


def admit_llbc(path, requested_target, current_sources):
    receipt = json.loads((path.parent / 'invocation.json').read_text(), object_pairs_hook=unique_object)
    payload = path.read_bytes()
    value = json.loads(payload, object_pairs_hook=unique_object)
    actual = hashlib.sha256(payload).hexdigest()
    target = receipt.get('target')
    if (receipt.get('stage') != 'charon' or receipt.get('exit_code') != 0
            or receipt.get('tool_sha256') != EXPECTED
            or target not in TARGETS
            or receipt.get('generated_sha256', {}).get(path.name) != actual
            or receipt.get('source_sha256_before') != receipt.get('source_sha256_after')
            or receipt.get('source_sha256_after') != current_sources):
        raise ValueError('LLBC does not match a successful, current-source Charon receipt')
    if requested_target is not None and requested_target != target:
        raise ValueError('Aeneas target conflicts with its Charon input')
    actual_targets = {entry['key'] for entry in value['translated']['target_information']}
    if value.get('has_errors') is not False or actual_targets != {target}:
        raise ValueError('LLBC has extraction errors or conflicting machine target information')
    return receipt, payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('stage', choices=('charon', 'aeneas'))
    parser.add_argument('--target', choices=TARGETS,
                        help='Charon defaults to Wasm; Aeneas inherits and checks the input target')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--llbc', type=Path)
    args = parser.parse_args()
    if (args.stage == 'aeneas') != (args.llbc is not None):
        parser.error('only aeneas requires --llbc')
    output = args.output.resolve()
    OUTPUT.mkdir(parents=True, exist_ok=True)
    with (OUTPUT / 'run.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        rust_bin, compiler_version = checked_toolchain()
        current_sources = sources()
        parent_receipt = None
        payload = None
        if args.stage == 'aeneas':
            parent_receipt, payload = admit_llbc(args.llbc.resolve(), args.target, current_sources)
            target = parent_receipt['target']
            initial_sources = parent_receipt['source_sha256_after']
        else:
            target = args.target or 'wasm32v1-none'
            initial_sources = current_sources
        output.mkdir(parents=True, exist_ok=False)
        env = {
            'PATH': f'{rust_bin}:{os.environ.get("PATH", "")}',
            'LD_LIBRARY_PATH': f'{rust_bin.parent / "lib"}:{os.environ.get("LD_LIBRARY_PATH", "")}',
            'RUSTUP_HOME': str(SANDBOX / 'rustup'),
            'RUSTUP_AUTO_INSTALL': '0',
            'CARGO_HOME': str(SANDBOX / 'cargo-home'),
            'CARGO_TARGET_DIR': str(output / 'target'),
            'CARGO_BUILD_JOBS': '1',
            'CHARON_CACHE_DIR': str(SANDBOX / 'charon-cache'),
            # Pinned compiler is already checked and in PATH. This disables
            # Charon's fallback path that can invoke rustup install.
            'CHARON_TOOLCHAIN_IS_IN_PATH': '1',
            'RUSTC': str(rust_bin / 'rustc'),
            'RUSTFLAGS': '', 'CARGO_ENCODED_RUSTFLAGS': '',
            'RUSTC_WRAPPER': '', 'RUSTC_WORKSPACE_WRAPPER': '',
        }
        if args.stage == 'charon':
            command = [str(TOOLS / 'charon'), 'cargo', '--preset=aeneas',
                       '--sysroot=default', '--error-on-warnings',
                       '--include=soroban_fixed_point_math', '--opaque=soroban_sdk',
                       '--dest-file', str(output / 'pool.llbc'), '--start-from=crate::math']
            # The pinned matcher needs one wildcard for the inherent impl segment.
            command += [f'--start-from=crate::pool::reserve::_::{name}' for name in METHODS]
            command += ['--', '--locked', '--release', '--lib', '--no-default-features',
                        '--manifest-path', str(ROOT / 'pool/Cargo.toml'), '--target', target]
        else:
            (output / 'lean').mkdir()
            snapshot = output / 'input.llbc'
            snapshot.write_bytes(payload)
            command = [str(TOOLS / 'aeneas'), '-backend', 'lean', '-split-files',
                       '-warnings-as-errors', '-sequential', '-dest', str(output / 'lean'),
                       str(snapshot)]
        guarded = ['env', '-u', 'CHARON_MIRI_SYSROOTS', '-u', 'CHARON_RESOLVED_MIRI_SYSROOT',
                   '-u', 'RUSTUP_TOOLCHAIN', '-u', 'CARGO_BUILD_TARGET',
                   *[f'{key}={value}' for key, value in env.items()], *command]
        record = {
            'stage': args.stage, 'target': target, 'claim': 'feasibility_only_not_a_proof',
            'command': guarded, 'tool_sha256': EXPECTED, 'compiler_version': compiler_version,
            'source_sha256_before': initial_sources,
            'llbc_input_sha256': hashlib.sha256(payload).hexdigest() if payload else None,
            'charon_receipt': parent_receipt,
        }
        record_path = output / 'invocation.json'
        record_path.write_text(json.dumps(record, indent=2) + '\n')
        result = contained(guarded, 600, output / 'command.log')
        record['exit_code'] = result
        record['source_sha256_after'] = sources()
        record['dependency_source_sha256'] = dependency_sources()
        record['generated_sha256'] = {
            str(path.relative_to(output)): digest(path)
            for pattern in ('*.llbc', 'lean/**/*.lean', 'lean/**/*.json')
            for path in sorted(output.glob(pattern)) if path.is_file()
        }
        record_path.write_text(json.dumps(record, indent=2) + '\n')
        if record['source_sha256_after'] != initial_sources:
            raise ValueError('Source changed during extraction; artifact provenance is invalid')
        if payload is not None and snapshot.read_bytes() != payload:
            raise ValueError('LLBC snapshot changed during Aeneas execution')
    print(json.dumps({'exit_code': result, 'artifacts': str(output), 'claim': record['claim']}))
    return result


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(f'extraction: {error}', file=sys.stderr)
        sys.exit(1)

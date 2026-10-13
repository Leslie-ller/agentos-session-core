#!/usr/bin/env python3
"""Assemble the session extension against an exact Codeg source revision."""
from __future__ import annotations
import argparse
import hashlib
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[1]


def run(*args: str, cwd: Path | None = None) -> bytes:
    return subprocess.check_output(args, cwd=cwd)


def validate_bundle(root: Path) -> dict:
    manifest = json.loads((root / 'upstream.json').read_text())
    digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    if digest(root / 'patches/session-core.patch') != manifest['patch_sha256']:
        raise ValueError('Integration patch differs from the recorded bundle')
    actual = {p.relative_to(root / 'overlay').as_posix(): digest(p)
              for p in (root / 'overlay').rglob('*') if p.is_file()}
    if actual != manifest['overlay']:
        raise ValueError('Overlay differs from the recorded bundle')
    return manifest


def prepare(output: Path, source: Path | None = None) -> None:
    manifest = validate_bundle(ROOT)
    output = output.expanduser().absolute()
    if output.exists():
        raise ValueError(f'Destination already exists; choose a new directory: {output}')
    commit = manifest['upstream_commit']
    # Validate local source before creating the destination; git archive reads
    # only the pinned commit, never its working changes or untracked files.
    archive = None
    if source is not None:
        source = source.expanduser().resolve()
        resolved = run('git', 'rev-parse', f'{commit}^{{commit}}', cwd=source).decode().strip()
        if resolved != commit:
            raise ValueError('Local source does not contain the pinned upstream commit')
        archive = run('git', 'archive', commit, cwd=source)
    output.mkdir(parents=True)
    run('git', 'init', '--quiet', output.as_posix())
    if archive is not None:
        with tarfile.open(fileobj=io.BytesIO(archive)) as files:
            files.extractall(output, filter='data')
    else:
        run('git', 'remote', 'add', 'upstream', manifest['upstream_repository'], cwd=output)
        run('git', 'fetch', '--depth=1', 'upstream', commit, cwd=output)
        run('git', 'checkout', '--detach', '--quiet', 'FETCH_HEAD', cwd=output)
        if run('git', 'rev-parse', 'HEAD', cwd=output).decode().strip() != commit:
            raise ValueError('Fetched source revision does not match the pinned commit')
    run('git', 'apply', '--check', (ROOT / 'patches/session-core.patch').as_posix(), cwd=output)
    run('git', 'apply', (ROOT / 'patches/session-core.patch').as_posix(), cwd=output)
    for relative in manifest['overlay']:
        target = output / relative
        if target.exists():
            raise ValueError(f'Overlay unexpectedly replaces upstream file: {relative}')
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / 'overlay' / relative, target)
    shutil.copyfile(ROOT / 'NOTICE', output / 'SESSION_CORE_NOTICE')
    print(f'Prepared {output}\nUpstream revision: {commit}')
    print('Next: install frontend dependencies and build; see README.md.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'workspace')
    parser.add_argument('--source', type=Path, help='Optional local Codeg git repository, for offline preparation')
    args = parser.parse_args()
    try:
        prepare(args.output, args.source)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'Preparation failed: {error}\nNo existing directory was overwritten.\n')

"""Prove the idle-process regression fires when expiration is disabled, using a disposable source copy.

Run through scripts/linux.py on macOS. The working source is never mutated. Build products reuse
this checkout's target cache, so run after measurements have captured their binary identities.
"""
from pathlib import Path
import os
import shutil
import subprocess
import tempfile

root = Path.cwd()
with tempfile.TemporaryDirectory(prefix='elmdb-idle-mutation-') as temporary:
    copy = Path(temporary) / 'source'
    shutil.copytree(root, copy, ignore=shutil.ignore_patterns('target', 'node_modules', '.git'))
    source = copy / 'crates/elmdb/src/store/idle_reads.rs'
    original = 'state.entries.retain(|entry| entry.expires > now);'
    text = source.read_text()
    assert text.count(original) == 1, 'mutation must target exactly the expiry operation'
    source.write_text(text.replace(original, 'for entry in &mut state.entries { entry.expires = now + RETAIN_FOR; }'))
    result = subprocess.run([
        'cargo', 'test', '-p', 'elmdb', '--lib', '--target-dir', str(root / 'target'),
        'store::retention_tests::idle_process_releases_pages_without_another_database_call',
        '--', '--exact', '--nocapture',
    ], cwd=copy, env={**os.environ, 'CARGO_INCREMENTAL': '0'},
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=180)
    print(result.stdout)
    assert result.returncode != 0, 'disabled expiry passed the regression'
    assert 'completed snapshots were not released without another read' in result.stdout, \
        'failure must demonstrate retained snapshots, not a build or unrelated test failure'
    assert '1 failed' in result.stdout, 'the selected regression must actually run'
    print('IDLE_READER_MUTATION_CAUGHT: disabled expiration retains the live child snapshot')

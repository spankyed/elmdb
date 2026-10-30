"""Prove idle expiration, reuse or worker-cleanup regressions fire on a disposable source copy.

Run through scripts/linux.py on macOS. The working source is never mutated. Mutation build products
have their own target directory: Cargo's shared executable paths must not overwrite the unmodified
source's binaries or taint a later validation run.
"""
from pathlib import Path
import argparse
import os
import shutil
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--case', choices=['expiry', 'reuse', 'close', 'drop'], default='expiry')
case = parser.parse_args().case
root = Path.cwd()
with tempfile.TemporaryDirectory(prefix='elmdb-idle-mutation-') as temporary:
    copy = Path(temporary) / 'source'
    shutil.copytree(root, copy, ignore=shutil.ignore_patterns('target', 'node_modules', '.git'))
    source = copy / 'crates/elmdb/src/store/idle_reads.rs'
    text = source.read_text()
    tests = 'store::idle_reads::tests::'
    if case == 'expiry':
        original = 'state.entries.retain(|entry| entry.expires > now);'
        replacement = 'for entry in &mut state.entries { entry.expires = now + RETAIN_FOR; }'
        test = 'store::retention_tests::idle_process_releases_pages_without_another_database_call'
        expected = 'completed snapshots were not released without another read'
    elif case == 'reuse':
        original = 'if entry.expires > now && entry.txn.id() == current {'
        replacement = 'if entry.expires == now && entry.txn.id() == current {'
        test = tests + 'a_hot_read_takes_its_cached_transaction'
        expected = 'a completed hot snapshot was not reused'
    elif case == 'close':
        start = text.index('    pub fn close(&mut self) {')
        end = text.index('    /// At process exit', start)
        original = text[start:end]
        replacement = '    pub fn close(&mut self) {}\n\n'
        test = tests + 'close_and_drop_release_the_worker_state'
        expected = 'the cache worker survived explicit close'
    else:
        original = '        self.close();'
        replacement = ''
        test = tests + 'close_and_drop_release_the_worker_state'
        expected = 'the cache worker survived close/drop'
    assert text.count(original) == 1, 'mutation must target exactly one operation'
    source.write_text(text.replace(original, replacement))
    result = subprocess.run([
        'cargo', 'test', '-p', 'elmdb', '--lib', '--target-dir', str(root / 'target/idle-reader-mutations'),
        test,
        '--', '--exact', '--nocapture',
    ], cwd=copy, env={**os.environ, 'CARGO_INCREMENTAL': '0'},
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=300)
    print(result.stdout)
    assert result.returncode != 0, 'selected mutation passed the regression'
    assert expected in result.stdout, \
        'failure must demonstrate the selected behavior, not a build or unrelated test failure'
    assert '1 failed' in result.stdout, 'the selected regression must actually run'
    print(f'IDLE_READER_MUTATION_CAUGHT: {case}')

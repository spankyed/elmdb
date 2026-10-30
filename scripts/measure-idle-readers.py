"""Measure cached idle readers against explicit release controls in private subprocess fixtures.

Run on Linux through scripts/linux.py on a Mac. No timing assertion or production data is used.
The mdb_stat executable is compiled from the exact lmdb-master-sys source Cargo resolves, and runs
in a separate process. Free-list pages are not automatically reusable while older readers protect them.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys


def run(command, **kwargs):
    return subprocess.run(command, check=True, text=True, **kwargs)


parser = argparse.ArgumentParser()
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--source', required=True, help='source commit; dirty source file digests are recorded separately')
parser.add_argument('--debug', action='store_true', help='execution smoke only, not performance evidence')
args = parser.parse_args()
if platform.system() != 'Linux':
    sys.exit('Run on Linux; on a Mac use python3 scripts/linux.py -- python3 scripts/measure-idle-readers.py ...')
assert not args.output.exists(), 'use a new output directory to preserve earlier evidence'
args.output.mkdir(parents=True)
root = Path.cwd()
metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version=1'], text=True))
lmdb = [package for package in metadata['packages'] if package['name'] == 'lmdb-master-sys']
assert len(lmdb) == 1, 'one resolved LMDB implementation'
sources = Path(lmdb[0]['manifest_path']).parent / 'lmdb/libraries/liblmdb'
stat = root / 'target/idle-reader-mdb-stat'
run(['cc', '-O2', '-pthread', '-I', str(sources), '-o', str(stat),
     *[str(sources / name) for name in ['mdb_stat.c', 'mdb.c', 'midl.c']]])
command = ['cargo', 'test', '-p', 'elmdb', '--lib', '--no-run', '--message-format=json']
if not args.debug:
    command.append('--release')
build = run(command, stdout=subprocess.PIPE)
binaries = [row['executable'] for line in build.stdout.splitlines()
            if (row := json.loads(line)).get('reason') == 'compiler-artifact'
            and row.get('target', {}).get('kind') == ['lib'] and row.get('profile', {}).get('test')
            and row.get('executable')]
assert len(binaries) == 1
binary = binaries[0]
env = {**os.environ, 'ELMDB_MDB_STAT': str(stat)}
records = []
for name, prefix in [('measure_idle_reader_page_retention', 'ELMDB_RETENTION_JSON '),
                     ('measure_idle_reader_read_overhead', 'ELMDB_READ_OVERHEAD_JSON ')]:
    result = subprocess.run([binary, '--ignored', '--exact', 'store::retention_tests::' + name,
                  '--nocapture', '--test-threads=1'], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                 timeout=120)
    (args.output / (name + '.log')).write_text(result.stdout)
    if result.returncode:
        print(result.stdout, file=sys.stderr)
        result.check_returncode()
    selected = [json.loads(line.split(prefix, 1)[1]) for line in result.stdout.splitlines() if prefix in line]
    assert len(selected) == 4, 'all four selected controls must actually execute'
    records.append(selected)
manifest = {
    'sourceBaseCommit': args.source, 'profile': 'debug-smoke' if args.debug else 'release',
    'rustc': subprocess.check_output(['rustc', '-vV'], text=True),
    'host': platform.uname()._asdict(),
    'cpuLimit': Path('/sys/fs/cgroup/cpu.max').read_text().strip(),
    'memoryLimitBytes': Path('/sys/fs/cgroup/memory.max').read_text().strip(),
    'lmdbVersion': lmdb[0]['version'],
    'binarySha256': hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
    'files': {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in
              ['Cargo.lock', 'crates/elmdb/src/store/mod.rs', 'crates/elmdb/src/store/retention_tests.rs']},
    'limitations': ['synthetic raw store overwrites, not application throughput',
                    'syncs disabled in disposable databases',
                    'release timing control includes an additional cache mutex acquisition',
                    'free-list pages include pages that older readers still protect'],
}
(args.output / 'results.json').write_text(json.dumps({'manifest': manifest, 'retention': records[0],
                                                    'readOverhead': records[1]}, indent=2) + '\n')
print(json.dumps({'output': str(args.output), 'retention': [
    {'mode': row['mode'], 'readers': row['readers'], 'allocatedPages': [s['allocatedPages'] for s in row['samples']],
     'fileBytes': [s['fileBytes'] for s in row['samples']]} for row in records[0]],
    'readOverhead': records[1]}, indent=2))

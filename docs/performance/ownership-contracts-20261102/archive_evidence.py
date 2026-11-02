"""Archive source snapshots and raw qualification evidence, excluding build/data files."""
import hashlib
import json
import os
import subprocess
import sys
import tarfile
from pathlib import Path

root, out, repo = map(Path, sys.argv[1:4])
out.mkdir(parents=True, exist_ok=True)
accepted = {'.log', '.json', '.jsonl', '.stderr', '.txt', '.patch', '.rs', '.py', '.sh', '.asm'}
excluded_roots = {'target-after', 'target-before', 'target-fixtures', 'target-kani', 'target-miri', 'target-models', 'target-stage1'}
files = []
for directory, folders, names in os.walk(root):
    folders[:] = [name for name in folders if name not in excluded_roots and not name.endswith('-bin') and name != 'disk-data' and not name.endswith('-target') and name != 'build']
    for name in names:
        if name.startswith('pr-body-'):
            continue
        path = Path(directory) / name
        if path.suffix in accepted:
            files.append(path)
files.sort()
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
(out / 'evidence-members.sha256').write_text(''.join(f'{sha(p)}  {p.relative_to(root)}\n' for p in files))
with tarfile.open(out / 'evidence.tar.gz', 'w:gz', compresslevel=6) as archive:
    for path in files:
        archive.add(path, arcname=str(path.relative_to(root)), recursive=False)
(out / 'archive.sha256').write_text(f'{sha(out / "evidence.tar.gz")}  evidence.tar.gz\n')
paths = subprocess.check_output(['git', 'diff', '--name-only', '--', 'crates', 'scripts', '.github', 'docs/verification/proof-contracts.json', 'Cargo.lock', 'tools', 'fuzz'], cwd=repo, text=True).splitlines()
paths += subprocess.check_output(['git', 'ls-files', '--others', '--exclude-standard', 'crates', 'scripts', '.github'], cwd=repo, text=True).splitlines()
(out / 'source-sha256.json').write_text(json.dumps({p: sha(repo / p) for p in sorted(set(paths))}, indent=2) + '\n')
binaries = {folder.name: {p.name: sha(p) for p in folder.iterdir() if p.is_file()}
            for folder in root.glob('*-bin')}
(out / 'binary-sha256.json').write_text(json.dumps(binaries, indent=2) + '\n')
print(f'{len(files)} evidence files; {sum(p.stat().st_size for p in files)} uncompressed bytes')

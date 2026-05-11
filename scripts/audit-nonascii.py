"""Scan the repo for non-ASCII characters. Excluded from git."""
import os, re
from collections import defaultdict

non_ascii_re = re.compile(rb'[^\x00-\x7f]+')
exts = {'.rs', '.toml', '.md', '.ftl', '.yml', '.yaml', '.json',
        '.html', '.css', '.lock'}
named = {'Cargo.lock', 'rust-toolchain.toml', 'rustfmt.toml',
         '.gitignore', '.gitattributes', '.editorconfig'}
skip_dirs = {'.git', 'target', '.cargo', '.vscode', '.idea'}

per_file = defaultdict(list)
for root, dirs, files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in skip_dirs]
    for f in files:
        if os.path.splitext(f)[1].lower() not in exts and f not in named:
            continue
        path = os.path.join(root, f).replace(os.sep, '/')
        try:
            with open(path, 'rb') as fh:
                for i, line in enumerate(fh, 1):
                    if non_ascii_re.search(line):
                        chars = sorted({c for c in line.decode('utf-8', 'replace')
                                        if ord(c) > 127})
                        per_file[path].append((i, ''.join(chars)))
        except Exception:
            pass

for p in sorted(per_file):
    lines = per_file[p]
    cs = sorted({c for _, ch in lines for c in ch})
    rendered = ' '.join(f'U+{ord(c):04X}' for c in cs)
    print(f'{p}  lines={len(lines)}  chars={rendered}')

print(f'\nTOTAL files-with-nonascii: {len(per_file)}')

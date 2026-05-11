"""Replace every non-ASCII character in the repo with an ASCII equivalent.

Strategy:
- Mass substitutions for the common offenders (section sign, dashes,
  ellipsis, arrows, box drawing, status marks).
- One special case: the test that asserts the postgres reconciler
  rejects unicode names needs to keep its unicode -- but in
  Rust source we can write it as a `\\u{...}` escape so the .rs file
  itself stays ASCII.
"""
import os
import re

SUBS = [
    # Section sign first (it's standalone -- no risk of breaking words)
    ('§ ', 'section '),
    ('§', 'section '),
    # Dashes
    ('—', '--'),
    ('–', '-'),
    # Ellipsis
    ('…', '...'),
    # Arrows
    ('↔', '<->'),
    ('→', '->'),
    ('←', '<-'),
    # Status marks
    ('✓', 'OK'),
    ('✗', 'FAIL'),
    # Box drawing -> ASCII tree
    ('├──', '+--'),
    ('└──', '+--'),
    ('│', '|'),
    ('─', '-'),
    ('├', '+'),
    ('└', '+'),
]

# Files where we intentionally keep unicode by replacing with a Rust
# `\u{xx}` escape rather than substituting away the character.
SPECIAL_FILES = {
    'crates/computeza-reconciler-postgres/src/reconciler.rs': [
        # "na\u{ef}ve" represents "naive" with i-with-diaeresis -- the test
        # asserts validate_identifier rejects non-[A-Za-z0-9_-] chars.
        ('naïve', 'na\\u{00ef}ve'),
    ],
}

EXTS = {'.rs', '.toml', '.md', '.ftl', '.yml', '.yaml', '.json',
        '.html', '.css', '.lock'}
NAMED = {'Cargo.lock', 'rust-toolchain.toml', 'rustfmt.toml',
         '.gitignore', '.gitattributes', '.editorconfig'}
SKIP_DIRS = {'.git', 'target', '.cargo', '.vscode', '.idea'}

changed_files = []
for root, dirs, files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
    for f in files:
        ext = os.path.splitext(f)[1].lower()
        if ext not in EXTS and f not in NAMED:
            continue
        path = os.path.join(root, f).replace(os.sep, '/').lstrip('./')
        try:
            with open(path, 'r', encoding='utf-8') as fh:
                text = fh.read()
        except (UnicodeDecodeError, OSError):
            continue
        original = text
        # Special-file substitutions first
        for find, repl in SPECIAL_FILES.get(path, []):
            text = text.replace(find, repl)
        # Generic substitutions
        for find, repl in SUBS:
            text = text.replace(find, repl)
        if text != original:
            with open(path, 'w', encoding='utf-8', newline='') as fh:
                fh.write(text)
            changed_files.append(path)

print(f'Rewrote {len(changed_files)} files.')
for p in sorted(changed_files):
    print(f'  {p}')

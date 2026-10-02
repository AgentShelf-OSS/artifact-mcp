#!/usr/bin/env python3
"""Check snapshot body digests and sync a staged backup before publication.

Bundle manifests use JavaScript's UTF-16 path ordering and compact JSON encoding,
matching both application runtimes. The database defines the required body set;
included regular preview files are checked against hashes captured before their copy.
Absent previews and opaque symlinks do not become required artifact content.
"""

import argparse
import hashlib
import json
import os
import sqlite3
import sys
from urllib.parse import quote

class VerificationError(Exception):
    pass

def utf16_key(value):
    raw = value.encode('utf-16-le', 'surrogatepass')
    return tuple(raw[i] | raw[i+1] << 8 for i in range(0, len(raw), 2))

def sha_file(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()

PREVIEW_MANIFEST = 'preview-sha256.json'

def preview_files(directory):
    if not os.path.lexists(directory) or os.path.islink(directory):
        return
    if not os.path.isdir(directory):
        raise VerificationError('preview cache must be a directory when included')
    for base, dirs, files in os.walk(directory, followlinks=False):
        dirs[:] = sorted(name for name in dirs if not os.path.islink(os.path.join(base, name)))
        for name in sorted(files):
            full = os.path.join(base, name)
            if os.path.islink(full) or not os.path.lexists(full):
                continue
            if not os.path.isfile(full):
                raise VerificationError('preview cache contains an unsupported entry')
            yield os.path.relpath(full, directory).replace(os.sep, '/'), full

def capture_previews(root, source):
    hashes = {}
    for relative, full in preview_files(source):
        try:
            hashes[relative] = sha_file(full)
        except FileNotFoundError:
            # Optional cache entries may disappear while their manifest is being captured.
            continue
    with open(os.path.join(root, PREVIEW_MANIFEST), 'x') as manifest:
        json.dump({'version': 1, 'files': hashes}, manifest, sort_keys=True)
        manifest.write('\n')

def verify_previews(root, required):
    manifest_path = os.path.join(root, PREVIEW_MANIFEST)
    if not os.path.lexists(manifest_path):
        if required:
            raise VerificationError('optional preview manifest is missing')
        return
    if os.path.islink(manifest_path) or not os.path.isfile(manifest_path):
        raise VerificationError('optional preview manifest is not a regular file')
    try:
        with open(manifest_path) as manifest:
            captured = json.load(manifest)
    except (ValueError, UnicodeError) as exc:
        raise VerificationError('optional preview manifest is invalid') from exc
    if not isinstance(captured, dict) or captured.get('version') != 1 or not isinstance(captured.get('files'), dict):
        raise VerificationError('optional preview manifest is invalid')
    hashes = captured['files']
    for relative, full in preview_files(os.path.join(root, 'previews')):
        if hashes.get(relative) != sha_file(full):
            raise VerificationError(f'optional preview digest mismatch: {relative}')

def digest_body(path, bundle):
    os.lstat(path)
    if os.path.islink(path) or not os.path.isfile(path) and not os.path.isdir(path):
        raise VerificationError(f'required body is not a regular file/directory: {path}')
    if not bundle:
        if not os.path.isfile(path) or os.path.islink(path):
            raise VerificationError(f'required file body missing or invalid: {path}')
        return sha_file(path)
    if not os.path.isdir(path) or os.path.islink(path):
        raise VerificationError(f'required bundle directory missing or invalid: {path}')
    entries = []
    def walk(directory, prefix=''):
        names = sorted(os.listdir(directory), key=utf16_key)
        for name in names:
            full = os.path.join(directory, name)
            rel = f'{prefix}/{name}' if prefix else name
            os.lstat(full)
            if os.path.islink(full):
                raise VerificationError(f'bundle contains symlink: {rel}')
            if os.path.isdir(full):
                walk(full, rel)
            elif os.path.isfile(full):
                entries.append([rel.replace(os.sep, '/'), sha_file(full)])
            else:
                raise VerificationError(f'bundle contains unsupported entry: {rel}')
    walk(path)
    entries.sort(key=lambda item: utf16_key(item[0]))
    encoded = json.dumps(entries, ensure_ascii=False, separators=(',', ':')).encode()
    return hashlib.sha256(encoded).hexdigest()

def verify_row(root, row, history=False):
    ident, revision, bundle, expected = row if history else (row[0], None, row[1], row[2])
    base = os.path.join(root, 'artifacts', '.history', ident, str(revision)) if history and bundle else None
    if history:
        if bundle:
            path = base
        else:
            path = os.path.join(root, 'artifacts', '.history', ident, f'{revision}.html')
    else:
        path = os.path.join(root, 'artifacts', ident if bundle else f'{ident}.html')
    relative = os.path.relpath(path, root)
    if relative == '..' or relative.startswith('../'):
        raise VerificationError('required body escapes the backup directory')
    current = root
    for component in relative.split(os.sep):
        current = os.path.join(current, component)
        if os.path.islink(current):
            raise VerificationError(f'required body depends on a symlink: {relative}')
    if not os.path.lexists(path):
        raise VerificationError(f'required body missing: {os.path.relpath(path, root)}')
    actual = digest_body(path, bool(bundle))
    if expected and actual != expected:
        raise VerificationError(f'digest mismatch: {os.path.relpath(path, root)}')

def sync_tree(root):
    for directory, dirs, files in os.walk(root, topdown=False, followlinks=False):
        for name in files:
            path = os.path.join(directory, name)
            if os.path.islink(path):
                continue
            fd = os.open(path, os.O_RDONLY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        fd = os.open(directory, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    fd = os.open(root, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('root')
    ap.add_argument('--database-required', action='store_true')
    ap.add_argument('--capture-previews-from')
    ap.add_argument('--preview-manifest-required', action='store_true')
    args = ap.parse_args()
    root = os.path.abspath(args.root)
    if os.path.islink(root) or not os.path.isdir(root):
        raise VerificationError('backup root must be exactly one existing directory')
    if args.capture_previews_from is not None:
        capture_previews(root, args.capture_previews_from)
        return
    verify_previews(root, args.preview_manifest_required)
    dbpath = os.path.join(root, 'artifacts.db')
    if not os.path.exists(dbpath):
        if args.database_required:
            raise VerificationError('database snapshot is missing')
        sync_tree(root)
        return
    if os.path.islink(dbpath) or not os.path.isfile(dbpath):
        raise VerificationError('database snapshot is not a regular file')
    db = sqlite3.connect(f'file:{quote(dbpath, safe="/")}?mode=ro', uri=True)
    try:
        try:
            pending = db.execute('SELECT count(*) FROM artifact_durability_intents').fetchone()[0]
        except sqlite3.Error as exc:
            raise VerificationError(f'durability intent table unavailable: {exc}')
        if pending:
            raise VerificationError(f'{pending} pending durability intent(s)')
        try:
            rows = db.execute('SELECT id,is_bundle,body_sha256 FROM artifacts').fetchall()
            artifact_columns = {row[1] for row in db.execute('PRAGMA table_info(artifacts)').fetchall()}
            if 'revision' in artifact_columns:
                histories = db.execute('''
                    SELECT r.artifact_id,r.revision,r.is_bundle,r.body_sha256
                    FROM artifact_revisions r LEFT JOIN artifacts a ON a.id = r.artifact_id
                    WHERE a.id IS NULL OR r.revision < a.revision
                ''').fetchall()
            else:
                histories = db.execute('SELECT artifact_id,revision,is_bundle,body_sha256 FROM artifact_revisions').fetchall()
        except sqlite3.Error as exc:
            raise VerificationError(f'artifact tables unavailable: {exc}')
        for row in rows:
            verify_row(root, row)
        for row in histories:
            verify_row(root, row, True)
    finally:
        db.close()
    sync_tree(root)

if __name__ == '__main__':
    try:
        main()
    except (VerificationError, OSError, sqlite3.Error) as exc:
        print(f'backup: coherence check failed: {exc}', file=sys.stderr)
        sys.exit(1)

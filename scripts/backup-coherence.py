#!/usr/bin/env python3
"""Check snapshot body digests and sync a staged backup before publication.

Bundle manifests use JavaScript's UTF-16 path ordering and compact JSON encoding,
matching both application runtimes. The database defines the required body set;
previews and unreferenced files do not participate in content verification.
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
    args = ap.parse_args()
    root = os.path.abspath(args.root)
    if os.path.islink(root) or not os.path.isdir(root):
        raise VerificationError('backup root must be exactly one existing directory')
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

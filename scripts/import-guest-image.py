#!/usr/bin/env python3
"""Verify and atomically import a guest builder output into an offline HyperOS disk."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile


def read_manifest(source):
    with open_regular(source / 'manifest.json') as stream:
        manifest = json.load(stream)
    if (not isinstance(manifest, dict)
            or set(manifest) != {'image_id', 'sha256', 'format'}
            or not isinstance(manifest['image_id'], str)
            or not re.fullmatch(r'[A-Za-z0-9_-]{1,64}', manifest['image_id'])
            or manifest['format'] != 'raw'
            or not isinstance(manifest['sha256'], str)
            or not re.fullmatch(r'[0-9a-f]{64}', manifest['sha256'])):
        raise ValueError('invalid guest manifest')
    return manifest


def open_regular(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise ValueError(f'expected a regular file: {path}')
    return os.fdopen(fd, 'rb')


def checksum(stream):
    digest = hashlib.sha256()
    for block in iter(lambda: stream.read(1024 * 1024), b''):
        digest.update(block)
    return digest.hexdigest()


def import_into(source, base):
    manifest = read_manifest(source)
    if base.is_symlink():
        raise ValueError('base-images must not be a symlink')
    base.mkdir(mode=0o700, parents=True, exist_ok=True)
    # Serialize publication; never replace an image that instances may reference.
    fd = os.open(base / '.import.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'r+b') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        destination = base / manifest['image_id']
        if destination.exists() or destination.is_symlink():
            raise ValueError(f'image already exists: {destination.name}')
        with tempfile.TemporaryDirectory(prefix='.import-', dir=base) as td:
            staged = Path(td) / 'image'
            staged.mkdir(mode=0o700)
            with open_regular(source / 'disk.raw') as disk:
                if checksum(disk) != manifest['sha256']:
                    raise ValueError('guest disk checksum mismatch')
                # Copy the held descriptor, preserving sparse extents across filesystems.
                subprocess.run(['cp', '--sparse=always', '--reflink=auto',
                                f'/proc/self/fd/{disk.fileno()}', str(staged / 'disk.raw')],
                               pass_fds=(disk.fileno(),), check=True)
            with open_regular(staged / 'disk.raw') as disk:
                if checksum(disk) != manifest['sha256']:
                    raise ValueError('copied disk checksum mismatch')
            (staged / 'manifest.json').write_text(json.dumps(manifest) + '\n')
            for path in staged.iterdir():
                path.chmod(0o444)
                with path.open('rb') as stream:
                    os.fsync(stream.fileno())
            directory = os.open(staged, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
            os.rename(staged, destination)
            directory = os.open(base, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
    return manifest['image_id']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('guest', type=Path, help='builder output directory with manifest.json and disk.raw')
    parser.add_argument('image', type=Path, help='offline hyperos.img (never a mounted/running disk)')
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error('loop mounting requires root; run with sudo')
    # Reject invalid input before attaching or mounting the destination.
    read_manifest(args.guest)
    with open_regular(args.guest / 'disk.raw') as disk:
        if checksum(disk) != read_manifest(args.guest)['sha256']:
            parser.error('guest disk checksum mismatch')
    if subprocess.check_output(['losetup', '-j', str(args.image.resolve())], text=True).strip():
        parser.error('destination image is already attached to a loop device')
    with args.image.open('r+b') as image:
        fcntl.flock(image, fcntl.LOCK_EX | fcntl.LOCK_NB)
        loop = subprocess.check_output(['losetup', '--find', '--show', '--partscan',
                                        str(args.image.resolve())], text=True).strip()
        try:
            td = tempfile.mkdtemp(prefix='hyperos-import-')
            mounted = False
            try:
                part = loop + 'p3'
                label = subprocess.check_output(['blkid', '-s', 'LABEL', '-o', 'value', part], text=True).strip()
                fs = subprocess.check_output(['blkid', '-s', 'TYPE', '-o', 'value', part], text=True).strip()
                if label != 'HYPEROS_DATA' or fs != 'btrfs':
                    raise ValueError('partition 3 must be HYPEROS_DATA Btrfs')
                subprocess.run(['mount', '-o', 'nosuid,nodev,noexec,compress=zstd:3', part, td], check=True)
                mounted = True
                print(import_into(args.guest, Path(td) / 'base-images'))
            finally:
                if mounted:
                    subprocess.run(['umount', td], check=True)
                # Never recursively remove a mountpoint: a failed unmount must preserve data.
                os.rmdir(td)
        finally:
            subprocess.run(['losetup', '-d', loop], check=True)


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        raise SystemExit(f'FAIL: {error}')

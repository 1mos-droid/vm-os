#!/usr/bin/env python3
"""Unprivileged checks for guest admission and the release runner's safe path."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

REPO = Path(__file__).resolve().parent.parent
import sys
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('guest_import', REPO / 'scripts/import-guest-image.py')
admission = importlib.util.module_from_spec(spec)
spec.loader.exec_module(admission)


class ImageTools(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / 'guest'
        self.source.mkdir()
        self.base = self.root / 'base-images'
        self.payload = b'guest disk fixture\0' * 100
        (self.source / 'disk.raw').write_bytes(self.payload)
        self.manifest = {'image_id': 'alpine-test', 'format': 'raw',
                         'sha256': hashlib.sha256(self.payload).hexdigest()}
        self.write_manifest()

    def write_manifest(self):
        (self.source / 'manifest.json').write_text(json.dumps(self.manifest))

    def test_publish_verified_readonly_copy_without_overwrite(self):
        self.assertEqual(admission.import_into(self.source, self.base), 'alpine-test')
        target = self.base / 'alpine-test'
        self.assertEqual((target / 'disk.raw').read_bytes(), self.payload)
        self.assertEqual((target / 'disk.raw').stat().st_mode & 0o777, 0o444)
        self.assertEqual(json.loads((target / 'manifest.json').read_text()), self.manifest)
        with self.assertRaisesRegex(ValueError, 'already exists'):
            admission.import_into(self.source, self.base)

    def test_invalid_ids_and_schema(self):
        for image_id in ('../escape', '', 'a' * 65, 'é', 7):
            self.manifest['image_id'] = image_id
            self.write_manifest()
            with self.assertRaises(ValueError):
                admission.import_into(self.source, self.base)
        self.assertFalse(self.base.exists())

    def test_corruption_does_not_publish(self):
        (self.source / 'disk.raw').write_bytes(b'corrupt')
        with self.assertRaisesRegex(ValueError, 'checksum'):
            admission.import_into(self.source, self.base)
        self.assertFalse((self.base / 'alpine-test').exists())
        self.assertFalse(list(self.base.glob('.import-*')))

    def test_copy_failure_does_not_publish(self):
        with patch.object(admission.subprocess, 'run', side_effect=OSError('disk full')):
            with self.assertRaises(OSError):
                admission.import_into(self.source, self.base)
        self.assertFalse((self.base / 'alpine-test').exists())
        self.assertFalse(list(self.base.glob('.import-*')))

    def test_symlink_source_rejected(self):
        (self.source / 'disk.raw').unlink()
        (self.root / 'outside').write_bytes(self.payload)
        (self.source / 'disk.raw').symlink_to(self.root / 'outside')
        with self.assertRaises(OSError):
            admission.import_into(self.source, self.base)

    def test_symlink_destination_rejected(self):
        self.base.symlink_to(self.source, target_is_directory=True)
        with self.assertRaises(ValueError):
            admission.import_into(self.source, self.base)

    def test_copied_data_is_reverified(self):
        def corrupt_copy(args, **kwargs):
            Path(args[-1]).write_bytes(b'changed during copy')
        with patch.object(admission.subprocess, 'run', side_effect=corrupt_copy):
            with self.assertRaisesRegex(ValueError, 'copied disk checksum'):
                admission.import_into(self.source, self.base)
        self.assertFalse((self.base / 'alpine-test').exists())

    def test_loop_and_mount_cleanup_on_import_failure(self):
        image = self.root / 'host.img'
        image.touch()
        mountpoint = self.root / 'mount'
        mountpoint.mkdir()
        calls = []

        def output(args, **kwargs):
            if args[0] == 'losetup':
                return '' if '-j' in args else '/dev/loop-test\n'
            return 'HYPEROS_DATA\n' if 'LABEL' in args else 'btrfs\n'

        with patch.object(sys, 'argv', ['import', str(self.source), str(image)]), \
                patch.object(admission.os, 'geteuid', return_value=0), \
                patch.object(admission.tempfile, 'mkdtemp', return_value=str(mountpoint)), \
                patch.object(admission.subprocess, 'check_output', side_effect=output), \
                patch.object(admission.subprocess, 'run', side_effect=lambda args, **kw: calls.append(args)), \
                patch.object(admission, 'import_into', side_effect=ValueError('test failure')):
            with self.assertRaisesRegex(ValueError, 'test failure'):
                admission.main()
        self.assertEqual(calls[-2:], [['umount', str(mountpoint)], ['losetup', '-d', '/dev/loop-test']])
        self.assertFalse(mountpoint.exists())

    def test_release_dry_run_and_invalid_arguments(self):
        script = REPO / 'scripts/test-v1-release.sh'
        result = subprocess.run([script, '--dry-run'], capture_output=True, text=True, check=True)
        self.assertIn('input image remains unchanged', result.stdout)
        for arguments in (['--image'], ['--ram', 'bad'], ['--expand-gib', '0']):
            self.assertNotEqual(subprocess.run([script, *arguments], capture_output=True).returncode, 0)

    @unittest.skipUnless(shutil.which('jq'), 'jq is needed for the installed VM command')
    def test_vm_command_uses_current_rpc_and_reports_start_failure(self):
        bin_dir = self.root / 'bin'
        bin_dir.mkdir()
        socat = bin_dir / 'socat'
        socat.write_text('''#!/usr/bin/env python3
import json, os, sys
request = json.load(sys.stdin)
if request['method'] == 'create_vm':
    assert request['params'] == {'name':'alpine-test','base_image_id':'alpine-test',
        'vcpu_count':1,'ram_mib':512,'network_mode':'isolated','disposable':False}
    response = {'result': {'vm_id':'b6f51eed-8dc7-47f6-a8e7-81b6ae783a21'}}
else:
    assert request['method'] == 'start_vm'
    assert request['params']['vm_id'] == 'b6f51eed-8dc7-47f6-a8e7-81b6ae783a21'
    response = ({'error': {'code':-32005,'message':'launch failed'}}
        if os.environ.get('TEST_START_FAILURE') else {'result':{'state':'running'}})
print(json.dumps({'jsonrpc':'2.0','id':request['id'], **response}))
''')
        socat.chmod(0o755)
        env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ['PATH'])
        command = REPO / 'kernel-buildroot/rootfs-overlay/usr/bin/hyperos-vm'
        result = subprocess.run([command, 'alpine-test'], env=env, capture_output=True, text=True, check=True)
        self.assertEqual(json.loads(result.stdout), {'state': 'running'})
        result = subprocess.run([command, 'alpine-test'], env=dict(env, TEST_START_FAILURE='1'),
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('launch failed', result.stderr)


if __name__ == '__main__':
    unittest.main()

import tempfile
import unittest
import subprocess
import tarfile
from pathlib import Path
from lib.remote_build import make_snapshot


class ClientTests(unittest.TestCase):
    def test_snapshot_uses_working_files_without_caches_or_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)/'src';root.mkdir()
            subprocess.run(['git','init','-q',str(root)],check=True)
            for name, data in [('Cargo.toml','source'),('.env','secret'),('target/build','cache'),('.codex/config.toml','private')]:
                path=root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text(data)
            subprocess.run(['git','-C',str(root),'add','.'],check=True)
            (root/'Cargo.toml').write_text('working change')
            archive=Path(directory)/'source.tar.gz'
            receipt=make_snapshot(root,archive)
            with tarfile.open(archive) as tar:
                self.assertEqual(tar.getnames(), ['Cargo.toml'])
                self.assertEqual(tar.extractfile('Cargo.toml').read(), b'working change')
            self.assertEqual(receipt['file_count'],1)

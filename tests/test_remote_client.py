import tempfile
import unittest
import subprocess
import tarfile
import hashlib
import shlex
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

    def execute_transaction(self,profile,header_failure=False,artifact_symlink=False):
        from lib.remote_build import transaction_script
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory).resolve();source=root/'remote/sources/checkout';source.mkdir(parents=True)
            target=root/'remote/targets/checkout'/profile;target.mkdir(parents=True)
            (source/'deleted.rs').write_text('stale source')
            (target/'retained.cache').write_text('retained')
            new=root/'new.rs';new.write_text('new snapshot')
            archive=root/'upload.tar.gz'
            with tarfile.open(archive,'w:gz') as tar:tar.add(new,arcname='new.rs')
            digest=hashlib.sha256(archive.read_bytes()).hexdigest()
            command,export=transaction_script('checkout',str(archive),digest,profile,'run-test')
            body=shlex.split(command)[-1].replace('/mnt/runner-data/remote-build',str(root/'remote'))
            body=body.replace('/run/oxidex-',str(root/'lock-'))
            docker=root/'docker';docker.write_text('#!/bin/sh\nexit 0\n');docker.chmod(0o755)
            body=body.replace('/usr/bin/docker',str(docker))
            launcher=root/'launcher'
            launcher.write_text('#!/bin/sh\n'
                'if [ "$2" = just ]; then exit '+('1' if header_failure else '0')+'; fi\n'
                'if [ "$2 $3" = "cargo build" ]; then\n'
                ' touch '+shlex.quote(str(root/'compiled'))+'\n'
                ' printf "#!/bin/sh\\ntouch '+str(root/'root-executed')+'\\n" >'+shlex.quote(str(target/'oxidex'))+'\n'
                ' chmod +x '+shlex.quote(str(target/'oxidex'))+'\n'+
                (' ln -sf '+shlex.quote(str(new))+' '+shlex.quote(str(target/'oxidex'))+'\n' if artifact_symlink else '')+
                'fi\n'
                'if [ "$2 $3" = "cargo run" ]; then touch '+shlex.quote(str(root/'container-verified'))+'; fi\n')
            launcher.chmod(0o755);body=body.replace('/usr/local/bin/oxidex-remote-build',str(launcher))
            # This local fixture exercises ordering, extraction and shell failure
            # propagation; Linux lock/cancellation behavior has a live canary.
            body=body.replace('flock -n -E 75 '+str(root/'lock-cache-active-checkout.lock')+' true','true')
            result=subprocess.run(['sh','-c',body],capture_output=True,text=True)
            self.assertFalse((source/'deleted.rs').exists())
            self.assertEqual((source/'new.rs').read_text(),'new snapshot')
            self.assertEqual((target/'retained.cache').read_text(),'retained')
            self.assertFalse(archive.exists())
            self.assertFalse((root/'root-executed').exists())
            if header_failure or artifact_symlink:
                self.assertNotEqual(result.returncode,0)
                if header_failure:self.assertFalse((root/'compiled').exists())
            else:
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertTrue((root/'container-verified').exists())
                exported=root/'remote/exports/run-test/oxidex'
                self.assertEqual(exported.read_bytes(),(target/'oxidex').read_bytes())

    def test_failed_remote_header_validation_prevents_release_compilation(self):
        self.execute_transaction('release',header_failure=True)

    def test_deleted_snapshot_source_does_not_survive_cache_reuse(self):
        self.execute_transaction('debug')

    def test_symlinked_artifact_is_not_exported(self):
        self.execute_transaction('debug',artifact_symlink=True)

    def test_worker_interruption_is_retryable_but_compiler_errors_are_not(self):
        from lib.remote_build import retryable_exit
        self.assertTrue(retryable_exit(255))
        self.assertTrue(retryable_exit(137))
        self.assertTrue(retryable_exit(143))
        self.assertFalse(retryable_exit(101))
        self.assertFalse(retryable_exit(1))

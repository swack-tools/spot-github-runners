"""Small synthetic checks for the installed-runtime finish prerequisite."""
import base64
import hashlib
import inspect
import fcntl
import tempfile
import subprocess
from pathlib import Path
import sys
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'src'))
from lib import builder_runtime_update as runtime

class InstalledRuntimeTests(unittest.TestCase):
    def fixture(self):
        raw=b'bounded synthetic runtime\n'
        digest=hashlib.sha256(raw).hexdigest()
        assets={path:{'data':base64.b64encode(raw).decode(),'sha256':digest} for path in runtime.TARGETS}
        files={path:{'sha256':digest,'mode':0o644 if path==runtime.LAUNCHER_TARGET else mode} for path,mode in runtime.TARGETS.items()}
        return assets,files,digest

    def verify(self,assets,files,pin):
        self.assertTrue(callable(getattr(runtime,'verify_installed_runtime',None)), 'Installed runtime verifier is missing')
        return runtime.verify_installed_runtime(assets,files,client_launcher_sha256=pin)

    def test_exact_held_install_matches_client_pin(self):
        assets,files,pin=self.fixture()
        self.assertEqual(self.verify(assets,files,pin),pin)

    def test_changed_file_denied(self):
        assets,files,pin=self.fixture();files['/usr/local/bin/docker']['sha256']='0'*64
        with self.assertRaises(ValueError):self.verify(assets,files,pin)

    def test_open_launcher_denied_before_finish(self):
        assets,files,pin=self.fixture();files[runtime.LAUNCHER_TARGET]['mode']=0o755
        with self.assertRaises(ValueError):self.verify(assets,files,pin)

    def test_missing_and_extra_runtime_denied(self):
        for extra in (False,True):
            assets,files,pin=self.fixture()
            if extra:files['/etc/unrelated']={}
            else:files.pop('/usr/local/bin/docker')
            with self.subTest(extra=extra),self.assertRaises(ValueError):self.verify(assets,files,pin)

    def test_old_or_missing_client_pin_denied(self):
        for pin in (None,'0'*64,'invalid'):
            assets,files,_=self.fixture()
            with self.subTest(pin=pin),self.assertRaises(ValueError):self.verify(assets,files,pin)

    def test_missing_file_state_denied(self):
        assets,files,pin=self.fixture();files['/usr/local/bin/docker']=None
        with self.assertRaises(ValueError):self.verify(assets,files,pin)

class FinishCliTests(unittest.TestCase):
    def invoke(self,*args):
        return subprocess.run([sys.executable,'-I','-B',str(Path(runtime.__file__).resolve()),*args],capture_output=True,text=True,timeout=2)

    def test_finish_is_an_explicit_cli_action(self):
        result=self.invoke('--help')
        self.assertEqual(result.returncode,0)
        self.assertIn('--finish',result.stdout)
        self.assertIn('--client-launcher-sha256',result.stdout)
        self.assertIn('--finish-nonce',result.stdout)

    def test_apply_and_finish_are_mutually_exclusive(self):
        result=self.invoke('--plan','synthetic.json','--apply','--finish')
        self.assertEqual(result.returncode,2)
        self.assertIn('not allowed with argument',result.stderr)

class LauncherRestoreTests(unittest.TestCase):
    def state(self,path):
        info=path.stat()
        return dict(sha256=hashlib.sha256(path.read_bytes()).hexdigest(),mode=info.st_mode&0o777,uid=info.st_uid,device=info.st_dev,inode=info.st_ino)

    def restore(self,path,expected):
        self.assertTrue(callable(getattr(runtime,'restore_launcher_execution',None)), 'Verified launcher restore is missing')
        return runtime.restore_launcher_execution(path,expected)

    def test_only_exact_held_launcher_is_restored(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'launcher';path.write_bytes(b'fixture');path.chmod(0o644)
            expected=self.state(path);self.restore(path,expected)
            self.assertEqual(path.stat().st_mode&0o777,0o755)
            self.assertEqual(path.stat().st_ino,expected['inode'])
            self.assertEqual(path.read_bytes(),b'fixture')

    def test_changed_bytes_or_inode_remain_held(self):
        for changed in ('bytes','inode'):
            with tempfile.TemporaryDirectory() as directory:
                path=Path(directory).resolve()/'launcher';path.write_bytes(b'fixture');path.chmod(0o644);expected=self.state(path)
                if changed=='bytes':path.write_bytes(b'tampered')
                else:
                    other=path.parent/'new';other.write_bytes(b'fixture');other.chmod(0o644);other.replace(path)
                with self.subTest(changed=changed),self.assertRaises(ValueError):self.restore(path,expected)
                self.assertEqual(path.stat().st_mode&0o777,0o644)

class InstalledHistoryTests(unittest.TestCase):
    def fixture(self):
        raw=b'bounded synthetic runtime\n';sha=hashlib.sha256(raw).hexdigest()
        assets={path:{'data':base64.b64encode(raw).decode(),'sha256':sha} for path in runtime.TARGETS}
        files={path:{'sha256':sha,'mode':0o644 if path==runtime.LAUNCHER_TARGET else mode} for path,mode in runtime.TARGETS.items()}
        original=dict(schema=1,nonce='a'*32,source_main='b'*40,vm_id='123',boot='0b99c4d2-ca5f-4471-bf84-ff4ab8dbefd3',image_id='sha256:'+'c'*64,config_sha256='d'*64,admission_sha256='e'*64,service_sha256='f'*64,service_pid=10,service_startticks=20,observed_at=1000.0,assets=assets,before={path:None for path in runtime.TARGETS})
        current=dict(original,service_pid=30,service_startticks=40,observed_at=2000.0,before=files)
        journal=[dict(stage='FILES_AND_SERVICE_VERIFIED',service_pid=30,service_startticks=40,files=files)]
        return original,current,journal,sha

    def verify(self,original,current,journal,pin):
        self.assertTrue(callable(getattr(runtime,'verify_installed_history',None)), 'Successful installation history verifier is missing')
        return runtime.verify_installed_history(current,original,journal,pin,2001.0)

    def test_historical_install_and_fresh_finish_are_distinct(self):
        original,current,journal,pin=self.fixture()
        self.assertEqual(self.verify(original,current,journal,pin),pin)
        self.assertEqual(original['observed_at'],1000.0)

    def test_failed_or_partial_install_refused(self):
        for stage in ('FILE_REPLACED','HOLD_EXPLICIT_RECOVERY','HOLD_BEFORE_DRAIN_RELEASE'):
            original,current,journal,pin=self.fixture();journal[-1]['stage']=stage
            with self.subTest(stage=stage),self.assertRaises(ValueError):self.verify(original,current,journal,pin)

    def test_different_install_identity_refused(self):
        for field,value in (('nonce','9'*32),('source_main','9'*40),('vm_id','999')):
            original,current,journal,pin=self.fixture();original[field]=value
            with self.subTest(field=field),self.assertRaises(ValueError):self.verify(original,current,journal,pin)

    def test_changed_service_or_file_after_install_refused(self):
        for changed in ('service','file'):
            original,current,journal,pin=self.fixture()
            if changed=='service':journal[-1]['service_pid']=99
            else:journal[-1]['files']={}
            with self.subTest(changed=changed),self.assertRaises(ValueError):self.verify(original,current,journal,pin)

    def test_expired_finish_refused(self):
        original,current,journal,pin=self.fixture();current['observed_at']=1500.0
        with self.assertRaises(ValueError):self.verify(original,current,journal,pin)

class OwnedFinishTests(unittest.TestCase):
    def run_finish(self,lock,drain,action,check=lambda:None):
        self.assertTrue(callable(getattr(runtime,'finish_owned_drain',None)), 'Owned finish transaction is missing')
        return runtime.finish_owned_drain(lock,drain,'owned',check,action)

    def fixture(self,directory):
        lock=Path(directory).resolve()/'lock';lock.touch()
        drain=lock.parent/'drain';drain.write_text('owned');drain.chmod(0o600)
        return lock,drain

    def test_success_keeps_drain_during_action_then_removes_it(self):
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory)
            def action():
                self.assertEqual(drain.read_text(),'owned')
                return 'verified'
            self.assertEqual(self.run_finish(lock,drain,action),'verified')
            self.assertFalse(drain.exists())

    def test_busy_build_lock_preserves_drain(self):
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory)
            with lock.open('r+') as holder:
                fcntl.flock(holder,fcntl.LOCK_EX|fcntl.LOCK_NB)
                with self.assertRaises(BlockingIOError):self.run_finish(lock,drain,lambda:self.fail('ran'))
            self.assertEqual(drain.read_text(),'owned')

    def test_foreign_drain_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory);drain.write_text('foreign')
            with self.assertRaises(ValueError):self.run_finish(lock,drain,lambda:self.fail('ran'))
            self.assertEqual(drain.read_text(),'foreign')

    def test_failed_reopen_keeps_drain(self):
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory)
            def action():raise ValueError('failed proof')
            with self.assertRaises(ValueError):self.run_finish(lock,drain,action)
            self.assertEqual(drain.read_text(),'owned')

    def test_replaced_drain_is_never_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory)
            def action():
                drain.unlink();drain.write_text('foreign')
            with self.assertRaises(ValueError):self.run_finish(lock,drain,action)
            self.assertEqual(drain.read_text(),'foreign')

    def test_final_check_failure_keeps_drain(self):
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory);checks=[]
            def check():
                checks.append(1)
                if len(checks)>1:raise ValueError('state changed')
            with self.assertRaises(ValueError):self.run_finish(lock,drain,lambda:None,check)
            self.assertEqual(drain.read_text(),'owned')

    def test_lock_becoming_writable_during_finish_keeps_drain(self):
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory)
            def action():lock.chmod(0o666)
            with self.assertRaises(ValueError):self.run_finish(lock,drain,action)
            self.assertEqual(drain.read_text(),'owned')

    def test_failure_callback_runs_under_same_exclusive_lock(self):
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory);seen=[]
            def action():raise ValueError('partial finish')
            def hold(error):
                with lock.open('r+') as other:
                    with self.assertRaises(BlockingIOError):fcntl.flock(other,fcntl.LOCK_EX|fcntl.LOCK_NB)
                self.assertEqual(drain.read_text(),'owned');seen.append(type(error).__name__)
            self.assertIn('on_failure',inspect.signature(runtime.finish_owned_drain).parameters)
            with self.assertRaises(ValueError):runtime.finish_owned_drain(lock,drain,'owned',lambda:None,action,on_failure=hold)
            self.assertEqual(seen,['ValueError'])

if __name__=='__main__':unittest.main()

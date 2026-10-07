"""Small synthetic checks for the installed-runtime finish prerequisite."""
import base64
import hashlib
import inspect
import os
import time
from contextlib import ExitStack
from unittest.mock import patch
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

class RecoveryLockTests(unittest.TestCase):
    def ensure(self,path):
        self.assertTrue(callable(getattr(runtime,'ensure_recovery_lock',None)), 'Checked reboot lock initialization is missing')
        runtime.ensure_recovery_lock(path)

    def test_missing_lock_created_and_existing_inode_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'lock';self.ensure(path);inode=path.stat().st_ino
            self.assertEqual(path.stat().st_mode&0o777,0o600)
            self.ensure(path);self.assertEqual(path.stat().st_ino,inode)

    def test_foreign_symlink_or_writable_lock_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory).resolve();target=root/'target';target.touch();path=root/'lock';path.symlink_to(target)
            with self.assertRaises((ValueError,OSError)):self.ensure(path)
            path.unlink();path.touch();path.chmod(0o666)
            with self.assertRaises(ValueError):self.ensure(path)
            self.assertEqual(path.stat().st_mode&0o777,0o666)

class InactiveServiceTests(unittest.TestCase):
    def parse(self,raw):
        self.assertTrue(callable(getattr(runtime,'parse_inactive_service_state',None)), 'Native inactive state parser is missing')
        return runtime.parse_inactive_service_state(raw)

    def test_exact_inactive_service_record(self):
        raw='ControlPID=0\nMainPID=0\nSubState=dead\nActiveState=inactive\nLoadState=loaded\n'
        self.assertEqual(self.parse(raw),dict(LoadState='loaded',ActiveState='inactive',SubState='dead',MainPID='0',ControlPID='0'))

    def test_unknown_duplicate_missing_or_running_state_refused(self):
        good='LoadState=loaded\nActiveState=inactive\nSubState=dead\nMainPID=0\nControlPID=0\n'
        for raw in (good+'MainPID=0\n',good+'Unknown=0\n',good.replace('ControlPID=0\n',''),good.replace('MainPID=0','MainPID=123'),good.replace('loaded','not-found')):
            with self.subTest(raw=raw),self.assertRaises(ValueError):self.parse(raw)

    def test_recovery_parser_accepts_exact_running_record(self):
        self.assertTrue(callable(getattr(runtime,'parse_recovery_service_state',None)), 'Recoverable service parser is missing')
        raw='LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=50\nControlPID=0\n'
        self.assertEqual(runtime.parse_recovery_service_state(raw)['MainPID'],'50')
        with self.assertRaises(ValueError):runtime.parse_recovery_service_state(raw.replace('running','start'))

class OrdinaryFinishClockTests(unittest.TestCase):
    def exercise(self,expire_at):
        _,plan,_,pin=InstalledHistoryTests().fixture();plan['observed_at']=time.time();clock=[plan['observed_at']];opened=[]
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory).resolve();lock=root/'lock';lock.touch();drain=root/'drain';drain.write_text(plan['nonce']);drain.chmod(0o600)
            pending=root/'pending';pending.write_text('owned');permit=root/'permit';permit.write_text('owned');launcher=root/'launcher';launcher.write_bytes(b'bounded synthetic runtime\n');launcher.chmod(0o644)
            def state(path):
                path=launcher if str(path)==runtime.LAUNCHER_TARGET else Path(path)
                if not path.exists():return None
                info=path.stat();return dict(sha256=hashlib.sha256(path.read_bytes()).hexdigest(),mode=info.st_mode&0o777,uid=info.st_uid,gid=info.st_gid,device=info.st_dev,inode=info.st_ino)
            plan['before']={k:dict(v) for k,v in plan['before'].items()};plan['before'][runtime.LAUNCHER_TARGET]=state(launcher)
            guard=root/'guard';guard.write_bytes(runtime.BOOT_GUARD_BYTES);guard.chmod(0o644)
            def snapshot(expected):return dict(expected,image_pin=expected['image_id'],admission_valid=True,cpu=16,role='builder',cpus=16,memory_gib=28,tools_ready=True,leases={},containers=[],upload_processes=[],pending_nonce=plan['nonce'] if pending.exists() else None,drain_token=drain.read_text())
            def record(path,row):
                if row['stage']==expire_at:clock[0]=plan['observed_at']+121
            real_fsync,real_owned,real_restore,real_close=os.fsync,runtime.finish_owned_drain,runtime.restore_launcher_execution,runtime.close_launcher_execution
            def sync(fd):
                real_fsync(fd)
                if expire_at=='pending-fsync' and not pending.exists():clock[0]=plan['observed_at']+121
            def restore(path,expected):opened.append(1);real_restore(launcher,expected)
            with ExitStack() as stack:
                replacements=dict(PENDING_UPDATE=pending,CURRENT_BOOT_PERMIT=permit,BOOT_GUARD=guard,installed_history=lambda *args,**kwargs:root,live_snapshot=snapshot,mutation_idle_fence=lambda plan:None,admission_ready=lambda *args,**kwargs:None,verify_boot_hold=lambda plan:None,root_file_state=state,append_journal_record=record,restore_launcher_execution=restore,close_launcher_execution=lambda path,**kwargs:real_close(launcher,**kwargs),finish_owned_drain=lambda _lock,_drain,*args,**kwargs:real_owned(lock,drain,*args,**kwargs))
                for name,value in replacements.items():stack.enter_context(patch.object(runtime,name,value))
                stack.enter_context(patch.object(time,'time',side_effect=lambda:clock[0]));stack.enter_context(patch.object(runtime.os,'fsync',side_effect=sync))
                with self.assertRaises(ValueError):runtime.finish_update(plan,pin)
            self.assertEqual(drain.read_text(),plan['nonce'])
            self.assertEqual(launcher.stat().st_mode&0o777,0o644)
            return opened

    def test_start_journal_expiry_does_not_restore_execution(self):self.assertEqual(self.exercise('FINISH_START'),[])
    def test_pending_directory_fsync_expiry_keeps_drain(self):self.exercise('pending-fsync')

class CommittedFlowTests(unittest.TestCase):
    def exercise(self,*,failure=None,existing=False,already_running=False,missing_lock=False,held=False):
        _,_,plan,pin=InstalledHistoryTests().reboot_fixture();plan['observed_at']=time.time()
        if already_running:plan['service_state']=dict(LoadState='loaded',ActiveState='active',SubState='running',MainPID='50',ControlPID='0',StartTicks=60)
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory).resolve();lock=root/'lock'
            if not missing_lock:lock.touch()
            drain=root/'drain';pending=root/'pending';pending.write_text('retained')
            launcher=root/'launcher';launcher.write_bytes(b'bounded synthetic runtime\n');launcher.chmod(0o644 if held else 0o755)
            if held:
                info=launcher.stat();plan['before']={k:dict(v) for k,v in plan['before'].items()}
                plan['before'][runtime.LAUNCHER_TARGET].update(mode=0o644,uid=info.st_uid,device=info.st_dev,inode=info.st_ino)
            if existing:drain.write_text(plan['nonce']);drain.chmod(0o600)
            running=already_running;executed=already_running;events=[];snapshots=0;captures=0;clock=[plan['observed_at']]
            def file_state(path):
                path=Path(path)
                if path==pending:
                    if not path.exists():return None
                    info=path.stat();return dict(inode=info.st_ino,sha256=hashlib.sha256(path.read_bytes()).hexdigest())
                return None
            def read(path):
                if str(path)=='/run/oxidex-build-draining':return None,drain.read_bytes() if drain.exists() else b''
                raise AssertionError('Unexpected fixture read '+str(path))
            def capture(*args):
                nonlocal captures
                captures+=1
                if failure=='capture-expiry' and captures>1:clock[0]=plan['observed_at']+121
                if running and not already_running:raise ValueError('Inactive capture used after service start')
                return dict(plan)
            def command(argv,**kwargs):
                nonlocal running
                self.assertEqual(argv,['/usr/bin/systemctl','start','gha-admission.service'])
                self.assertEqual(drain.read_text(),plan['nonce']);self.assertFalse(pending.exists())
                events.append('start')
                if failure=='start':raise OSError('service start failed')
                running=True;return ''
            def identity(*,strict=True):
                if strict and not executed:raise ValueError('Unexpected admission process')
                return 50,60
            def ready(active,pid,ticks,**kwargs):
                nonlocal executed
                executed=True
                self.assertTrue(running);self.assertEqual((pid,ticks),(50,60));events.append('ready')
                if failure=='readiness':raise ConnectionRefusedError('not ready')
            def snapshot(active):
                nonlocal snapshots
                snapshots+=1
                if failure=='final' and snapshots>1:raise ValueError('final host observation failed')
                return dict(active,image_pin=active['image_id'],admission_valid=True,cpu=16,role='builder',cpus=16,memory_gib=28,tools_ready=True,leases={},containers=[],upload_processes=[],pending_nonce=None,drain_token=drain.read_text())
            real_new,real_owned,real_init,real_restore=runtime.drained_update,runtime.finish_owned_drain,runtime.ensure_recovery_lock,runtime.restore_launcher_execution
            def new_transaction(_lock,_drain,*args,**kwargs):return real_new(lock,drain,*args,**kwargs)
            def owned_transaction(_lock,_drain,*args,**kwargs):return real_owned(lock,drain,*args,**kwargs)
            def journal(path,row):
                events.append(row['stage'])
                if failure=='checkpoint-expiry':clock[0]=plan['observed_at']+121
            with ExitStack() as stack:
                stack.enter_context(patch.object(time,'time',side_effect=lambda:clock[0]))
                replacements=dict(restore_launcher_execution=lambda path,expected:real_restore(launcher,expected),ensure_recovery_lock=lambda path:real_init(lock),PENDING_UPDATE=pending,capture_committed_recovery=capture,root_file_state=file_state,root_file_read=read,checked_command=command,service_identity=identity,admission_ready=ready,live_snapshot=snapshot,append_journal_record=journal,drained_update=new_transaction,finish_owned_drain=owned_transaction)
                for name,value in replacements.items():stack.enter_context(patch.object(runtime,name,value))
                if failure:
                    with self.assertRaises((OSError,ValueError)):runtime.complete_committed_recovery(plan,pin)
                    self.assertEqual(drain.read_text(),plan['nonce'])
                    if failure=='capture-expiry':self.assertEqual(launcher.stat().st_mode&0o777,0o644)
                else:
                    result=runtime.complete_committed_recovery(plan,pin)
                    self.assertEqual(result['status'],'REBOOT_RECOVERED_UNQUALIFIED');self.assertIs(result['qualification_ready'],False)
                    self.assertFalse(drain.exists());self.assertTrue(running);self.assertGreaterEqual(snapshots,2)
                    if held:self.assertEqual(launcher.stat().st_mode&0o777,0o755)
            return events

    def test_slow_capture_expiry_does_not_restore_launcher(self):self.exercise(failure='capture-expiry',existing=True,held=True)

    def test_held_postcommit_launcher_restored_under_owned_transaction(self):self.exercise(held=True,missing_lock=True)

    def test_recovery_initializes_missing_reboot_lock(self):self.exercise(missing_lock=True)
    def test_checkpoint_expiry_keeps_owned_drain(self):self.exercise(failure='checkpoint-expiry')

    def test_running_retry_uses_exact_service_without_restart(self):
        events=self.exercise(existing=True,already_running=True)
        self.assertNotIn('start',events)

    def test_success_releases_new_drain_after_actual_active_checks(self):self.exercise()
    def test_success_releases_existing_owned_drain(self):self.exercise(existing=True)
    def test_start_failure_keeps_owned_drain(self):self.exercise(failure='start')
    def test_readiness_failure_keeps_owned_drain(self):self.exercise(failure='readiness')
    def test_late_postflight_failure_keeps_owned_drain(self):
        events=self.exercise(failure='final')
        self.assertNotIn('COMMITTED_REBOOT_RECOVERED',events)

class JournalPublicationTests(unittest.TestCase):
    def reader(self,path):
        path=Path(path)
        if not path.exists():return None,b''
        raw=path.read_bytes();info=path.stat()
        return dict(sha256=hashlib.sha256(raw).hexdigest(),inode=info.st_ino,mode=info.st_mode&0o777),raw

    def append(self,path,row):
        self.assertTrue(callable(getattr(runtime,'append_journal_record',None)), 'Crash-safe journal publication is missing')
        # Root custody is covered separately; this adapter retains real file IO.
        with patch.object(runtime,'root_file_read',side_effect=self.reader):
            runtime.append_journal_record(path,row)

    def test_complete_records_preserve_historical_prefix(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'journal';before=b'{"stage":"VERIFIED"}\n';path.write_bytes(before)
            self.append(path,dict(stage='FINISH_START',observed_at=1))
            self.assertTrue(path.read_bytes().startswith(before))
            self.assertEqual(len(path.read_bytes().splitlines()),2)
            self.assertEqual(path.stat().st_mode&0o777,0o600)

    def test_failed_publication_preserves_complete_old_history(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'journal';before=b'{"stage":"VERIFIED"}\n';path.write_bytes(before)
            with patch.object(runtime.os,'replace',side_effect=OSError('interrupted publication')):
                with self.assertRaises(OSError):self.append(path,dict(stage='FINISH_START'))
            self.assertEqual(path.read_bytes(),before);self.assertEqual(list(path.parent.iterdir()),[path])

    def test_malformed_prior_history_is_never_extended(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'journal';before=b'{"stage":"VERIFIED"}\n{"stage":';path.write_bytes(before)
            with self.assertRaises(ValueError):self.append(path,dict(stage='FINISH_START'))
            self.assertEqual(path.read_bytes(),before)

class DrainPublicationTests(unittest.TestCase):
    def publish(self,path,callback=None):
        self.assertTrue(callable(getattr(runtime,'publish_owned_drain',None)), 'Atomic complete-token publication is missing')
        return runtime.publish_owned_drain(path,'owned',before_publish=callback)

    def test_publication_exposes_only_complete_token(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'drain';seen=[]
            self.assertTrue(callable(getattr(runtime,'rename_exclusive',None)), 'Exclusive atomic rename is missing')
            rename=runtime.rename_exclusive
            def observe(source,target,**kwargs):
                self.assertFalse(path.exists());self.assertEqual(Path(source).read_bytes(),b'owned');seen.append(1)
                self.assertEqual(Path(source).stat().st_nlink,1)
                return rename(source,target)
            with patch.object(runtime,'rename_exclusive',side_effect=observe):fd=self.publish(path)
            try:
                self.assertEqual(path.read_bytes(),b'owned');self.assertEqual(os.fstat(fd).st_ino,path.stat().st_ino)
                self.assertEqual(path.stat().st_nlink,1);self.assertEqual(path.stat().st_mode&0o777,0o600)
            finally:os.close(fd)
            self.assertEqual(seen,[1])

    def test_foreign_token_created_before_publication_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'drain'
            with self.assertRaises(FileExistsError):self.publish(path,lambda:path.write_text('foreign'))
            self.assertEqual(path.read_text(),'foreign');self.assertEqual(list(path.parent.iterdir()),[path])

    def test_expired_publication_leaves_no_public_or_staging_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'drain'
            def expired():raise ValueError('expired')
            with self.assertRaises(ValueError):self.publish(path,expired)
            self.assertEqual(list(path.parent.iterdir()),[])

class FinishCliTests(unittest.TestCase):
    def invoke(self,*args):
        return subprocess.run([sys.executable,'-I','-B',str(Path(runtime.__file__).resolve()),*args],capture_output=True,text=True,timeout=2)

    def test_finish_is_an_explicit_cli_action(self):
        result=self.invoke('--help')
        self.assertEqual(result.returncode,0)
        self.assertIn('--finish',result.stdout)
        self.assertIn('--client-launcher-sha256',result.stdout)
        self.assertIn('--finish-nonce',result.stdout)
        self.assertIn('--recover-finish',result.stdout)
        self.assertIn('--recover-committed',result.stdout)

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

    def test_failure_close_refuses_replaced_launcher(self):
        self.assertIn('expected',inspect.signature(runtime.close_launcher_execution).parameters)
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'launcher';path.write_bytes(b'fixture');path.chmod(0o755);expected=self.state(path)
            other=path.parent/'replacement';other.write_bytes(b'foreign');other.chmod(0o755);other.replace(path)
            with self.assertRaises(ValueError):runtime.close_launcher_execution(path,expected=expected)
            self.assertEqual(path.stat().st_mode&0o777,0o755)

    def test_failure_close_changes_only_selected_launcher(self):
        self.assertIn('expected',inspect.signature(runtime.close_launcher_execution).parameters)
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory).resolve()/'launcher';path.write_bytes(b'fixture');path.chmod(0o755);expected=self.state(path)
            runtime.close_launcher_execution(path,expected=expected)
            self.assertEqual(path.stat().st_mode&0o777,0o644)
            self.assertEqual(path.stat().st_ino,expected['inode'])

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

    def test_checked_recovery_accepts_interrupted_finish_without_rewriting_history(self):
        self.assertIn('recovery',inspect.signature(runtime.verify_installed_history).parameters)
        original,current,journal,pin=self.fixture();journal.append(dict(stage='FINISH_START',client_launcher_sha256=pin));journal.append(dict(stage='HOLD_FINISH_RECOVERY',execution_held=True))
        prior=list(journal)
        self.assertEqual(runtime.verify_installed_history(current,original,journal,pin,2001.0,recovery=True),pin)
        self.assertEqual(journal,prior)

    def test_recovery_refuses_failed_apply_and_unknown_suffix(self):
        self.assertIn('recovery',inspect.signature(runtime.verify_installed_history).parameters)
        for stage in ('HOLD_BEFORE_DRAIN_RELEASE','HOLD_EXPLICIT_RECOVERY','UNKNOWN'):
            original,current,journal,pin=self.fixture();journal.append(dict(stage=stage))
            with self.subTest(stage=stage),self.assertRaises(ValueError):runtime.verify_installed_history(current,original,journal,pin,2001.0,recovery=True)

    def test_recovery_recognizes_exact_opened_launcher_inode(self):
        self.assertIn('recovery',inspect.signature(runtime.verify_installed_history).parameters)
        original,current,journal,pin=self.fixture()
        current['before']={k:dict(v) for k,v in current['before'].items()};current['before'][runtime.LAUNCHER_TARGET]['mode']=0o755
        journal.append(dict(stage='FINISH_READY_TO_RELEASE'))
        self.assertEqual(runtime.verify_installed_history(current,original,journal,pin,2001.0,recovery=True),pin)

    def test_recovered_held_record_can_be_finished_normally(self):
        self.assertIn('recovery',inspect.signature(runtime.verify_installed_history).parameters)
        original,current,journal,pin=self.fixture();journal[-1]['stage']='FINISH_RECOVERED_HELD'
        self.assertEqual(self.verify(original,current,journal,pin),pin)

    def reboot_fixture(self):
        original,current,journal,pin=self.fixture()
        opened={k:dict(v) for k,v in current['before'].items()};opened[runtime.LAUNCHER_TARGET]['mode']=0o755
        journal.append(dict(stage='FINISH_RELEASE_COMMITTED',files=opened,client_launcher_sha256=pin,qualification_ready=False))
        observation={k:v for k,v in current.items() if k not in ('service_pid','service_startticks')}
        observation.update(schema=2,boot='5333c11d-fbbd-42e5-9638-cff800bd7e19',before=opened,
            service_state=dict(LoadState='loaded',ActiveState='inactive',SubState='dead',MainPID='0',ControlPID='0'))
        return original,journal,observation,pin

    def reboot_verify(self,original,journal,observation,pin):
        self.assertTrue(callable(getattr(runtime,'verify_committed_reboot',None)), 'Committed reboot verifier is missing')
        return runtime.verify_committed_reboot(original,journal,observation,pin,2001.0)

    def test_reboot_recovery_requires_durable_committed_release(self):
        original,journal,observation,pin=self.reboot_fixture()
        self.assertEqual(self.reboot_verify(original,journal,observation,pin),pin)
        journal[-1]['stage']='FINISH_READY_TO_RELEASE'
        with self.assertRaises(ValueError):self.reboot_verify(original,journal,observation,pin)

    def test_reboot_recovery_refuses_changed_assets_host_or_client(self):
        for field,value in (('vm_id','999'),('config_sha256','0'*64),('source_main','0'*40)):
            original,journal,observation,pin=self.reboot_fixture();observation[field]=value
            with self.subTest(field=field),self.assertRaises(ValueError):self.reboot_verify(original,journal,observation,pin)
        original,journal,observation,pin=self.reboot_fixture()
        with self.assertRaises(ValueError):self.reboot_verify(original,journal,observation,'0'*64)

    def test_reboot_recovery_refuses_running_or_foreign_service(self):
        original,journal,observation,pin=self.reboot_fixture();observation['service_state']['MainPID']='99'
        with self.assertRaises(ValueError):self.reboot_verify(original,journal,observation,pin)

    def test_failed_poststart_can_be_observed_for_committed_recovery(self):
        original,journal,observation,pin=self.reboot_fixture()
        observation['service_state']=dict(LoadState='loaded',ActiveState='active',SubState='running',MainPID='50',ControlPID='0',StartTicks=60)
        try:result=self.reboot_verify(original,journal,observation,pin)
        except ValueError as error:self.fail('Committed recovery cannot resume after service started: '+str(error))
        self.assertEqual(result,pin)

    def test_committed_checkpoint_can_resume_final_drain_release(self):
        original,journal,observation,pin=self.reboot_fixture()
        observation['service_state']=dict(LoadState='loaded',ActiveState='active',SubState='running',MainPID='50',ControlPID='0',StartTicks=60)
        journal.append(dict(stage='COMMITTED_REBOOT_RECOVERED',files=observation['before'],client_launcher_sha256=pin,qualification_ready=False,boot=observation['boot'],service_pid=50,service_startticks=60))
        try:result=self.reboot_verify(original,journal,observation,pin)
        except ValueError as error:self.fail('Durable checkpoint cannot resume drain release: '+str(error))
        self.assertEqual(result,pin)

    def test_committed_reboot_accepts_held_postcommit_failure(self):
        original,journal,observation,pin=self.reboot_fixture()
        observation['before']={k:dict(v) for k,v in observation['before'].items()};observation['before'][runtime.LAUNCHER_TARGET]['mode']=0o644
        journal.append(dict(stage='HOLD_FINISH_RECOVERY',execution_held=True))
        try:result=self.reboot_verify(original,journal,observation,pin)
        except ValueError as error:self.fail('Held committed release lost recovery authority: '+str(error))
        self.assertEqual(result,pin)

    def test_committed_permission_intents_allow_either_crash_side(self):
        for stage in ('HOLD_FINISH_INTENT','COMMITTED_LAUNCHER_RESTORE_INTENT'):
            for mode in (0o644,0o755):
                original,journal,observation,pin=self.reboot_fixture()
                observation['before']={k:dict(v) for k,v in observation['before'].items()};observation['before'][runtime.LAUNCHER_TARGET]['mode']=mode
                journal.append(dict(stage=stage,launcher=observation['before'][runtime.LAUNCHER_TARGET]))
                with self.subTest(stage=stage,mode=mode):
                    try:result=self.reboot_verify(original,journal,observation,pin)
                    except ValueError as error:self.fail('Intent lost retry authority: '+str(error))
                    self.assertEqual(result,pin)

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

    def test_release_commit_runs_after_final_check_while_drained(self):
        self.assertIn('commit',inspect.signature(runtime.finish_owned_drain).parameters)
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory);events=[]
            def check():events.append('check')
            def action():events.append('action');return 1
            def commit():
                self.assertEqual(drain.read_text(),'owned');events.append('commit')
            self.assertEqual(runtime.finish_owned_drain(lock,drain,'owned',check,action,commit=commit),1)
            self.assertEqual(events,['check','action','check','commit'])
            self.assertFalse(drain.exists())

    def test_failed_final_check_never_commits_release(self):
        self.assertIn('commit',inspect.signature(runtime.finish_owned_drain).parameters)
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory);events=[]
            def check():
                events.append('check')
                if len(events)>1:raise ValueError('late failure')
            with self.assertRaises(ValueError):runtime.finish_owned_drain(lock,drain,'owned',check,lambda:None,commit=lambda:self.fail('committed'))
            self.assertEqual(drain.read_text(),'owned')

    def test_recovery_transaction_retains_owned_drain(self):
        self.assertIn('release',inspect.signature(runtime.finish_owned_drain).parameters)
        with tempfile.TemporaryDirectory() as directory:
            lock,drain=self.fixture(directory)
            self.assertEqual(runtime.finish_owned_drain(lock,drain,'owned',lambda:None,lambda:3,release=False),3)
            self.assertEqual(drain.read_text(),'owned')

if __name__=='__main__':unittest.main()

"""Checked runtime-file updates for an idle dedicated builder; no provisioning."""
import base64
import hashlib
import math
import re
import uuid
import os
from pathlib import Path
import stat
import tempfile

QUALIFICATION = '/opt/oxidex-builder/qualification_trust/'
TARGETS = {
    '/usr/local/bin/oxidex-remote-build': 0o755,
    '/usr/local/bin/docker': 0o755,
    '/opt/gha-runners/host_admission.py': 0o600,
    '/opt/gha-runners/builder_access.py': 0o644,
    '/etc/oxidex-remote-build/qualification.gitconfig': 0o444,
    '/etc/oxidex-remote-build/qualification-maintainer.allowed_signers': 0o444,
    **{QUALIFICATION + name + '.py': 0o644 for name in (
        '__init__', 'git_sandbox', 'host_gate', 'import_supervisor',
        'launcher_contract', 'mounts', 'qualification_source', 'runtime')},
    QUALIFICATION + 'runtime_git.py': 0o555,
}


def validate_assets(assets):
    if not isinstance(assets, dict) or set(assets) != set(TARGETS):
        raise ValueError('Exactly the reviewed fifteen runtime targets are required')
    result = {}
    total = 0
    for target, row in assets.items():
        if not isinstance(row, dict) or set(row) != {'data', 'sha256'}:
            raise ValueError('Unexpected runtime asset fields')
        try:
            raw = base64.b64decode(row['data'], validate=True)
        except (TypeError, ValueError) as error:
            raise ValueError('Invalid runtime asset encoding') from error
        total += len(raw)
        if (not raw or len(raw) > 262144 or total > 1048576
                or hashlib.sha256(raw).hexdigest() != row['sha256']):
            raise ValueError('Runtime asset differs from reviewed bytes')
        result[target] = raw
    return result



def validate_plan(plan, now):
    fields = {'schema', 'nonce', 'source_main', 'vm_id', 'boot', 'image_id',
              'config_sha256', 'service_sha256', 'service_pid', 'service_startticks',
              'observed_at', 'assets', 'before'}
    if not isinstance(plan, dict) or set(plan) != fields or type(plan['schema']) is not int or plan['schema'] != 1:
        raise ValueError('Unknown runtime update plan')
    for field in ('service_pid', 'service_startticks'):
        if type(plan[field]) is not int or plan[field] <= 0:
            raise ValueError('Invalid admission process identity')
    observed = plan['observed_at']
    if (type(observed) not in (int, float) or not math.isfinite(observed)
            or not 0 <= now - observed <= 120):
        raise ValueError('Expired or invalid runtime observation')
    for key, pattern in (('nonce', r'[a-f0-9]{32}'), ('source_main', r'[a-f0-9]{40}'),
                         ('vm_id', r'[0-9]{1,20}'), ('image_id', r'sha256:[a-f0-9]{64}'),
                         ('config_sha256', r'[a-f0-9]{64}'), ('service_sha256', r'[a-f0-9]{64}')):
        if not isinstance(plan[key], str) or not re.fullmatch(pattern, plan[key]):
            raise ValueError('Unbound runtime identity: ' + key)
    try:
        if str(uuid.UUID(plan['boot'])) != plan['boot']:
            raise ValueError('Noncanonical boot identity')
    except (TypeError, AttributeError, ValueError) as error:
        raise ValueError('Invalid boot identity') from error
    if not isinstance(plan['before'], dict) or set(plan['before']) != set(TARGETS):
        raise ValueError('Complete current file observations required')
    return validate_assets(plan['assets'])


def atomic_write(path, data, mode):
    path = Path(path)
    if any(p.is_symlink() for p in (path, *path.parents)):
        raise ValueError('Symlinked runtime path')
    if path.exists() and not path.is_file():
        raise ValueError('Runtime target is not a regular file')
    descriptor, temporary = tempfile.mkstemp(prefix='.runtime-', dir=path.parent)
    try:
        with os.fdopen(descriptor, 'wb') as stream:
            os.fchmod(stream.fileno(), mode)
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        parent = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            os.fsync(parent)
        finally:
            os.close(parent)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def root_file_state(path):
    """Inspect root custody before trusting a live file or its absence."""
    path = Path(path)
    for parent in path.parents:
        if parent.exists():
            info = parent.lstat()
            if (not stat.S_ISDIR(info.st_mode) or info.st_uid != 0
                    or info.st_mode & 0o022):
                raise ValueError('Untrusted runtime ancestor: ' + str(parent))
        elif parent.is_symlink():
            raise ValueError('Symlinked runtime ancestor')
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != 0
                or info.st_nlink != 1 or info.st_mode & 0o022 or info.st_size > 1048576):
            raise ValueError('Untrusted runtime file')
        raw = os.read(fd, 1048577)
        if len(raw) != info.st_size:
            raise ValueError('Runtime file changed while reading')
        return {'sha256': hashlib.sha256(raw).hexdigest(),
                'mode': stat.S_IMODE(info.st_mode), 'uid': info.st_uid,
                'gid': info.st_gid, 'device': info.st_dev, 'inode': info.st_ino}
    finally:
        os.close(fd)


def drained_update(lock_path, drain_path, token, action):
    """Run under the existing lock; failures retain the protective owned drain."""
    import fcntl
    if not isinstance(token, str) or not re.fullmatch('[a-zA-Z0-9_-]{1,128}', token):
        raise ValueError('Invalid update token')
    lock_path, drain_path = Path(lock_path), Path(drain_path)
    lock = os.open(lock_path, os.O_RDWR | os.O_NOFOLLOW)
    drain = None
    try:
        original = os.fstat(lock)
        if (not stat.S_ISREG(original.st_mode) or original.st_nlink != 1
                or original.st_uid != os.geteuid()):
            raise ValueError('Untrusted build lock')
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        current = lock_path.lstat()
        if (current.st_dev, current.st_ino) != (original.st_dev, original.st_ino):
            raise ValueError('Build lock replaced')
        drain = os.open(drain_path, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        raw = token.encode('ascii')
        with os.fdopen(os.dup(drain), 'wb') as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        directory = os.open(drain_path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            os.fsync(directory)
            result = action()
            owned, current = os.fstat(drain), drain_path.lstat()
            if (current.st_dev, current.st_ino) != (owned.st_dev, owned.st_ino) or current.st_nlink != 1:
                raise ValueError('Owned drain replaced; explicit recovery required')
            os.lseek(drain, 0, os.SEEK_SET)
            if os.read(drain, 129) != raw:
                raise ValueError('Owned drain changed; explicit recovery required')
            os.unlink(drain_path)
            os.fsync(directory)
            return result
        finally:
            os.close(directory)
    finally:
        if drain is not None:
            os.close(drain)
        os.close(lock)


def check_snapshot(plan, snapshot, *, owned_drain=False):
    for field in ('vm_id', 'boot', 'image_id', 'config_sha256', 'service_sha256',
                  'service_pid', 'service_startticks'):
        if snapshot.get(field) != plan[field]:
            raise ValueError('Live builder binding changed: ' + field)
    if (snapshot.get('cpu') != 16 or snapshot.get('role') != 'builder'
            or snapshot.get('cpus') != 16 or snapshot.get('memory_gib') != 28
            or snapshot.get('tools_ready') is not True):
        raise ValueError('Builder role or host tools differ')
    if (snapshot.get('leases') != {} or snapshot.get('containers') != []
            or snapshot.get('upload_processes') != []):
        raise ValueError('Builder is not idle')
    if snapshot.get('before') != plan['before']:
        raise ValueError('Current runtime files differ from reviewed observations')
    expected_drain = plan['nonce'] if owned_drain else None
    if snapshot.get('drain_token') != expected_drain:
        raise ValueError('Foreign or unexpected builder drain')


def checked_command(arguments):
    import subprocess
    result = subprocess.run(arguments, capture_output=True, text=True,
                            check=True, timeout=20)
    if result.stderr:
        raise ValueError('Unexpected command diagnostics')
    return result.stdout.strip()


def live_snapshot(plan):
    """Read fixed host facts, never acquire leases or start containers."""
    import json
    config_path = Path('/etc/oxidex-remote-build.json')
    config_state = root_file_state(config_path)
    config = json.loads(config_path.read_bytes())
    ledger = Path('/var/lib/gha-admission/leases.json')
    root_file_state(ledger)
    service = root_file_state('/etc/systemd/system/gha-admission.service')
    drain = Path('/run/oxidex-build-draining')
    root_file_state(drain)
    service_pid = int(checked_command(['/usr/bin/systemctl', 'show', 'gha-admission.service',
                                       '--property=MainPID', '--value']))
    if service_pid <= 0:
        raise ValueError('Admission service is not running')
    proc = Path('/proc') / str(service_pid)
    if proc.stat().st_uid != 0 or (proc / 'cmdline').read_bytes().split(b'\0')[:-1] != [
            b'/usr/bin/python3', b'/opt/gha-runners/host_admission.py', b'serve']:
        raise ValueError('Unexpected admission process')
    service_ticks = int((proc / 'stat').read_text().rsplit(')', 1)[1].split()[19])
    executables = ('/usr/bin/git', '/usr/bin/ssh-keygen', '/usr/bin/python3',
                   '/usr/bin/docker', '/usr/bin/systemctl')
    tools_ready = all(stat.S_ISREG(Path(p).stat().st_mode) and Path(p).stat().st_uid == 0
                      and not Path(p).stat().st_mode & 0o022
                      and os.access(p, os.X_OK) for p in executables)
    uploads = []
    for proc_path in Path('/proc').iterdir():
        if not proc_path.name.isdecimal():
            continue
        try:
            comm = (proc_path / 'comm').read_text().strip()
            argv = (proc_path / 'cmdline').read_bytes()
        except (FileNotFoundError, ProcessLookupError):
            continue
        if (comm in ('scp', 'rsync', 'sftp-server', 'cargo', 'rustc')
                or b'@internal-sftp' in argv):
            uploads.append(int(proc_path.name))
    return dict(
        upload_processes=uploads,
        vm_id=checked_command(['/usr/bin/curl', '-q', '--fail', '--silent', '--show-error',
            '--max-time', '2', '-H', 'Metadata-Flavor:Google',
            'http://metadata.google.internal/computeMetadata/v1/instance/id']),
        boot=Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
        image_id=checked_command(['/usr/bin/docker', 'image', 'inspect',
                                 plan['image_id'], '--format={{.Id}}']),
        config_sha256=config_state['sha256'], service_sha256=service['sha256'],
        service_pid=service_pid, service_startticks=service_ticks,
        cpu=os.cpu_count(), role=config.get('role'), cpus=config.get('cpus'),
        memory_gib=config.get('memory_gib'), tools_ready=tools_ready,
        leases=json.loads(ledger.read_bytes()),
        containers=checked_command(['/usr/bin/docker', 'ps', '-aq', '--no-trunc']).split(),
        before={target: root_file_state(target) for target in TARGETS},
        drain_token=drain.read_text() if drain.exists() else None)


def trusted_directory(path, *, create=False):
    path = Path(path)
    for current in reversed((path, *path.parents)):
        if not current.exists() and not current.is_symlink() and create:
            current.mkdir(mode=0o755)
        info = current.lstat()
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != 0
                or info.st_mode & 0o022):
            raise ValueError('Untrusted update directory: ' + str(current))


def apply_update(plan):
    """Replace only approved runtime files after actual idle-state confirmation."""
    import json
    import time
    assets = validate_plan(plan, time.time())
    check_snapshot(plan, live_snapshot(plan))
    backup = Path('/var/lib/oxidex-builder') / ('runtime-update-' + plan['nonce'])
    trusted_directory(backup.parent)
    if backup.exists() or backup.is_symlink():
        raise ValueError('Update nonce already used')

    def install():
        validate_plan(plan, time.time())
        check_snapshot(plan, live_snapshot(plan), owned_drain=True)
        backup.mkdir(mode=0o700)
        atomic_write(backup / 'plan.json', (json.dumps(plan, sort_keys=True) + '\n').encode(), 0o600)
        journal = backup / 'journal.jsonl'

        def record(stage, **facts):
            with journal.open('ab') as output:
                os.chmod(journal, 0o600)
                output.write((json.dumps(dict(stage=stage, observed_at=time.time(), **facts),
                                         sort_keys=True) + '\n').encode())
                output.flush()
                os.fsync(output.fileno())

        record('BACKUP_START')
        try:
            for index, target in enumerate(sorted(TARGETS)):
                if root_file_state(target) != plan['before'][target]:
                    raise ValueError('Runtime changed before backup')
                if plan['before'][target] is not None:
                    atomic_write(backup / (str(index) + '.before'), Path(target).read_bytes(), 0o600)
            record('BACKUPS_COMPLETE', before=plan['before'])
            for target in sorted(TARGETS):
                validate_plan(plan, time.time())
                if (Path('/proc/sys/kernel/random/boot_id').read_text().strip() != plan['boot']
                        or root_file_state('/etc/oxidex-remote-build.json')['sha256'] != plan['config_sha256']
                        or root_file_state(target) != plan['before'][target]):
                    raise ValueError('Immediate file mutation fence failed')
                trusted_directory(Path(target).parent, create=True)
                atomic_write(target, assets[target], TARGETS[target])
                record('FILE_REPLACED', target=target,
                       sha256=hashlib.sha256(assets[target]).hexdigest())
            trusted_directory('/mnt/runner-data/remote-build/qualification-sealed', create=True)
            validate_plan(plan, time.time())
            checked_command(['/usr/bin/systemctl', 'restart', 'gha-admission.service'])
            after = live_snapshot(plan)
            expected = dict(plan, service_pid=after['service_pid'],
                            service_startticks=after['service_startticks'], before=after['before'])
            check_snapshot(expected, after, owned_drain=True)
            if (after['service_pid'] == plan['service_pid']
                    or Path('/proc/' + str(plan['service_pid'])).exists()):
                raise ValueError('Original admission process is still present')
            for target, data in assets.items():
                state = after['before'][target]
                if state['sha256'] != hashlib.sha256(data).hexdigest() or state['mode'] != TARGETS[target]:
                    raise ValueError('Installed runtime proof differs')
            record('FILES_AND_SERVICE_VERIFIED', service_pid=after['service_pid'],
                   service_startticks=after['service_startticks'], files=after['before'])
            return dict(status='UPDATED', source_main=plan['source_main'], backup=str(backup),
                        service_pid=after['service_pid'], qualification_ready=False)
        except BaseException as error:
            record('HOLD_EXPLICIT_RECOVERY', error_type=type(error).__name__)
            raise

    return drained_update('/run/oxidex-build.lock', '/run/oxidex-build-draining', plan['nonce'], install)


def plan_from_snapshot(source_main, assets, snapshot, observed_at, nonce):
    plan = dict(schema=1, nonce=nonce, source_main=source_main,
                observed_at=observed_at, assets=assets,
                **{field: snapshot[field] for field in (
                    'vm_id', 'boot', 'image_id', 'config_sha256', 'service_sha256',
                    'service_pid', 'service_startticks', 'before')})
    validate_plan(plan, observed_at)
    check_snapshot(plan, snapshot)
    return plan


def main():
    import argparse
    import json
    import sys
    import time
    parser = argparse.ArgumentParser(description=__doc__)
    inputs = parser.add_mutually_exclusive_group(required=True)
    inputs.add_argument('--plan', type=Path)
    inputs.add_argument('--capture-assets', type=Path)
    parser.add_argument('--plan-sha256')
    parser.add_argument('--assets-sha256')
    parser.add_argument('--source-main')
    parser.add_argument('--apply', action='store_true', help='Apply the reviewed plan; default checks only')
    args = parser.parse_args()
    if (sys.platform != 'linux' or os.geteuid() != 0
            or not sys.flags.isolated or not sys.dont_write_bytecode):
        raise ValueError('Run through the existing authenticated root control path on the builder')
    if args.capture_assets:
        if args.apply or not args.source_main or not args.assets_sha256:
            parser.error('Capture requires source-main/assets-sha256 and cannot apply')
        state = root_file_state(args.capture_assets)
        if state is None or state['sha256'] != args.assets_sha256:
            raise ValueError('Candidate assets differ from selected digest')
        assets = json.loads(args.capture_assets.read_bytes())
        validate_assets(assets)
        config = Path('/etc/oxidex-remote-build.json')
        root_file_state(config)
        image = json.loads(config.read_bytes())['approved_image_id']
        observed_at = time.time()
        snapshot = live_snapshot({'image_id': image})
        plan = plan_from_snapshot(args.source_main, assets, snapshot, observed_at, uuid.uuid4().hex)
        validate_plan(plan, time.time())
        print(json.dumps(plan, sort_keys=True))
        return
    if not args.plan_sha256:
        parser.error('Check/apply requires plan-sha256')
    state = root_file_state(args.plan)
    if state is None or state['sha256'] != args.plan_sha256:
        raise ValueError('Plan differs from the explicitly selected reviewed digest')
    plan = json.loads(args.plan.read_bytes())
    validate_plan(plan, time.time())
    check_snapshot(plan, live_snapshot(plan))
    result = apply_update(plan) if args.apply else dict(status='CHECKED_ONLY', executed=False)
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()

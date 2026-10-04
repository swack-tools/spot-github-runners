"""Snapshot a local checkout, transfer it over authenticated SSH, and time builds."""
import argparse
import hashlib
import json
import secrets
import shlex
import subprocess
import tarfile
import time
from pathlib import Path


def retryable_exit(code):
    return code in (75, 137, 143, 255)


def transaction_script(project,upload,digest,profile,run):
    """Serialize source replacement, builds, and immutable artifact export."""
    root='/mnt/runner-data/remote-build'
    source=f'{root}/sources/{project}'
    target=f'{root}/targets/{project}'
    export=f'{root}/exports/{run}/oxidex'
    launcher=f'/usr/local/bin/oxidex-remote-build {shlex.quote(project)}'
    stages=[f'{launcher} cargo fetch --locked --target x86_64-unknown-linux-gnu']
    if profile=='release':stages.append(f'{launcher} just cbindgen-check')
    stages.append(f'{launcher} cargo build '+('--release ' if profile=='release' else '')+'--locked --bin oxidex')
    body='\n'.join([
        'set -eu',
        'test ! -e /run/oxidex-build-draining || exit 75',
        f'flock -n -E 75 /run/oxidex-cache-active-{project}.lock true',
        'ids=$(/usr/bin/docker ps -aq --filter label=oxidex.remote-build=true) || exit 75',
        'for id in $ids; do',
        "  mounts=$(/usr/bin/docker inspect --format '{{range .Mounts}}{{if eq .Destination \"/target\"}}{{.Source}}{{end}}{{end}}' \"$id\") || exit 75",
        f'  test "$mounts" != {shlex.quote(target)} || exit 75',
        'done',
        f'{launcher} prepare',
        f'echo {shlex.quote(digest+"  "+upload)} | sha256sum -c -',
        # The validated source is replaceable; Cargo and target caches are separate.
        f'find {shlex.quote(source)} -mindepth 1 -maxdepth 1 -exec rm -rf -- {{}} +',
        f'tar -xzf {shlex.quote(upload)} -C {shlex.quote(source)}',
        *stages,
        f'{shlex.quote(target+"/"+profile+"/oxidex")} --version',
        f'install -d -m 0755 {shlex.quote(str(Path(export).parent))}',
        f'install -m 0755 {shlex.quote(target+"/"+profile+"/oxidex")} {shlex.quote(export)}',
        f'sha256sum {shlex.quote(export)}',
    ])
    return ('sudo flock --shared /run/oxidex-build.lock flock --exclusive '
            f'/run/oxidex-project-{project}.lock sh -c '+shlex.quote(body)),export


def make_snapshot(source: Path, archive: Path) -> dict:
    names = subprocess.check_output(['git','-C',str(source),'ls-files','-z']).decode().split('\0')
    files=[]
    with tarfile.open(archive,'w:gz',compresslevel=3) as tar:
        for name in names:
            parts=Path(name).parts
            if not name or any(p in {'.git','.codex','.claude','.agents','target','node_modules','.venv'} for p in parts):
                continue
            path=source/name
            if path.name.startswith('.env') or path.suffix in {'.pem','.key'} or not path.is_file() or path.is_symlink():
                continue
            files.append({'path':name,'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'bytes':path.stat().st_size})
            tar.add(path,arcname=name,recursive=False)
    return {'files':files,'file_count':len(files),'archive_bytes':archive.stat().st_size,
            'archive_sha256':hashlib.sha256(archive.read_bytes()).hexdigest()}


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source',type=Path,required=True)
    parser.add_argument('--instance',required=True)
    parser.add_argument('--zone',required=True)
    parser.add_argument('--project',default='homelab-424523')
    parser.add_argument('--worktree-id',required=True)
    parser.add_argument('--evidence-dir',type=Path,required=True)
    parser.add_argument('--profile',choices=['debug','release'],default='release')
    parser.add_argument('--artifact-dir',type=Path)
    args=parser.parse_args(argv)
    import re
    if not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}',args.worktree_id):
        raise ValueError('Invalid worktree identifier')
    source=args.source.expanduser().resolve();evidence=args.evidence_dir.expanduser().resolve()
    evidence.mkdir(parents=True,exist_ok=True)
    receipt={'profile':args.profile,'source':str(source),'instance':args.instance,'zone':args.zone,'project':args.project}
    def save():
        (evidence/'remote-build.json').write_text(json.dumps(receipt,indent=2))
    def ssh(command):
        return ['gcloud','compute','ssh',args.instance,'--zone='+args.zone,'--project='+args.project,
                '--quiet','--command='+command]
    try:
        start=time.monotonic();archive=evidence/'remote-source.tar.gz'
        receipt['snapshot']=make_snapshot(source,archive)
        receipt['packaging_seconds']=time.monotonic()-start
        receipt['source_commit']=subprocess.check_output(['git','-C',str(source),'rev-parse','HEAD'],text=True).strip()
        receipt['source_status']=subprocess.check_output(['git','-C',str(source),'status','--porcelain'],text=True)
        save()
        start=time.monotonic()
        run='run-'+secrets.token_hex(16)
        upload='oxidex-remote-source-'+run+'.tar.gz'
        subprocess.run(['gcloud','compute','scp',str(archive),args.instance+':~/'+upload,
                        '--zone='+args.zone,'--project='+args.project,'--quiet'],check=True)
        digest=receipt['snapshot']['archive_sha256']
        receipt['upload_seconds']=time.monotonic()-start;receipt['run_id']=run;save()
        command,binary=transaction_script(args.worktree_id,upload,digest,args.profile,run)
        start=time.monotonic();log_path=evidence/'transaction.log'
        with log_path.open('w') as log:
            result=subprocess.run(ssh(command),stdout=log,stderr=subprocess.STDOUT)
        receipt['transaction_seconds']=time.monotonic()-start
        receipt['transaction_exit_code']=result.returncode
        receipt['retryable']=retryable_exit(result.returncode);save()
        if result.returncode:
            raise RuntimeError(f'Remote transaction failed; see {log_path}')
        with log_path.open() as log:
            last_line=''
            for line in log:last_line=line.strip()
        digest=last_line.split()[0] if last_line else ''
        if not re.fullmatch('[0-9a-f]{64}',digest):
            raise RuntimeError('Remote artifact checksum was missing')
        receipt['binary_sha256']=digest
        if args.artifact_dir:
            output=args.artifact_dir.expanduser().resolve();output.mkdir(parents=True,exist_ok=True)
            artifact=output/'oxidex'
            subprocess.run(['gcloud','compute','scp',args.instance+':'+binary,str(artifact),
                            '--zone='+args.zone,'--project='+args.project,'--quiet','--scp-flag=-C'],check=True)
            if hashlib.sha256(artifact.read_bytes()).hexdigest()!=digest:
                raise RuntimeError('Downloaded binary checksum mismatch')
            receipt['artifact']=str(artifact)

        receipt['verified']=True;save()
    except Exception as exc:
        receipt['error']=str(exc)
        if isinstance(exc,subprocess.CalledProcessError):
            receipt['retryable']=retryable_exit(exc.returncode)
        save();raise
    print(json.dumps({k:v for k,v in receipt.items() if k!='snapshot'},indent=2))
    return 0

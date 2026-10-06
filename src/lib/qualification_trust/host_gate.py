"""Root-owned qualification preflight. Offline candidate, not yet installed."""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
from . import qualification_source as verifier

MODULE_SHA='b895fd0614537eda9fd8cb922964b779baca50e5eeccfe80f7177f381ba31933'
ANCHOR=Path('/etc/oxidex-remote-build/qualification-maintainer.allowed_signers')

def digest(data):
    return hashlib.sha256(data).hexdigest()

def regular_bytes(path, limit=256*1024*1024):
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW)
    try:
        before=os.fstat(fd)
        if not stat.S_ISREG(before.st_mode) or before.st_size>limit:
            raise ValueError('Unsafe or oversized source file')
        chunks=[]
        while block:=os.read(fd,1024*1024):chunks.append(block)
        after=os.fstat(fd)
        if (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns,before.st_ctime_ns)!=(after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns,after.st_ctime_ns):
            raise ValueError('Source file changed during copy')
        return b''.join(chunks)
    finally:os.close(fd)

def validate_identity(head,bundle_sha,run_id):
    if not re.fullmatch('[0-9a-f]{40}',head) or not re.fullmatch('[0-9a-f]{64}',bundle_sha):
        raise ValueError('Full source and bundle identities required')
    if not re.fullmatch('qualification-'+head[:12]+'-[0-9a-f]{32}',run_id):
        raise ValueError('Run identity mismatch')

def copy_sealed(source,destination):
    """Descriptor-relative bounded snapshot; reject links, special files and mounts."""
    source=Path(source)
    if source.resolve()!=source:raise ValueError('Symlinked upload ancestor')
    top=os.open(source,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW)
    count=[0];total=[0];device=os.fstat(top).st_dev
    def copy(directory,target,depth):
        if depth>64:raise ValueError('Source depth bound')
        target.mkdir(mode=0o755)
        for name in os.listdir(directory):
            count[0]+=1
            if count[0]>100000:raise ValueError('Source entry bound')
            info=os.stat(name,dir_fd=directory,follow_symlinks=False)
            if info.st_dev!=device:raise ValueError('Foreign upload filesystem')
            if stat.S_ISDIR(info.st_mode):
                child=os.open(name,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,dir_fd=directory)
                try:copy(child,target/name,depth+1)
                finally:os.close(child)
            elif stat.S_ISREG(info.st_mode):
                fd=os.open(name,os.O_RDONLY|os.O_NOFOLLOW,dir_fd=directory)
                try:
                    before=os.fstat(fd)
                    if not stat.S_ISREG(before.st_mode) or before.st_size>256*1024*1024:raise ValueError('Unsafe file')
                    out=os.open(target/name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o444)
                    with os.fdopen(out,'wb') as stream:
                        while block:=os.read(fd,1024*1024):
                            total[0]+=len(block)
                            if total[0]>2*1024*1024*1024:raise ValueError('Source byte bound')
                            stream.write(block)
                        stream.flush();os.fsync(stream.fileno())
                    if os.fstat(fd)!=before:raise ValueError('Upload changed during copy')
                finally:os.close(fd)
            else:raise ValueError('Special or symlinked upload member')
    try:copy(top,destination,0)
    finally:os.close(top)

def verify_sealed(sealed,head,bundle_sha,run_id):
    """Caller must hold upload/admission locks and own this sealed tree."""
    validate_identity(head,bundle_sha,run_id)
    if digest(Path(verifier.__file__).read_bytes())!=MODULE_SHA:raise ValueError('Host verifier digest differs')
    info=ANCHOR.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid!=0 or info.st_mode&0o222:raise ValueError('Anchor must be root-owned readonly')
    bundle=sealed/'repository.bundle'
    if digest(regular_bytes(bundle))!=bundle_sha:raise ValueError('Bundle digest differs')
    repo=sealed/'verified.git'
    env={'PATH':'/usr/bin:/bin','HOME':'/nonexistent','GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null','GIT_NO_REPLACE_OBJECTS':'1'}
    def git(*args):
        return subprocess.check_output(['/usr/bin/git',*args],env=env,timeout=60)
    git('init','--bare',str(repo))
    git('-C',str(repo),'fetch','--no-tags',str(bundle),head)
    facts=verifier.verify_source(repo,head,ANCHOR,sealed/'source-attestation',bundle_sha256=bundle_sha,run_id=run_id)
    for uploaded,tracked in [('qualification_bootstrap.py','tools/remote-build/qualification_bootstrap.py'),('qualification_source.py','tools/remote-build/qualification_source.py'),('rust-toolchain.toml','rust-toolchain.toml')]:
        if regular_bytes(sealed/uploaded)!=git('-C',str(repo),'show',head+':'+tracked):
            raise ValueError('Uploaded executable/pin differs from authenticated tree: '+uploaded)
    # Root-owned authenticated checkout is separate from writable uploaded targets.
    checkout=sealed/'authenticated-checkout'
    git('clone','--no-hardlinks','--no-checkout',str(repo),str(checkout))
    git('-C',str(checkout),'-c','core.hooksPath=/dev/null','checkout','--detach',head)
    receipt={'schema':1,'head':head,'bundle_sha256':bundle_sha,'run_id':run_id,'source_identity':facts}
    fd=os.open(sealed/'host-source-binding.json',os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o444)
    with os.fdopen(fd,'w') as stream:json.dump(receipt,stream,sort_keys=True);stream.flush();os.fsync(stream.fileno())
    return receipt


def execution_arguments(arguments,binding):
    """Attested namespaces never expose arbitrary Python/Cargo entrypoints."""
    head=binding['head'];run=binding['run_id'];output='/target/ops/evidence/remote-qualification/'+run
    if arguments[:2]==['python3','/src/qualification_bootstrap.py']:
        expected=['python3','/src/qualification_bootstrap.py','--head',head,'--output',output,
                  '--bundle-sha256',binding['bundle_sha256'],'--signers-sha256',binding['uploaded_signers_sha256']]
        if arguments==expected:return arguments
    base=['python3','/target/checkout/tools/remote-build/qualification.py']
    tails=[['--remote-run','--output',output,'--expected-head',head],
           ['--pack-results','--output',output,'--expected-head',head,'--result-file','/target/pack-result.json'],
           ['--verify-results-tar','/target/results.tar.gz','--output',output,'--expected-head',head,'--result-file','/target/replay-result.json']]
    if arguments in [base+tail for tail in tails]:return arguments
    raise ValueError('Attested execution is outside exact verified command contract')

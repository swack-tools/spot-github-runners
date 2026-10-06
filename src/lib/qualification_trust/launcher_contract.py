"""Fixed qualification namespace dispatch for the root-owned builder launcher."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
from .host_gate import copy_sealed,verify_sealed,execution_arguments,regular_bytes

TRUST=Path('/var/lib/oxidex-builder/qualification-sealed')

def dispatch(root,project,arguments,container_command,config,execute):
    if not re.fullmatch('qualification-[0-9a-f]{12}-[0-9a-f]{32}',project):
        raise ValueError('Qualification run namespace required')
    if TRUST.is_symlink() or TRUST.stat().st_uid!=0 or TRUST.stat().st_mode&0o022:
        raise ValueError('Untrusted sealed namespace ancestor')
    # This exact run lock covers snapshot creation and all its executions.
    fd=os.open('/run/oxidex-qualification-'+project+'.lock',os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW,0o600)
    try:
        fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
        sealed=TRUST/project
        if arguments[:1]==['verify-source']:
            if len(arguments)!=7 or arguments[1::2]!=['--head','--bundle-sha256','--run-id'] or arguments[6]!=project:
                raise ValueError('Exact verify-source arguments required')
            copy_sealed(root/'sources'/project,sealed)
            binding=verify_sealed(sealed,arguments[2],arguments[4],arguments[6])
            # Bind uploaded signers independently as input, not host authority.
            binding['uploaded_signers_sha256']=hashlib.sha256(regular_bytes(sealed/'maintainer.allowed_signers')).hexdigest()
            path=sealed/'execution-binding.json'
            out=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o444)
            with os.fdopen(out,'w') as stream:json.dump(binding,stream,sort_keys=True)
            return {'verified':True,'binding':binding}
        binding=json.loads(regular_bytes(sealed/'execution-binding.json',65536))
        execution_arguments(arguments,binding)
        safe_arguments=list(arguments)
        if safe_arguments[1].startswith('/target/checkout/'):
            safe_arguments[1]=safe_arguments[1].replace('/target/checkout/','/qualification-checkout/',1)
        invocation=container_command(project,safe_arguments,config['cpus'],config['memory_gib'],config['image'],root=root)
        old=f'{root}/sources/{project}:/src'
        invocation[invocation.index(old)]=f'{sealed}:/src:ro'
        # Additional immutable host authority mount, never an uploaded key.
        image_index=invocation.index(config['image'])
        invocation[image_index:image_index]=['--mount',f'type=bind,src={sealed}/authenticated-checkout,dst=/qualification-checkout,readonly','--mount','type=bind,src=/etc/oxidex-remote-build/qualification-maintainer.allowed_signers,dst=/etc/oxidex-remote-build/qualification-maintainer.allowed_signers,readonly']
        return execute(invocation)
    finally:os.close(fd)

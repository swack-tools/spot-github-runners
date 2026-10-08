"""Complete signed-source unittest measurement for the neutral CI profile."""
import hashlib
import os
from pathlib import Path
import stat
import sys
import types

PRODUCER_SHA = '14f70ae8939ef533f86cddc6ce759972dea73f700e453421c635e2dcf0e0a6ff'

def load_producer():
    path = Path(__file__).with_name('infra_python_test_proof.py')
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, 'rb') as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise ValueError('Producer must be a regular file')
        data = source.read(262145)
    if len(data) > 262144 or hashlib.sha256(data).hexdigest() != PRODUCER_SHA:
        raise ValueError('Reviewed producer bytes changed')
    module = types.ModuleType('neutral_infra_producer')
    module.__file__ = str(path)
    exec(compile(data, str(path), 'exec'), module.__dict__)
    return module

def measure(module):
    _, _, names = module._source_snapshot()
    # Each exact signed manifest module must contribute a case. The producer
    # also requires every discovered unique case to finish without skips.
    module.MIN_CASES = len(names)
    module.MIN_MODULES = len(names)
    module.RUNNER_ARGV = ['python3', '-B', 'tools/infra_python_complete_proof.py']
    return module.run()

if __name__ == '__main__':
    try:
        sys.exit(measure(load_producer()))
    except Exception as error:
        print(f'Complete infra test proof refused: {error}', file=sys.stderr)
        sys.exit(2)

#!/usr/bin/env python3
"""Choose a lightly used worker, then delegate the build over SSH."""
import argparse
import os
import subprocess
import sys
import tomllib
from datetime import datetime, timezone
from pathlib import Path
from lib.worker_selection import select_worker


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', choices=['debug','release'],required=True)
    args=parser.parse_args()
    source=Path(subprocess.check_output(['git','rev-parse','--show-toplevel'],text=True).strip())
    settings=tomllib.loads((source/'.cargo/config.toml').read_text()).get('env',{})
    project=os.environ.get('OXIDEX_REMOTE_PROJECT') or settings.get('OXIDEX_REMOTE_PROJECT')
    if not project:
        project=subprocess.check_output(['gcloud','config','get-value','project'],text=True).strip()
    if not project or project=='(unset)':
        raise RuntimeError('Set OXIDEX_REMOTE_PROJECT or configure a gcloud project')
    vm,sample=select_worker(project)
    env=dict(os.environ,OXIDEX_REMOTE_INSTANCE=vm.name,OXIDEX_REMOTE_ZONE=vm.zone,
             OXIDEX_REMOTE_PROJECT=project)
    stamp=datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')
    evidence=Path.home()/'oxidex-ops'/'evidence'/('auto-remote-'+stamp)
    evidence.mkdir(parents=True)
    import json
    (evidence/'selection.json').write_text(json.dumps({'instance':vm.name,'instance_id':vm.instance_id,
        'zone':vm.zone,'cpu':sample[0],'memory':sample[1]},indent=2))
    return subprocess.call(['cargo','remote-build','--profile',args.profile,
        '--evidence-dir',str(evidence),'--artifact-dir',str(source/'target'/'remote-linux'/args.profile)],env=env)


if __name__ == '__main__':
    raise SystemExit(main())

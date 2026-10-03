#!/usr/bin/env python3
"""Cargo external command adapter for a configured OxiDex checkout."""
import os
import subprocess
import sys
import tomllib
from datetime import datetime, timezone
from pathlib import Path
from lib.remote_build import main

source=Path(subprocess.check_output(['git','rev-parse','--show-toplevel'],text=True).strip())
settings=tomllib.loads((source/'.cargo/config.toml').read_text())['env']
def setting(name):
    return os.environ.get(name, settings[name])
stamp=datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')
args=['--source',str(source),'--instance',setting('OXIDEX_REMOTE_INSTANCE'),
      '--zone',setting('OXIDEX_REMOTE_ZONE'),'--project',setting('OXIDEX_REMOTE_PROJECT'),
      '--worktree-id',setting('OXIDEX_REMOTE_WORKTREE'),
      '--evidence-dir',str(Path.home()/'oxidex-ops'/'evidence'/('cargo-remote-'+stamp))]
raise SystemExit(main(args+sys.argv[1:]))

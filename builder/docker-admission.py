#!/usr/bin/python3
"""Root Docker adapter for the existing pinned remote-launcher protocol.

Other Docker commands exec the vendor binary unchanged. Only the dedicated
remote build's fixed label acquires a host memory reservation before starting.
Installed only on tier hosts; runner containers never receive a Docker socket.
"""
import os
import subprocess
import sys


def run_build(arguments,admission):
    try:
        admission.acquire_build()
    except (OSError,RuntimeError,SystemExit):
        return 75
    try:
        return subprocess.run(['/usr/bin/docker',*arguments]).returncode
    finally:
        admission.release_build()


def main():
    arguments=sys.argv[1:]
    if arguments and arguments[0]=='run' and '--label=oxidex.remote-build=true' in arguments:
        if os.geteuid()!=0:
            raise SystemExit('Remote builder admission requires root')
        sys.path.insert(0,'/opt/gha-runners')
        import host_admission
        raise SystemExit(run_build(arguments,host_admission))
    os.execv('/usr/bin/docker',['docker',*arguments])


if __name__=='__main__':
    main()

#!/usr/bin/env bash
set -Eeuo pipefail
if [[ "${GHA_ADMISSION_REQUIRED:-0}" == 1 ]]; then
  python3 /usr/local/bin/gha-host-admission.py release
fi
exec python3 /usr/local/bin/gha-runner-clean-target.py

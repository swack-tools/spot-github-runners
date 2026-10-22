#!/usr/bin/env bash
set -Eeuo pipefail
if [[ "${GHA_ADMISSION_REQUIRED:-0}" == 1 ]]; then
  host-agent host-admission release
fi
exec host-agent clean-target /work

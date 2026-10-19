#!/usr/bin/env python3
"""Create or update the runner autoscaler Cloud Run job and its scheduler; previews unless --apply."""

from lib.autoscaler_deploy import main

if __name__ == "__main__":
    raise SystemExit(main())

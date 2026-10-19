#!/usr/bin/env python3
"""Cloud Run job entrypoint of the runner autoscaler: one tiered --once check per execution."""

from lib.autoscaler_job import main

if __name__ == "__main__":
    raise SystemExit(main())

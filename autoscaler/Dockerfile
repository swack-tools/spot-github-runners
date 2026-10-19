# Runner autoscaler image: the Cloud Run job whose every execution runs one tiered
# `spot_runner_autoscaler.py --once` check (src/spot_runner_autoscaler_job.py).
#
# Built and pushed to Artifact Registry by .github/workflows/control-watchdog-image.yml (job "autoscaler"), next to
# the watchdog image. Build from the repository root:
#   docker build -f autoscaler/Dockerfile --build-arg BASE_IMAGE=<google-cloud-cli slim image pinned by digest> \
#     --build-arg GH_VERSION=<x.y.z> --build-arg GH_SHA256=<sha256 of gh_<x.y.z>_linux_amd64.tar.gz> .
#
# No build argument has a default on purpose: BASE_IMAGE must name an exact digest of
# gcr.io/google.com/cloudsdktool/google-cloud-cli:<version>-slim (gcloud, Python, OpenSSH client), and the GitHub CLI
# release tarball must match GH_SHA256 exactly, or the build fails.
ARG BASE_IMAGE
FROM ${BASE_IMAGE}

ARG GH_VERSION
ARG GH_SHA256

ENV PYTHONPATH=/app/src \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    HOME=/home/autoscaler \
    USER=autoscaler \
    LOGNAME=autoscaler \
    CLOUDSDK_CONFIG=/tmp/gcloud-config \
    CLOUDSDK_CORE_DISABLE_PROMPTS=1 \
    CLOUDSDK_COMPONENT_MANAGER_DISABLE_UPDATE_CHECK=true \
    GH_CONFIG_DIR=/tmp/gh-config \
    GH_PROMPT_DISABLED=1 \
    GH_NO_UPDATE_NOTIFIER=1

WORKDIR /app

# The autoscaler launches runner hosts through spot_runner_manager.py, which renders startup scripts from runner/,
# builder/ and cloud-init/ files besides what src/ embeds; the image carries all four trees.
COPY src/ /app/src/
COPY cloud-init/ /app/cloud-init/
COPY runner/ /app/runner/
COPY builder/ /app/builder/

RUN set -eu; \
    test -n "${GH_VERSION}" && test -n "${GH_SHA256}" || { echo 'GH_VERSION and GH_SHA256 are required' >&2; exit 1; }; \
    echo "${GH_VERSION}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; \
    echo "${GH_SHA256}" | grep -Eq '^[0-9a-f]{64}$'; \
    missing=""; \
    command -v python3 >/dev/null 2>&1 || missing="$missing python3"; \
    command -v ssh >/dev/null 2>&1 || missing="$missing openssh-client"; \
    command -v curl >/dev/null 2>&1 || missing="$missing curl"; \
    if [ -n "$missing" ]; then \
        apt-get update && apt-get install -y --no-install-recommends ca-certificates $missing \
        && rm -rf /var/lib/apt/lists/*; \
    fi; \
    python3 -c "import sys; sys.exit(0 if sys.version_info >= (3, 11) else 'python3 is older than 3.11')"; \
    curl -fsSL --proto '=https' --tlsv1.2 -o /tmp/gh.tar.gz \
        "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_amd64.tar.gz"; \
    echo "${GH_SHA256}  /tmp/gh.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/gh.tar.gz -C /tmp; \
    install -m 0755 "/tmp/gh_${GH_VERSION}_linux_amd64/bin/gh" /usr/local/bin/gh; \
    rm -rf /tmp/gh.tar.gz "/tmp/gh_${GH_VERSION}_linux_amd64"; \
    chown -R root:root /app; \
    chmod -R u=rwX,go=rX /app; \
    groupadd --system --gid 10002 autoscaler; \
    useradd --system --uid 10002 --gid 10002 --home-dir /home/autoscaler --create-home --shell /usr/sbin/nologin \
        autoscaler; \
    python3 -B -c "import lib.spot_runner_autoscaler, lib.autoscaler_job, lib.autoscaler_state_store, lib.runner_manager, lib.tier_controller"; \
    # Build-time checks run as root: give them their own config directories and remove them, so the runtime user
    # finds CLOUDSDK_CONFIG and GH_CONFIG_DIR absent (and creates them) instead of root-owned.
    CLOUDSDK_CONFIG=/tmp/build-gcloud gcloud --version >/dev/null; \
    GH_CONFIG_DIR=/tmp/build-gh gh --version >/dev/null; \
    ssh -V; \
    ssh-keygen --help 2>&1 | grep -q usage; \
    rm -rf /tmp/build-gcloud /tmp/build-gh /tmp/gcloud-config /tmp/gh-config

USER 10002:10002

# One check per execution; for a one-off read-only check:
#   gcloud run jobs execute spot-runner-autoscaler --update-env-vars=SPOT_AUTOSCALER_DRY_RUN=1 --wait
ENTRYPOINT ["python3", "-B", "/app/src/spot_runner_autoscaler_job.py"]

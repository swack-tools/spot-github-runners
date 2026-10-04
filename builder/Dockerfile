FROM oxidex-self-hosted-runner:2.337.0
USER root
RUN apt-get update && apt-get install -y --no-install-recommends rsync time python3-venv \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/uv && /opt/uv/bin/pip install --no-cache-dir uv
ENV UV_PYTHON_INSTALL_DIR=/opt/python
RUN /opt/uv/bin/uv python install 3.11 \
    && mkdir -p /opt/build-tools \
    && ln -s "$(/opt/uv/bin/uv python find 3.11)" /opt/build-tools/python3
COPY entrypoint.py /usr/local/bin/oxidex-build-entrypoint
RUN chmod 0755 /usr/local/bin/oxidex-build-entrypoint \
    && install -m 0444 /dev/null /run/oxidex-build-container
USER runner
ENV RUSTUP_HOME=/home/runner/.rustup
ENV PATH=/home/runner/.cargo/bin:/opt/build-tools:/opt/uv/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN curl --proto '=https' --tlsv1.2 -fsS https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain 1.97.1 \
    && rm /tmp/rustup-init.sh \
    && rustup component add rustfmt clippy --toolchain 1.97.1 \
    && cargo install --locked just \
    && cargo install --locked cargo-nextest \
    && cargo install --locked --version 0.29.2 cbindgen \
    && rustc -vV && cargo -V && cargo nextest --version && cbindgen --version \
    && just --version && uv --version && python3 --version
ENV CARGO_HOME=/cargo
ENV CARGO_TARGET_DIR=/target
ENV UV_CACHE_DIR=/cargo/uv-cache
WORKDIR /src
ENTRYPOINT ["/usr/bin/python3", "/usr/local/bin/oxidex-build-entrypoint"]

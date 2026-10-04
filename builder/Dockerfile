FROM oxidex-self-hosted-runner:2.337.0
USER root
RUN apt-get update && apt-get install -y --no-install-recommends rsync time python3-venv llvm \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/uv && /opt/uv/bin/pip install --no-cache-dir uv
ENV UV_PYTHON_INSTALL_DIR=/opt/python
RUN /opt/uv/bin/uv python install 3.11 \
    && mkdir -p /opt/build-tools \
    && ln -s "$(/opt/uv/bin/uv python find 3.11)" /opt/build-tools/python3
# Primary release checksums: https://ziglang.org/download/index.json and
# https://index.crates.io/ca/rg/cargo-zigbuild. Verify both archives before use.
# Zig supplies target musl headers/libraries and compiles zip's C-backed codecs;
# the host's x86_64 musl-tools alone cannot cross-compile those for ARM64.
RUN curl --proto '=https' --tlsv1.2 -fsS \
      https://ziglang.org/download/0.15.2/zig-x86_64-linux-0.15.2.tar.xz -o /tmp/zig.tar.xz \
    && echo '02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239  /tmp/zig.tar.xz' | sha256sum -c - \
    && mkdir -p /opt/zig \
    && tar -xJf /tmp/zig.tar.xz --strip-components=1 -C /opt/zig \
    && rm /tmp/zig.tar.xz \
    && test "$(/opt/zig/zig version)" = 0.15.2 \
    && curl --proto '=https' --tlsv1.2 -fsS \
      https://static.crates.io/crates/cargo-zigbuild/cargo-zigbuild-0.23.4.crate -o /tmp/cargo-zigbuild.crate \
    && echo '0b0c2fe7d5f1a782047272e3fc1cd44214c450776bbde564bf92b9776dc2d781  /tmp/cargo-zigbuild.crate' | sha256sum -c - \
    && mkdir -p /tmp/cargo-zigbuild-source \
    && tar -xzf /tmp/cargo-zigbuild.crate --strip-components=1 -C /tmp/cargo-zigbuild-source \
    && rm /tmp/cargo-zigbuild.crate \
    && chown -R 1001:1001 /tmp/cargo-zigbuild-source
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
    && rustup target add --toolchain 1.97.1 x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    && cargo install --locked just \
    && cargo install --locked cargo-nextest \
    && cargo install --locked --version 0.29.2 cbindgen \
    && cargo install --locked --path /tmp/cargo-zigbuild-source \
    && rm -rf /tmp/cargo-zigbuild-source \
    && test "$(cargo zigbuild --version)" = 'cargo-zigbuild 0.23.4' \
    && rustc -vV && cargo -V && cargo nextest --version && cbindgen --version \
    && just --version && uv --version && python3 --version \
    && /opt/zig/zig version && cargo zigbuild --version && llvm-strip --version
ENV CARGO_HOME=/cargo
ENV CARGO_TARGET_DIR=/target
ENV UV_CACHE_DIR=/cargo/uv-cache
WORKDIR /src
ENTRYPOINT ["/usr/bin/python3", "/usr/local/bin/oxidex-build-entrypoint"]

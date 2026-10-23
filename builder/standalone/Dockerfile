FROM ubuntu:24.04
ARG DEBIAN_FRONTEND=noninteractive
USER root
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential clang cmake curl git jq libssl-dev make pkg-config \
      perl cpanminus libarchive-zip-perl zlib1g-dev libzstd-dev libarchive-dev ca-certificates \
      unzip zip xz-utils zstd procps util-linux openssh-client gnupg \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1001 runner \
    && useradd --uid 1001 --gid 1001 --create-home --shell /bin/bash runner
RUN apt-get update && apt-get install -y --no-install-recommends rsync time llvm \
    && rm -rf /var/lib/apt/lists/*
# Official Node distribution checksums for x64 and arm64, resolved once and pinned in source.
RUN ARCH="$(uname -m)" \
    && case "$ARCH" in \
         x86_64) NODE_ARCH="x64"; NODE_SHA256="fd8e59d5a511510f6a298afb548f18c7d2b1be404d8b4a27d94fbe49f56cb2d6" ;; \
         aarch64|arm64) NODE_ARCH="arm64"; NODE_SHA256="6ad1325edbdb5649c379b75a237147a666c95d4f9ae8d340fef2d1575d289ad2" ;; \
         *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;; \
       esac \
    && curl --proto '=https' --tlsv1.2 -fsS \
      "https://nodejs.org/dist/v24.21.0/node-v24.21.0-linux-${NODE_ARCH}.tar.xz" -o /tmp/node.tar.xz \
    && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
    && install -d -m 0755 /opt/node \
    && tar -xJf /tmp/node.tar.xz --no-same-owner --strip-components=1 -C /opt/node \
    && chown -hR 0:0 /opt/node \
    && chmod -R go-w /opt/node \
    && rm /tmp/node.tar.xz
ENV PATH=/opt/node/bin:$PATH
ENV PLAYWRIGHT_BROWSERS_PATH=/opt/playwright-browsers
COPY website-package.json /opt/playwright/package.json
COPY website-package-lock.json /opt/playwright/package-lock.json
# The locked package pins Chromium/headless-shell revision 1243. Dependencies
# and browser downloads happen during image construction, never job startup.
RUN cd /opt/playwright \
    && npm ci --ignore-scripts --no-audit --no-fund \
    && ./node_modules/.bin/playwright install --with-deps chromium \
    && chmod -R a+rX /opt/node /opt/playwright /opt/playwright-browsers \
    && rm -rf /var/lib/apt/lists/* /root/.npm \
    && test "$(node --version)" = 'v24.21.0' \
    && test "$(./node_modules/.bin/playwright --version)" = 'Version 1.63.0'
# Primary release checksums: https://ziglang.org/download/index.json and
# https://index.crates.io/ca/rg/cargo-zigbuild. Verify both archives before use.
# Supports both native x86_64 and native aarch64 (ARM64) Linux container builds.
RUN ARCH="$(uname -m)" \
    && case "$ARCH" in \
         x86_64) ZIG_ARCH="x86_64"; ZIG_SHA256="02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239" ;; \
         aarch64|arm64) ZIG_ARCH="aarch64"; ZIG_SHA256="958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f" ;; \
         *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;; \
       esac \
    && curl --proto '=https' --tlsv1.2 -fsS \
      "https://ziglang.org/download/0.15.2/zig-${ZIG_ARCH}-linux-0.15.2.tar.xz" -o /tmp/zig.tar.xz \
    && echo "${ZIG_SHA256}  /tmp/zig.tar.xz" | sha256sum -c - \
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
# SDK-free Darwin framework interface, derived from core-foundation-sys 0.8.7
# (MIT), commit 652bab0a62d87df8a974ffad2ad9f7be218a8b56. No implementation
# or proprietary SDK is included. See builder/DARWIN-STUB-NOTICE.md.
RUN install -d -m 0755 /opt/opensourceDarwinStubs/CoreFoundation.framework \
    && printf '%s\n' \
      '--- !tapi-tbd' \
      'tbd-version: 4' \
      'targets: [ arm64-macos, x86_64-macos ]' \
      'install-name: /System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation' \
      'current-version: 1.0' \
      'compatibility-version: 1.0' \
      'exports:' \
      '  - targets: [ arm64-macos, x86_64-macos ]' \
      '    symbols: [ _CFRelease, _CFTimeZoneCopySystem, _CFTimeZoneGetName, _CFTimeZoneResetSystem, _CFStringGetCStringPtr, _CFStringGetLength, _CFStringGetBytes ]' \
      '...' > /opt/opensourceDarwinStubs/CoreFoundation.framework/CoreFoundation.tbd \
    && printf '%s\n' \
      'CoreFoundation interface symbols derived from core-foundation-sys 0.8.7.' \
      'Source: https://github.com/servo/core-foundation-rs/tree/652bab0a62d87df8a974ffad2ad9f7be218a8b56/core-foundation-sys' \
      'Licensed under MIT (selected from MIT OR Apache-2.0).' \
      '' \
      'Copyright (c) 2012-2013 Mozilla Foundation' \
      '' \
      'Permission is hereby granted, free of charge, to any person obtaining a copy' \
      'of this software and associated documentation files (the "Software"), to deal' \
      'in the Software without restriction, including without limitation the rights' \
      'to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies' \
      'of the Software, and to permit persons to whom the Software is furnished to' \
      'do so, subject to the following conditions:' \
      '' \
      'The above copyright notice and this permission notice shall be included in' \
      'all copies or substantial portions of the Software.' \
      '' \
      'THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR' \
      'IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,' \
      'FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE' \
      'AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER' \
      'LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,' \
      'OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN' \
      'THE SOFTWARE.' > /opt/opensourceDarwinStubs/NOTICE \
    && chmod 0444 /opt/opensourceDarwinStubs/CoreFoundation.framework/CoreFoundation.tbd /opt/opensourceDarwinStubs/NOTICE
COPY host-agent /usr/local/bin/host-agent
RUN chmod 0755 /usr/local/bin/host-agent \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/oxidex-build-entrypoint \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/example-rust-project-build-entrypoint \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/builder-entrypoint \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/oxidex-worktree-commands \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/example-rust-project-worktree-commands \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/worktree-commands \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/oxidex-toolchain-probe \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/example-rust-project-toolchain-probe \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/toolchain-probe \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/docker-admission \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/host-launcher \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/oxidex-remote-build \
    && ln -s /usr/local/bin/host-agent /usr/local/bin/example-rust-project-remote-build \
    && install -m 0444 /dev/null /run/oxidex-build-container
USER runner
ENV RUSTUP_HOME=/home/runner/.rustup
ENV PATH=/home/runner/.cargo/bin:/opt/node/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN curl --proto '=https' --tlsv1.2 -fsS https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain 1.99.0 \
    && rm /tmp/rustup-init.sh \
    && rustup component add rustfmt clippy --toolchain 1.99.0 \
    && rustup target add --toolchain 1.99.0 x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    && rustup target add --toolchain 1.99.0 aarch64-apple-darwin x86_64-apple-darwin aarch64-apple-ios aarch64-apple-ios-sim \
    && cargo install --locked -j4 just \
    && cargo install --locked -j4 cargo-nextest \
    && cargo install --locked -j4 --version 0.29.2 cbindgen \
    && cargo install --locked -j4 --path /tmp/cargo-zigbuild-source \
    && rm -rf /tmp/cargo-zigbuild-source \
    && test "$(cargo-zigbuild --version)" = 'cargo-zigbuild 0.23.4' \
    && rustc -vV | grep -Fx 'commit-hash: b940084d7eb6a299eb4bfeb8e34901bc051e7ac4' \
    && rustc -vV && cargo -V && cargo nextest --version && cbindgen --version \
    && just --version \
    && /opt/zig/zig version && cargo-zigbuild --version && llvm-strip --version
ENV CARGO_HOME=/cargo
ENV CARGO_TARGET_DIR=/target
WORKDIR /src
ENTRYPOINT ["/usr/local/bin/host-agent", "builder-entrypoint"]

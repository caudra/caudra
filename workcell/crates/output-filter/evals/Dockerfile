# Eval harness for the command-output rule corpus.
#
# Rules must be written against real tool output, not remembered formats. This
# image installs the tools the corpus claims to filter so their output can be
# captured and replayed through the server. It is a development harness: it is
# never published and is not part of the runtime image.
#
FROM rust:1.99.0-slim-bookworm AS rust
RUN rustup toolchain install 1.99.1 --profile minimal \
    && rustup default 1.99.1 \
    && rustup toolchain uninstall 1.99.0

FROM golang:1.27.2-bookworm AS go

# The base matches the development host so a host-built `workcell-mcp` can be
# mounted instead of rebuilt. Keep this in sync with the host distribution.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        docker-compose-v2 \
        docker.io \
        git \
        make \
        maven \
        python3 \
        python3-pip \
        python3-venv \
        wget \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*

ENV GOTOOLCHAIN=local \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/go/bin:${PATH}
COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo /usr/local/cargo
COPY --from=go /usr/local/go /usr/local/go
RUN rustc --version && cargo --version && go version

# This distribution's Node is too old for the current tools. A pinned upstream
# tarball avoids running a remote setup script during the build.
ARG NODE_VERSION=26.12.0
RUN curl -fsSLo /tmp/node.tar.xz \
        "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
    && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
        --exclude CHANGELOG.md --exclude LICENSE --exclude README.md \
    && rm /tmp/node.tar.xz \
    && node --version

# Node and Python tools the corpus targets, installed once at image build time.
# Cases that deliberately exercise a resolver, such as `npm install` or
# `pip install`, still reach the network when they run.
RUN npm install --global --no-fund --no-audit typescript eslint jest vitest \
    && npm cache clean --force
# `tqdm` and `rich` are the progress libraries the reduction is measured
# against. `tqdm` does not consult isatty, so it redraws into a pipe and is the
# dominant real source of redraw noise; `rich` does consult it, and the harness
# reports that as a measurement rather than assuming it either way.
RUN python3 -m pip install --break-system-packages --no-cache-dir pytest tqdm rich

# cargo-nextest is not packaged by the distribution, and `cargo install` would
# rebuild it from source on every image build. A pinned upstream tarball is used
# for the same reason as Node above. Its report format differs from `cargo test`
# in every respect except Cargo's build preamble, which is why the corpus gives
# it a rule of its own and why that rule needs its own measured case.
ARG NEXTEST_VERSION=0.9.148
RUN curl -fsSLo /tmp/nextest.tar.gz "https://get.nexte.st/${NEXTEST_VERSION}/linux" \
    && tar -xzf /tmp/nextest.tar.gz -C /usr/local/bin cargo-nextest \
    && rm /tmp/nextest.tar.gz \
    && cargo nextest --version

# The harness is baked in rather than bind-mounted: an eval run may target a
# remote daemon, where a bind mount would resolve on the daemon's filesystem
# instead of the caller's.
COPY setup-workspace.sh /usr/local/bin/setup-workspace
COPY capture-progress.py /usr/local/bin/capture-progress

# Progress generators, including the Go, Rust, and Node libraries. These take
# real third-party dependencies so the harness measures what each one writes to
# a pipe instead of assuming. Most go quiet there; `schollz/progressbar` does
# not, which is why assuming is not safe.
COPY progress /usr/local/share/workcell-progress

# Sample projects and their dependency caches are baked in for the same reason,
# and so a run captures warm output instead of first-resolution noise.
RUN setup-workspace /work --warm

WORKDIR /work

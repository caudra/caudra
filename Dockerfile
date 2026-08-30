# The code execution worker is a separate stage so that editing server sources does not rebuild
# Monty. `--locked` builds it against Monty's own published lockfile, the only resolution its ruff/ty
# dependency tree is known to satisfy, and `--no-default-features` drops Monty's standalone CLI,
# leaving a binary that only serves `monty subprocess`.
#
# MONTY_VERSION must match the `monty-pool` pin in Cargo.toml: the worker protocol is version-coupled.
FROM rust:1.98.0-bookworm AS worker

ARG MONTY_VERSION=0.0.21

RUN cargo install monty-runtime --version "=${MONTY_VERSION}" --locked --no-default-features \
      --root /out \
    && strip /out/bin/monty

FROM rust:1.98.0-bookworm AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY src src
COPY README.md ./

# Built as a single package rather than `--workspace`: the workspace build is only used for
# verification, and the shipped server must not link anything the code group needs.
RUN cargo build --locked --release --package workcell-mcp \
    && strip target/release/workcell-mcp

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends bash ca-certificates libgcc-s1 tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 workcell \
    && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /nonexistent \
      --shell /usr/sbin/nologin workcell

COPY --from=builder /build/target/release/workcell-mcp /usr/local/bin/workcell-mcp
# The server discovers the worker beside its own executable, so no configuration is needed in the
# image. Keep the file name `monty`: it is what the discovery path looks for.
COPY --from=worker /out/bin/monty /usr/local/bin/monty
COPY LICENSE.md THIRD_PARTY_LICENSES/Monty.txt /usr/share/doc/workcell-mcp/

USER 10001:10001
EXPOSE 3001
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/workcell-mcp"]
CMD []

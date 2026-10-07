# The code execution worker is a separate stage so that editing server sources does not rebuild
# Monty. `--locked` builds it against Monty's own published lockfile, the only resolution its ruff/ty
# dependency tree is known to satisfy, and `--no-default-features` drops Monty's standalone CLI,
# leaving a binary that only serves `monty subprocess`.
#
# Build from the repository root: docker build -f workcell/Dockerfile .
FROM rust:1.99.0-bookworm AS worker

RUN apt-get update \
    && apt-get install --yes --no-install-recommends python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.lock ./
COPY scripts/build-code-worker.py scripts/build-code-worker.py
RUN python3 scripts/build-code-worker.py --root /out --target-dir /build/target/code-worker-build

FROM rust:1.99.0-bookworm AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
COPY --from=worker /usr/local/cargo/registry /usr/local/cargo/registry
COPY --from=worker /out/source-manifest-path /out/source-manifest-path

# Built as a single package rather than `--workspace`: the workspace build is only used for
# verification, and the shipped server must not link anything the code group needs. Symbols are
# kept in the build for the diagnostics stage and stripped from the copy the runtime ships.
RUN CARGO_PROFILE_RELEASE_STRIP=none cargo build --locked --release --package workcell-mcp \
    && mkdir -p /out/bin \
    && strip -o /out/bin/workcell-mcp target/release/workcell-mcp \
    && python3 scripts/build-attribution.py \
      --manifest-path Cargo.toml --package workcell-mcp \
      --target "$(rustc -vV | sed -n 's/^host: //p')" \
      --worker-manifest-path "$(cat /out/source-manifest-path)" \
      --worker-package monty-runtime --output-dir /out/licenses \
    && test -s /out/licenses/manifest.json

# Unstripped executables for symbolizing a crash, exported rather than run:
# `docker build -f workcell/Dockerfile --target diagnostics --output <directory> .`
FROM scratch AS diagnostics

COPY --from=builder /build/target/release/workcell-mcp /workcell-mcp
COPY --from=worker /out/symbols/bin/monty /monty
COPY --from=builder /out/licenses /licenses

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends bash ca-certificates libgcc-s1 tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 workcell \
    && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /nonexistent \
      --shell /usr/sbin/nologin workcell

COPY --from=builder /out/bin/workcell-mcp /usr/local/bin/workcell-mcp
# The server discovers the worker beside its own executable, so no configuration is needed in the
# image. Keep the file name `monty`: it is what the discovery path looks for.
COPY --from=worker /out/bin/monty /usr/local/bin/monty
COPY --from=builder /out/licenses /usr/share/licenses/workcell-mcp
COPY workcell/LICENSE.md workcell/THIRD_PARTY.md workcell/THIRD_PARTY_LICENSES/Monty.txt THIRD_PARTY_LICENSES/RTK.txt /usr/share/doc/workcell-mcp/
COPY workcell/crates/output-filter/NOTICE /usr/share/doc/workcell-mcp/RTK-NOTICE

USER 10001:10001
EXPOSE 3001
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/workcell-mcp"]
CMD []

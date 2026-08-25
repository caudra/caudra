FROM rust:1.97.0-bookworm AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY src src
COPY README.md ./

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
COPY LICENSE.md /usr/share/doc/workcell-mcp/

USER 10001:10001
EXPOSE 3001
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/workcell-mcp"]
CMD []

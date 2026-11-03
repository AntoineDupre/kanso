FROM rust:1.99-alpine AS builder

RUN apk add --no-cache gcc musl-dev

WORKDIR /src
COPY Cargo.toml Cargo.lock ./

# Compile dependencies first so source edits do not rebuild the whole tree.
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY skills ./skills
# The official alpine Rust image targets x86_64-unknown-linux-musl natively,
# and musl binaries link libc statically by default.
RUN cargo build --release --locked

FROM alpine:3.24 AS runtime

RUN apk add --no-cache ca-certificates tzdata \
    && addgroup -g 1000 kanso \
    && adduser -D -u 1000 -G kanso -h /data -s /sbin/nologin kanso \
    && mkdir -p /data \
    && chown -R 1000:1000 /data

WORKDIR /app
COPY --from=builder /src/target/release/kanso /usr/local/bin/kanso
COPY web /app/web

USER 1000:1000
VOLUME ["/data"]
EXPOSE 8087

ENV KANSO_DATA_DIR=/data \
    KANSO_BIND=0.0.0.0:8087

HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
    CMD wget -qO- "http://127.0.0.1:${KANSO_BIND##*:}/api/health" >/dev/null || exit 1

ENTRYPOINT ["kanso"]
CMD ["serve"]

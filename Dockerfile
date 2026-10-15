# ---- build ---------------------------------------------------------------
FROM rust:1-slim-bookworm AS build
WORKDIR /src

# Build dependencies first so code-only changes rebuild quickly.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
COPY migrations ./migrations
COPY data ./data
# Touch so cargo notices the real sources replaced the stubs.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ---- runtime -------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin wordle

COPY --from=build /src/target/release/wordle-bot /usr/local/bin/wordle-bot
USER wordle
ENV RUST_LOG=info,sqlx=warn
ENTRYPOINT ["/usr/local/bin/wordle-bot"]

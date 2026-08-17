FROM rust:bookworm AS builder

RUN rustup toolchain install nightly --profile minimal \
    && apt-get update \
    && apt-get install --yes --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
ENV RUSTUP_TOOLCHAIN=nightly

# Build the server as an isolated workspace so Cargo does not resolve desktop-only dependencies.
COPY Cargo.lock ./
RUN printf '[workspace]\nmembers = ["dirigent_server"]\nresolver = "3"\n\n[workspace.package]\nversion = "0.0.0"\nedition = "2024"\n' > Cargo.toml
COPY dirigent_server/Cargo.toml dirigent_server/Cargo.toml

# Cache dependencies separately from the application source.
RUN mkdir -p dirigent_server/src \
    && printf 'pub fn placeholder() {}\n' > dirigent_server/src/lib.rs \
    && printf 'fn main() {}\n' > dirigent_server/src/main.rs \
    && cargo build --release -p dirigent_server \
    && rm -rf dirigent_server/src

COPY dirigent_server/src dirigent_server/src
RUN touch dirigent_server/src/lib.rs dirigent_server/src/main.rs \
    && cargo build --locked --release -p dirigent_server \
    && strip target/release/dirigent_server

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 dirigent

COPY --from=builder /app/target/release/dirigent_server /usr/local/bin/dirigent_server

USER dirigent
ENV DIRIGENT_BIND=0.0.0.0:8080
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/dirigent_server"]

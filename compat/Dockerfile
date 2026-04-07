FROM rust:1.87-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends kmod && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/microvm-compat /usr/local/bin/microvm-compat
ENTRYPOINT ["microvm-compat"]

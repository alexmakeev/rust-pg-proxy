# Stage 1: Build
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Cache dependencies build
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY src/ src/
RUN cargo build --release

# Stage 2: Runtime
FROM alpine:3.19
RUN apk add --no-cache ca-certificates
COPY --from=builder /app/target/release/rust-pg-proxy /usr/local/bin/
EXPOSE 5433
CMD ["rust-pg-proxy"]

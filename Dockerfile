FROM rust:1.96-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY tests ./tests
RUN cargo build --locked --release --bins

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/invoice-service /usr/local/bin/invoice-service
COPY --from=build /app/target/release/mock-psp /usr/local/bin/mock-psp
USER 65534:65534
CMD ["invoice-service"]

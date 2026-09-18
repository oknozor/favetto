# Multi-stage build for the mock-linear dev server (see docker-compose.yml).
FROM rust:1-slim AS build
WORKDIR /app
COPY . .
RUN cargo build -p mock-linear --release

FROM debian:bookworm-slim
COPY --from=build /app/target/release/mock-linear /usr/local/bin/mock-linear
EXPOSE 4000
CMD ["mock-linear"]

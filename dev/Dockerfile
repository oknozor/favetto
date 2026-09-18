# Multi-stage build for the dev mock servers (see docker-compose.yml).
ARG BIN=mock-linear
FROM rust:1-slim AS build
ARG BIN=mock-linear
WORKDIR /app
COPY . .
RUN cargo build -p ${BIN} --release

FROM debian:bookworm-slim
ARG BIN=mock-linear
COPY --from=build /app/target/release/${BIN} /usr/local/bin/mock
EXPOSE 4000
CMD ["mock"]

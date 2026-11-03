# syntax=docker/dockerfile:1
FROM rust:1.99.1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG CONTENT_GATEWAY_COMMIT
ARG CONTENT_GATEWAY_BUILT_AT
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin content-gateway \
    && cp target/release/content-gateway /content-gateway

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /content-gateway /usr/local/bin/content-gateway
ENV CONTENT_GATEWAY_HOST=0.0.0.0 \
    CONTENT_GATEWAY_PORT=5005
EXPOSE 5005
USER nonroot
ENTRYPOINT ["/usr/local/bin/content-gateway"]

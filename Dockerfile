# syntax = docker/dockerfile:1.2

FROM clux/muslrust:stable as build
# Build application
COPY src/ ./src
COPY Cargo.toml Cargo.lock FiraSans-Regular.otf ./

RUN --mount=type=cache,target=/root/.cargo/registry --mount=type=cache,target=/volume/target \
    cargo b --profile ship --target x86_64-unknown-linux-musl && \
    cp target/x86_64-unknown-linux-musl/ship/unsafe-track unsafe-track

FROM bash AS get-tini

# Add Tini init-system
ENV TINI_VERSION v0.19.0
ADD https://github.com/krallin/tini/releases/download/${TINI_VERSION}/tini-static /tini
RUN chmod +x /tini

FROM gcr.io/distroless/static

LABEL org.opencontainers.image.source https://github.com/DCNick3/unsafe-track
EXPOSE 8080

COPY --from=get-tini /tini /tini
COPY --from=build /volume/unsafe-track /unsafe-track

ENTRYPOINT ["/tini", "--", "/unsafe-track", "server", "8080"]

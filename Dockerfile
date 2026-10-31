# syntax=docker/dockerfile:1
#
# kurin — CLI + embedded web UI in one image.
#
#   docker build -t willdockerhub/kurin:0.6.0 .
#   docker run --rm willdockerhub/kurin:0.6.0 --help
#   docker run --rm -p 127.0.0.1:7080:7080 -v kurin-data:/data \
#     willdockerhub/kurin:0.6.0 serve --listen 0.0.0.0:7080 --allow-insecure-http
#
# The web SPA is built in the `web` stage and embedded into the binary by
# rust-embed (`--features ui`), so the runtime image is a single executable.

FROM node:22-bookworm-slim AS web
WORKDIR /src/web
RUN corepack enable && corepack prepare pnpm@9.15.9 --activate
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY kurin ./kurin
COPY addons ./addons
COPY --from=web /src/web/dist ./web/dist
RUN cargo build --release -p kurin --features ui

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/kurin /usr/local/bin/kurin

# `serve` keeps its token (and later its database) under XDG_DATA_HOME.
ENV HOME=/root \
    XDG_DATA_HOME=/data
VOLUME ["/data"]
EXPOSE 7080
ENTRYPOINT ["kurin"]
CMD ["--help"]
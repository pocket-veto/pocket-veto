FROM mcr.microsoft.com/devcontainers/rust:2-1-trixie

RUN apt-get update && apt-get install -yq --no-install-recommends \
    libdbus-1-dev

RUN mkdir -p /workspaces/pocket-veto/target/release /tmp/cursor-sandbox-cache /usr/local/cargo /usr/local/rustup

RUN rustup update


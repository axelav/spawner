FROM mcr.microsoft.com/devcontainers/rust:1-bullseye

# Install NATS
RUN curl -L https://github.com/nats-io/natscli/releases/download/v0.0.35/nats-0.0.35-amd64.deb -o nats.deb && dpkg -i nats.deb

USER vscode

# Allow pushing directly to new branches.
RUN git config --global --add push.autoSetupRemote true

# Update Rust version to latest.
RUN rustup update stable

# Install cargo ws
RUN cargo install cargo-workspaces

# Install cargo nextest
RUN curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C ${CARGO_HOME:-~/.cargo}/bin


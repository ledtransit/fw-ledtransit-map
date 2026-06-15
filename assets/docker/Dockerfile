FROM ubuntu:24.04

ENV PATH="/root/.cargo/bin:${PATH}"

# Args: Product and firmware version
ARG PRODUCT=bln1-2512-1
ARG FW_VERSION=latest

# System dependencies
RUN apt update && apt install -y --no-install-recommends \
    build-essential \
    curl \
    git \
    protobuf-compiler \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# Install required Rust toolchain/target
RUN rustup toolchain install stable --component rust-src && \
    rustup target add riscv32imc-unknown-none-elf

# Install probe-rs
RUN curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/probe-rs/probe-rs/releases/latest/download/probe-rs-tools-installer.sh \
    | sh

# Install espflash
RUN cargo install espflash --locked

WORKDIR /workspace

# Clone firmware repository
RUN git clone https://github.com/ledtransit/fw-ledtransit-map.git

# Checkout provided version tag or latest tag
RUN cd fw-ledtransit-map && \
    LATEST_VERSION=$(git tag | sort -V | tail -1) && \
    if [ "$FW_VERSION" = "latest" ]; then \
        git checkout $LATEST_VERSION; \
    else \
        git checkout $FW_VERSION; \
    fi

WORKDIR /workspace/fw-ledtransit-map

# Verify toolchain setup
RUN cargo xtask doctor

# Build selected product
RUN cargo xtask build ${PRODUCT}

CMD ["/bin/bash"]

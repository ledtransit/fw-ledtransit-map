FROM ubuntu:24.04

ENV PATH="/root/.cargo/bin:${PATH}"
ENV PRODUCT=bln1-2512-1

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        build-essential \
        curl \
        protobuf-compiler \
        ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    curl https://sh.rustup.rs -sSf | sh -s -- -y && \
    rustup toolchain install stable --component rust-src && \
    rustup target add riscv32imc-unknown-none-elf && \
    rm -rf /root/.cargo/registry /root/.cargo/git

WORKDIR /workspace

ENTRYPOINT ["bash", "-c"]
CMD ["cargo xtask build ${PRODUCT}"]

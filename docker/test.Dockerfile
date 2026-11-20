# Runs the full test suite (unit tests + `shotglass selftest`) on Mesa's software
# OpenGL, the same way CI does. Reproducible on any machine:
#
#   docker build -f docker/test.Dockerfile -t shotglass-test .
#   docker run --rm shotglass-test
#
# This tests shotglass's logic, not your GPU driver: for that, run
# `cargo test` (or `shotglass selftest`) natively on Windows.
FROM rust:1-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
        libasound2-dev pkg-config \
        xvfb xauth \
        libgl1 libegl1 libgl1-mesa-dri \
        libx11-xcb1 libxkbcommon-x11-0 libxcursor1 libxrandr2 libxi6 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY src src
COPY tests tests
COPY templates templates
COPY movies movies
RUN cargo build --release --tests

CMD ["xvfb-run", "-a", "cargo", "test", "--release"]

# The venue: identity, booths, and one turn at a time.
#
# Two images meet here. The RUNTIME half is this repo's `prism` binary, which needs
# a Python 3.14 shared library because the python booth is embedded (pyo3), so the
# base is `python:3.14-slim` rather than a slim Debian with Python bolted on. The
# GUEST half is gravity's two binaries, copied out of the gravity image — prism
# spawns them per turn, so a version skew between the venue and its guests would be
# invisible until a turn ran; copying from a tag keeps them one artifact.
#
# Build context is workspace-shaped: prism at `prism/`, its path dependencies beside
# it (memory-stack/build.sh assembles the tree).
ARG GUESTS_IMAGE=ghcr.io/orbsh/gravity:latest
FROM ${GUESTS_IMAGE} AS guests

FROM python:3.14-slim AS build

# Rust comes from rustup because the base image's job is Python, not Rust: the
# toolchain is a build-only detail and pinning it to a Debian release would pin the
# interpreter with it.
ARG http_proxy=
ARG https_proxy=
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config curl ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
 && /root/.cargo/bin/cargo --version

ENV PATH=/root/.cargo/bin:$PATH
WORKDIR /src
COPY aura/ aura/
COPY effector/ effector/
COPY induction/ induction/
COPY krystallizer/ krystallizer/
COPY gravity/ gravity/
COPY prism/ prism/

# No `RUSTFLAGS`: libpython lives in /usr/local/lib here, which the linker searches
# by default. The flag exists for Nix's layout, where it does not.
#
# Copied out of `target/` in the same step: a build harness may mount that directory
# as an incremental cache (memory-stack/build.sh does) and a mounted directory is not
# part of the stage's filesystem, so `COPY --from` would come up empty.
RUN cargo build --release --locked \
      --manifest-path prism/Cargo.toml \
      -p prism --bin prism \
 && mkdir -p /out \
 && cp prism/target/release/prism /out/prism

FROM python:3.14-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/prism /usr/local/bin/prism

# The steel echo booth keeps a home directory under `$HOME` and does not create its
# parents; without this it logs "Unable to create steel home directory" at every boot
# and the steel verb answers from a booth with no home.
RUN mkdir -p /root/.local/share/steel
COPY --from=guests /usr/local/bin/gravity-turn /usr/local/bin/gravity-turn
COPY --from=guests /usr/local/bin/gravity-provider /usr/local/bin/gravity-provider

# Deployment facts, not policy: where the venue listens and where its identity store
# lives. Everything else — store URL, model endpoint, key variable NAME — belongs to
# the deployment that runs this image, so it is left unset rather than guessed.
ENV PRISM_ADDR=0.0.0.0:8765 \
    PRISM_DATA=/data
VOLUME ["/data"]
EXPOSE 8765

ENTRYPOINT ["/usr/local/bin/prism"]
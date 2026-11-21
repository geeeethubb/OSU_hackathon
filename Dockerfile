# Shopping Sessions: one static binary in an empty image.
# The scorers are linux/amd64 and build this file natively, so no FROM line
# pins a platform.
FROM rust:1.100.0-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# A fully static binary: nothing to load at start, and it runs on the scorer's
# read-only root filesystem with no libraries in the image.
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo build --release --locked --target x86_64-unknown-linux-gnu \
 && mkdir /out \
 && cp target/x86_64-unknown-linux-gnu/release/pipeline \
       target/x86_64-unknown-linux-gnu/release/reference /out/

FROM scratch
# /pipeline is what gets scored. /reference is the simple, correct
# implementation kept for cross-checking (docker run --entrypoint /reference).
COPY --from=build /out/pipeline /pipeline
COPY --from=build /out/reference /reference
USER 1000:1000
CMD ["/pipeline"]

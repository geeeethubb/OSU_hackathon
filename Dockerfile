# Shopping Sessions: one static binary in an empty image.
# The scorers are linux/amd64 and build this file natively, so no FROM line
# pins a platform.
FROM rust:1.100.0-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Profile-guided optimization: branch and call counts of this pipeline on
# practice-size generated data, made with this same image by
# .github/workflows/pgo.yml (counts only: no input data, no answers). A
# function whose code changed since then is simply built without them.
COPY pgo ./pgo
# A fully static binary: nothing to load at start, and it runs on the scorer's
# read-only root filesystem with no libraries in the image.
# x86-64-v3: the engine uses AVX2/BMI2 directly (the scorers are Cascade Lake or newer).
ENV RUSTFLAGS="-C target-feature=+crt-static -C target-cpu=x86-64-v3"
RUN cargo build --release --locked --target x86_64-unknown-linux-gnu \
 && mkdir /out \
 && cp target/x86_64-unknown-linux-gnu/release/pipeline \
       target/x86_64-unknown-linux-gnu/release/reference \
       target/x86_64-unknown-linux-gnu/release/chmod /out/

FROM scratch
# /pipeline is what gets scored. /reference is the simple, correct
# implementation kept for cross-checking (docker run --entrypoint /reference).
COPY --from=build /out/pipeline /pipeline
COPY --from=build /out/reference /reference
# The organizers' docker_run.sh runs `chmod -R a+rwX` inside the image to hand
# the run's files back to the host user; an empty image needs its own.
COPY --from=build /out/chmod /bin/chmod
USER 1000:1000
CMD ["/pipeline"]

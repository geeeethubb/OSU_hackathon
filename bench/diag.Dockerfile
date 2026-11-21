# ss-diag: the benchmark harness's diagnostics binary in an empty image.
#
#   docker buildx build --platform linux/amd64 --load -t ss-diag:latest -f bench/diag.Dockerfile bench
#
# The build stage is the same base image as the pipeline's Dockerfile, so a
# job that builds both pulls it once. The binary is fully static, like the
# pipeline's, and the final image holds nothing else: starting it costs what
# starting the pipeline's image costs.
FROM rust:1.100.0-slim-bookworm AS build
WORKDIR /diag
COPY diag/diag.rs .
# x86-64-v3 (AVX2) runs on both the GitHub runners (AMD EPYC) and the scorer
# (Intel Xeon, Cascade Lake or newer).
RUN mkdir /out \
 && rustc --edition 2021 -C opt-level=3 -C target-cpu=x86-64-v3 -C target-feature=+crt-static \
          -C panic=abort -C strip=symbols -o /out/diag diag.rs

FROM scratch
COPY --from=build /out/diag /diag
USER 1000:1000
ENTRYPOINT ["/diag"]
CMD ["noop"]

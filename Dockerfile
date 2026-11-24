# syntax=docker/dockerfile:1
FROM rust:1.99.1-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7 AS build
ARG TARGETARCH
ARG HQW_COMMIT=unknown
RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY rust-toolchain.toml ./
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl > /target ;; \
      arm64) echo aarch64-unknown-linux-musl > /target ;; \
      *) echo "unsupported arch $TARGETARCH" && exit 1 ;; \
    esac \
 && rustup target add "$(cat /target)"
COPY Cargo.toml Cargo.lock clippy.toml ./
COPY .cargo .cargo
COPY .sqlx .sqlx
COPY migrations migrations
COPY src src
ENV SQLX_OFFLINE=true HQW_COMMIT=${HQW_COMMIT}
RUN cargo build --release --locked --target "$(cat /target)" \
 && cp "target/$(cat /target)/release/hq-watchman" /hq-watchman
RUN mkdir -p /rootfs/etc/ssl/certs /rootfs/usr/share /rootfs/config /data-dir \
 && cp /etc/ssl/certs/ca-certificates.crt /rootfs/etc/ssl/certs/ \
 && cp -r /usr/share/zoneinfo /rootfs/usr/share/zoneinfo \
 && echo "nonroot:x:65532:65532:nonroot:/data:/sbin/nologin" > /rootfs/etc/passwd \
 && echo "nonroot:x:65532:" > /rootfs/etc/group \
 && chmod 700 /data-dir

FROM scratch
COPY --from=build /rootfs/ /
COPY --from=build --chown=65532:65532 /data-dir /data
COPY --from=build /hq-watchman /usr/local/bin/hq-watchman
USER 65532:65532
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=60s --timeout=10s --start-period=30s --retries=3 CMD ["/usr/local/bin/hq-watchman", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/hq-watchman"]
CMD ["run", "--config", "/config/config.toml"]

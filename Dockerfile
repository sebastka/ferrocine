# syntax=docker/dockerfile:1.28.0@sha256:bb22d9815c728170f72750f4e5b0d672e06176142e1d602c7e66c050100b7e5b

# Docker Hardened Images, pinned by multi-arch index digest (amd64 + arm64).
# Renovate keeps the digests (and the syntax directive) up to date.
FROM dhi.io/rust:1-alpine3.24-dev@sha256:093724ebcbc1af3c7c706e96e73c5d65e5f435477affe92215cc440b44c101b8 AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
# Fully static binary: it needs nothing from the runtime image. An explicit
# --target keeps RUSTFLAGS away from proc-macros, which must stay dynamic.
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN --mount=type=cache,target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    target="$(rustc -vV | sed -n 's/^host: //p')" \
 && cargo build --release --locked --target "$target" \
 && cp "target/$target/release/ferrocine" /ferrocine

# Hardened Debian runtime (non-root user 65532, CA bundle, no package
# manager) plus Jellyfin's ffmpeg build, which bundles current GPU drivers:
# Intel's media driver and Quick Sync runtime, Mesa's VA-API driver for AMD,
# libva, Vulkan, NVENC and the tone-mapping filters. One image serves every
# vendor. apt and dpkg are borrowed from the -dev image for this one step and
# not left in the image; see docker/install-packages.sh.
FROM dhi.io/debian-base:trixie@sha256:de80b468d8c492508f24b30e529accfaa8c59fe4505ddd72e969b324d276ad21

# renovate: datasource=deb depName=jellyfin-ffmpeg8 registryUrl=https://repo.jellyfin.org/debian?suite=trixie&components=main&binaryArch=amd64
ARG JELLYFIN_FFMPEG_VERSION=8.1.3-1-trixie

USER 0
RUN --mount=type=bind,from=dhi.io/debian-base:trixie-dev@sha256:9b025768946856e44f9775005eb199f2e1820484ea27bb5c8bcbb187b80464dc,target=/x \
    --mount=type=bind,source=docker,target=/docker \
    /x/bin/sh /docker/install-packages.sh "jellyfin-ffmpeg8=$JELLYFIN_FFMPEG_VERSION" \
 && mkdir -p /usr/local/bin \
 && ln -s /usr/lib/jellyfin-ffmpeg/ffmpeg /usr/lib/jellyfin-ffmpeg/ffprobe /usr/local/bin/ \
 && ffmpeg -hide_banner -version | head -1

LABEL org.opencontainers.image.title="Ferrocine" \
      org.opencontainers.image.description="Self-hosted media server: read-only libraries, metadata in SQL, hardware transcoding, OpenID Connect sign-in" \
      org.opencontainers.image.source="https://github.com/sebastka/ferrocine" \
      org.opencontainers.image.licenses="MIT"

COPY --from=build --chmod=0755 /ferrocine /usr/local/bin/ferrocine

# Unprivileged. VA-API/QSV need /dev/dri passed in and the render group of
# the host added (`group_add`); see docs/deployment.md.
USER 65532:65532
# Mesa keeps a shader cache; with a read-only root filesystem it goes to /tmp.
# With the NVIDIA container toolkit (`--gpus all`), mount the driver's CUDA,
# NVENC/NVDEC and management libraries.
ENV FERROCINE_CONFIG=/etc/ferrocine \
    XDG_CACHE_HOME=/tmp/.cache \
    NVIDIA_DRIVER_CAPABILITIES=compute,video,utility
EXPOSE 8096
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s \
    CMD ["/usr/local/bin/ferrocine", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/ferrocine"]
CMD ["run"]

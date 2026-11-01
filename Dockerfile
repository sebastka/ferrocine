# syntax=docker/dockerfile:1.28.0@sha256:bb22d9815c728170f72750f4e5b0d672e06176142e1d602c7e66c050100b7e5b

# Docker Hardened Images, pinned by multi-arch index digest (amd64 + arm64).
# Renovate keeps the digests (and the syntax directive) up to date.
FROM dhi.io/rust:1-alpine3.24-dev@sha256:77de5f93fcfa37ede037c590c57f6460c976158aea65bc133dd1739d6235e3ed AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY migrations ./migrations
# Fully static binary: it needs nothing from the runtime image. An explicit
# --target keeps RUSTFLAGS away from proc-macros, which must stay dynamic.
ENV RUSTFLAGS="-C target-feature=+crt-static"
# The release tag (e.g. 2026-10-23), shown by --version and /api/v1/system.
ARG VERSION=""
RUN --mount=type=cache,target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    target="$(rustc -vV | sed -n 's/^host: //p')" \
 && FERROCINE_VERSION="$VERSION" cargo build --release --locked --target "$target" \
 && cp "target/$target/release/ferrocine" /ferrocine

# Hardened Debian runtime (non-root user 65532, CA bundle, no package
# manager) plus Jellyfin's ffmpeg build, which bundles current GPU drivers:
# Intel's media driver and Quick Sync runtime, Mesa's VA-API driver for AMD,
# libva, Vulkan, NVENC and the tone-mapping filters. One image serves every
# vendor. apt and dpkg are borrowed from the -dev image for this one step and
# not left in the image; see docker/install-packages.sh.
FROM dhi.io/debian-base:trixie@sha256:fefc620cea08f18e60c8f1afbf670200463384726b6f31f8d59f8a4707976f81

# renovate: datasource=deb depName=jellyfin-ffmpeg8 registryUrl=https://repo.jellyfin.org/debian?suite=trixie&components=main&binaryArch=amd64
ARG JELLYFIN_FFMPEG_VERSION=8.1.3-1-trixie

USER 0
RUN --mount=type=bind,from=dhi.io/debian-base:trixie-dev@sha256:9e479c64f8d27f8fa368ef62804c2ddb9b3e43a68c13a200b2f668b4771b049e,target=/x \
    --mount=type=bind,source=docker,target=/docker \
    /x/bin/sh /docker/install-packages.sh "jellyfin-ffmpeg8=$JELLYFIN_FFMPEG_VERSION" \
 && mkdir -p /usr/local/bin /var/lib/ferrocine \
 && chown 65532:65532 /var/lib/ferrocine \
 && ln -s /usr/lib/jellyfin-ffmpeg/ffmpeg /usr/lib/jellyfin-ffmpeg/ffprobe /usr/local/bin/ \
 && ffmpeg -hide_banner -version | head -1

LABEL org.opencontainers.image.title="Ferrocine" \
      org.opencontainers.image.description="Self-hosted media server: read-only libraries, metadata in SQL, hardware transcoding, OpenID Connect sign-in" \
      org.opencontainers.image.source="https://github.com/sebastka/ferrocine" \
      org.opencontainers.image.licenses="MIT"

COPY --from=build --chmod=0755 /ferrocine /usr/local/bin/ferrocine

# Unprivileged. VA-API/QSV need /dev/dri passed in and the render group of
# the host added (`group_add`); see docs/deployment.md. Artwork, previews and
# downloaded subtitles go to /var/lib/ferrocine (`metadata.directory`): mount
# a volume there (a new named volume takes this directory's owner).
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

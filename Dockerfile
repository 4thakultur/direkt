# syntax=docker/dockerfile:1

# ---- Build ---------------------------------------------------------------
# Same Debian release as the runtime image, so the binary links against the
# same glibc.
FROM rust:1.96-trixie AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
COPY direkt.example.toml ./

# Cache mounts keep the crate registry and compiled dependencies between
# builds; the binary is copied out because the target cache is not part of
# the image.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/direkt /usr/local/bin/direkt

# ---- Runtime -------------------------------------------------------------
FROM debian:trixie-slim

LABEL org.opencontainers.image.title="Direkt" \
      org.opencontainers.image.description="Self-hostable direct download manager" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

# CA certificates for HTTPS downloads; nothing else is needed at runtime.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /usr/local/bin/direkt /usr/local/bin/direkt
# Default configuration with container paths (/data, /downloads). Mount your
# own file over it to change settings that only exist in the file.
COPY direkt.example.toml /etc/direkt/direkt.toml

# Non-root by default. Compose can run it as another UID/GID (`user:`); the
# mounted directories must then be writable by that user.
RUN mkdir -p /data /downloads \
 && chown 1000:1000 /data /downloads
USER 1000:1000

ENV DIREKT_CONFIG=/etc/direkt/direkt.toml
EXPOSE 9877

HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 \
    CMD ["direkt", "healthcheck"]

ENTRYPOINT ["direkt"]
CMD ["serve"]

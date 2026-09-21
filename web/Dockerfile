# ReOpenSCAD container image.
#
# Build context is `web/`, so everything the image needs lives under it — the
# shell's icon is our own `web/legacy-assets/icons/reopenscad.svg`, and nothing
# is pulled in from outside the context at build time.
#
#   docker build -t reopenscad web/
#
# Cloud Run runs x86-64 only, so a build on an Apple Silicon machine needs
# `docker buildx build --platform linux/amd64`. Cloud Build (see DEPLOY.md)
# already builds on x86-64 and is the recommended path.

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
FROM rust:1.88-slim-bookworm AS builder

# No extra system packages: the build below enables only the `postgres`
# feature, which is pure Rust. The optional `postgres-tls` feature is *not*
# built here — it pulls in `native-tls`/OpenSSL, which would need
# `pkg-config` and `libssl-dev` in this stage and the shared library in the
# distroless runtime. The Cloud Run path reaches Cloud SQL over a unix socket
# and needs no TLS of its own; see DEPLOY.md §3.
WORKDIR /src

# Dependencies first, against a stub source tree, so that editing the geometry
# engine does not invalidate the layer that compiled the Postgres driver.
COPY backend/Cargo.toml backend/Cargo.lock ./backend/
RUN mkdir -p backend/src \
    && echo 'fn main() {}' > backend/src/main.rs \
    && echo '' > backend/src/lib.rs \
    && cargo build --release --locked --features postgres --manifest-path backend/Cargo.toml \
    && rm -rf backend/src

COPY backend/src ./backend/src
# Cargo keys freshness on mtime; the stub `main.rs` above was newer than the
# real sources it is replaced with, so touch them to force a rebuild.
# `--locked` makes a Cargo.toml/Cargo.lock mismatch a build failure rather than
# a silent resolution to versions nobody tested.
RUN find backend/src -name '*.rs' -exec touch {} + \
    && cargo build --release --locked --features postgres --manifest-path backend/Cargo.toml

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
# distroless/cc carries glibc and libgcc and nothing else: no shell, no package
# manager, no setuid binaries. `:nonroot` runs as uid 65532.
FROM gcr.io/distroless/cc-debian12:nonroot

WORKDIR /app

COPY --from=builder /src/backend/target/release/reopenscad-server /app/reopenscad-server
# The browser shell. `handle_static` serves these from REOPENSCAD_WEB_ROOT.
COPY index.html app.js styles.css /app/web/
COPY legacy-assets /app/web/legacy-assets
# The kick-start catalogue: eighteen .scad sources, their preview renders and
# the manifest that lists them. Served straight out of the web root, so an
# image without them shows an empty-workspace panel with nothing in it.
COPY examples /app/web/examples

# `CARGO_MANIFEST_DIR` is baked in at compile time and points at the builder
# stage's `/src`, which does not exist here. This is what makes the static
# routes resolve inside the image.
ENV REOPENSCAD_WEB_ROOT=/app/web \
    # Cloud Run injects PORT; this is the documented default it injects, and
    # what the image listens on when run by hand.
    PORT=8080 \
    HOST=0.0.0.0 \
    # Only used when DATABASE_URL is unset. On Cloud Run /tmp is an in-memory
    # tmpfs that dies with the instance, so this fallback is for `docker run`
    # smoke tests, never for a real deployment.
    REOPENSCAD_DATA_DIR=/tmp/reopenscad-workspaces \
    # HOST is 0.0.0.0 above, and the server refuses to bind a non-loopback
    # address without knowing its own origin — it validates every request's
    # Host header against this, which is what stops DNS rebinding. A container
    # started without it would exit instead of listening, so the image carries
    # a placeholder that makes it *start*; requests are answered 403 until the
    # deployment overrides it with the real service URL. See DEPLOY.md §4.
    REOPENSCAD_PUBLIC_ORIGIN=http://localhost:8080

EXPOSE 8080
USER nonroot:nonroot

ENTRYPOINT ["/app/reopenscad-server"]

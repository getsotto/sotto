# Sotto sync server — multi-stage build.
#
#   docker build -t sotto-server .
#   docker run --rm -p 8080:8080 \
#     -e DATABASE_URL=postgres://sotto:sotto@host.docker.internal:5432/sotto \
#     -e SOTTO_PUBLIC_URL=https://api.example.com \
#     -e GITHUB_CLIENT_ID=... -e GITHUB_CLIENT_SECRET=... \
#     sotto-server
#
# Migrations are embedded at compile time (`sqlx::migrate!`) and applied on boot, so the runtime
# image carries only the binary. OAuth is optional: without GITHUB_CLIENT_ID/SECRET the server
# still serves health + sync, and the auth endpoints return 503.

FROM rust:1-bookworm AS build
WORKDIR /app
COPY . .
RUN cargo build --release -p sotto-server

FROM debian:bookworm-slim
# ca-certificates + libssl3: the OAuth client uses native-tls (system OpenSSL) to reach GitHub.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/sotto-server /usr/local/bin/sotto-server

# Bind on all interfaces inside the container; map the port at `docker run`.
ENV SOTTO_BIND=0.0.0.0:8080
EXPOSE 8080

# Run as a non-root user.
RUN useradd --system --no-create-home sotto
USER sotto

ENTRYPOINT ["sotto-server"]

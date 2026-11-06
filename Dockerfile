FROM rust:1.98-slim-bookworm AS build
WORKDIR /app

# Build dependencies in their own layer so source edits do not recompile them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY migrations ./migrations
COPY assets ./assets
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked --bin aduana \
    && mkdir /data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /app/target/release/aduana /usr/local/bin/aduana
# A new named volume inherits this ownership, so the non-root user can write.
COPY --from=build --chown=65532:65532 /data /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/aduana"]

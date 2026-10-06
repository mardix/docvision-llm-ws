# syntax=docker/dockerfile:1
# Static musl build (Alpine) on a distroless static base, running as non-root.
# Builds for the platform it runs on.
# The default build includes every feature except pdf-render (which needs the Pdfium library).
#   docker build -t docvision-llm-ws .
FROM rust:1.88-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock README.md ./
COPY src ./src
ARG FEATURES=""
RUN cargo build --release --locked ${FEATURES:+--features "$FEATURES"} \
 && mkdir -p /data

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/docvision-llm-ws /docvision-llm-ws
COPY --from=build --chown=65532:65532 /data /data
ENV DOCVISION_DATA_DIR=/data \
    DOCVISION_BIND=0.0.0.0:4242 \
    DOCVISION_LOG_FORMAT=json
VOLUME ["/data"]
EXPOSE 4242
USER 65532:65532
# Run with a read-only root filesystem: docker run --read-only -v docvision-data:/data ...
ENTRYPOINT ["/docvision-llm-ws"]

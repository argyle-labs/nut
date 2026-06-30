# TODO: base image + build for nut. Mirror jellyfin/Dockerfile conventions.
FROM debian:12-slim
LABEL org.opencontainers.image.source="https://github.com/argyle-labs/nut"
EXPOSE 3493

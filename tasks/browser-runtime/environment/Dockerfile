FROM alpine:3.22

RUN apk add --no-cache \
      ca-certificates \
      chromium \
      font-noto \
      mesa-dri-gallium \
      socat \
      xvfb-run \
    && adduser -D browser \
    && install -d -o browser -g browser /home/browser/profile

WORKDIR /home/browser

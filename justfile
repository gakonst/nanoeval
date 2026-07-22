set positional-arguments

check:
    cargo check

build: _sign

_build:
    cargo build

_sign: _build
    #!/bin/sh
    if [ "$(uname -s)" = "Darwin" ]; then
    codesign --entitlements nanoeval.entitlements --force --sign - target/debug/nanoeval
    fi

test:
    cargo test

lint:
    cargo clippy --all-targets -- -D warnings

run *args: build
    target/debug/nanoeval "$@"

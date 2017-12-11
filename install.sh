#!/usr/bin/env bash

if [ "$EUID" -eq 0 ]; then
    echo "Please DO NOT run as root"
    exit
fi

# Build librespot with minimal features
# Add our own audio sync
cargo build --release --no-default-features

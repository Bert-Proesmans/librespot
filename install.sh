#!/usr/bin/env bash

if [ "$EUID" -eq 0 ]; then
    echo "Please DO NOT run as root"
    exit
fi

sudo apt-get install -y build-essential portaudio19-dev

# Build librespot with minimal features
# Add our own audio sync
cargo build --release --no-default-features

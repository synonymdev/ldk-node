#!/usr/bin/env bash
set -euo pipefail

# Install gobley-uniffi-bindgen once for all Kotlin scripts
echo "Installing gobley-uniffi-bindgen fork..."
cargo install --git https://github.com/ovitrif/gobley.git \
  --branch fix-v0.2.0 gobley-uniffi-bindgen --force
export BINDGEN_GOBLEY_INSTALLED=1

# Standardize on release-smaller for all targets
export BINDGEN_PROFILE="release-smaller"

./scripts/uniffi_bindgen_generate.sh \
  && ./scripts/swift_create_xcframework_archive.sh

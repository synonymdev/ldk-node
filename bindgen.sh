#!/usr/bin/env bash
set -euo pipefail

source ./scripts/install_gobley_bindgen.sh

# Standardize on release-smaller for all targets
export BINDGEN_PROFILE="release-smaller"

./scripts/uniffi_bindgen_generate.sh \
  && ./scripts/swift_create_xcframework_archive.sh

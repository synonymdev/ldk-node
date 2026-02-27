#!/usr/bin/env bash
set -euo pipefail

RUSTFLAGS="--cfg no_download" cargo build \
  && ./scripts/uniffi_bindgen_generate.sh \
  && ./scripts/swift_create_xcframework_archive.sh \
  && sh scripts/uniffi_bindgen_generate_kotlin.sh \
  && sh scripts/uniffi_bindgen_generate_kotlin_android.sh

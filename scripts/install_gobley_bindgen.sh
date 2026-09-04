#!/usr/bin/env bash
# Pin gobley-uniffi-bindgen under target/ so other repos cannot overwrite ~/.cargo/bin.

GOBLEY_REV="36730a4219b2e8d06aa2c073936d6fc6a7f60e0f"
GOBLEY_ROOT="${PWD}/target/gobley-bindgen"
STAMP="${GOBLEY_ROOT}/rev"

if [ ! -x "${GOBLEY_ROOT}/bin/gobley-uniffi-bindgen" ] || [ "$(cat "${STAMP}" 2>/dev/null || true)" != "${GOBLEY_REV}" ]; then
	echo "Installing gobley-uniffi-bindgen ${GOBLEY_REV} into ${GOBLEY_ROOT}..."
	cargo install --git https://github.com/ovitrif/gobley.git --rev "${GOBLEY_REV}" --root "${GOBLEY_ROOT}" gobley-uniffi-bindgen --locked --force
	echo "${GOBLEY_REV}" > "${STAMP}"
fi

export PATH="${GOBLEY_ROOT}/bin:${PATH}"
export BINDGEN_GOBLEY_INSTALLED=1

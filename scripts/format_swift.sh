#!/bin/bash
set -euo pipefail

# Format generated Swift bindings. Requires: brew install swiftformat
if ! command -v swiftformat >/dev/null; then
	echo "swiftformat is required. Install with: brew install swiftformat" >&2
	exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SWIFT_FILE="$ROOT/bindings/swift/Sources/LDKNode/LDKNode.swift"

if [ ! -f "$SWIFT_FILE" ]; then
	echo "Generated Swift binding not found: $SWIFT_FILE" >&2
	exit 1
fi

swiftformat "$SWIFT_FILE"

#!/bin/bash
set -euo pipefail

SWIFT_BINDINGS_DIR="./bindings/swift"
IOS_DIST_DIR="./dist/ios"
XCFRAMEWORK_NAME="LDKNodeFFI.xcframework"
XCFRAMEWORK_PATH="$SWIFT_BINDINGS_DIR/$XCFRAMEWORK_NAME"
XCFRAMEWORK_ZIP_PATH="$IOS_DIST_DIR/$XCFRAMEWORK_NAME.zip"

rm -rf "$IOS_DIST_DIR"
mkdir -p "$IOS_DIST_DIR"
ARCHIVE_PATH="$PWD/$XCFRAMEWORK_ZIP_PATH"
find "$XCFRAMEWORK_PATH" -exec touch -t 198001010000 {} \;
(
    cd "$SWIFT_BINDINGS_DIR"
    find "$XCFRAMEWORK_NAME" -type f -print | LC_ALL=C sort | zip -X -q "$ARCHIVE_PATH" -@
) || exit 1
CHECKSUM=`swift package compute-checksum "$XCFRAMEWORK_ZIP_PATH"` || exit 1
echo "New checksum: $CHECKSUM" || exit 1
python3 ./scripts/swift_update_package_checksum.py --checksum "${CHECKSUM}" || exit 1

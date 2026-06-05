#!/bin/bash

BINDINGS_DIR="bindings/kotlin"
TARGET_DIR="target"
PROJECT_DIR="ldk-node-android"
ANDROID_LIB_DIR="$BINDINGS_DIR/$PROJECT_DIR"

# Install gobley-uniffi-bindgen from fork (skip if orchestrator already installed it)
if [ -z "${BINDGEN_GOBLEY_INSTALLED:-}" ]; then
	echo "Installing gobley-uniffi-bindgen fork..."
	cargo install --git https://github.com/ovitrif/gobley.git --branch fix-v0.2.0 gobley-uniffi-bindgen --force
fi
UNIFFI_BINDGEN_BIN="gobley-uniffi-bindgen"

export_variable_if_not_present() {
  local name="$1"
  local value="$2"

  # Check if the variable is already set
  if [ -z "${!name}" ]; then
    export "$name=$value"
    echo "Exported $name=$value"
  else
    echo "$name is already set to ${!name}, not exporting."
  fi
}

case "$OSTYPE" in
    linux-gnu)
      export_variable_if_not_present "ANDROID_NDK_ROOT" "/opt/android-ndk"
      export_variable_if_not_present "LLVM_ARCH_PATH" "linux-x86_64"
      ;;
    darwin*)
      export_variable_if_not_present "ANDROID_NDK_ROOT" "/opt/homebrew/share/android-ndk"
      export_variable_if_not_present "LLVM_ARCH_PATH" "darwin-x86_64"
      ;;
    *)
      echo "Unknown operating system: $OSTYPE"
      ;;
    esac

PATH="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/$LLVM_ARCH_PATH/bin:$PATH"

# Install the cargo-ndk version used by the mobile release scripts.
CARGO_NDK_VERSION="3.5.4"
if ! command -v cargo-ndk &> /dev/null || ! cargo ndk --version | grep -q "cargo-ndk $CARGO_NDK_VERSION"; then
    echo "Installing cargo-ndk $CARGO_NDK_VERSION..."
    cargo install cargo-ndk --version "$CARGO_NDK_VERSION" --locked --force
fi

# Add Android targets
echo "Adding Android targets..."
rustup target add x86_64-linux-android aarch64-linux-android armv7-linux-androideabi

# Build for all Android architectures with page size optimizations
echo "Building for Android architectures..."
JNI_LIB_DIR="$ANDROID_LIB_DIR/lib/src/main/jniLibs"
export CARGO_PROFILE_RELEASE_SMALLER_STRIP=false
export RUSTFLAGS="-C link-args=-Wl,-z,max-page-size=16384,-z,common-page-size=16384"
export CFLAGS="-D__ANDROID_MIN_SDK_VERSION__=21"

find_readelf() {
    if command -v llvm-readelf >/dev/null 2>&1; then
        command -v llvm-readelf
        return
    fi

    if command -v readelf >/dev/null 2>&1; then
        command -v readelf
        return
    fi

    echo "Error: llvm-readelf or readelf is required to validate Android native debug symbols"
    exit 1
}

has_debug_metadata() {
    for attempt in 1 2 3; do
        if "$READELF_BIN" -S "$1" | grep -Eq '\.(symtab|debug_|gnu_debugdata)'; then
            return 0
        fi

        sleep "$attempt"
    done

    "$READELF_BIN" -S "$1" | grep -E '\.(symtab|debug_|gnu_debugdata)' || true
    return 1
}

readelf_program_headers() {
    if "$READELF_BIN" -W -l "$1" >/dev/null 2>&1; then
        "$READELF_BIN" -W -l "$1"
        return
    fi

    "$READELF_BIN" -l "$1"
}

has_16kb_load_alignment() {
    alignments=$(readelf_program_headers "$1" | awk '$1 == "LOAD" { print $NF }')
    if [ -z "$alignments" ]; then
        return 1
    fi

    while read -r alignment; do
        if [ -z "$alignment" ]; then
            continue
        fi

        if [ "$((alignment))" -lt 16384 ]; then
            return 1
        fi
    done <<EOF
$alignments
EOF
}

validate_android_library() {
    lib="$1"
    if ! has_debug_metadata "$lib"; then
        echo "Error: Android native library has no usable debug metadata: $lib"
        exit 1
    fi

    if ! has_16kb_load_alignment "$lib"; then
        echo "Error: Android native library is not 16 KB page-size aligned: $lib"
        readelf_program_headers "$lib" | grep LOAD || true
        exit 1
    fi
}

validate_android_symbols() {
    READELF_BIN=$(find_readelf)

    for abi in armeabi-v7a arm64-v8a x86_64; do
        lib="$JNI_LIB_DIR/$abi/libldk_node.so"
        if [ ! -f "$lib" ]; then
            echo "Error: Android native library missing at $lib"
            exit 1
        fi

        validate_android_library "$lib"
    done
}

validate_android_aar_symbols() {
    READELF_BIN=$(find_readelf)
    aar=$(find "$ANDROID_LIB_DIR" -path '*/build/outputs/aar/*release.aar' -print | head -n 1)
    if [ -z "$aar" ]; then
        echo "Error: Android release AAR missing under $ANDROID_LIB_DIR"
        exit 1
    fi

    tmp_dir=$(mktemp -d)
    unzip -q "$aar" -d "$tmp_dir"

    for abi in armeabi-v7a arm64-v8a x86_64; do
        lib="$tmp_dir/jni/$abi/libldk_node.so"
        if [ ! -f "$lib" ]; then
            echo "Error: Android release AAR native library missing at $lib"
            rm -rf "$tmp_dir"
            exit 1
        fi

        validate_android_library "$lib"
    done

    rm -rf "$tmp_dir"
}

cargo ndk \
    -o "$JNI_LIB_DIR" \
    --no-strip \
    -t armeabi-v7a \
    -t arm64-v8a \
    -t x86_64 \
    build --profile release-smaller --features uniffi || exit 1

validate_android_symbols

# Clean up exported flags so they don't leak into subsequent scripts
# (e.g. the -z linker flags are Linux-only and break macOS builds)
unset CARGO_PROFILE_RELEASE_SMALLER_STRIP
unset RUSTFLAGS
unset CFLAGS

# Generate Kotlin bindings
echo "Generating Kotlin bindings..."
$UNIFFI_BINDGEN_BIN bindings/ldk_node.udl --lib-file $TARGET_DIR/aarch64-linux-android/release-smaller/libldk_node.so --config uniffi-android.toml -o "$ANDROID_LIB_DIR/lib/src" || exit 1

# Fix incorrect kotlinx.coroutines.IO import (removed in newer kotlinx.coroutines versions)
echo "Fixing Kotlin coroutines imports..."
KOTLIN_BINDINGS_FILE="$ANDROID_LIB_DIR/lib/src/main/kotlin/org/lightningdevkit/ldknode/ldk_node.android.kt"
sed -i.bak '/import kotlinx\.coroutines\.IO/d' "$KOTLIN_BINDINGS_FILE"
rm -f "$KOTLIN_BINDINGS_FILE.bak"

# Sync version from Cargo.toml
echo "Syncing version from Cargo.toml..."
CARGO_VERSION=$(grep '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/' | head -1)
sed -i.bak "s/^version=.*/version=$CARGO_VERSION/" "$ANDROID_LIB_DIR/gradle.properties"
rm -f "$ANDROID_LIB_DIR/gradle.properties.bak"
echo "Version synced: $CARGO_VERSION"

# Verify android library publish task graph
echo "Testing android library publish to Maven Local..."
$ANDROID_LIB_DIR/gradlew --project-dir "$ANDROID_LIB_DIR" clean publishToMavenLocal
validate_android_aar_symbols

echo "Android build process completed successfully!"

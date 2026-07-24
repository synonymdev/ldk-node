#!/bin/bash

set -e

BINDINGS_DIR="bindings/kotlin"
TARGET_DIR="target"
PROJECT_DIR="ldk-node-android"
ANDROID_LIB_DIR="$BINDINGS_DIR/$PROJECT_DIR"
NATIVE_DEBUG_SYMBOLS_ZIP="$ANDROID_LIB_DIR/native-debug-symbols.zip"

# Install gobley-uniffi-bindgen from fork (skip if orchestrator already installed it)
if [ -z "${BINDGEN_GOBLEY_INSTALLED:-}" ]; then
	echo "Installing gobley-uniffi-bindgen fork..."
	GOBLEY_REV="36730a4219b2e8d06aa2c073936d6fc6a7f60e0f"
	cargo install --git https://github.com/ovitrif/gobley.git --rev "$GOBLEY_REV" gobley-uniffi-bindgen --force
fi
UNIFFI_BINDGEN_BIN="gobley-uniffi-bindgen"

case "$OSTYPE" in
    linux-gnu)
      DEFAULT_ANDROID_NDK="/opt/android-ndk"
      LLVM_ARCH_PATH="${LLVM_ARCH_PATH:-linux-x86_64}"
      ;;
    darwin*)
      DEFAULT_ANDROID_NDK="/opt/homebrew/share/android-ndk"
      LLVM_ARCH_PATH="${LLVM_ARCH_PATH:-darwin-x86_64}"
      ;;
    *)
      echo "Unknown operating system: $OSTYPE"
      exit 1
      ;;
esac

if [ -n "${ANDROID_NDK_HOME:-}" ] &&
   [ -n "${ANDROID_NDK_ROOT:-}" ] &&
   [ "$ANDROID_NDK_HOME" != "$ANDROID_NDK_ROOT" ]; then
    echo "Error: ANDROID_NDK_HOME and ANDROID_NDK_ROOT select different NDKs"
    echo "ANDROID_NDK_HOME=$ANDROID_NDK_HOME"
    echo "ANDROID_NDK_ROOT=$ANDROID_NDK_ROOT"
    exit 1
fi

SELECTED_ANDROID_NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-$DEFAULT_ANDROID_NDK}}"
if [ ! -f "$SELECTED_ANDROID_NDK/source.properties" ]; then
    echo "Error: Android NDK source.properties missing at $SELECTED_ANDROID_NDK"
    exit 1
fi

export ANDROID_NDK_HOME="$SELECTED_ANDROID_NDK"
export ANDROID_NDK_ROOT="$SELECTED_ANDROID_NDK"
export LLVM_ARCH_PATH
echo "Using Android NDK from $SELECTED_ANDROID_NDK"
cat "$SELECTED_ANDROID_NDK/source.properties"

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
export CARGO_PROFILE_RELEASE_SMALLER_DEBUG=2
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

    for ndk_dir in "${ANDROID_NDK_ROOT:-}" "${ANDROID_NDK_HOME:-}" "${NDK_HOME:-}"; do
        if [ -z "$ndk_dir" ] || [ ! -d "$ndk_dir/toolchains/llvm/prebuilt" ]; then
            continue
        fi

        ndk_readelf=$(find "$ndk_dir/toolchains/llvm/prebuilt" -path '*/bin/llvm-readelf' | head -n 1)
        if [ -n "$ndk_readelf" ]; then
            echo "$ndk_readelf"
            return
        fi
    done

    echo "Error: llvm-readelf or readelf is required to validate Android native debug symbols"
    exit 1
}

find_strip() {
    if command -v llvm-strip >/dev/null 2>&1; then
        command -v llvm-strip
        return
    fi

    for ndk_dir in "${ANDROID_NDK_ROOT:-}" "${ANDROID_NDK_HOME:-}" "${NDK_HOME:-}"; do
        if [ -z "$ndk_dir" ] || [ ! -d "$ndk_dir/toolchains/llvm/prebuilt" ]; then
            continue
        fi

        ndk_strip=$(find "$ndk_dir/toolchains/llvm/prebuilt" -path '*/bin/llvm-strip' | head -n 1)
        if [ -n "$ndk_strip" ]; then
            echo "$ndk_strip"
            return
        fi
    done

    echo "Error: llvm-strip is required to strip Android native release libraries"
    exit 1
}

has_dwarf_debug_metadata() {
    local sections

    for attempt in 1 2 3; do
        sections=$("$READELF_BIN" -S "$1")
        case "$sections" in
            *".debug_info"*) return 0 ;;
        esac

        sleep "$attempt"
    done

    printf '%s\n' "$sections" | grep -E '\.debug_info' || true
    return 1
}

has_dwarf_sections() {
    local sections
    sections=$("$READELF_BIN" -S "$1")
    case "$sections" in
        *".debug_"*) return 0 ;;
        *) return 1 ;;
    esac
}

readelf_program_headers() {
    if "$READELF_BIN" -W -l "$1" >/dev/null 2>&1; then
        "$READELF_BIN" -W -l "$1"
        return
    fi

    "$READELF_BIN" -l "$1"
}

has_16kb_elf_alignment() {
    DETECTED_LOAD_ALIGNMENTS=""
    DETECTED_RELRO_ENDS=""
    program_headers=$(readelf_program_headers "$1")
    DETECTED_LOAD_ALIGNMENTS=$(printf '%s\n' "$program_headers" | awk '$1 == "LOAD" { print $NF }')
    if [ -z "$DETECTED_LOAD_ALIGNMENTS" ]; then
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
$DETECTED_LOAD_ALIGNMENTS
EOF

    relro_segments=$(printf '%s\n' "$program_headers" | awk '$1 == "GNU_RELRO" { print $3, $6 }')
    if [ -z "$relro_segments" ]; then
        return 1
    fi

    while read -r virtual_address memory_size; do
        if [ -z "$virtual_address" ] || [ -z "$memory_size" ]; then
            continue
        fi

        relro_end=$((virtual_address + memory_size))
        formatted_relro_end=$(printf '0x%x' "$relro_end")
        DETECTED_RELRO_ENDS="${DETECTED_RELRO_ENDS:+$DETECTED_RELRO_ENDS }$formatted_relro_end"
        if [ "$((relro_end % 16384))" -ne 0 ]; then
            return 1
        fi
    done <<EOF
$relro_segments
EOF

    [ -n "$DETECTED_RELRO_ENDS" ]
}

validate_android_library() {
    abi="$1"
    lib="$2"
    if ! has_dwarf_debug_metadata "$lib"; then
        echo "Error: Android native library has no .debug_info DWARF metadata: ABI=$abi library=$lib"
        exit 1
    fi

    if ! has_16kb_elf_alignment "$lib"; then
        echo "Error: Android native library is not 16 KB page-size compatible: ABI=$abi library=$lib LOAD=${DETECTED_LOAD_ALIGNMENTS:-missing} GNU_RELRO_end=${DETECTED_RELRO_ENDS:-missing}"
        readelf_program_headers "$lib" | grep -E 'LOAD|GNU_RELRO' || true
        exit 1
    fi
}

validate_stripped_android_library() {
    abi="$1"
    lib="$2"
    if has_dwarf_sections "$lib"; then
        echo "Error: Android release native library still contains .debug_* sections: ABI=$abi library=$lib"
        exit 1
    fi

    if ! has_16kb_elf_alignment "$lib"; then
        echo "Error: Android native library is not 16 KB page-size compatible: ABI=$abi library=$lib LOAD=${DETECTED_LOAD_ALIGNMENTS:-missing} GNU_RELRO_end=${DETECTED_RELRO_ENDS:-missing}"
        readelf_program_headers "$lib" | grep -E 'LOAD|GNU_RELRO' || true
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

        validate_android_library "$abi" "$lib"
    done
}

create_native_debug_symbols_archive() {
    tmp_dir=$(mktemp -d)

    for abi in armeabi-v7a arm64-v8a x86_64; do
        mkdir -p "$tmp_dir/$abi"
        cp "$JNI_LIB_DIR/$abi/libldk_node.so" "$tmp_dir/$abi/"
    done

    rm -f "$NATIVE_DEBUG_SYMBOLS_ZIP"
    archive_path="$PWD/$NATIVE_DEBUG_SYMBOLS_ZIP"
    if ! (
        cd "$tmp_dir"
        zip -qr "$archive_path" armeabi-v7a arm64-v8a x86_64
    ); then
        rm -rf "$tmp_dir"
        exit 1
    fi
    if ! zip -T "$NATIVE_DEBUG_SYMBOLS_ZIP" >/dev/null; then
        rm -rf "$tmp_dir"
        exit 1
    fi
    rm -rf "$tmp_dir"
}

strip_android_libraries() {
    STRIP_BIN=$(find_strip)

    for abi in armeabi-v7a arm64-v8a x86_64; do
        "$STRIP_BIN" --strip-unneeded "$JNI_LIB_DIR/$abi/libldk_node.so"
    done
}

validate_stripped_android_symbols() {
    READELF_BIN=$(find_readelf)

    for abi in armeabi-v7a arm64-v8a x86_64; do
        validate_stripped_android_library "$abi" "$JNI_LIB_DIR/$abi/libldk_node.so"
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
    done

    native_list="$tmp_dir/native-libraries.txt"
    find "$tmp_dir/jni" -type f -name '*.so' -print > "$native_list"
    if [ ! -s "$native_list" ]; then
        echo "Error: Android release AAR contains no native libraries"
        rm -rf "$tmp_dir"
        exit 1
    fi

    while IFS= read -r lib; do
        relative_path=${lib#"$tmp_dir/jni/"}
        abi=${relative_path%%/*}
        validate_stripped_android_library "$abi" "$lib"
    done < "$native_list"

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
create_native_debug_symbols_archive
strip_android_libraries
validate_stripped_android_symbols

# Clean up exported flags so they don't leak into subsequent scripts
# (e.g. the -z linker flags are Linux-only and break macOS builds)
unset CARGO_PROFILE_RELEASE_SMALLER_STRIP
unset CARGO_PROFILE_RELEASE_SMALLER_DEBUG
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

echo "Normalizing generated Kotlin whitespace..."
find "$ANDROID_LIB_DIR/lib/src/main/kotlin" -name "*.kt" -exec perl -0pi -e 's/[ \t]+(?=\n)//g; s/[ \t]+\z//; s/\n+\z/\n/; $_ .= "\n" unless /\n\z/' {} \;

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

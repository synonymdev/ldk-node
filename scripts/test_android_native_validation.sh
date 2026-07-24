#!/bin/bash

set -e

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck disable=SC1091
. "$SCRIPT_DIR/android_native_validation.sh"

TEST_DIR=$(mktemp -d)
trap 'rm -rf "$TEST_DIR"' EXIT

write_elf_header() {
    perl -e '
        my ($path, $class, $machine) = @ARGV;
        open my $file, ">:raw", $path or die "$path: $!";
        print {$file} pack("C*", 0x7f, 0x45, 0x4c, 0x46, $class, 1, (0) x 12, $machine & 0xff, $machine >> 8);
    ' "$1" "$2" "$3"
}

for fixture in \
    "armeabi-v7a 1 40" \
    "arm64-v8a 2 183" \
    "x86 1 3" \
    "x86_64 2 62"
do
    read -r abi elf_class elf_machine <<< "$fixture"
    write_elf_header "$TEST_DIR/$abi.so" "$elf_class" "$elf_machine"
    has_matching_android_elf_abi "$abi" "$TEST_DIR/$abi.so"
done

if has_matching_android_elf_abi arm64-v8a "$TEST_DIR/x86_64.so"; then
    echo "Expected swapped ABI fixture to fail"
    exit 1
fi
printf '\177ELF' > "$TEST_DIR/truncated.so"
if has_matching_android_elf_abi arm64-v8a "$TEST_DIR/truncated.so"; then
    echo "Expected truncated ELF fixture to fail"
    exit 1
fi

cat > "$TEST_DIR/readelf" <<'EOF'
#!/bin/bash
library="${!#}"
case "$1" in
    -S)
        case "$library" in
            *zdebug*) echo ".zdebug_info" ;;
            *debug*) echo ".debug_info" ;;
            *symtab*) echo ".symtab" ;;
            *) echo ".dynsym" ;;
        esac
        ;;
    -W)
        case "$library" in
            *readelf-fail*) exit 1 ;;
        esac
        case "$library" in
            *bad-load*) load_alignment=0x1000 ;;
            *) load_alignment=0x4000 ;;
        esac
        case "$library" in
            *bad-relro*) relro_address=0x7100 ;;
            *) relro_address=0x7000 ;;
        esac
        case "$library" in
            *no-load*) ;;
            *) echo "LOAD 0x0 0x0 0x0 0x100 0x100 R E $load_alignment" ;;
        esac
        echo "GNU_RELRO 0x0 $relro_address 0x0 0x0 0x1000 R 0x1"
        ;;
esac
EOF
chmod +x "$TEST_DIR/readelf"
export READELF_BIN="$TEST_DIR/readelf"

cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/valid.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/debug.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/zdebug.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/symtab.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/bad-load.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/bad-relro.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/no-load.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/readelf-fail.so"

has_unstripped_sections "$TEST_DIR/debug.so"
has_unstripped_sections "$TEST_DIR/zdebug.so"
has_unstripped_sections "$TEST_DIR/symtab.so"
if has_unstripped_sections "$TEST_DIR/valid.so"; then
    echo "Expected stripped fixture to pass"
    exit 1
fi

has_16kb_elf_alignment "$TEST_DIR/valid.so"
for invalid in bad-load bad-relro no-load readelf-fail; do
    if has_16kb_elf_alignment "$TEST_DIR/$invalid.so"; then
        echo "Expected $invalid alignment fixture to fail"
        exit 1
    fi
done

validate_android_library arm64-v8a "$TEST_DIR/debug.so"
validate_stripped_android_library arm64-v8a "$TEST_DIR/valid.so"
if validate_stripped_android_library arm64-v8a "$TEST_DIR/debug.so" >/dev/null; then
    echo "Expected unstripped release fixture to fail"
    exit 1
fi
if validate_stripped_android_library arm64-v8a "$TEST_DIR/x86_64.so" >/dev/null; then
    echo "Expected wrong-ABI release fixture to fail"
    exit 1
fi

create_required_aar_tree() {
    local root="$1"

    mkdir -p "$root/jni/armeabi-v7a" "$root/jni/arm64-v8a" "$root/jni/x86_64"
    write_elf_header "$root/jni/armeabi-v7a/libldk_node.so" 1 40
    write_elf_header "$root/jni/arm64-v8a/libldk_node.so" 2 183
    write_elf_header "$root/jni/x86_64/libldk_node.so" 2 62
}

create_aar() {
    local root="$1"
    local aar="$2"

    (
        cd "$root"
        zip -qr "$aar" jni
    )
}

valid_aar_root="$TEST_DIR/valid-aar"
create_required_aar_tree "$valid_aar_root"
mkdir -p "$valid_aar_root/jni/x86"
write_elf_header "$valid_aar_root/jni/x86/libextra.so" 1 3
create_aar "$valid_aar_root" "$TEST_DIR/valid.aar"
validate_android_aar_symbols "$TEST_DIR/valid.aar"

missing_aar_root="$TEST_DIR/missing-aar"
mkdir -p "$missing_aar_root/jni/arm64-v8a" "$missing_aar_root/jni/x86_64"
write_elf_header "$missing_aar_root/jni/arm64-v8a/libldk_node.so" 2 183
write_elf_header "$missing_aar_root/jni/x86_64/libldk_node.so" 2 62
create_aar "$missing_aar_root" "$TEST_DIR/missing.aar"
if validate_android_aar_symbols "$TEST_DIR/missing.aar" >/dev/null; then
    echo "Expected missing-required-library AAR fixture to fail"
    exit 1
fi

malformed_aar_root="$TEST_DIR/malformed-aar"
create_required_aar_tree "$malformed_aar_root"
mkdir -p "$malformed_aar_root/jni/arm64-v8a/nested"
write_elf_header "$malformed_aar_root/jni/arm64-v8a/nested/libbad.so" 2 183
create_aar "$malformed_aar_root" "$TEST_DIR/malformed.aar"
if validate_android_aar_symbols "$TEST_DIR/malformed.aar" >/dev/null; then
    echo "Expected malformed-native-path AAR fixture to fail"
    exit 1
fi

unknown_abi_root="$TEST_DIR/unknown-abi-aar"
create_required_aar_tree "$unknown_abi_root"
mkdir -p "$unknown_abi_root/jni/unknown"
write_elf_header "$unknown_abi_root/jni/unknown/libextra.so" 2 62
create_aar "$unknown_abi_root" "$TEST_DIR/unknown-abi.aar"
if validate_android_aar_symbols "$TEST_DIR/unknown-abi.aar" >/dev/null; then
    echo "Expected unknown-ABI AAR fixture to fail"
    exit 1
fi

echo "Android native validation regression fixtures passed"

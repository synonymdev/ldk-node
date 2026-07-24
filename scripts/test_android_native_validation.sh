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
            *fallback*) exit 1 ;;
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
    -l)
        echo "LOAD 0x0 0x0 0x0 0x100 0x100 R E 0x4000"
        echo "GNU_RELRO 0x0 0x7000 0x0 0x0 0x1000 R 0x1"
        ;;
esac
EOF
chmod +x "$TEST_DIR/readelf"
export READELF_BIN="$TEST_DIR/readelf"

cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/valid.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/debug.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/zdebug.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/symtab.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/fallback.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/bad-load.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/bad-relro.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/no-load.so"

has_unstripped_sections "$TEST_DIR/debug.so"
has_unstripped_sections "$TEST_DIR/zdebug.so"
has_unstripped_sections "$TEST_DIR/symtab.so"
if has_unstripped_sections "$TEST_DIR/valid.so"; then
    echo "Expected stripped fixture to pass"
    exit 1
fi

has_16kb_elf_alignment "$TEST_DIR/valid.so"
has_16kb_elf_alignment "$TEST_DIR/fallback.so"
for invalid in bad-load bad-relro no-load; do
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

echo "Android native validation regression fixtures passed"

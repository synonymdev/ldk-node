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
    -W)
        if [ "$2" = "-S" ]; then
            case "$library" in
                *section-inspection-recovers*)
                    recovery_counter="${library}.readelf-count"
                    recovery_count=0
                    if [ -f "$recovery_counter" ]; then
                        recovery_count=$(cat "$recovery_counter")
                    fi
                    echo $((recovery_count + 1)) > "$recovery_counter"
                    if [ "$recovery_count" -eq 0 ]; then
                        echo ".debug_info PROGBITS"
                        exit 1
                    fi
                    echo ".debug_info PROGBITS"
                    exit 0
                    ;;
                *section-inspection-partial*) echo ".debug_info PROGBITS"; exit 1 ;;
                *section-inspection-fail*) exit 1 ;;
            esac
            case "$library" in
                *zdebug*) echo ".zdebug_info PROGBITS" ;;
                *debug*) echo ".debug_info PROGBITS" ;;
                *renamed-symtab*) echo ".stripped_symbols SYMTAB" ;;
                *symtab*) echo ".symtab SYMTAB" ;;
                *) echo ".dynsym DYNSYM" ;;
            esac
            exit
        fi
        case "$library" in
            *program-inspection-partial*)
                echo "LOAD 0x0 0x0 0x0 0x100 0x100 R E 0x4000"
                echo "GNU_RELRO 0x0 0x7000 0x0 0x0 0x1000 R 0x1"
                exit 1
                ;;
            *readelf-fail*) exit 1 ;;
        esac
        case "$library" in
            *bad-load*) load_alignment=0x1000 ;;
            *malformed-load*) load_alignment=invalid ;;
            *too-large-load*) load_alignment=0x8000000000000000 ;;
            *) load_alignment=0x4000 ;;
        esac
        case "$library" in
            *bad-relro*) relro_address=0x7100 ;;
            *malformed-relro*) relro_address=invalid ;;
            *overflow-relro*) relro_address=0x7ffffffffffff000 ;;
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
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/renamed-symtab.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/bad-load.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/bad-relro.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/malformed-load.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/malformed-relro.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/too-large-load.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/overflow-relro.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/no-load.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/readelf-fail.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/section-inspection-fail.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/section-inspection-partial.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/section-inspection-recovers.so"
cp "$TEST_DIR/arm64-v8a.so" "$TEST_DIR/program-inspection-partial.so"

has_unstripped_sections "$TEST_DIR/debug.so"
has_unstripped_sections "$TEST_DIR/zdebug.so"
has_unstripped_sections "$TEST_DIR/symtab.so"
has_unstripped_sections "$TEST_DIR/renamed-symtab.so"
if has_unstripped_sections "$TEST_DIR/valid.so"; then
    echo "Expected stripped fixture to pass"
    exit 1
fi

has_16kb_elf_alignment "$TEST_DIR/valid.so"
for invalid in \
    bad-load \
    bad-relro \
    malformed-load \
    malformed-relro \
    too-large-load \
    overflow-relro \
    no-load \
    readelf-fail \
    program-inspection-partial
do
    if has_16kb_elf_alignment "$TEST_DIR/$invalid.so"; then
        echo "Expected $invalid alignment fixture to fail"
        exit 1
    fi
done
(
    # shellcheck disable=SC2329
    awk() {
        command awk "$@"
        return 1
    }
    set +e
    has_16kb_elf_alignment "$TEST_DIR/valid.so"
    parser_status=$?
    if [ "$parser_status" -ne 2 ]; then
        echo "Expected program-header parser failure to return inspection status 2"
        exit 1
    fi
)

validate_android_library arm64-v8a "$TEST_DIR/debug.so"
validate_stripped_android_library arm64-v8a "$TEST_DIR/valid.so"
if has_dwarf_debug_metadata "$TEST_DIR/section-inspection-partial.so"; then
    echo "Expected partial section-inspection fixture to fail"
    exit 1
fi
if recovery_output=$(
    validate_android_library arm64-v8a "$TEST_DIR/section-inspection-recovers.so" 2>&1
); then
    echo "Expected recovering section-inspection fixture to fail on its first tool error"
    exit 1
fi
case "$recovery_output" in
    *"Unable to inspect Android native library sections"*) ;;
    *)
        echo "Recovering section-inspection fixture failed for an unexpected reason: $recovery_output"
        exit 1
        ;;
esac
if [ "$(cat "$TEST_DIR/section-inspection-recovers.so.readelf-count")" -ne 1 ]; then
    echo "Expected section inspection to stop after the first tool error"
    exit 1
fi
if validate_stripped_android_library arm64-v8a "$TEST_DIR/debug.so" >/dev/null; then
    echo "Expected unstripped release fixture to fail"
    exit 1
fi
if validate_stripped_android_library arm64-v8a "$TEST_DIR/x86_64.so" >/dev/null; then
    echo "Expected wrong-ABI release fixture to fail"
    exit 1
fi
if section_failure_output=$(
    validate_stripped_android_library arm64-v8a "$TEST_DIR/section-inspection-fail.so" 2>&1
); then
    echo "Expected section-inspection-failure release fixture to fail"
    exit 1
fi
case "$section_failure_output" in
    *"Unable to inspect Android native library sections"*) ;;
    *)
        echo "Section-inspection fixture failed for an unexpected reason: $section_failure_output"
        exit 1
        ;;
esac
if partial_section_output=$(
    validate_android_library arm64-v8a "$TEST_DIR/section-inspection-partial.so" 2>&1
); then
    echo "Expected partial section-inspection pre-strip fixture to fail"
    exit 1
fi
case "$partial_section_output" in
    *"Unable to inspect Android native library sections"*) ;;
    *)
        echo "Partial section-inspection fixture failed for an unexpected reason: $partial_section_output"
        exit 1
        ;;
esac
if partial_stripped_section_output=$(
    validate_stripped_android_library arm64-v8a "$TEST_DIR/section-inspection-partial.so" 2>&1
); then
    echo "Expected partial section-inspection stripped fixture to fail"
    exit 1
fi
case "$partial_stripped_section_output" in
    *"Unable to inspect Android native library sections"*) ;;
    *)
        echo "Partial stripped section-inspection fixture failed for an unexpected reason: $partial_stripped_section_output"
        exit 1
        ;;
esac
if partial_program_output=$(
    validate_stripped_android_library arm64-v8a "$TEST_DIR/program-inspection-partial.so" 2>&1
); then
    echo "Expected partial program-header inspection fixture to fail"
    exit 1
fi
case "$partial_program_output" in
    *"Unable to inspect Android native library program headers"*) ;;
    *)
        echo "Partial program-header fixture failed for an unexpected reason: $partial_program_output"
        exit 1
        ;;
esac

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

REAL_UNZIP_BIN=$(command -v unzip)
export REAL_UNZIP_BIN
cat > "$TEST_DIR/unzip" <<'EOF'
#!/bin/bash
archive="${2:-}"
case "$1:$archive" in
    -Z1:*archive-list-partial.aar)
        "$REAL_UNZIP_BIN" "$@"
        exit 1
        ;;
    -p:*archive-extraction-partial.aar)
        "$REAL_UNZIP_BIN" "$@"
        exit 1
        ;;
esac
exec "$REAL_UNZIP_BIN" "$@"
EOF
chmod +x "$TEST_DIR/unzip"
PATH="$TEST_DIR:$PATH"
export PATH

run_production_aar_validation() (
    validate_android_aar_symbols "$1" || exit 1
)

cp "$TEST_DIR/valid.aar" "$TEST_DIR/archive-list-partial.aar"
if list_partial_output=$(
    run_production_aar_validation "$TEST_DIR/archive-list-partial.aar" 2>&1
); then
    echo "Expected partial archive-listing fixture to fail"
    exit 1
fi
case "$list_partial_output" in
    *"Unable to enumerate Android release AAR entries"*) ;;
    *)
        echo "Partial archive-listing fixture failed for an unexpected reason: $list_partial_output"
        exit 1
        ;;
esac

cp "$TEST_DIR/valid.aar" "$TEST_DIR/archive-extraction-partial.aar"
if extraction_partial_output=$(
    run_production_aar_validation "$TEST_DIR/archive-extraction-partial.aar" 2>&1
); then
    echo "Expected partial archive-extraction fixture to fail"
    exit 1
fi
case "$extraction_partial_output" in
    *"Unable to extract Android release AAR native library"*) ;;
    *)
        echo "Partial archive-extraction fixture failed for an unexpected reason: $extraction_partial_output"
        exit 1
        ;;
esac

cp "$TEST_DIR/valid.aar" "$TEST_DIR/corrupt.aar"
python3 - "$TEST_DIR/corrupt.aar" <<'PY'
import pathlib
import sys

archive = pathlib.Path(sys.argv[1])
archive.write_bytes(archive.read_bytes()[:-16])
PY
if corrupt_output=$(
    run_production_aar_validation "$TEST_DIR/corrupt.aar" 2>&1
); then
    echo "Expected corrupt archive fixture to fail"
    exit 1
fi
case "$corrupt_output" in
    *"failed archive integrity validation"*) ;;
    *)
        echo "Corrupt archive fixture failed for an unexpected reason: $corrupt_output"
        exit 1
        ;;
esac

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

python3 - "$valid_aar_root" "$TEST_DIR/duplicate.aar" <<'PY'
import pathlib
import sys
import warnings
import zipfile

root = pathlib.Path(sys.argv[1])
with warnings.catch_warnings():
    warnings.simplefilter("ignore", UserWarning)
    with zipfile.ZipFile(sys.argv[2], "w") as archive:
        for path in sorted(root.rglob("*")):
            if path.is_file():
                archive.write(path, path.relative_to(root))
        archive.write(
            root / "jni/x86_64/libldk_node.so",
            "jni/arm64-v8a/libldk_node.so",
        )
PY
if duplicate_output=$(validate_android_aar_symbols "$TEST_DIR/duplicate.aar" 2>&1); then
    echo "Expected duplicate-native-entry AAR fixture to fail"
    exit 1
fi
case "$duplicate_output" in
    *"duplicate native library entry"*) ;;
    *)
        echo "Duplicate AAR fixture failed for an unexpected reason: $duplicate_output"
        exit 1
        ;;
esac

section_failure_aar_root="$TEST_DIR/section-inspection-fail-aar"
create_required_aar_tree "$section_failure_aar_root"
cp \
    "$TEST_DIR/section-inspection-fail.so" \
    "$section_failure_aar_root/jni/arm64-v8a/libsection-inspection-fail.so"
create_aar "$section_failure_aar_root" "$TEST_DIR/section-inspection-fail.aar"
if section_aar_output=$(
    validate_android_aar_symbols "$TEST_DIR/section-inspection-fail.aar" 2>&1
); then
    echo "Expected section-inspection-failure AAR fixture to fail"
    exit 1
fi
case "$section_aar_output" in
    *"Unable to inspect Android native library sections"*) ;;
    *)
        echo "Section-inspection AAR fixture failed for an unexpected reason: $section_aar_output"
        exit 1
        ;;
esac

partial_section_aar_root="$TEST_DIR/section-inspection-partial-aar"
create_required_aar_tree "$partial_section_aar_root"
cp \
    "$TEST_DIR/section-inspection-partial.so" \
    "$partial_section_aar_root/jni/arm64-v8a/libsection-inspection-partial.so"
create_aar "$partial_section_aar_root" "$TEST_DIR/section-inspection-partial.aar"
if partial_section_aar_output=$(
    validate_android_aar_symbols "$TEST_DIR/section-inspection-partial.aar" 2>&1
); then
    echo "Expected partial section-inspection AAR fixture to fail"
    exit 1
fi
case "$partial_section_aar_output" in
    *"Unable to inspect Android native library sections"*) ;;
    *)
        echo "Partial section-inspection AAR fixture failed for an unexpected reason: $partial_section_aar_output"
        exit 1
        ;;
esac

partial_program_aar_root="$TEST_DIR/program-inspection-partial-aar"
create_required_aar_tree "$partial_program_aar_root"
cp \
    "$TEST_DIR/program-inspection-partial.so" \
    "$partial_program_aar_root/jni/arm64-v8a/libprogram-inspection-partial.so"
create_aar "$partial_program_aar_root" "$TEST_DIR/program-inspection-partial.aar"
if partial_program_aar_output=$(
    validate_android_aar_symbols "$TEST_DIR/program-inspection-partial.aar" 2>&1
); then
    echo "Expected partial program-header inspection AAR fixture to fail"
    exit 1
fi
case "$partial_program_aar_output" in
    *"Unable to inspect Android native library program headers"*) ;;
    *)
        echo "Partial program-header AAR fixture failed for an unexpected reason: $partial_program_aar_output"
        exit 1
        ;;
esac

echo "Android native validation regression fixtures passed"

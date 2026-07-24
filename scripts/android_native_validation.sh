#!/bin/bash

# Shared Android ELF validation helpers. The caller supplies READELF_BIN.

read_android_elf_identity() {
    local header
    local -a header_bytes

    DETECTED_ELF_CLASS=""
    DETECTED_ELF_MACHINE=""
    header=$(od -An -v -t u1 -N20 "$1") || return 1
    read -r -a header_bytes <<< "${header//$'\n'/ }"
    if [ "${#header_bytes[@]}" -lt 20 ] ||
       [ "${header_bytes[0]}" -ne 127 ] || [ "${header_bytes[1]}" -ne 69 ] ||
       [ "${header_bytes[2]}" -ne 76 ] || [ "${header_bytes[3]}" -ne 70 ] ||
       [ "${header_bytes[5]}" -ne 1 ]; then
        return 1
    fi

    DETECTED_ELF_CLASS="${header_bytes[4]}"
    DETECTED_ELF_MACHINE=$((header_bytes[18] + (header_bytes[19] * 256)))
}

has_matching_android_elf_abi() {
    local abi="$1"
    local library="$2"
    local expected_class
    local expected_machine

    case "$abi" in
        armeabi-v7a) expected_class=1; expected_machine=40 ;;
        arm64-v8a) expected_class=2; expected_machine=183 ;;
        x86) expected_class=1; expected_machine=3 ;;
        x86_64) expected_class=2; expected_machine=62 ;;
        *) return 1 ;;
    esac

    read_android_elf_identity "$library" &&
        [ "$DETECTED_ELF_CLASS" -eq "$expected_class" ] &&
        [ "$DETECTED_ELF_MACHINE" -eq "$expected_machine" ]
}

has_dwarf_debug_metadata() {
    local sections
    local successful_sections=""
    local inspection_succeeded=0
    local attempt

    for attempt in 1 2 3; do
        if sections=$("$READELF_BIN" -W -S "$1"); then
            inspection_succeeded=1
            successful_sections="$sections"
            case "$sections" in
                *".debug_info"*) return 0 ;;
            esac
        fi

        if [ "$attempt" -lt 3 ]; then
            sleep "$attempt"
        fi
    done

    if [ "$inspection_succeeded" -eq 0 ]; then
        return 2
    fi

    printf '%s\n' "$successful_sections" | grep -E '\.debug_info' || true
    return 1
}

has_unstripped_sections() {
    local sections
    if ! sections=$("$READELF_BIN" -W -S "$1"); then
        return 2
    fi
    case "$sections" in
        *".debug_"*|*".zdebug_"*) return 0 ;;
    esac

    printf '%s\n' "$sections" |
        awk '{
            for (field = 1; field <= NF; field++) {
                if ($field == "SYMTAB") {
                    found = 1
                }
            }
        }
        END { exit !found }'
}

readelf_program_headers() {
    "$READELF_BIN" -W -l "$1"
}

has_16kb_elf_alignment() {
    local program_headers
    local alignment
    local relro_segments
    local virtual_address
    local memory_size
    local relro_end
    local formatted_relro_end

    DETECTED_LOAD_ALIGNMENTS=""
    DETECTED_RELRO_ENDS=""
    if ! program_headers=$(readelf_program_headers "$1"); then
        return 2
    fi
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
    local abi="$1"
    local library="$2"
    local debug_status
    local alignment_status

    if ! has_matching_android_elf_abi "$abi" "$library"; then
        echo "Error: Android native library ELF identity does not match its ABI directory: ABI=$abi library=$library ELF_class=${DETECTED_ELF_CLASS:-unknown} ELF_machine=${DETECTED_ELF_MACHINE:-unknown}"
        return 1
    fi

    if has_dwarf_debug_metadata "$library"; then
        :
    else
        debug_status=$?
        if [ "$debug_status" -eq 2 ]; then
            echo "Error: Unable to inspect Android native library sections: ABI=$abi library=$library"
        else
            echo "Error: Android native library has no .debug_info DWARF metadata: ABI=$abi library=$library"
        fi
        return 1
    fi

    if has_16kb_elf_alignment "$library"; then
        :
    else
        alignment_status=$?
        if [ "$alignment_status" -eq 2 ]; then
            echo "Error: Unable to inspect Android native library program headers: ABI=$abi library=$library"
        else
            echo "Error: Android native library is not 16 KB page-size compatible: ABI=$abi library=$library LOAD=${DETECTED_LOAD_ALIGNMENTS:-missing} GNU_RELRO_end=${DETECTED_RELRO_ENDS:-missing}"
        fi
        readelf_program_headers "$library" | grep -E 'LOAD|GNU_RELRO' || true
        return 1
    fi
}

validate_stripped_android_library() {
    local abi="$1"
    local library="$2"
    local section_status
    local alignment_status

    if ! has_matching_android_elf_abi "$abi" "$library"; then
        echo "Error: Android native library ELF identity does not match its ABI directory: ABI=$abi library=$library ELF_class=${DETECTED_ELF_CLASS:-unknown} ELF_machine=${DETECTED_ELF_MACHINE:-unknown}"
        return 1
    fi

    if has_unstripped_sections "$library"; then
        echo "Error: Android release native library contains debug metadata or an SHT_SYMTAB section: ABI=$abi library=$library"
        return 1
    else
        section_status=$?
        if [ "$section_status" -ne 1 ]; then
            echo "Error: Unable to inspect Android native library sections: ABI=$abi library=$library"
            return 1
        fi
    fi

    if has_16kb_elf_alignment "$library"; then
        :
    else
        alignment_status=$?
        if [ "$alignment_status" -eq 2 ]; then
            echo "Error: Unable to inspect Android native library program headers: ABI=$abi library=$library"
        else
            echo "Error: Android native library is not 16 KB page-size compatible: ABI=$abi library=$library LOAD=${DETECTED_LOAD_ALIGNMENTS:-missing} GNU_RELRO_end=${DETECTED_RELRO_ENDS:-missing}"
        fi
        readelf_program_headers "$library" | grep -E 'LOAD|GNU_RELRO' || true
        return 1
    fi
}

validate_android_aar_symbols() {
    local aar="$1"
    local tmp_dir
    local entry_list
    local required_entry
    local native_entries
    local duplicate_native_entries
    local native_index
    local entry
    local relative_path
    local abi
    local file_name
    local library

    if [ ! -f "$aar" ]; then
        echo "Error: Android release AAR missing at $aar"
        return 1
    fi

    tmp_dir=$(mktemp -d)
    entry_list="$tmp_dir/archive-entries.txt"
    unzip -Z1 "$aar" > "$entry_list"

    for abi in armeabi-v7a arm64-v8a x86_64; do
        required_entry="jni/$abi/libldk_node.so"
        if ! grep -Fqx "$required_entry" "$entry_list"; then
            echo "Error: Android release AAR native library missing at $required_entry"
            rm -rf "$tmp_dir"
            return 1
        fi
    done

    native_entries="$tmp_dir/native-entry-names.txt"
    if ! awk '
        /^jni\/.*[.]so$/ {
            if ($0 !~ /^jni\/[A-Za-z0-9._+-]+\/lib[A-Za-z0-9._+-]*[.]so$/) {
                exit 1
            }
            print
        }
    ' "$entry_list" > "$native_entries"; then
        echo "Error: Android release AAR contains an unsafe native library entry"
        rm -rf "$tmp_dir"
        return 1
    fi
    if [ ! -s "$native_entries" ]; then
        echo "Error: Android release AAR contains no native libraries"
        rm -rf "$tmp_dir"
        return 1
    fi
    duplicate_native_entries=$(sort "$native_entries" | uniq -d)
    if [ -n "$duplicate_native_entries" ]; then
        echo "Error: Android release AAR contains a duplicate native library entry: $duplicate_native_entries"
        rm -rf "$tmp_dir"
        return 1
    fi

    native_index=0
    while IFS= read -r entry; do
        native_index=$((native_index + 1))
        relative_path=${entry#jni/}
        abi=${relative_path%%/*}
        file_name=${relative_path#*/}
        library="$tmp_dir/native/$native_index/$file_name"
        mkdir -p "$(dirname "$library")"
        unzip -p "$aar" "$entry" > "$library"
        if ! validate_stripped_android_library "$abi" "$library"; then
            rm -rf "$tmp_dir"
            return 1
        fi
    done < "$native_entries"

    rm -rf "$tmp_dir"
}

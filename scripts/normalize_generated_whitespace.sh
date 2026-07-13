#!/bin/bash
set -euo pipefail

if [ "$#" -eq 0 ]; then
	echo "Usage: $0 FILE..." >&2
	exit 1
fi

for file in "$@"; do
	if [ ! -f "$file" ]; then
		echo "Generated binding not found: $file" >&2
		exit 1
	fi
done

perl -0pi -e 's/[ \t]+(?=\n)//g; s/[ \t]+\z//; s/\n+\z/\n/; $_ .= "\n" unless /\n\z/' -- "$@"

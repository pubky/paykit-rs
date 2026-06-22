#!/bin/bash

set -e

if [ "$#" -eq 0 ]; then
    echo "Usage: $0 <generated binding file>..." >&2
    exit 1
fi

perl -0pi -e '
    s/PaykitFfi/Paykit/g;
    s/\b(FfiConverter(?:SequenceType|OptionType|Type)?)(Ffi)(?=[A-Z])/$1/g;
    s/\b(Ffi[A-Z][A-Za-z0-9_]*)\b/
        do {
            my $name = $1;
            ($name =~ m{^Ffi(?:Converter|Type)}) ? $name : substr($name, 3)
        }
    /gex;
' "$@"

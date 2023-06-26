#!/bin/bash

proxy_url="http://127.0.0.1:8081/"
use_proxy=$proxy_url

while [[ $# -gt 0 ]]; do
    key="$1"
    case $key in
        # no proxy
        -np)
            use_proxy=""
            shift
            ;;
        *)
            echo "Unknown: $key"
            ;;
    esac
    shift
done

# 256
block=$(cat << EOF
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
EOF
)

hey -x "$use_proxy" \
  -cpus 1 \
  -n 20000 \
  -c 15 \
  -m POST \
  -d "$block" \
  http://localhost:3030/hi

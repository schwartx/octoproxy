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

hey -x "$use_proxy" \
  -cpus 1 \
  -n 2000 \
  -c 5 \
  http://localhost:3030/hi

#!/bin/bash
# Awesome lua script taken from
# https://github.com/wg/wrk/issues/162#issuecomment-143963862

proxy="http://localhost:8081/"
url="http://localhost:3030/hi"
method="GET"

while [[ $# -gt 0 ]]; do
    key="$1"
    case $key in
        -proxy)
            proxy="$2"
            shift
            ;;
        -url)
            url="$2"
            shift
            ;;
        *)
            echo "Unknown: $key"
            ;;
    esac
    shift
done

echo "proxy: $proxy"
echo "URL: $url"

lua_str=$(cat << EOF
local connected = false
function extractHostname(url)
  local protocol, host, port = url:match("(https?://.-):?/?/?([^:/]+):?(%d*)/")
  return host
end

local url           = "$url"
local host          = extractHostname(url)
wrk.headers["Host"] = host
request             = function()
  if not connected then
    connected = true
    return wrk.format("CONNECT", host)
  end
  return wrk.format("$method", url)
end
EOF
)

# cat <(echo "$lua_str")

# NOTE: Can't use Process Substitution
tmp_file=$(mktemp)
echo "$lua_str" > "$tmp_file"

wrk -t1 --latency -c15 -d10s -s "$tmp_file" $proxy

function extractHostname(url)
  local protocol, host, port = url:match("(https?://.-):?/?/?([^:/]+):?(%d*)/")
  return host
end

local url = "http://localhost:8080/hi"
local host = extractHostname(url)
print(host)

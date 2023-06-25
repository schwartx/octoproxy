local connected = false
function extractHostname(url)
  local protocol, host, port = url:match("(https?://.-):?/?/?([^:/]+):?(%d*)/")
  return host
end

local url           = "http://localhost:8080/hi"

local host          = extractHostname(url)

wrk.headers["Host"] = host

request             = function()
  if not connected then
    connected = true
    return wrk.format("CONNECT", host)
  end

  return wrk.format("GET", url)
end

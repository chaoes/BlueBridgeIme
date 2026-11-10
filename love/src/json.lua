-- Minimal JSON codec. The service protocol only uses flat objects, arrays of
-- objects and primitives, so this stays deliberately small (no indentation,
-- no null sentinel beyond `json.null`).
local json = {}

json.null = setmetatable({}, { __tostring = function() return "null" end })

local escapes = {
  ['"'] = '\\"', ['\\'] = '\\\\', ['\b'] = '\\b', ['\f'] = '\\f',
  ['\n'] = '\\n', ['\r'] = '\\r', ['\t'] = '\\t',
}

local function escape_char(c)
  return escapes[c] or string.format('\\u%04x', c:byte())
end

local function is_array(value)
  if value.__object then return false end
  local n = 0
  for k in pairs(value) do
    if type(k) ~= 'number' then return false end
    n = n + 1
  end
  return n == #value
end

local encode_value
encode_value = function(value, out)
  local kind = type(value)
  if value == json.null then
    out[#out + 1] = 'null'
  elseif kind == 'nil' then
    out[#out + 1] = 'null'
  elseif kind == 'boolean' then
    out[#out + 1] = value and 'true' or 'false'
  elseif kind == 'number' then
    if value ~= value or value == math.huge or value == -math.huge then
      out[#out + 1] = 'null'
    elseif math.type and math.type(value) == 'integer' then
      out[#out + 1] = tostring(value)
    elseif value == math.floor(value) and math.abs(value) < 1e15 then
      out[#out + 1] = string.format('%d', value)
    else
      out[#out + 1] = string.format('%.14g', value)
    end
  elseif kind == 'string' then
    out[#out + 1] = '"'
    out[#out + 1] = value:gsub('[%z\1-\31\\"]', escape_char)
    out[#out + 1] = '"'
  elseif kind == 'table' then
    if is_array(value) then
      out[#out + 1] = '['
      for i = 1, #value do
        if i > 1 then out[#out + 1] = ',' end
        encode_value(value[i], out)
      end
      out[#out + 1] = ']'
    else
      out[#out + 1] = '{'
      local first = true
      for key, item in pairs(value) do
        if type(key) == 'string' and key ~= '__object' then
          if not first then out[#out + 1] = ',' end
          first = false
          out[#out + 1] = '"'
          out[#out + 1] = key:gsub('[%z\1-\31\\"]', escape_char)
          out[#out + 1] = '":'
          encode_value(item, out)
        end
      end
      out[#out + 1] = '}'
    end
  else
    error('cannot encode ' .. kind)
  end
end

function json.encode(value)
  local out = {}
  encode_value(value, out)
  return table.concat(out)
end

local function utf8_char(code)
  if code < 0x80 then
    return string.char(code)
  elseif code < 0x800 then
    return string.char(0xC0 + math.floor(code / 0x40), 0x80 + code % 0x40)
  elseif code < 0x10000 then
    return string.char(0xE0 + math.floor(code / 0x1000),
      0x80 + math.floor(code / 0x40) % 0x40, 0x80 + code % 0x40)
  end
  return string.char(0xF0 + math.floor(code / 0x40000),
    0x80 + math.floor(code / 0x1000) % 0x40,
    0x80 + math.floor(code / 0x40) % 0x40, 0x80 + code % 0x40)
end

local function skip_ws(text, pos)
  local _, stop = text:find('^[ \t\r\n]*', pos)
  return stop + 1
end

local decode_value

local function decode_string(text, pos)
  local out, i = {}, pos + 1
  while true do
    local c = text:sub(i, i)
    if c == '' then error('unterminated string') end
    if c == '"' then return table.concat(out), i + 1 end
    if c == '\\' then
      local esc = text:sub(i + 1, i + 1)
      if esc == 'u' then
        local hex = text:sub(i + 2, i + 5)
        local code = tonumber(hex, 16) or 0
        i = i + 6
        if code >= 0xD800 and code <= 0xDBFF then
          local low = tonumber(text:sub(i + 2, i + 5), 16)
          if text:sub(i, i + 1) == '\\u' and low and low >= 0xDC00 and low <= 0xDFFF then
            code = 0x10000 + (code - 0xD800) * 0x400 + (low - 0xDC00)
            i = i + 6
          end
        end
        out[#out + 1] = utf8_char(code)
      else
        out[#out + 1] = ({ b = '\b', f = '\f', n = '\n', r = '\r', t = '\t' })[esc] or esc
        i = i + 2
      end
    else
      local chunk = text:match('^[^"\\]+', i)
      if not chunk then chunk = c end
      out[#out + 1] = chunk
      i = i + #chunk
    end
  end
end

decode_value = function(text, pos)
  pos = skip_ws(text, pos)
  local c = text:sub(pos, pos)
  if c == '{' then
    local obj = { __object = true }
    pos = skip_ws(text, pos + 1)
    if text:sub(pos, pos) == '}' then return obj, pos + 1 end
    while true do
      local key
      key, pos = decode_string(text, skip_ws(text, pos))
      pos = skip_ws(text, pos)
      if text:sub(pos, pos) ~= ':' then error('expected colon') end
      local value
      value, pos = decode_value(text, pos + 1)
      obj[key] = value
      pos = skip_ws(text, pos)
      local sep = text:sub(pos, pos)
      if sep == '}' then return obj, pos + 1 end
      if sep ~= ',' then error('expected , or }') end
      pos = skip_ws(text, pos + 1)
    end
  elseif c == '[' then
    local arr = {}
    pos = skip_ws(text, pos + 1)
    if text:sub(pos, pos) == ']' then return arr, pos + 1 end
    while true do
      local value
      value, pos = decode_value(text, pos)
      arr[#arr + 1] = value
      pos = skip_ws(text, pos)
      local sep = text:sub(pos, pos)
      if sep == ']' then return arr, pos + 1 end
      if sep ~= ',' then error('expected , or ]') end
      pos = pos + 1
    end
  elseif c == '"' then
    return decode_string(text, pos)
  elseif text:sub(pos, pos + 3) == 'true' then
    return true, pos + 4
  elseif text:sub(pos, pos + 4) == 'false' then
    return false, pos + 5
  elseif text:sub(pos, pos + 3) == 'null' then
    return json.null, pos + 4
  else
    local number = text:match('^-?%d+%.?%d*[eE]?[-+]?%d*', pos)
    if not number or number == '' then error('unexpected character at ' .. pos) end
    return tonumber(number), pos + #number
  end
end

function json.decode(text)
  local value, pos = decode_value(text, 1)
  if skip_ws(text, pos) <= #text then error('trailing characters') end
  return value
end

return json

-- Client for the headless Rust service's control socket.
--
-- Uses LuaJIT FFI (LÖVE ships LuaJIT on every desktop platform) so the app has
-- no extra native dependency: one AF_UNIX stream socket, newline-delimited
-- JSON, non-blocking reads on the LÖVE main thread.
local json = require('src.json')

local Service = {
  connected = false,
  status = nil,
  lastRate = 0,
  onStatus = function() end,
  onError = function() end,
  onConnection = function() end, -- (connected:boolean, reason:string|nil)
}

local ffi_ok, ffi = pcall(require, 'ffi')
local native = false

if ffi_ok and ffi.os ~= 'Windows' then
  local ok = pcall(ffi.cdef, [[
    typedef unsigned short sa_family_t;
    struct sockaddr_un { sa_family_t sun_family; char sun_path[108]; };
    int socket(int domain, int type, int protocol);
    int connect(int sockfd, const struct sockaddr_un *addr, unsigned int addrlen);
    int close(int fd);
    long send(int fd, const void *buf, unsigned long len, int flags);
    long recv(int fd, void *buf, unsigned long len, int flags);
    int fcntl(int fd, int cmd, ...);
    unsigned int getuid(void);
    typedef void (*sighandler_t)(int);
    sighandler_t signal(int signum, sighandler_t handler);
    struct pollfd { int fd; short events; short revents; };
    int poll(struct pollfd *fds, unsigned long nfds, int timeout);
  ]])
  native = ok
end

local AF_UNIX = 1
local SOCK_STREAM = 1
local F_GETFL, F_SETFL = 3, 4
local O_NONBLOCK = ffi_ok and (ffi.os == 'OSX' and 4 or 0x800) or 0
local POLLIN = 1

local fd = -1
local rx = ''
local queue = {}
local retry = 0
local last_spawn = -100
local last_error = nil

local recv_buffer = ffi_ok and ffi.new('char[?]', 65536) or nil

local function socket_path()
  local runtime = os.getenv('XDG_RUNTIME_DIR')
  if not runtime or runtime == '' then
    local uid = '1000'
    if native then uid = tostring(ffi.C.getuid()) end
    runtime = '/run/user/' .. uid
  end
  return runtime .. '/bluetooth-input-bridge/control.sock'
end

Service.socket_path = socket_path

local function platform_note()
  if ffi_ok and ffi.os == 'Windows' then
    return 'Windows 后端尚未实现（服务只支持 Linux）'
  end
  if not ffi_ok then
    return '当前 Lua 运行时没有 FFI，无法连接后台服务'
  end
  return nil
end

Service.platform_note = platform_note

local function close_socket()
  if fd >= 0 and native then pcall(ffi.C.close, fd) end
  fd = -1
  rx = ''
  queue = {}
end

local function mark_down(reason)
  if Service.connected then
    Service.connected = false
    Service.onConnection(false, reason)
  end
  close_socket()
  retry = 3
end

local function try_connect()
  if not native then return false end
  local path = socket_path()
  local sock = ffi.C.socket(AF_UNIX, SOCK_STREAM, 0)
  if sock < 0 then return false end

  local addr = ffi.new('struct sockaddr_un')
  addr.sun_family = AF_UNIX
  ffi.copy(addr.sun_path, path, math.min(#path, 107))

  local len = ffi.sizeof('sa_family_t') + #path + 1
  if ffi.C.connect(sock, addr, len) ~= 0 then
    ffi.C.close(sock)
    return false
  end

  fd = sock
  -- Non-blocking reads keep the render loop smooth even if the service stalls.
  local flags = ffi.C.fcntl(fd, F_GETFL)
  if flags >= 0 then ffi.C.fcntl(fd, F_SETFL, ffi.cast('int', bit.bor(flags, O_NONBLOCK))) end

  Service.connected = true
  last_error = nil
  Service.onConnection(true)
  Service.send({ cmd = 'subscribe' })
  return true
end

local function flush()
  if fd < 0 then queue = {} return end
  while #queue > 0 do
    local line = queue[1]
    local sent = tonumber(ffi.C.send(fd, line, #line, 0)) or -1
    if sent == #line then
      table.remove(queue, 1)
    elseif sent > 0 then
      queue[1] = line:sub(sent + 1)
    else
      break -- EAGAIN or a dead socket; retried next frame or dropped on close
    end
  end
end

local strip_nulls

-- The service treats "subscribe" connections as read-only event streams, so
-- every command opens its own short-lived connection (same as the Tauri client).
local function request(message)
  if not native then return nil end
  local fd = ffi.C.socket(AF_UNIX, SOCK_STREAM, 0)
  if fd < 0 then return nil end

  local address = ffi.new('struct sockaddr_un')
  address.sun_family = AF_UNIX
  local path = socket_path()
  ffi.copy(address.sun_path, path, #path + 1)
  local connected = ffi.C.connect(fd, address, ffi.offsetof('struct sockaddr_un', 'sun_path') + #path + 1)
  if connected ~= 0 then
    ffi.C.close(fd)
    return nil
  end

  local line = json.encode(message) .. '\n'
  local sent = tonumber(ffi.C.send(fd, line, #line, 0)) or -1
  if sent ~= #line then
    ffi.C.close(fd)
    return nil
  end

  local buffer = ffi.new('char[65536]')
  local response = ''
  local poll_fds = ffi.new('struct pollfd[1]')
  poll_fds[0].fd = fd
  poll_fds[0].events = POLLIN
  local deadline = 2.0
  while deadline > 0 and not response:find('\n', 1, true) do
    local ready = ffi.C.poll(poll_fds, 1, 200)
    if ready > 0 then
      local size = tonumber(ffi.C.recv(fd, buffer, 65536, 0)) or -1
      if size > 0 then
        response = response .. ffi.string(buffer, size)
      else
        break
      end
    end
    deadline = deadline - 0.2
  end
  if ffi.C.recv then ffi.C.close(fd) end
  if response == '' then return nil end
  local ok, reply = pcall(json.decode, response:match('^[^\n]*'))
  if not ok or type(reply) ~= 'table' then return nil end
  strip_nulls(reply)
  return reply
end

local function apply_reply(reply)
  if type(reply) ~= 'table' then return end
  if type(reply.status) == 'table' then
    Service.status = reply.status
    Service.onStatus(reply.status)
  end
  if reply.ok == false and reply.error then
    Service.onError(tostring(reply.error))
  end
end

function Service.send(message)
  -- Only the subscribe handshake goes over the event socket.
  queue[#queue + 1] = json.encode(message) .. '\n'
  flush()
end

strip_nulls = function(value, seen)
  if type(value) ~= 'table' or value == json.null then return end
  seen = seen or {}
  if seen[value] then return end
  seen[value] = true
  for key, item in pairs(value) do
    if item == json.null then
      value[key] = nil
    else
      strip_nulls(item, seen)
    end
  end
end

local function handle_line(line)
  if line == '' then return end
  local ok, message = pcall(json.decode, line)
  if not ok or type(message) ~= 'table' then return end
  strip_nulls(message)
  if message.status then
    Service.status = message.status
    Service.onStatus(message.status)
  end
  if message.event then
    Service.status = message.event
    Service.onStatus(message.event)
  end
  if message.ok == false and message.error then
    last_error = tostring(message.error)
    Service.onError(last_error)
  end
end

local function read_available()
  if fd < 0 then return end
  while true do
    local size = tonumber(ffi.C.recv(fd, recv_buffer, 65536, 0)) or -1
    if size > 0 then
      rx = rx .. ffi.string(recv_buffer, size)
    elseif size == 0 then
      mark_down('service closed the connection')
      return
    else
      break -- EAGAIN: nothing more right now
    end
  end
  while true do
    local newline = rx:find('\n', 1, true)
    if not newline then break end
    local line = rx:sub(1, newline - 1)
    rx = rx:sub(newline + 1)
    handle_line(line)
  end
  if #rx > 1024 * 1024 then rx = '' end -- runaway line guard
end

local function find_service_binary()
  local explicit = os.getenv('BLUETOOTH_INPUT_BRIDGE_SERVICE')
  if explicit and explicit ~= '' then return explicit end

  local pipe = io.popen('command -v bluetooth-input-bridge-service 2>/dev/null')
  if pipe then
    local found = pipe:read('*l')
    pipe:close()
    if found and found ~= '' then return found end
  end

  if love and love.filesystem then
    local source = love.filesystem.getSource()
    if source and source ~= '' then
      -- .love 文件 -> 所在目录；目录模式 -> 目录本身
      local dir = source:match('%.love$') and (source:match('^(.*)/[^/]*$') or '.')
          or source:gsub('/+$', '')
      local root = dir:gsub('/love$', '')
      for _, path in ipairs({
        dir .. '/bluetooth-input-bridge-service',                        -- 打包后的 linux 目录
        root .. '/bluetooth-input-bridge-service',
        root .. '/desktop/src-tauri/target/release/bluetooth-input-bridge-service',  -- 源码树
        root .. '/desktop/src-tauri/target/debug/bluetooth-input-bridge-service',
      }) do
        local file = io.open(path, 'rb')
        if file then
          file:close()
          return path
        end
      end
    end
  end
  return nil
end

function Service.spawn()
  if ffi.os == 'Windows' then return false end
  local binary = find_service_binary()
  if not binary then return false end
  local command = string.format("nohup %q >/dev/null 2>&1 &", binary)
  os.execute(command)
  Service.spawned = binary
  return true
end

function Service.update(dt)
  if Service.connected then
    read_available()
    flush()
    return
  end
  retry = retry - dt
  if retry > 0 then return end
  retry = 3
  if try_connect() then return end
  -- Start the headless service ourselves (dev tree or $PATH), at most every 12s.
  Service.since_spawn = (Service.since_spawn or 0) + dt
  if native and Service.since_spawn - last_spawn >= 12 then
    last_spawn = Service.since_spawn
    if Service.spawn() then Service.onError('后台服务未运行，已尝试启动。') end
  end
end

function Service.query_status()
  apply_reply(request({ cmd = 'status' }))
end

function Service.set_registration(enabled)
  apply_reply(request({ cmd = 'setRegistration', enabled = not not enabled }))
end

function Service.set_autostart(enabled)
  apply_reply(request({ cmd = 'setAutostart', enabled = not not enabled }))
end

function Service.enable_pairing()
  apply_reply(request({ cmd = 'enablePairing' }))
end

if ffi_ok and native then
  -- A phone can drop mid-write; without this the whole UI would die on SIGPIPE.
  pcall(ffi.C.signal, 13, ffi.cast('sighandler_t', 1))
end

return Service

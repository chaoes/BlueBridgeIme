-- Client for the headless Rust service's control socket.
--
-- Uses LuaJIT FFI (LÖVE ships LuaJIT on every desktop platform) so the app has
-- no extra native dependency: one AF_UNIX stream socket, newline-delimited
-- JSON, non-blocking reads on the LÖVE main thread.
local json = require('src.json')

local DEBUG = os.getenv('BLUETOOTH_INPUT_BRIDGE_DEBUG') == '1'
local function log(...)
  if DEBUG then print('[service]', ...) end
end

local Service = {
  connected = false,
  status = nil,
  lastRate = 0,
  onStatus = function() end,
  onError = function() end,
  onConnection = function() end, -- (connected:boolean, reason:string|nil)
}

local ffi_ok, ffi = pcall(require, 'ffi')
-- Windows 上「能用」不是由 POSIX 那段 cdef 决定的（那段在 Windows 上根本不该跑），
-- 而是由 ffi 是否加载成功决定；ws2_32/kernel32 具体在下面 Windows 段落里加载。
local WINDOWS_FFI_OK = ffi_ok and ffi.os == 'Windows'
local native = false

if ffi_ok then
  -- POSIX 全局符号（Linux/macOS）：LÖVE 里 ffi.C 直接解析动态库符号。
  -- Windows 上不要声明这些名字：MSVCRT 里有同名但 ABI 不同的 socket()/close()
  -- 等，必须走 ffi.load('ws2_32') / ffi.load('kernel32')。
  --
  -- 也**不能**在 Windows 上再声明一遍 `sockaddr_un`：下面 Windows 段落已经声明过
  -- 同名结构，LuaJIT 的 ffi.cdef 会报 “attempt to redefine 'sockaddr_un'”，
  -- 那段整个 cdef 调用直接失败（参数工作区全是默认值，连提示都不会说清楚），
  -- 于是 native 永远为 false、界面永远连不上。所以这里按平台只声明一份。
  local posix_cdef = [[
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
  ]]
  if ffi.os ~= 'Windows' then
    native = pcall(ffi.cdef, posix_cdef)
  end
end

-- Windows 上真正的 AF_UNIX 支持从 Windows 10 1803 开始，地址就是普通文件路径
-- （不是 \\.\pipe\ 名字），所以 sockaddr_un 的 BSD 布局和 Linux 一样。API 名和
-- 类型名都是 _WIN64 / _WIN32 下的 64 位变体。
local ws, k32 = nil, nil
-- Windows：WSAStartup 是否成功。LÖVE 自己从不调用它，进程里多半还没初始化过，
-- 必须由这里初始化，详见 sys_socket。
local startup_ok = false
if ffi_ok and ffi.os == 'Windows' then
  local ok_ws, loaded = pcall(ffi.load, 'ws2_32')
  if ok_ws then ws = loaded end
  local ok_k, kloaded = pcall(ffi.load, 'kernel32')
  if ok_k then k32 = kloaded end
  local ok = false
  if ws and k32 then
    -- LoadLibrary 会自动把 ws2_32 初始化；通常还要 WSAStartup 一次（多数进程
    -- 已经做过），做一下才算完整。
    ok = pcall(ffi.cdef, [[
      typedef unsigned short sa_family_t;
      struct sockaddr_un { sa_family_t sun_family; char sun_path[108]; };
      typedef unsigned long long SOCKET;
      typedef unsigned int DWORD;
      struct WSAData_win {
        unsigned short wVersion; unsigned short wHighVersion;
        char szDescription[257]; char szSystemStatus[129];
        unsigned short iMaxSockets; unsigned short iMaxUdpDg;
        char *lpVendorInfo;
      };
      int WSAStartup(unsigned short wVersionRequested, struct WSAData_win *lpWSAData);
      SOCKET connect(SOCKET s, const struct sockaddr_un *name, int namelen);
      SOCKET WSASocketA(int af, int type, int protocol, void *lpProtocolInfo, unsigned int g, unsigned int flags);
      int closesocket(SOCKET s);
      int WSAGetLastError(void);
      int ioctlsocket(SOCKET s, long cmd, unsigned long *argp);
      struct timeval_win { long tv_sec; long tv_usec; };
      struct fd_set_win { unsigned int fd_count; SOCKET fd_array[64]; };
      int select(int nfds, struct fd_set_win *readfds, struct fd_set_win *writefds,
                 struct fd_set_win *exceptfds, struct timeval_win *timeout);
      int send(SOCKET s, const char *buf, int len, int flags);
      int recv(SOCKET s, char *buf, int len, int flags);
      DWORD GetCurrentProcessId(void);
      DWORD GetModuleFileNameA(unsigned long long hmodule, char *buf, DWORD size);
    ]])
    -- WSAStartup 的返回值必须单独存：写成 `pcall(function() return ws.WSAStartup(...) end)`
    -- 会把返回码 0（成功）当成「假」，初始化白做了（实测每次 socket() 报 10093）。
    if ok then
      local data = ffi.new('struct WSAData_win')
      startup_ok = (ws.WSAStartup(0x0202, data) == 0) -- Winsock 2.2
    end
  end
  -- Windows 上「可用」= ffi 加载成功 + ws2_32/kernel32 拿到 + 结构声明成功。
  native = WINDOWS_FFI_OK and ws ~= nil and k32 ~= nil and ok
end

-- 注意：WSAStartup 的返回值必须单独存下来。别写成 `pcall(function() return ws.WSAStartup(...) end)`——
-- 那样 pcall 会把返回码 0（成功）当成「假」，初始化就白做了（实测每次 socket() 报 10093）。

local AF_UNIX = 1
local SOCK_STREAM = 1
local F_GETFL, F_SETFL = 3, 4
local O_NONBLOCK = ffi_ok and (ffi.os == 'OSX' and 4 or 0x800) or 0
local POLLIN = 1
local FIONBIO = 0x8004667E -- Windows: ioctlsocket 的非阻塞开关
local EAGAIN_WIN, EINPROGRESS_WIN = 10035, 10036

local WINDOWS = ffi_ok and ffi.os == 'Windows'
-- SOCKET 是 64 位无符号，INVALID_SOCKET = (SOCKET)-1。用 ull 类型转换而不是
-- 0xFFFFFFFFFFFFFFFFULL 字面量：语义一样，但后者会让不认 ULL 后缀的 luac -p
-- 语法检查失败。
local INVALID_SOCKET = WINDOWS and ffi.cast('uint64_t', -1) or -1

-- 旧的 POSIX 调用点全部经过这几个包装，Windows 走 ws2_32。
local function sys_socket()
  -- Windows：进程里必须先 WSAStartup 过才能建套接字（LÖVE 自己不会调），
  -- 所以在建 socket 时补一次；失败时再试一次，兼容别处提前调用 WSAStartup
  -- 却没成功的情况。
  if WINDOWS then
    -- 注意：ws2_32 导出的名字是 WSASocketA/WSASocketW，没有 socket()。
    -- 写成 ws.socket(...) 会落到 LuaJIT 的未声明符号兜底上（调用了空指针，
    -- 返回 0 且不报错），于是 connect() 拿无效句柄也“成功”，
    -- 界面就永远停在「正在启动服务」。这一条坑了整个启动流程。
    local sock = ws.WSASocketA(AF_UNIX, SOCK_STREAM, 0, nil, 0, 0)
    if not startup_ok or tonumber(sock) == -1 then
      local data = ffi.new('struct WSAData_win')
      startup_ok = (ws.WSAStartup(0x0202, data) == 0)
      if startup_ok then sock = ws.WSASocketA(AF_UNIX, SOCK_STREAM, 0, nil, 0, 0) end
    end
    if tonumber(sock) == -1 then trace('WSASocketA failed wsa=%s', tostring(ws.WSAGetLastError())) end
    return sock
  end
  return ffi.C.socket(AF_UNIX, SOCK_STREAM, 0)
end

local function sys_close(s)
  if WINDOWS then ws.closesocket(s) else ffi.C.close(s) end
end

-- 注意名字：这里必须叫 sys_error。文件后面有一个 `local last_error = nil`
-- （记录最近的错误文案），如果同名，它会在整个文件里把这个函数遮蔽掉，
-- read_available 就会 `attempt to call upvalue 'last_error' (a nil value)`，
-- 每帧都报错、界面永远停在「正在启动服务」。
local function sys_error()
  if WINDOWS then return ws.WSAGetLastError() end
  return 0
end

local function send_n(fd, buf, len)
  if WINDOWS then return tonumber(ws.send(fd, buf, len, 0)) or -1 end
  return tonumber(ffi.C.send(fd, buf, len, 0)) or -1
end

local function recv_n(fd, buf, len)
  if WINDOWS then return tonumber(ws.recv(fd, buf, len, 0)) or -1 end
  return tonumber(ffi.C.recv(fd, buf, len, 0)) or -1
end

-- 等待 fd 可读最多 timeout_ms 毫秒；返回是否就绪。
local function wait_readable(fd, timeout_ms)
  if WINDOWS then
    local set = ffi.new('struct fd_set_win')
    set.fd_count = 1
    set.fd_array[0] = fd
    local tv = ffi.new('struct timeval_win')
    tv.tv_sec = 0
    tv.tv_usec = timeout_ms * 1000
    local n = ws.select(0, set, nil, nil, tv)
    return n > 0
  end
  local poll_fds = ffi.new('struct pollfd[1]')
  poll_fds[0].fd = fd
  poll_fds[0].events = POLLIN
  local n = ffi.C.poll(poll_fds, 1, timeout_ms)
  return n > 0
end

local function set_nonblocking(fd)
  if WINDOWS then
    local on = ffi.new('unsigned long[1]')
    on[0] = 1
    ws.ioctlsocket(fd, FIONBIO, on)
    return
  end
  local flags = ffi.C.fcntl(fd, F_GETFL)
  if flags >= 0 then ffi.C.fcntl(fd, F_SETFL, ffi.cast('int', bit.bor(flags, O_NONBLOCK))) end
end

local try_connect -- forward declaration，定义见 try_connect_impl 附近
local TRACE_PATH = os.getenv('BLUETOOTH_INPUT_BRIDGE_TRACE')
if TRACE_PATH == '' then TRACE_PATH = nil end

local fd = -1
local rx = ''
local queue = {}
local retry = 0
local last_spawn = -100
local last_error = nil
-- 连上过套接字之前不无限拉起程序：用户直接用 `\\.\pipe\` 跑服务（没有桥）时，
-- 不至于每 12 秒弹一次 bluetooth-input-bridge-socket.exe。
local bridge_launches = 0
local first_connected = false
local BRIDGE_GRACE = 3 -- 拉起桥后等它建套接字的秒数
local last_bridge_launch = nil

local recv_buffer = ffi_ok and ffi.new('char[?]', 65536) or nil

-- [[ 调试钩子 ]] 设了 BLUETOOTH_INPUT_BRIDGE_TRACE=<文件路径> 时，把启动/连接
-- 过程写到该文件（LÖVE 在 Windows 上没有控制台，只能这样看）。对用户的影响
-- 仅限多一次 getenv；不设就用一个空函数。
local function trace(fmt, ...)
  if not TRACE_PATH then return end
  local file = io.open(TRACE_PATH, 'a')
  if not file then return end
  file:write(string.format('[%7.3f] ', love.timer and love.timer.getTime() or 0))
  file:write(string.format(fmt, ...))
  file:write('\n')
  file:close()
end
Service.trace = trace

-- --- 套接字路径 -------------------------------------------------------------
--
-- Linux：$XDG_RUNTIME_DIR/bluetooth-input-bridge/control.sock（见 service-api.md）。
--
-- Windows：AF_UNIX 路径是普通文件，桥（bluetooth-input-bridge-socket.exe）把命名
-- 管道转成 <dir>\control.sock。目录由桥按「父进程 pid」建：
--   <dir>\bluetooth-input-bridge-<love pid>\control.sock
-- 路径上限是 sockaddr_un.sun_path 的 108 字节（含结尾 NUL），超出会被 Windows
-- 拒绝。桥超长时会简化成 <dir>\bluetooth-input-bridge-<pid>.sock（单文件、不再
-- 建子目录），所以候选按「正常 → 单文件 → 固定名」排列，并且一律按 107 字节过滤，
-- 而不是截断字符串。
local WINDOWS_SOCKET_DIR_ENV = 'BLUETOOTH_INPUT_BRIDGE_SOCKET_DIR'
local SOCKET_DIR_PREFIX = 'bluetooth-input-bridge'
local SOCKET_SUFFIX = '\\control.sock'
local SOCKET_FLAT_SUFFIX = '.sock'
-- sun_path[108] 含结尾 NUL，可用长度最多 107 字节。
local MAX_PATH_BYTES = 107
-- 超过这个长度就不再优先用带 -<pid> 子目录的长形式，退到短形式（避免贴着上限）。
local PREFER_SHORT_PATH_BYTES = 100

local function windows_pid()
  if WINDOWS and k32 then
    local ok, pid = pcall(k32.GetCurrentProcessId)
    if ok and pid then return tonumber(pid) end
  end
  return nil
end

local function trim_slashes(path)
  return (path:gsub('[\\/]+$', ''))
end

local function windows_temp_dirs()
  local dirs, seen = {}, {}
  local function add(path)
    if not path or path == '' then return end
    path = trim_slashes(path)
    if path == '' then return end
    local key = path:lower()
    if seen[key] then return end
    seen[key] = true
    dirs[#dirs + 1] = path
  end
  add(os.getenv('TEMP'))
  add(os.getenv('TMP'))
  add(os.getenv('LOCALAPPDATA') and (os.getenv('LOCALAPPDATA') .. '\\Temp'))
  add(os.getenv('USERPROFILE') and (os.getenv('USERPROFILE') .. '\\AppData\\Local\\Temp'))
  add('C:\\Windows\\Temp')
  return dirs
end

--- Windows 上可能的套接字路径，按优先级排列。第一个一定是「正常情况」。
local function windows_socket_candidates()
  local paths, seen = {}, {}
  local pid = windows_pid()
  local function add(path)
    if not path or #path > MAX_PATH_BYTES then return end
    local key = path:lower()
    if seen[key] then return end
    seen[key] = true
    paths[#paths + 1] = path
  end
  local function add_dir_forms(base)
    local long_form = pid and (base .. '\\' .. SOCKET_DIR_PREFIX .. '-' .. pid .. SOCKET_SUFFIX)
    local flat_form = pid and (base .. '\\' .. SOCKET_DIR_PREFIX .. '-' .. pid .. SOCKET_FLAT_SUFFIX)
    local fixed_form = base .. '\\' .. SOCKET_DIR_PREFIX .. SOCKET_SUFFIX
    -- 太长就跳过长的形式，别贴着 108 字节上限。
    if long_form and (#long_form <= PREFER_SHORT_PATH_BYTES or #fixed_form > MAX_PATH_BYTES) then
      add(long_form)
    end
    add(flat_form)
    add(fixed_form)
  end

  local dir = os.getenv(WINDOWS_SOCKET_DIR_ENV)
  if dir and dir ~= '' then
    -- 既可以是「放 <名称> 目录的父目录」，也可以直接给完整的 .sock 文件。
    if dir:sub(-5):lower() == '.sock' then
      add(dir)
    else
      add_dir_forms(trim_slashes(dir))
    end
  end

  for _, base in ipairs(windows_temp_dirs()) do
    add_dir_forms(base)
  end
  return paths
end

Service.windows_socket_candidates = windows_socket_candidates

--- Linux 的路径（行为与以前完全相同）。
local function posix_socket_path()
  local runtime = os.getenv('XDG_RUNTIME_DIR')
  if not runtime or runtime == '' then
    local uid = '1000'
    if ffi_ok then uid = tostring(ffi.C.getuid()) end
    runtime = '/run/user/' .. uid
  end
  return runtime .. '/bluetooth-input-bridge/control.sock'
end

--- 要尝试连接的路径列表。Linux 只有一个；Windows 按优先级多个。
local function socket_paths()
  if not WINDOWS then return { posix_socket_path() } end
  local paths = windows_socket_candidates()
  if #paths == 0 then paths[1] = (os.getenv('TEMP') or '.') .. SOCKET_SUFFIX end
  return paths
end

local function socket_path()
  return socket_paths()[1]
end

Service.socket_path = socket_path
Service.socket_paths = socket_paths

local function platform_note()
  if not ffi_ok then
    return '当前 Lua 运行时没有 FFI，无法连接后台服务'
  end
  if not native then
    return '无法加载系统套接字 API（ws2_32 / libc），无法连接后台服务'
  end
  return nil
end

Service.platform_note = platform_note

-- 关闭事件流连接：Windows 上是 closesocket，POSIX 上是 close。
local function close_socket()
  if fd >= 0 and native then pcall(sys_close, fd) end
  fd = -1
  rx = ''
  queue = {}
end

-- 事件流断开：通知界面，然后清空连接状态，3 秒后重连。
local function mark_down(reason)
  trace('mark_down %s', tostring(reason))
  log('down:', reason)
  if Service.connected then
    Service.connected = false
    Service.onConnection(false, reason)
  end
  close_socket()
  retry = 3
end

--- 连接一个套接字路径。Windows 上 connect() 是同步的：桥未运行/未建目录时
--- 会阻塞几十毫秒才返回错误，多个候选叠加仍可接受（只在失联时每 3 秒试一次）。
local function connect_to(path)
  local sock = sys_socket()
  if WINDOWS then
    trace('connect_to pid=%s ws=%s', tostring(windows_pid()), tostring(ws ~= nil))
    -- `sys_socket()` 失败时返回的是 ffi 的 unsigned long long 最大值，
    -- 不能直接和 -1 比较（ts ~= -1 永远为真，后面的 connect 只是又失败一次）。
    if sock == nil or ws == nil then return -1 end
    if tonumber(sock) == -1 then return -1 end
    local addr = ffi.new('struct sockaddr_un')
    addr.sun_family = AF_UNIX
    local n = math.min(#path, MAX_PATH_BYTES)
    ffi.copy(addr.sun_path, path, n)
    addr.sun_path[n] = 0
    local len = ffi.offsetof('struct sockaddr_un', 'sun_path') + n + 1
    local result = ws.connect(sock, addr, len)
    if tonumber(result) == -1 then
      ws.closesocket(sock)
      log('connect failed', path, 'wsa', ws.WSAGetLastError())
      trace('connect failed %s wsa=%s', path, tostring(ws.WSAGetLastError()))
      return -1
    end
  else
    local addr = ffi.new('struct sockaddr_un')
    addr.sun_family = AF_UNIX
    ffi.copy(addr.sun_path, path, math.min(#path, 107))
    local len = ffi.sizeof('sa_family_t') + #path + 1
    if ffi.C.connect(sock, addr, len) ~= 0 then
      ffi.C.close(sock)
      log('connect failed', path)
      return -1
    end
  end
  -- Non-blocking reads keep the render loop smooth even if the service stalls.
  set_nonblocking(sock)
  log('connected', path)
  return sock
end

-- 注意是 try_connect = function() 而不是 local function try_connect()：
-- request() → apply_reply() → Service.onStatus/onError 里会间接调用它，
-- 写成 local 的话内层引用的就是「还没赋值的局部变量」，运行到那里会报
-- ttempt to call a nil value（实测就是整个 update 链挂掉）。
local function try_connect_impl()
  trace('try_connect native=%s', tostring(native))
  if not native then return false end
  for _, path in ipairs(socket_paths()) do
    local sock = connect_to(path)
    if sock ~= nil and (not WINDOWS or tonumber(sock) ~= -1) then
      fd = sock
      first_connected = true
      Service.connected = true
      last_error = nil
      trace('connected %s', tostring(path))
      Service.onConnection(true)
      Service.send({ cmd = 'subscribe' })
      return true
    end
  end
  return false
end
try_connect = function()
  return try_connect_impl()
end

local function flush()
  if fd == nil or fd == -1 then queue = {} return end
  while #queue > 0 do
    local line = queue[1]
    local sent = send_n(fd, line, #line)
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
  local message_fd
  local path
  for _, candidate in ipairs(socket_paths()) do
    local sock = sys_socket()
    if WINDOWS then
      if tonumber(sock) ~= -1 then
        local address = ffi.new('struct sockaddr_un')
        address.sun_family = AF_UNIX
        local n = math.min(#candidate, MAX_PATH_BYTES)
        ffi.copy(address.sun_path, candidate, n)
        address.sun_path[n] = 0
        if tonumber(ws.connect(sock, address, ffi.offsetof('struct sockaddr_un', 'sun_path') + n + 1))
            ~= -1 then
          message_fd, path = sock, candidate
          break
        end
        ws.closesocket(sock)
      end
    else
      if tonumber(sock) >= 0 then
        local address = ffi.new('struct sockaddr_un')
        address.sun_family = AF_UNIX
        ffi.copy(address.sun_path, candidate, math.min(#candidate, 107))
        if ffi.C.connect(sock, address, ffi.offsetof('struct sockaddr_un', 'sun_path') + #candidate + 1) == 0 then
          message_fd, path = sock, candidate
          break
        end
        ffi.C.close(sock)
      end
    end
  end
  if not message_fd then return nil end

  local line = json.encode(message) .. '\n'
  local sent = send_n(message_fd, line, #line)
  if sent ~= #line then
    sys_close(message_fd)
    return nil
  end

  local buffer = ffi.new('char[65536]')
  local response = ''
  local deadline = 2.0
  while deadline > 0 and not response:find('\n', 1, true) do
    if wait_readable(message_fd, 200) then
      local size = recv_n(message_fd, buffer, 65536)
      if size > 0 then
        response = response .. ffi.string(buffer, size)
      else
        break
      end
    end
    deadline = deadline - 0.2
  end
  sys_close(message_fd)
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
  log('line', line:sub(1, 90))
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
    local size = recv_n(fd, recv_buffer, 65536)
    if size > 0 then
      rx = rx .. ffi.string(recv_buffer, size)
    elseif size == 0 then
      mark_down('service closed the connection')
      return
    else
      local err = sys_error()
      if WINDOWS and (err == EAGAIN_WIN or err == EINPROGRESS_WIN) then
        break -- 没有更多数据
      elseif err == 0 then
        break -- POSIX 上一般不会出现：把「读到一个字节也没有」当成暂时空闲
      else
        mark_down('recv failed (' .. tostring(err) .. ')')
        return
      end
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

-- --- 查找并启动可执行文件 ----------------------------------------------------
-- Linux：bluetooth-input-bridge-service（原有逻辑，一字未改）。
-- Windows：bluetooth-input-bridge-socket.exe（命名管道 → AF_UNIX 的桥），
--          以及 bluetooth-input-bridge-service.exe（真正的后端）。

local WINDOWS_BRIDGE_EXE = 'bluetooth-input-bridge-socket.exe'
local WINDOWS_SERVICE_EXE = 'bluetooth-input-bridge-service.exe'

--- .love 文件 / 源码目录所在的目录，以及它上一级的根目录（用于找打包和源码树）。
local function source_dirs()
  if not (love and love.filesystem and love.filesystem.getSource) then return nil, nil end
  local ok, source = pcall(love.filesystem.getSource)
  if not ok or type(source) ~= 'string' or source == '' then return nil, nil end
  if WINDOWS then
    local dir = source:match('%.love$') and (source:match('^(.*)[\\/][^\\/]*$') or '.')
        or trim_slashes(source)
    return dir, dir:match('^(.*)[\\/][^\\/]*$') -- <root>/love -> <root>
  end
  -- .love 文件 -> 所在目录；目录模式 -> 目录本身
  local dir = source:match('%.love$') and (source:match('^(.*)/[^/]*$') or '.')
      or source:gsub('/+$', '')
  local root = dir:gsub('/love$', '')
  return dir, root
end

--- Windows：优先环境变量，其次与 exe 同目录，再是 .love 所在目录和源码树，
--- 最后 PATH（`where`）。返回存在的第一个路径。
local function find_windows_binary(env_var, exe_name)
  local explicit = os.getenv(env_var)
  if explicit and explicit ~= '' then
    local file = io.open(explicit, 'rb')
    if file then file:close() return explicit end
  end

  local candidates = {}
  local function add(path) if path and path ~= '' then candidates[#candidates + 1] = path end end

  -- 1) 与正在运行的 exe 同目录（打包版 BlueBridgeIme.exe 旁边）。
  if k32 then
    local buf = ffi.new('char[4096]')
    if k32.GetModuleFileNameA(0, buf, 4096) > 0 then
      local exe = ffi.string(buf)
      local exe_dir = exe:match('^(.*)[\\/][^\\/]*$')
      if exe_dir then add(exe_dir .. '\\' .. exe_name) end
    end
  end

  -- 2) .love 所在目录、其父目录、源码树里的 cargo 输出目录。
  local dir, root = source_dirs()
  if dir then
    add(dir .. '\\' .. exe_name)
    if root then
      add(root .. '\\' .. exe_name)
      add(root .. '\\desktop\\src-tauri\\target\\release\\' .. exe_name)
      add(root .. '\\desktop\\src-tauri\\target\\debug\\' .. exe_name)
    end
  end

  for _, path in ipairs(candidates) do
    local file = io.open(path, 'rb')
    if file then file:close() return path end
  end

  -- 3) PATH。where.exe 会把命中路径逐行打印；其中未找到时退出码非 0，输出为空。
  local pipe = io.popen('where ' .. exe_name .. ' 2>NUL')
  if pipe then
    local found = pipe:read('*l')
    pipe:close()
    if found and found ~= '' then
      found = found:gsub('%s+$', '')
      local file = io.open(found, 'rb')
      if file then file:close() return found end
    end
  end
  return nil
end

local function file_exists(path)
  local file = io.open(path, 'rb')
  if file then file:close() return true end
  return false
end

local function find_service_binary()
  if WINDOWS then
    return find_windows_binary('BLUETOOTH_INPUT_BRIDGE_SERVICE', WINDOWS_SERVICE_EXE)
  end

  local explicit = os.getenv('BLUETOOTH_INPUT_BRIDGE_SERVICE')
  if explicit and explicit ~= '' then return explicit end

  local pipe = io.popen('command -v bluetooth-input-bridge-service 2>/dev/null')
  if pipe then
    local found = pipe:read('*l')
    pipe:close()
    if found and found ~= '' then return found end
  end

  local dir, root = source_dirs()
  if dir then
    for _, path in ipairs({
      dir .. '/bluetooth-input-bridge-service',                        -- 打包后的 linux 目录
      root .. '/bluetooth-input-bridge-service',
      root .. '/desktop/src-tauri/target/release/bluetooth-input-bridge-service',  -- 源码树
      root .. '/desktop/src-tauri/target/debug/bluetooth-input-bridge-service',
    }) do
      if file_exists(path) then return path end
    end
  end
  return nil
end

local function find_bridge_binary()
  if not WINDOWS then return nil end
  return find_windows_binary('BLUETOOTH_INPUT_BRIDGE_SOCKET_EXE', WINDOWS_BRIDGE_EXE)
end

Service.find_service_binary = find_service_binary
Service.find_bridge_binary = find_bridge_binary

--- Windows：用后台、无窗口的方式执行一条 cmd 命令。绝对路径要自己加引号——
--- Lua 的 %q 在 Windows 上会写出 \" 这种 cmd 不认的转义。
--
-- 注意不要用 io.popen('start "" /B ...')：实测（LÖVE 11.5 / LuaJIT on Windows）
-- 这种写法只把 cmd 的命令行丢掉，什么都不会启动，而且不会报错，界面就一直
-- 卡在「正在启动服务」。os.execute 走 CMD.EXE /C <命令>，是真正会执行的。
local function windows_run(command)
  local wrapped = 'start "" /B ' .. command
  log('spawn', wrapped)
  trace('run %s', wrapped)
  local ok = os.execute(wrapped)
  trace('run result %s', tostring(ok))
  return ok == true or ok == 0
end

local function kill_processes(exe_name)
  -- 只按名字杀：名字是固定前缀，不会误伤别的程序。
  io.popen('taskkill /F /IM ' .. exe_name .. ' >NUL 2>NUL', 'r'):close()
end

function Service.spawn_bridge()
  if not WINDOWS then return false end
  local bridge = find_bridge_binary()
  if not bridge then return false end
  local pid = windows_pid()
  local command = '"' .. bridge .. '"' .. (pid and (' --parent ' .. pid) or '')
  Service.bridge_binary = bridge
  bridge_launches = bridge_launches + 1
  return windows_run(command)
end

function Service.spawn_service()
  local binary = find_service_binary()
  if not binary then return false end
  if WINDOWS then
    log('spawn service', binary)
    -- 真正的后台进程，窗口不显示；服务自己写日志。
    return windows_run('"' .. binary .. '"')
  end
  local command = string.format("nohup %q >/dev/null 2>&1 &", binary)
  log('spawn', binary)
  os.execute(command)
  Service.spawned = binary
  return true
end

function Service.spawn()
  if WINDOWS then return Service.spawn_bridge() end
  return Service.spawn_service()
end

--- 退出时清理：Windows 下通知桥退出（桥负责删控制套接字、关命名管道）。
--- Linux 下是空操作——服务由 systemd 管理，不该被界面关掉。
function Service.stop_bridge()
  if not WINDOWS then return true end
  local binary = Service.bridge_binary or find_bridge_binary()
  if binary then
    log('stop bridge', binary)
    -- --stop 让桥自行清理套接字目录；cmd 会等它退出再返回，避免残留。
    windows_run('"' .. binary .. '" --stop' .. (windows_pid() and (' --parent ' .. windows_pid()) or ''))
  end
  -- 桥若还有别的实例（例如上次异常退出），再兜底清一次；名字固定，不会误伤。
  kill_processes(WINDOWS_BRIDGE_EXE)
  Service.bridge_binary = nil
  return true
end

-- --- Windows 启动编排 -------------------------------------------------------
--
-- 顺序（lead 定的）：连 socket 失败 → 起桥 → 再等一轮 → 还连不上就起服务。
-- 桥不负责拉服务，服务没起时桥会一直重连命名管道；所以必须由界面发起。
-- 用探针连接而不是 Status.status 判断「桥在说话」，因为桥连着、服务没起时，
-- 探针连接能成功但读不到任何数据。

--- Windows：探针一次控制套接字（连上即关），返回是否连得上。
local function probe_socket()
  if not (WINDOWS and native) then return false end
  for _, path in ipairs(socket_paths()) do
    local sock = sys_socket()
    if WINDOWS then
      local addr = ffi.new('struct sockaddr_un')
      addr.sun_family = AF_UNIX
      local n = math.min(#path, MAX_PATH_BYTES)
      ffi.copy(addr.sun_path, path, n)
      addr.sun_path[n] = 0
      local reached = ws.connect(sock, addr, ffi.offsetof('struct sockaddr_un', 'sun_path') + n + 1)
      local connected = tonumber(reached) ~= -1
      ws.closesocket(sock)
      if connected then return true end
    else
      ffi.C.close(sock)
      return false
    end
  end
  return false
end

--- Windows 启动编排状态机。返回 (是否启动了某个程序, 是否值得稍后再试)。
local function windows_bootstrap()
  local bridge = find_bridge_binary()
  local service = find_service_binary()
  trace('bootstrap bridge=%s service=%s launches=%d conn=%s', tostring(bridge), tostring(service), bridge_launches, tostring(first_connected))
  if not bridge and not service then return false, false end

  if bridge and bridge_launches == 0 then
    last_bridge_launch = Service.since_spawn
    local started = Service.spawn_bridge()
    trace('spawn_bridge -> %s', tostring(started))
    if started then Service.bridge_running = true end
    return started, started
  end

  -- 桥已经拉过：等它建好套接字（给它一个宽限期），再决定是不是服务没起。
  if bridge and not first_connected and last_bridge_launch
      and Service.since_spawn - last_bridge_launch < BRIDGE_GRACE then
    return false, true
  end

  if probe_socket() then
    -- 桥已就绪（套接字在，虽然可能还没转发到管道）。
    Service.bridge_up = true
    if Service.bridge_running and not Service.service_launched then
      Service.service_launched = true
      return Service.spawn_service(), true
    end
    return false, false
  end

  -- 套接字都没有：桥可能挂了/被杀，再拉一次（总次数受 update 里限制）。
  if bridge and Service.bridge_running then
    last_bridge_launch = Service.since_spawn
    local started = Service.spawn_bridge()
    return started, started
  end

  if service and not Service.service_launched then
    Service.service_launched = true
    return Service.spawn_service(), true
  end
  return false, false
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
  if not native or Service.since_spawn - last_spawn < 12 then return end
  last_spawn = Service.since_spawn
  last_error = nil
  if not WINDOWS then
    if Service.spawn_service() then Service.onError('后台服务未运行，已尝试启动。') end
    return
  end
  -- Windows：最多拉 3 次。用户可能直接用 `\\.\pipe\` 跑服务（没有桥），
  -- 不该每 12 秒弹一次 exe。
  if not first_connected and bridge_launches >= 3 then return end
  local started, retry_soon = windows_bootstrap()
  if started then Service.onError('后台服务未运行，已尝试启动。') end
  -- 宽限期内很快再探一次，别等满 12 秒。
  if retry_soon then last_spawn = Service.since_spawn - 11 end
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

if ffi_ok and native and not WINDOWS then
  -- A phone can drop mid-write; without this the whole UI would die on SIGPIPE.
  -- Windows 的 send() 只返回 WSAECONNRESET，不会发 SIGPIPE。
  pcall(ffi.C.signal, 13, ffi.cast('sighandler_t', 1))
end

return Service

-- BlueBridge Ime — LÖVE front-end for the headless Rust service.
--
-- The window is a thin client: the Rust service (bluetooth-input-bridge-service)
-- owns Bluetooth, the control socket and text injection. This app renders the
-- state and sends commands. It is capped at 30 FPS and only repaints when
-- something changed, which keeps it usable on low-end and low-resolution PCs.
io.stdout:setvbuf('no')
io.stderr:setvbuf('no')
local ui = require('src.ui')
local Service = require('src.service')
local i18n = require('src.i18n')

local LOCALES = { 'zh', 'en', 'ru', 'ja', 'ko' }

local function detect_locale()
  local candidates = { 'BLUETOOTH_INPUT_BRIDGE_LOCALE', 'LANGUAGE', 'LC_ALL', 'LC_MESSAGES', 'LANG' }
  for _, name in ipairs(candidates) do
    local raw = os.getenv(name)
    if raw and raw ~= '' then
      raw = raw:gsub('[.:].*$', '')    -- zh_CN.UTF-8 -> zh_CN
      raw = raw:match('^[^:]+') or raw -- LANGUAGE=zh_CN:en_US -> zh_CN
      local primary = raw:match('^(%a+)') or ''
      primary = primary:lower()
      for _, code in ipairs(LOCALES) do
        if primary == code then return code end
      end
    end
  end
  return 'en'
end

local locale = detect_locale()
local t = i18n[locale] or i18n.en

local status = nil
local socket = false
local dirty = true
local input_text = ''
local focused = false
local error_text, error_at = nil, 0
local busy = { registration = false, autostart = false, pairing = false }
local busy_deadline = 0
local pairing_message, pairing_ok, pairing_failed, pairing_at = nil, false, false, 0

local FRAME = 1 / 30

local function mark_dirty() dirty = true end

Service.onStatus = function(st)
  status = st
  socket = true
  busy.registration, busy.autostart, busy.pairing = false, false, false
  mark_dirty()
end

Service.onError = function(message)
  error_text = tostring(message)
  error_at = love.timer.getTime()
  mark_dirty()
end

Service.onConnection = function(connected)
  socket = connected
  if not connected then status = nil end
  mark_dirty()
end

local function trim_last_char(s)
  if s == '' then return s end
  local i = #s
  while i > 1 do
    local b = s:byte(i)
    if b < 0x80 or b >= 0xC0 then break end
    i = i - 1
  end
  return s:sub(1, i - 1)
end

local function current_error()
  local now = love.timer.getTime()
  if status and type(status.lastError) == 'string' and status.lastError ~= '' then
    return status.lastError
  end
  if error_text and now - error_at < 10 then
    return error_text
  end
  return nil
end

local function toggle(kind)
  if busy[kind] or not status or not status.serviceRunning then return end
  busy[kind] = true
  busy_deadline = love.timer.getTime() + 4
  if kind == 'registration' then
    Service.set_registration(not status.registrationEnabled)
  else
    Service.set_autostart(not status.autostartEnabled)
  end
  mark_dirty()
end

local function frame()
  local pairing
  if pairing_message and love.timer.getTime() - pairing_at < 8 then
    pairing = pairing_message
  end
  return {
    t = t,
    status = status,
    socket = socket,
    text = input_text,
    focus = focused,
    error = current_error(),
    pairing_message = pairing,
    pairing_ok = pairing_ok and pairing ~= nil,
    pairing_failed = pairing_failed and pairing ~= nil,
    pairing_busy = busy.pairing,
  }
end

function love.load()
  love.graphics.setDefaultFilter('linear', 'linear')
  love.window.setMode(780, 560, {
    resizable = true,
    minwidth = 520,
    minheight = 420,
    vsync = 0,
  })
  love.window.setTitle(t.appTitle)
  ui.resize(love.graphics.getDimensions())
  dirty = true
end

function love.resize()
  ui.resize(love.graphics.getDimensions())
  dirty = true
end

function love.update(dt)
  Service.update(dt)
  local now = love.timer.getTime()
  if busy_deadline > 0 and now > busy_deadline then
    busy.registration, busy.autostart, busy.pairing = false, false, false
    busy_deadline = 0
    mark_dirty()
  end
  if focused then
    local blink = (now * 2) % 2 < 1
    if blink ~= last_blink then
      last_blink = blink
      mark_dirty()
    end
  end
end

function love.draw()
  love.graphics.origin()
  ui.draw(frame())
end

function love.mousepressed(x, y, button)
  if button ~= 1 then return end
  local action = ui.hit(x, y)
  if action == 'registration' or action == 'autostart' then
    toggle(action)
  elseif action == 'pairing' then
    if not busy.pairing and socket then
      busy.pairing = true
      busy_deadline = love.timer.getTime() + 4
      Service.enable_pairing()
      pairing_message = t.pairingOk
      pairing_ok, pairing_failed = true, false
      pairing_at = love.timer.getTime()
    end
  elseif action == 'test' then
    focused = true
    mark_dirty()
  else
    focused = false
    mark_dirty()
  end
  dirty = true
end

function love.mousemoved()
  dirty = true
end

function love.textinput(text)
  if focused then
    input_text = input_text .. text
    dirty = true
  end
end

local TEST_KEYS = os.getenv('BLUETOOTH_INPUT_BRIDGE_TEST_KEYS') == '1'

function love.keypressed(key)
  if TEST_KEYS and key == 'r' then
    toggle('registration')
  elseif TEST_KEYS and key == 'a' then
    toggle('autostart')
  elseif TEST_KEYS and key == 'p' then
    if not busy.pairing and socket then
      busy.pairing = true
      busy_deadline = love.timer.getTime() + 4
      Service.enable_pairing()
      pairing_message, pairing_ok, pairing_failed = t.pairingOk, true, false
      pairing_at = love.timer.getTime()
    end
  elseif key == 'escape' then
    focused = false
    dirty = true
  elseif key == 'tab' then
    focused = not focused
    dirty = true
  elseif focused and key == 'backspace' then
    input_text = trim_last_char(input_text)
    dirty = true
  elseif focused and (key == 'return' or key == 'kpenter') then
    input_text = input_text .. '\n'
    dirty = true
  elseif key == 'q' and love.keyboard.isDown('lctrl', 'rctrl') then
    love.event.quit()
  end
end

-- Custom run loop: 30 FPS ceiling, and no repaint when nothing changed.
function love.run()
  if love.load then love.load(love.arg.parseGameArguments(arg), arg) end
  if love.timer then love.timer.step() end
  return function()
    if love.event then
      love.event.pump()
      for name, a, b, c, d, e, f in love.event.poll() do
        if name == 'quit' then return 0 end
        love.handlers[name](a, b, c, d, e, f)
      end
    end
    local dt = love.timer.step()
    love.update(dt)
    if dirty then
      dirty = false
      love.graphics.origin()
      ui.draw(frame())
      love.graphics.present()
    end
    local rest = FRAME - dt
    if rest > 0 then love.timer.sleep(rest) end
  end
end

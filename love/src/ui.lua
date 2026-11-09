-- Layout and drawing for the LÖVE front-end.
--
-- Everything is authored on a 900x650 design canvas and scaled as a whole so
-- the window can be resized (or opened small) without any layout re-flow. Text
-- is rendered at the final pixel size, not scaled up, so it stays crisp.
local ui = {}

ui.W, ui.H = 900, 650
ui.scale, ui.ox, ui.oy = 1, 0, 0
ui.win_w, ui.win_h = 900, 650
ui.rects = {}
ui.mouse = { x = -1, y = -1 }

local C = {
  bg_top = { 0.047, 0.059, 0.082 },
  bg_bottom = { 0.063, 0.075, 0.106 },
  panel = { 0.106, 0.129, 0.188 },
  panel_soft = { 0.086, 0.106, 0.157 },
  line = { 0.188, 0.227, 0.302 },
  mint = { 0.553, 0.827, 0.761 },
  mint_bright = { 0.176, 0.831, 0.749 },
  text = { 0.910, 0.929, 0.961 },
  muted = { 0.663, 0.706, 0.776 },
  dim = { 0.435, 0.490, 0.573 },
  live = { 0.463, 0.847, 0.686 },
  error = { 0.929, 0.537, 0.498 },
  status_a = { 0.125, 0.227, 0.278 },
  status_b = { 0.141, 0.192, 0.294 },
  field = { 0.055, 0.067, 0.094 },
}

ui.C = C

local ffi_ok, ffi = pcall(require, 'ffi')

local fonts = {}
local cjk_data = nil
local cjk_scanned = false

local cjk_paths = {
  '/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc',
  '/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc',
  '/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc',
  '/usr/share/fonts/truetype/wqy/wqy-microhei.ttc',
  '/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc',
  '/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf',
  '/System/Library/Fonts/PingFang.ttc',
  '/System/Library/Fonts/STHeiti Medium.ttc',
  '/Library/Fonts/Arial Unicode.ttf',
}

local function windows_font(name)
  local windir = os.getenv('WINDIR')
  if windir then return windir .. '\\Fonts\\' .. name end
end

cjk_paths[#cjk_paths + 1] = windows_font('msyh.ttc')
cjk_paths[#cjk_paths + 1] = windows_font('msyh.ttf')
cjk_paths[#cjk_paths + 1] = windows_font('simhei.ttf')

local function scan_cjk()
  if cjk_scanned then return end
  cjk_scanned = true
  for _, path in ipairs(cjk_paths) do
    if path then
      local handle = io.open(path, 'rb')
      if handle then
        local bytes = handle:read('*a')
        handle:close()
        if bytes and #bytes > 1024 then
          local ok, data = pcall(love.filesystem.newFileData, bytes, 'cjk-font')
          if ok then cjk_data = data end
          return
        end
      end
    end
  end
end

ui.has_cjk_font = function()
  scan_cjk()
  return cjk_data ~= nil
end

-- CJK / kana / hangul live in three-byte sequences starting with E2..ED.
local function needs_cjk(text)
  return cjk_data ~= nil and text:find('[\226-\237]') ~= nil
end

local function font(px, kind)
  px = math.max(1, math.floor(px + 0.5))
  local key = kind .. ':' .. px
  local f = fonts[key]
  if not f then
    if kind == 'cjk' and cjk_data then
      f = love.graphics.newFont(cjk_data, px)
    elseif kind == 'bold' then
      f = love.graphics.newFont('assets/DejaVuSans-Bold.ttf', px)
    elseif kind == 'latin' then
      f = love.graphics.newFont('assets/DejaVuSans.ttf', px)
    else
      f = love.graphics.newFont(px)
    end
    fonts[key] = f
  end
  return f
end

local function font_for(text, size, bold)
  local px = size * ui.scale
  if needs_cjk(text) then
    return font(px, 'cjk')
  end
  return font(px, bold and 'bold' or 'latin')
end

local function sx(v) return ui.ox + v * ui.scale end
local function sy(v) return ui.oy + v * ui.scale end

local function set_color(c, a)
  love.graphics.setColor(c[1], c[2], c[3], a or c[4] or 1)
end

local function rect(x, y, w, h, radius, fill, border, border_color)
  if fill then set_color(fill) end
  love.graphics.rectangle(fill and 'fill' or 'line', sx(x), sy(y), w * ui.scale, h * ui.scale,
    math.max(0, (radius or 0) * ui.scale), math.max(0, (radius or 0) * ui.scale))
  if border and border_color then
    set_color(border_color)
    love.graphics.setLineWidth(math.max(1, ui.scale))
    love.graphics.rectangle('line', sx(x), sy(y), w * ui.scale, h * ui.scale,
      math.max(0, (radius or 0) * ui.scale), math.max(0, (radius or 0) * ui.scale))
  end
end

local function text_at(str, size, x, y, color, bold)
  local f = font_for(str, size, bold)
  love.graphics.setFont(f)
  set_color(color)
  love.graphics.print(str, sx(x), sy(y))
end

local function text_right(str, size, right, y, color, bold)
  local f = font_for(str, size, bold)
  love.graphics.setFont(f)
  set_color(color)
  love.graphics.print(str, sx(right) - f:getWidth(str), sy(y))
end

local function para(str, size, x, y, w, color, bold, align)
  local f = font_for(str, size, bold)
  love.graphics.setFont(f)
  set_color(color)
  love.graphics.printf(str, sx(x), sy(y), w * ui.scale, align or 'left')
end

local function para_h(str, size, w, bold)
  local f = font_for(str, size, bold)
  local _, lines = f:getWrap(str, math.max(1, w * ui.scale))
  return #lines * f:getHeight() / ui.scale
end

local function circle(x, y, r, color)
  set_color(color)
  love.graphics.circle('fill', sx(x), sy(y), r * ui.scale)
end

local function line(x1, y1, x2, y2, color, width)
  set_color(color)
  love.graphics.setLineWidth((width or 1) * ui.scale)
  love.graphics.line(sx(x1), sy(y1), sx(x2), sy(y2))
end

function ui.resize(w, h)
  scan_cjk()
  ui.win_w, ui.win_h = w, h
  ui.scale = math.min(w / ui.W, h / ui.H)
  ui.ox = (w - ui.W * ui.scale) / 2
  ui.oy = (h - ui.H * ui.scale) / 2
  fonts = {}
  ui.rects = {}
end

function ui.to_design(x, y)
  return (x - ui.ox) / ui.scale, (y - ui.oy) / ui.scale
end

local function inside(r, x, y)
  return r and x >= r.x and y >= r.y and x <= r.x + r.w and y <= r.y + r.h
end

-- Which control is under the pointer; strings match the actions in main.lua.
function ui.hit(x, y)
  local dx, dy = ui.to_design(x, y)
  if inside(ui.rects.registration, dx, dy) then return 'registration', dx, dy end
  if inside(ui.rects.autostart, dx, dy) then return 'autostart', dx, dy end
  if inside(ui.rects.pairing, dx, dy) then return 'pairing', dx, dy end
  if inside(ui.rects.test, dx, dy) then return 'test', dx, dy end
  return nil, dx, dy
end

local function draw_toggle(label, note, checked, disabled, x, y, w, h, hovered)
  local bg = C.panel_soft
  if hovered and not disabled then bg = { 0.121, 0.149, 0.216 } end
  rect(x, y, w, h, 10, bg, true, checked and { 0.24, 0.55, 0.52 } or C.line)
  local label_color = disabled and C.dim or C.text
  local note_color = disabled and C.dim or C.muted
  text_at(label, 12.5, x + 12, y + 6.5, label_color, false)
  text_at(note, 9.5, x + 12, y + 22.5, note_color, false)

  local tw, th = 34, 19
  local tx, ty = x + w - 12 - tw, y + (h - th) / 2
  local track = checked and C.mint_bright or { 0.19, 0.23, 0.30 }
  if disabled then track = { track[1] * 0.7 + 0.06, track[2] * 0.7 + 0.06, track[3] * 0.7 + 0.06 } end
  rect(tx, ty, tw, th, th / 2, track)
  circle(tx + (checked and tw - th / 2 or th / 2), ty + th / 2, th / 2 - 3.2,
    disabled and { 0.75, 0.77, 0.80 } or { 1, 1, 1 })
  love.graphics.setColor(1, 1, 1, 1)
end

function ui.draw(frame)
  local t = frame.t
  local st = frame.status
  local socket = frame.socket
  local device_name = st and type(st.connectedDevice) == 'string' and st.connectedDevice or nil
  local connected = socket and device_name ~= nil

  love.graphics.origin()
  love.graphics.clear(C.bg_top)
  set_color(C.bg_bottom)
  love.graphics.rectangle('fill', 0, 0, ui.win_w, ui.win_h)

  local pad_x = 26
  local card_w = ui.W - pad_x * 2
  ui.rects = {}

  -- header ---------------------------------------------------------------
  circle(pad_x + 3, 26, 4.5, C.mint_bright)
  text_at(t.brand, 16.5, pad_x + 13, 14, C.text, false)
  text_at(t.tagline, 10, pad_x + 13, 35, C.mint, false)
  local meta
  if ffi and ffi.os == 'Windows' then
    meta = 'Windows · LÖVE'
  elseif ffi and ffi.os == 'OSX' then
    meta = 'macOS · LÖVE'
  else
    meta = 'Linux · LÖVE'
  end
  text_right(meta, 10, ui.W - pad_x, 20, C.dim, false)
  line(pad_x, 53, ui.W - pad_x, 53, C.line)

  -- status card ----------------------------------------------------------
  local y0 = 66
  local px, pw = pad_x, card_w
  local ix, iw = px + 18, pw - 36

  local state_text, hint_text
  if not socket then
    if st then
      state_text, hint_text = t.stateServiceDown, t.hintServiceDown
    else
      state_text, hint_text = t.stateStarting, t.hintConnecting
    end
  elseif connected then
    state_text, hint_text = t.stateConnected, t.hintConnected
  elseif st and not st.listening then
    state_text, hint_text = t.stateRegistrationOff, t.hintRegistrationOff
  else
    state_text, hint_text = t.stateWaiting, t.hintWaiting
  end

  local row_y = y0 + 13
  local device_y = row_y + 22
  local hint_y = device_y + 34
  local hint_h = para_h(hint_text, 11.5, iw)
  local toggles_y = hint_y + hint_h + 9
  local status_h = toggles_y + 40 + 13 - y0
  local error_y = nil
  if type(frame.error) == 'string' and frame.error ~= '' then
    error_y = toggles_y + 40 + 8
    status_h = status_h + 20
  end

  -- status card fill + top-left glow-ish gradient approximation
  rect(px, y0, pw, status_h, 14, C.status_a, true, { 0.16, 0.31, 0.36 })

  local dot_color = C.dim
  if connected then
    dot_color = C.live
  elseif frame.error and frame.error ~= '' then
    dot_color = C.error
  elseif st and st.listening then
    dot_color = C.live
  end
  circle(ix, row_y + 7, 4, dot_color)
  text_at(state_text, 13.5, ix + 12, row_y, C.text, true)
  if st then
    text_right(t.received .. ' ' .. tostring(st.receivedCount or 0), 10.5, ix + iw, row_y + 2, C.muted)
  end

  text_at(t.deviceLabel, 9.5, ix, device_y, C.dim)
  text_at(device_name or t.notConnected, 13, ix, device_y + 12, device_name and C.text or C.muted, true)
  if st and type(st.connectedAddress) == 'string' then
    text_right(st.connectedAddress, 10, ix + iw, device_y + 15, C.mint)
  end

  para(hint_text, 11.5, ix, hint_y, iw, C.muted)

  local running = socket and st and st.serviceRunning
  local mx, my = love.mouse.getPosition()
  local hdx, hdy = ui.to_design(mx, my)
  local toggle_w = (iw - 10) / 2
  ui.rects.registration = { x = ix, y = toggles_y, w = toggle_w, h = 40 }
  ui.rects.autostart = { x = ix + toggle_w + 10, y = toggles_y, w = toggle_w, h = 40 }
  draw_toggle(t.registrationTitle, st and st.registrationEnabled and t.registrationNoteOn or t.registrationNoteOff,
    st and st.registrationEnabled, not running, ix, toggles_y, toggle_w, 40, inside(ui.rects.registration, hdx, hdy))
  draw_toggle(t.autostartTitle, t.autostartNote, st and st.autostartEnabled, not running,
    ix + toggle_w + 10, toggles_y, toggle_w, 40, inside(ui.rects.autostart, hdx, hdy))

  if error_y then
    text_at(frame.error, 10.5, ix, error_y, C.error)
  end

  -- two cards ------------------------------------------------------------
  local grid_y = y0 + status_h + 10
  local footer_y = ui.H - 26
  local grid_h = footer_y - 8 - grid_y
  local gap = 12
  local cw = (card_w - gap) / 2
  local cx1, cx2 = pad_x, pad_x + cw + gap

  local function card(x, y, w, h)
    rect(x, y, w, h, 14, C.panel, true, C.line)
  end

  -- 01 guide
  local gx, gy, gw, gh = cx1, grid_y, cw, grid_h
  card(gx, gy, gw, gh)
  local gix = gx + 17
  local giw = gw - 34
  text_at(t.guideTitle, 15.5, gix, gy + 18, C.text, false)
  local intro_y = gy + 44
  local intro_h = para_h(t.guideIntro, 11.5, giw)
  para(t.guideIntro, 11.5, gix, intro_y, giw, C.muted)

  local step_y = intro_y + intro_h + 10
  local steps = { t.guideStep1, t.guideStep2, t.guideStep3 }
  for i, step in ipairs(steps) do
    circle(gix + 6, step_y + 5.5, 6, { 0.12, 0.24, 0.28 })
    text_at(tostring(i), 8.5, gix + 4, step_y + 1, C.mint, true)
    para(step, 11, gix + 18, step_y, giw - 18, C.muted)
    step_y = step_y + math.max(18, para_h(step, 11, giw - 18) + 5)
  end

  local btn_w, btn_h = giw, 34
  local btn_y = gy + gh - 15 - 34 - 8 - 24
  ui.rects.pairing = { x = gix, y = btn_y, w = btn_w, h = btn_h }
  local hovered = inside(ui.rects.pairing, hdx, hdy)
  rect(gix, btn_y, btn_w, btn_h, 9,
    hovered and { 0.16, 0.33, 0.33 } or { 0.12, 0.26, 0.27 }, true, { 0.24, 0.55, 0.52 })
  local label = t.pairingButton
  local f = font_for(label, 12, false)
  love.graphics.setFont(f)
  set_color(frame.pairing_busy and C.dim or C.mint)
  love.graphics.print(label, sx(gix + btn_w / 2) - f:getWidth(label) / 2, sy(btn_y + (btn_h - f:getHeight() / ui.scale) / 2))

  local pair_note = t.pairingHint
  if frame.pairing_message and frame.pairing_message ~= '' then
    pair_note = frame.pairing_message
  end
  para(pair_note, 10, gix, gy + gh - 15 - 24, giw,
    frame.pairing_ok and C.mint or (frame.pairing_failed and C.error or C.dim))

  -- 02 test field
  local tx, ty, tw_, th = cx2, grid_y, cw, grid_h
  card(tx, ty, tw_, th)
  local tix = tx + 17
  local tiw = tw_ - 34
  text_at(t.testTitle, 15.5, tix, ty + 18, C.text, false)
  local t_intro_y = ty + 44
  local t_intro_h = para_h(t.testIntro, 11.5, tiw)
  para(t.testIntro, 11.5, tix, t_intro_y, tiw, C.muted)

  local field_y = t_intro_y + t_intro_h + 9
  local field_h = ty + th - 15 - field_y
  local focused = frame.focus
  ui.rects.test = { x = tix, y = field_y, w = tiw, h = field_h }
  rect(tix, field_y, tiw, field_h, 10, C.field, true, focused and C.mint_bright or C.line)

  -- text inside the field, scrolled to the caret
  local inner_x, inner_y = tix + 11, field_y + 9
  local inner_w = tiw - 22
  local inner_h = field_h - 18
  local body = frame.text
  local show = body
  if show == '' and not focused then show = t.testPlaceholder end
  local fsize = 12
  local ff = font_for(show, fsize, false)
  local _, lines = ff:getWrap(show, inner_w * ui.scale)
  local line_h = ff:getHeight() / ui.scale
  local total_h = #lines * line_h
  local offset = math.max(0, total_h - inner_h)

  love.graphics.setScissor(sx(inner_x), sy(inner_y), inner_w * ui.scale, inner_h * ui.scale)
  love.graphics.setFont(ff)
  if show == '' then
    set_color(C.dim)
  else
    set_color(C.text)
  end
  local yy = inner_y - offset
  for _, line in ipairs(lines) do
    love.graphics.print(line, sx(inner_x), sy(yy))
    yy = yy + line_h
  end
  if focused then
    local blink = (love.timer.getTime() * 2) % 2 < 1
    if blink then
      local last = lines[#lines] or ''
      local cx = inner_x + ff:getWidth(last) / ui.scale
      local cy = inner_y - offset + (#lines - 1) * line_h
      set_color(C.mint)
      love.graphics.rectangle('fill', sx(cx), sy(cy + 1), math.max(1, ui.scale), line_h * ui.scale * 0.9)
    end
  end
  love.graphics.setScissor()

  -- footer ---------------------------------------------------------------
  local method = (st and st.injectionMethod) or 'wl-copy + uinput Ctrl+V'
  local sep = needs_cjk(t.footerMethod) and '：' or ': '
  text_at(t.footerMethod .. sep .. method, 9.5, pad_x, footer_y, C.dim)
  text_right(t.footerPrivacy, 9.5, ui.W - pad_x, footer_y, C.dim)

  love.graphics.setColor(1, 1, 1, 1)
end

return ui

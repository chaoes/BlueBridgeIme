function love.conf(c)
  c.identity = 'bluebridge-ime'
  c.version = '11.5'
  c.console = false
  c.window.title = 'BlueBridge Ime'
  c.window.width = 780
  c.window.height = 560
  c.window.minwidth = 520
  c.window.minheight = 420
  c.window.resizable = true
  c.window.vsync = 0
  c.window.highdpi = true
  c.modules.joystick = false
  c.modules.physics = false
  c.modules.audio = false
  c.modules.sound = false
  c.modules.video = false
  c.modules.touch = false
end

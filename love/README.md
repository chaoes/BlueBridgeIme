# LÖVE 前端

桌面端的轻量界面，用 [LÖVE](https://love2d.org/) 11.5 重写，替代 Tauri 外壳。布局与
Tauri 版一致（状态卡、软件指引、测试输入框），但设计稿按 900×650 画，运行时等比缩放到
窗口大小，适合低分辨率、低配置的电脑。

它只做界面：蓝牙转发、BlueZ / Windows 蓝牙注册、文字注入都在无界面的
`bluetooth-input-bridge-service` 里；界面通过 JSON 行协议通信，Linux 上连
`$XDG_RUNTIME_DIR/bluetooth-input-bridge/control.sock`，Windows 上连
`<TEMP>\bluetooth-input-bridge-<界面进程号>\control.sock`（由下面的 socket 桥建）。
因此可以随时换成别的前端技术栈。

## 运行

源码目录直接跑（需要系统装了 LÖVE 11.5）：

```sh
love love/
```

或者用打好的包：

| 包 | 用法 |
| --- | --- |
| `bluetooth-input-bridge-love-<ver>.love` | 任何装了 LÖVE 11.5 的机器：`love 包.love` |
| `…-linux-x86_64.tar.gz` | 解压后 `./run.sh`；包里自带服务二进制，界面连不上会自动在旁启动 |
| `…-windows-x86_64.zip` | 解压后双击 `BlueBridgeIme.exe`（LÖVE 运行时已熔进 exe）；包里同时带服务与桥两个 exe |
| `…-macos.zip` | 打开 `love.app`；首次运行需右键「打开」绕过 Gatekeeper |

打包：

```sh
./package.sh 0.2.1 dist [Linux 服务二进制路径] [Windows 二进制目录]
```

脚本会下载官方 LÖVE 11.5 运行时（不修改），把 `.love` 附加到 Windows 的 `love.exe`、
放进 macOS 的 `love.app/Contents/Resources/`，Linux 则给 AppImage + `.love`。第 3、4 个
参数可选：给了 Linux 服务二进制，Linux 包里就带上它；给了 Windows 二进制目录（里面要有
`bluetooth-input-bridge-service.exe` 与 `bluetooth-input-bridge-socket.exe`），Windows
包里就带上这两个 exe。CI 的 `love` 作业会同时下载两个平台的产物再打包。

## Windows 用法

Windows 上后端是 Rust 服务 + 一个很小的 socket 桥（把服务的命名管道转成本机
AF_UNIX 套接字，LÖVE 的 LuaJIT 才能直接连）：

| 文件 | 作用 |
| --- | --- |
| `bluetooth-input-bridge-service.exe` | 后端：蓝牙 SDP 注册 / RFCOMM 转发 / SendInput 注入 |
| `bluetooth-input-bridge-socket.exe` | 桥：`\\.\pipe\bluetooth-input-bridge-control` ↔ `<TEMP>\bluetooth-input-bridge-<pid>\control.sock` |
| `BlueBridgeIme.exe` | 界面（LÖVE 运行时） |

1. 下载官方 LÖVE 11.5 的 Windows x64 包（`love-11.5-win64.zip`），解压。
2. 把上面三个 exe 放进同一个目录；如果只有源码，也可以把 `love-11.5\love.exe`
   和 `love\` 目录放在一起，用 `love-11.5\love.exe love` 启动。
3. 双击 `BlueBridgeIme.exe`。界面连不上 socket 时会自己先拉起桥，几秒后还连不上
   再拉起服务；退出界面时会把桥关掉（`bluetooth-input-bridge-socket.exe --stop`）。

注意事项：

- **不需要管理员权限**，也不需要放行防火墙——桥与服务之间走本机命名管道，界面与桥
  之间走本机 Unix 域套接字，都不过网络。
- Windows 10 1803 及以上才有 AF_UNIX 套接字，更老的系统上界面会一直显示未连接。
- 本机实测（Windows 11 / LÖVE 11.5）：界面启动后先拉起桥、约 3 秒内拉起服务，
  日志里能看到 `run start "" /B "…\bluetooth-input-bridge-socket.exe" --parent <pid>`
  然后 `connected …\control.sock`。若卡在「正在启动服务」，按下面两条自查：

  | 现象 | 原因 |
  | --- | --- |
  | `connect` 对不存在的路径也“成功”，随后 `recv failed (10057)` 刷屏 | 用了 `ws.socket(...)`。`ws2_32` 没有这个导出，LuaJIT 会返回 0 且不报错；必须用 `ws.WSASocketA(af, type, 0, nil, 0, 0)`，协议参数是 `0`（传 6 报 `WSAEAFNOSUPPORT 10043`） |
  | 界面一直 `Not connected`、`native` 为假 | `sockaddr_un` 被 POSIX 与 Windows 两段 `cdef` 各声明一次，`ffi.cdef` 整体失败。现在按平台只声明一份 |
- socket 目录默认在 `%TEMP%\bluetooth-input-bridge-<界面进程号>\`。路径受
  `sockaddr_un.sun_path` 的 107 字节限制，如果 `%TEMP%` 特别长，会依次退到
  `%TEMP%\bluetooth-input-bridge-<进程号>.sock`（桥建不出子目录时用的短名）、
  `%TMP%`、`%LOCALAPPDATA%\Temp`、`%USERPROFILE%\AppData\Local\Temp`，兜底是不带
  进程号的 `%TEMP%\bluetooth-input-bridge\control.sock`。界面会把所有候选都试一遍，
  也可以用 `BLUETOOTH_INPUT_BRIDGE_SOCKET_DIR` 指定目录（或直接给完整的 `.sock`
  路径），桥和界面读同一个变量。
- 输入方式用 `SendInput` + `KEYEVENTF_UNICODE` 模拟按键，不走剪贴板，会覆盖当前
  选中的内容。

## 省电与性能

- 刷新率上限 30 FPS，并且只在内容变化时 `present()`，空闲时进程基本不占 CPU
  （实测 10 秒 3 个时钟 tick，约 0.3%）。
- 中文字体只读一次系统 TTC，按窗口缩放后的真实像素字号缓存，避免每个字号复制一遍
  20 MB 字体（实测常驻内存 146 MB）。
- 窗口最小 520×420，缩放时保持等比，多余部分留边。

## 字体

- 拉丁 / 西里尔用包内的 DejaVu Sans（Regular/Bold）。
- 中文、日语、韩语按顺序找系统字体：Noto CJK、文泉驿、Droid Sans Fallback、
  macOS 的 PingFang/STHeiti、Windows 的微软雅黑/黑体。找不到 CJK 字体时会退回 DejaVu，
  界面里的汉字会显示成方框。

## 环境变量

| 变量 | 作用 |
| --- | --- |
| `BLUETOOTH_INPUT_BRIDGE_LOCALE` | 强制界面语言：`zh` / `en` / `ru` / `ja` / `ko` |
| `BLUETOOTH_INPUT_BRIDGE_SERVICE` | 指定服务二进制路径（默认在 PATH 和包内查找；Windows 上是 `…-service.exe`） |
| `BLUETOOTH_INPUT_BRIDGE_SOCKET_EXE` | Windows：指定桥 `bluetooth-input-bridge-socket.exe` 的路径 |
| `BLUETOOTH_INPUT_BRIDGE_SOCKET_DIR` | Windows：指定控制套接字目录（默认 `%TEMP%\bluetooth-input-bridge-<pid>\`） |
| `BLUETOOTH_INPUT_BRIDGE_DEBUG=1` | 把连接 / 启动过程的日志打到 stderr |
| `BLUETOOTH_INPUT_BRIDGE_TEST_KEYS=1` | 测试用：`r` 切服务注册、`a` 切开机自启、`p` 触发配对提示 |

界面语言默认跟随 `LANGUAGE` / `LC_ALL` / `LC_MESSAGES` / `LANG`，匹配不到时英文。

## 限制

- macOS 目前只有界面，后端还没实现。
- Windows 需要 Windows 10 1803+（AF_UNIX 套接字），并且要带上
  `bluetooth-input-bridge-socket.exe`，否则界面连不上服务。
- 服务端返回的错误文本仍是中文，界面原样显示。
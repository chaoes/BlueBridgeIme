# LÖVE 前端

桌面端的轻量界面，用 [LÖVE](https://love2d.org/) 11.5 重写，替代 Tauri 外壳。布局与
Tauri 版一致（状态卡、软件指引、测试输入框），但设计稿按 900×650 画，运行时等比缩放到
窗口大小，适合低分辨率、低配置的电脑。

它只做界面：蓝牙转发、BlueZ 注册、文字注入都在无界面的
`bluetooth-input-bridge-service` 里，通过 `$XDG_RUNTIME_DIR/bluetooth-input-bridge/control.sock`
的 JSON 行协议通信。因此可以随时换成别的前端技术栈。

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
| `…-windows-x86_64.zip` | 解压后双击 `BlueBridgeIme.exe`（LÖVE 运行时已熔进 exe） |
| `…-macos.zip` | 打开 `love.app`；首次运行需右键「打开」绕过 Gatekeeper |

打包：

```sh
./package.sh 0.1.0 dist [服务二进制路径]
```

脚本会下载官方 LÖVE 11.5 运行时（不修改），把 `.love` 附加到 Windows 的 `love.exe`、
放进 macOS 的 `love.app/Contents/Resources/`，Linux 则给 AppImage + `.love`。

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
| `BLUETOOTH_INPUT_BRIDGE_SERVICE` | 指定服务二进制路径（默认在 PATH 和包内查找） |
| `BLUETOOTH_INPUT_BRIDGE_TEST_KEYS=1` | 测试用：`r` 切服务注册、`a` 切开机自启、`p` 触发配对提示 |

界面语言默认跟随 `LANGUAGE` / `LC_ALL` / `LC_MESSAGES` / `LANG`，匹配不到时英文。

## 限制

- Windows、macOS 目前只有界面：它们的蓝牙与注入后端还没实现（见
  [`desktop/docs/windows.md`](../desktop/docs/windows.md)），要配合 Linux 上的服务才能完整工作。
- 服务端返回的错误文本仍是中文，界面原样显示。
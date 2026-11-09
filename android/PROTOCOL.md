# 蓝牙输入桥：Windows 客户端接口协议 v1

## 分工与连接

安卓端是**经典蓝牙 BR/EDR RFCOMM 客户端**，Windows 端是 RFCOMM 服务端。Windows 先启动监听并发布 SDP 服务记录，安卓端扫描或从已配对列表选择电脑，调用 `createRfcommSocketToServiceRecord(UUID)` 连接。仅有 BLE 的设备不适用。

- 服务名称建议：`Bluetooth Input Bridge`
- 服务 UUID：`0000e8b6-0000-1000-8000-00805f9b34fb`
- 安全：使用配对和加密的 RFCOMM 服务；Windows 只接受已授权设备。
- 连接方式：一条双向字节流，安卓主动发起；无需固定 RFCOMM channel，也没有 IP 端口。
- Windows 必须发布与上述 UUID 匹配的 SDP 服务，否则安卓连接会失败。

Windows 可选 WinRT `RfcommServiceProvider.CreateAsync(RfcommServiceId.FromUuid(...))`、`StreamSocketListener.BindServiceNameAsync(...)` 与 `StartAdvertising(...)`。若用 WinSock，创建 `AF_BTH` / `SOCK_STREAM` / `BTHPROTO_RFCOMM` socket，绑定自动分配的 channel，`listen` / `accept`，并用 `WSASetService` 注册 SDP；仅 `bind` 不会发布服务。

## 字节协议

单条消息是**一行 UTF-8 JSON**，以单个 LF (`0A`) 结束。JSON 字符串内的换行由 JSON 转义。双方都应按流式读取和按 LF 组帧，不能假设一次 `recv` 对应一条消息。安卓每次只保留一条未回执消息。`text` UTF-8 编码后最多 **16 KiB**；Windows 应限制整行长度（建议 64 KiB）。

安卓 → Windows：

```json
{"type":"text","id":"08811261-7ee0-49fe-9b40-7186101d932b","text":"你好，世界\n"}
```

Windows → 安卓，成功：

```json
{"type":"ack","id":"08811261-7ee0-49fe-9b40-7186101d932b","ok":true}
```

Windows → 安卓，失败：

```json
{"type":"ack","id":"08811261-7ee0-49fe-9b40-7186101d932b","ok":false,"error":"没有可输入的窗口"}
```

`id` 是每次发送生成的 UUID。Windows 应在完成注入后发送 `ok:true`，失败时返回 `ok:false` 和可读的 `error`；不要在仅收到字节时就确认成功。安卓收到匹配 `id` 的成功回执后清理已发送文本。8 秒无回执时保留草稿并提示用户确认后手动重发。Windows 应在会话内缓存已处理的 `id`，重复收到同一 `id` 时只重发原回执，不重复输入。断线后的手动重发会产生新 `id`，因此超时后用户应先检查目标窗口内容。

协议只发送输入法已提交的文本，不发送拼音组字内容。安卓端从最后一次输入更改开始静默计时，可设 0–3 秒、每档 0.25 秒，默认 1 秒；0 秒表示输入法提交后立即发送。可关闭自动发送，仅使用“立即发送”。每次仍以独立 `text` 帧和 `ack` 回执确认；成功回执后从输入框移除已发送文字，当前内容全部送达时输入框变空。发送期间新增的文字会保留，并在本次回执后重新计时。语音识别结果由当前输入法提交到编辑区后遵循同一规则。

## Windows 端输入约定

收到文本时，针对**当时前台窗口的焦点控件**注入 Unicode 文本，不要将文字输入到客户端自身。可用 `GetForegroundWindow` 与 `GetGUIThreadInfo` 查询焦点；是否为可编辑控件需要 Windows 客户端自行判断，可结合 UI Automation，无法确认时返回失败回执。`SendInput` 加 `KEYEVENTF_UNICODE` 是一种注入方式；它受 Windows 完整性级别限制，某些高权限窗口或特殊输入控件可能拒绝输入。不要为了绕过限制自动提权。收到文本的前后应核对焦点，防止用户切换窗口时把文字送往错误位置。

## Linux / Wayland 适配

安卓端协议与桌面系统无关，**无需修改 APK**。Wayland 桌面可另写 Linux 接收端：用 BlueZ `org.bluez.ProfileManager1.RegisterProfile` 以同一 UUID 注册 RFCOMM 服务，接收 `NewConnection` 提供的文件描述符，按上述 JSON 行协议处理并回传 `ack`。可设置 `RequireAuthentication` 与 `RequireAuthorization`。

现有 [GNOME PC 端实现](../desktop/README.md) 已用 Rust + Tauri 完成上述服务。它使用剪贴板加 `/dev/uinput` 的 Ctrl+V 输入；Linux 的成功回执表示粘贴事件已发送，Wayland 无法通用地确认目标控件实际接受文本。请在目标应用验证输入结果，尤其是终端、密码框和禁用粘贴的控件。

文本注入需要按桌面合成器选实现。支持 `virtual-keyboard-unstable-v1` 的合成器可以使用虚拟键盘协议，但它发送的是按键事件，中文等任意 Unicode 文本需要额外的输入法或剪贴板方案；有的合成器不开放这个协议。另一种是通过 Linux `/dev/uinput` 创建虚拟键盘，但它同样是按键事件，并需要相应设备访问权限。Wayland 不提供一个跨合成器、可任意写入其他应用焦点文本框的通用接口，因此“检测当前输入框并输入”必须在目标 GNOME、KDE、Sway 等环境逐一验证。无法确认输入成功时应返回失败回执，不能仅凭收到蓝牙字节返回成功。

参考：[BlueZ Profile API](https://bluez.readthedocs.io/en/latest/profile-api/)、[Wayland 虚拟键盘协议](https://wayland.app/protocols/virtual-keyboard-unstable-v1)、[Linux uinput](https://docs.kernel.org/input/uinput.html)。

## 对接顺序

1. Windows 服务启动、绑定 RFCOMM、发布 SDP UUID，并在系统蓝牙设置中允许配对/发现。
2. 安卓端授予附近设备权限（Android 11 及以下扫描需定位权限），搜索或选已配对电脑。
3. 连接成功进入输入界面，调用系统输入法打字或语音输入。
4. Windows 按 LF 组帧，解析 `text` 和 `id`，对当前焦点执行输入，回发同 `id` 的 `ack`。
5. 安卓收到成功回执后将已发送内容标绿，继续保留在输入区域；失败或超时保留待发送文字。

## 参考接口

- Android：[连接蓝牙设备](https://developer.android.com/develop/connectivity/bluetooth/connect-bluetooth-devices)、[发现设备](https://developer.android.com/develop/connectivity/bluetooth/find-bluetooth-devices)、[蓝牙权限](https://developer.android.com/develop/connectivity/bluetooth/bt-permissions)
- Windows WinRT：[RFCOMM 服务端](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.rfcomm.rfcommserviceprovider)、[StreamSocketListener](https://learn.microsoft.com/en-us/uwp/api/windows.networking.sockets.streamsocketlistener)
- Windows WinSock：[蓝牙 socket](https://learn.microsoft.com/en-us/windows/win32/bluetooth/bluetooth-and-socket)、[bind 与 SDP](https://learn.microsoft.com/en-us/windows/win32/bluetooth/bluetooth-and-bind)
- Windows 输入：[SendInput](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)、[焦点查询](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getfocus)

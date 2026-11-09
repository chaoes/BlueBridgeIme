# Windows 后端：实现说明

Windows 端已经实现，与 Linux 端共用同一套线路协议（[PROTOCOL.md](../../android/PROTOCOL.md)）、控制协议（[service-api.md](service-api.md)）和前端（`web/`）。本文记录具体做法、与 Linux 的差异，以及实测中发现、值得写下来的坑。

## 组成

```
desktop/src-tauri/src/
  service/win/mod.rs          入口：run()（给 Tauri 外壳用）、run_headless()（给服务二进制用）
  service/win/service.rs      服务状态、配置、控制命令分发、连接状态观察者
  service/win/rfcomm.rs       Win32 蓝牙 RFCOMM 服务端（bind / listen / accept + SDP 登记）
  service/win/session.rs      LF 分帧、按 id 去重、回执顺序；SocketWire 走 recv/send
  service/win/injection.rs    SendInput + KEYEVENTF_UNICODE 文本注入
  service/win/focus.rs        前台窗口/焦点快照与全部安全策略
  service/win/control.rs      命名管道控制通道（\\.\pipe\bluetooth-input-bridge-control）
  service/win/client.rs       界面侧命名管道客户端
  service/win/autostart.rs    HKCU ...\CurrentVersion\Run 开机自启
  service/win/device_name.rs  用 BluetoothGetDeviceInfo 把 MAC 换成设备名
```

**服务与界面分离**：`bluetooth-input-bridge-service.exe` 可以完全无界面运行，界面只是命名管道客户端。关闭界面不会断开已建立的蓝牙会话。

## 蓝牙：为什么不是 WinRT

最初按 `PROTOCOL.md` 里给的 WinRT 方案（`RfcommServiceProvider.CreateAsync` + `StreamSocketListener.BindServiceNameAsync` + `StartAdvertising`）实现，在本机实测走不通：

- `BindServiceNameAsync` 对任意蓝牙 UUID 都返回 `0x8007277D`（`WSATYPE_NOT_FOUND`，找不到指定的类）——`StreamSocketListener` 把服务名当成 IP 服务名去解析了。
- `BindServiceNameWithProtectionLevelAsync` 只接受 `PlainSocket`；传 `BluetoothEncryption*` 直接 `0x80070057`（「IP StreamSocketListener 只能使用未加密的纯文本套接字」）。
- `StartAdvertising` 一律返回 `0xD000000D`（`E_INVALIDARG`，发送无效的参数给服务或功能）。换成本机的标准 `RfcommServiceId::SerialPort()`（0x1101）、穷举 SDK 副本、改 `HKCU\Software\Microsoft\Windows\CurrentVersion\Bluetooth\SupportUnsupportedService` 都是同样的错误。这台机器的 `HKLM\SYSTEM\CurrentControlSet\Services\BTHPORT\Parameters\SupportedServices` 只登记了 `{00001124,00001126,00001103,00001101}` 几个白名单服务，而该项需要管理员权限才能改，没在这台机器上验证成功。

于是改用 Win32 蓝牙 Winsock 路径：`AF_BTH` + `BTHPROTO_RFCOMM` + `WSASetService`。这条路径在本机实测可用。

### 关键实现点

1. **bind 的 port 必须是 `BT_PORT_ANY`（`0xFFFFFFFF`），不能是 0**。微软文档《Bluetooth and Bind》写明：服务端时端口必须是有效通道号或 `BT_PORT_ANY`，`0` 只适用于客户端。用 0 时 `bind` 会成功，但 `getsockname` 拿回的还是 0，随后 `WSASetServiceW` 报 WSA 10022（`提供了一个无效的参数`）——这正是最初 SDP 登记失败的根因。
2. **通道号要问协议栈**：`bind(BT_PORT_ANY)` 之后 `getsockname` 得到实际分配的通道（本机是 4），SDP 记录里必须写这个通道号。
3. **`SOCKADDR_BTH` 是 packed 结构**：对它取字段引用会被 rustc 拒绝（`E0793: reference to field of packed struct is unaligned`），所有字段都按值读出。
4. **登记**：`WSAQUERYSETW{ lpServiceClassId, dwNameSpace: NS_BTH（16，crate 没导出，自己定义）, lpszServiceInstanceName: "Bluetooth Input Bridge", dwNumberOfCsAddrs: 1, lpcsaBuffer: CSADDR_INFO{ LocalAddr: 监听套接字的 SOCKADDR_BTH, iSocketType: SOCK_STREAM, iProtocol: BTHPROTO_RFCOMM } }` + `WSASetServiceW(..., RNRSERVICE_REGISTER, 0)`。
5. **注销只能用 `RNRSERVICE_DELETE`**（文档明确说蓝牙命名空间下 `RNRSERVICE_DEREGISTER` 非法），而且必须带上 `dwNumberOfCsAddrs: 1` 与监听地址，否则系统找不到要删哪条记录（返回 `WSASERVICE_NOT_FOUND`），记录会越堆越多。退出时先注销再 `closesocket`，因为通道号是从套接字上读的。
6. **重复登记不会合并**：系统每次 `RNRSERVICE_REGISTER` 都追加一条记录。后台重试路径因此先删后登记（`republish_service`），保证始终只有一条。实测：运行 2 秒、14 秒、26 秒三个时间点查询，记录数稳定为 1；服务停止后查询为 0。
7. **降级可用**：登记失败不会让服务退出，监听照常进行（已配对、已知服务的手机仍可直连），后台按 1 s → 10 s 退避重试，状态里显示「已监听但未发布到蓝牙服务列表」。

### 实测验证

- `cargo build --bins` 产出的服务启动后日志：
  ```
  后台服务启动
  [蓝牙] SDP 已登记，RFCOMM 通道 4
  [蓝牙] 蓝牙服务状态：已发布 Bluetooth Input Bridge（0000e8b6-0000-1000-8000-00805f9b34fb）
  ```
- 用 `WSALookupServiceBeginW`（与手机侧 SDP 查询同一套 API，`lpszContext` 必须是 `WSAAddressToString` 出来的本机地址）按 UUID 查询本机 SDP 数据库：服务运行时有记录，停止后为 0 条。
- 控制通道：`{"cmd":"status"}` 返回 `{"ok":true,"status":{...}}`，字段与 Linux 完全一致。

## 服务 UUID 改为 16 位基址形式

UUID 从 `e8b6a86a-73ce-4cbb-9ea8-9f9c2fae6d51` 改为 `0000e8b6-0000-1000-8000-00805f9b34fb`（同一 128 位分支的 16 位基址写法）。原因：SDP 的 `ServiceClassIdList` 只存 16/32 位 UUID，Windows 蓝牙栈对这个 48 位私有 UUID 的登记/查询行为不可预期；16 位形式是标准蓝牙基址，各平台处理最一致。**Android 端已同步修改，需要重新构建 APK**，两端 UUID 必须一致。

## 文本注入：为什么不用剪贴板

Linux 端是「wl-copy 写剪贴板 → uinput 发 Ctrl+V → 还原剪贴板」。Windows 端**没有沿用**这种做法，改用 `SendInput` + `KEYEVENTF_UNICODE` 直接把字符事件送进前台窗口的焦点控件：

| 对比项 | 剪贴板 + Ctrl+V | SendInput + KEYEVENTF_UNICODE |
| --- | --- | --- |
| 用户剪贴板 | 被改写再还原，中间窗口期任何程序都可能读到或改动 | 完全不碰 |
| 密码框 | 多数屏蔽粘贴，收不到文字 | 走 `WM_CHAR`，正常输入 |
| 终端 | 需要 `Ctrl+Shift+V`，还得判断目标类型 | 与普通窗口一致 |
| 耗时 | 写 + 等 120 ms + 按键 + 等 25 ms + 还原 500 ms | 实测 12 个码元 8 ms，整条消息通常 < 30 ms |
| 可验证性 | `ok:true` 只代表按键事件发出 | `SendInput` 返回实际注入的事件数，可判断是否被拒 |

实现细节：

- 文本按 UTF-16 码元处理，代理对（emoji）原样成对发送；`\n`（含 `\r\n`）转成回车键（`VK_RETURN`，扫描码 0x1C），单独的 `\r` 丢弃。
- 单条最多 256 码元（`MAX_CODE_UNITS`，约 8 秒回执超时内稳过）；一段文字 ≤ 64 码元时一次性发送，更多时按 8 码元一块、块间 2 ms。
- 每块前后都重新取一次焦点快照，窗口变了立即返回「窗口失去焦点（请重新点击输入框后重试）」，不会把文字打到别的窗口里。
- 环境变量 `BLUETOOTH_BRIDGE_TYPING_IDLE_MS`：设为 >0 时，用户最近有键盘/鼠标动作就等安静下来再输入（0 = 立即，默认；上限 5000）。

## 焦点与安全策略

注入目标 = `GetForegroundWindow()` 的焦点控件（`GetGUIThreadInfo(hwndFocus)`）。拒绝注入的情况，错误文本会原样进回执，手机端直接显示：

| 情况 | 回执里的 `error` |
| --- | --- |
| 锁屏 / 安全桌面（类名 `Windows.UI.Core.CoreWindow`，或不在默认输入桌面且打不开输入桌面） | `系统正在锁定或显示安全桌面，解锁后重试` |
| 前台是本程序自己，或本程序正挡在目标前面 | `当前窗口属于本程序（或本程序正挡在它前面），请把要输入的窗口切到最前面` |
| 目标更高权限（UIPI 会拦截） | `目标窗口权限较高，请以普通权限运行本程序（不要提权）或切换到普通窗口` |
| 目标最小化 | `目标窗口已最小化，请恢复窗口后重试` |
| 焦点落在无输入能力的控件上 | `没有可输入的窗口（请先点击要输入的输入框）` |
| 按住 Shift/Ctrl/Alt/Win | `有按键（Shift/Ctrl/Alt/Win）被按住，请松开后重试` |

程序**永不自动提权**：运行在普通权限下，遇到高权限窗口就报错让用户换窗口。

调试用：`BLUETOOTH_BRIDGE_ALLOW_SHARED_FOREGROUND=1` 可以临时放松「前台属于本程序」这一条（只用于自动化测试）。

## 控制通道与自启

- 命名管道 `\\.\pipe\bluetooth-input-bridge-control`，字节模式，请求/回复各一行 JSON，命令与字段见 [service-api.md](service-api.md)。`subscribe` 先回一行状态，随后推送 `{"event":{...}}`。
- 开机自启：写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的 `BluetoothInputBridge` 值，指向服务二进制；用 `BLUETOOTH_BRIDGE_SERVICE` 环境变量可以覆盖路径（打包/调试时有用）。
- 配置与日志：`%APPDATA%\bluebridge-ime\service.json`（服务注册开关）、`%APPDATA%\bluebridge-ime\service.log`。
- 「允许配对 3 分钟」在 Windows 上打开系统的蓝牙设置页（`ms-settings:bluetooth`）——配对授权由系统负责，程序不去改可发现状态。

## 运行方式

```powershell
cd desktop\src-tauri
cargo build --release
# 只跑服务（无界面）
.\target\release\bluetooth-input-bridge-service.exe
# 跑界面（Tauri）；服务没启动时界面会尝试拉起同目录的服务
.\target\release\bluetooth-input-desktop.exe
```

首次使用：Windows 设置里确认蓝牙已打开 → 手机与本机配对 → 启动服务 → 手机 App 里连接。之后再切换窗口都不需要重新配对。

## 已知限制

- 只读控件（例如某些列表项）接受焦点但没有输入能力，`SendInput` 返回成功而文字被丢弃——没有用 UIA 逐个验证控件可写（跨进程 UIA 查询延迟高，会挤占 8 秒回执预算），这是这里的取舍。
- 目标窗口若在另一个桌面上（虚拟桌面/远程会话），焦点校验会失败并返回错误，不会盲注入。
- SDP 登记失败的机器上手机搜不到服务，但用已知通道直连仍可用（见上文第 7 点）。

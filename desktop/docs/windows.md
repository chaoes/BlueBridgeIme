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

**平台文案由后端提供**：webview 在 Windows 与 Linux 上都是 Chromium 内核，前端靠 User-Agent 分不出系统。所以 `src/lib.rs` 有一个 `get_platform` 命令，返回 `{os, label, registrationNoteKey, pairingHintKey}`——标题栏徽标（Windows 是 `Windows 10/11`，Linux 是 `GNOME · Wayland`）、「服务注册」下面那句「登记到哪个蓝牙服务」、配对失败时提示去哪个设置页，全部按这个返回值查 `web/main.js` 里的 i18n 键，新增平台只在 `get_platform` 里加一个 `cfg` 分支即可。

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

## 会话、并发与自愈

- **一连接一会话**：`accept` 线程把连接交给会话线程（同一时刻只保持一个活动会话，避免手机反复重连时线程堆积）。新连接到来时旧会话先有序收尾（最多 200 ms 等它把最后一条回执写完）。
- **跨会话回执交接**：旧会话里「已经注入、回执还没写出去」的条目会继承给新会话。不继承的话，手机会在超时后重发同一个 `id`，而新会话去重表是空的 → 同一段文字会被二次注入。交接缓存上限 8 条。
- **读超时不是断线**：套接字 5 s 收超时只算「空闲」，连续 60 s 无数据才结束会话并写「空闲 60 秒无数据，判定手机挂起或已离开」。旧实现把读超时当成对端关闭，手机闲着 5 秒就会在界面上闪一次「断开」。
- **停止不挂死**：`Server::stop()` 先置停止标志 → 对监听/会话套接字 `shutdown(SD_BOTH)` → 带 3 s 上限 join → 注销 SDP → `closesocket`（句柄只关一次）。`accept` 循环改成 200 ms 时间片，实测停止在 200 ms 级完成；`SessionTail` 的 `Drop` 兜底保证注入 panic 时也会关闭连接、通知观察者。
- **控制命令不能在 async 工作线程上做阻塞收尾**：`setRegistration(false)` 曾经直接在 tokio worker 线程里调 `stop_server()`（join accept 线程 + 注销 + 关套接字），把这个 worker 钉死，同一连接的回执协程永远轮不到 → 客户端**永久等不到回复**（真机复现过）。现在两个分支都走 `spawn_blocking`，实测 207 ms 返回。
- **控制通道**：`read_line` 有 64 KiB 上限（以前是读完再检查，客户端不发换行可撑爆内存）；`subscribe` 长连接在服务停止时靠 500 ms tick 检查停止标志退出（以前会变成僵尸任务）；界面侧读回复有 20 s 超时，服务卡住时给中文错误而不是永久转圈。
- **节流日志**：坏帧/超长帧/重复的 accept 错误都做了节流，长时间运行不会刷屏。
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

注入目标 = `GetForegroundWindow()` 的焦点控件（`GetGUIThreadInfo(hwndFocus)`）。判定顺序：属于本程序 → 前台被本程序挡住 → 锁屏/安全桌面 → 最小化 → 高权限目标 → 无焦点 → 修饰键被按住 → 焦点不在可输入状态（焦点属于另一个顶级窗口，典型的输入法候选窗/离屏输入）。拒绝注入时错误文本会原样进回执，手机端直接显示：

| 情况 | 回执里的 `error` |
| --- | --- |
| 锁屏 / 安全桌面（类名 `Windows.UI.Core.CoreWindow`，或不在默认输入桌面） | `系统正在锁定或显示安全桌面，解锁后重试` |
| 前台是本程序自己，或本程序正挡在目标前面 | `当前窗口属于本程序（或本程序正挡在它前面），请把要输入的窗口切到最前面` |
| 目标更高权限（UIPI 会拦截） | `目标窗口权限较高，请以普通权限运行本程序（不要提权）或切换到普通窗口` |
| 目标最小化 | `目标窗口已最小化，请恢复窗口后重试` |
| 焦点落在无输入能力的控件上 | `没有可输入的窗口（请先点击要输入的输入框）` |
| 有按键（Shift/Ctrl/Alt/Win）被按住 | `有按键（Shift/Ctrl/Alt/Win）被按住，请松开后重试` |
| 焦点被输入法候选框/另一个顶级窗口截住 | `当前窗口不在输入状态（可能有输入法候选框或窗口正忙），请点一下要输入的输入框后重试` |
| 注入途中焦点变了 | `窗口失去焦点（请重新点击输入框后重试）` |
| `SendInput` 只送进去一部分 | `输入未完成：已输入 X 个事件（共 Y 个），请核对输入框内容后再决定是否重发` |

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

### 安装包（NSIS）

CI 的 `windows` 作业会额外用 `cargo tauri bundle --bundles nsis --ci` 产出
`bluetooth-input-bridge-<ref>-windows-x64-setup.exe`。它由 tauri 的 NSIS 模板生成，
`tauri.conf.json` 里做了这些设置：

```json
"bundle": {
  "active": true,
  "targets": ["nsis"],
  "windows": {
    "nsis": {
      "installMode": "both",
      "languages": ["SimpChinese", "English"],
      "displayLanguageSelector": true,
      "customLanguageFiles": {
        "SimpChinese": "gen/nsis/SimpChinese.nsh",
        "English": "gen/nsis/English.nsh"
      },
      "installerHooks": "gen/nsis/installer-hooks.nsh",
      "template": "generated-nsis/installer.nsi"
    }
  }
}
```

要点：

- **可自定义安装位置**：标准 `MUI_PAGE_DIRECTORY` 页面会出现在安装模式之后，点「浏览」即可改目录；`installMode: "both"` 还让用户在「仅为我安装」（无 UAC、装到 `%LOCALAPPDATA%`）/「所有用户」（需要 UAC、装到 `Program Files`）之间选。该页的 `MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive` 由补丁去掉，所以 `/P`、`/UPDATE` 分支下也会显示目录页；静默安装 `/S` 本来就不显示任何页面，用 `/D=路径` 指定目录。
- **界面是 GUI 子系统、没有控制台窗口**：`src/main.rs` 里有 `#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]`。Tauri v2 不再自动加这个属性（v1 的模板自带），漏了它双击 exe 会附带一个黑色控制台窗口；可以看 PE 头的 `Subsystem` 字段（2 = WINDOWS，3 = CONSOLE）确认。无界面服务 `bluetooth-input-bridge-service.exe` 反过来必须留在 CONSOLE 子系统，日志才打得出来。
- **安装包是 perMachine、RequireAdministrator 清单**：tauri 的 bundler 会写一份
  “elevated”应用清单交给 `tauri-build`，这样 NSIS 的提权检测不会误判为绕过。
- **目录页文案与语言选择框由 `build.rs` 定制**：tauri 的模板里这两处是固定的，而 `MUI_DIRECTORYPAGE_TEXT_*`、`MUI_LANGDLL_ALWAYSSHOW` 都是“立即生效”的宏，必须在 `!insertmacro MUI_PAGE_DIRECTORY` / `.onInit` 之前出现，事后用 `--define` 补不进去。所以 `build.rs` 在本地 cargo registry 里找 `tauri-bundler` 的 `installer.nsi`，按 `gen/nsis/directory-page.patch.nsi`、`gen/nsis/language-selector.patch.nsi`（`=== ANCHOR ===` / `=== REPLACE ===` 两段）做**文本替换**，结果写到 `src-tauri/generated-nsis/installer.nsi`（生成的，已 gitignore），再由 `template` 字段引用。模板升级导致锚点对不上时，`build.rs` 打印警告并跳过该定制，构建不会失败。
- **双语界面**：`customLanguageFiles` 里的 `.nsh` 是 tauri 自带语言表的副本（`{{product_name}}` 已换成产品名——这些文件不做模板替换），并补了模板读的旧拼写键 `choowHowToInstall`。打包时 makensis 会对该键报 `warning 6030 set multiple times`，属于 tauri 自己也有的已知噪音，不影响产物。
- **钩子脚本** `desktop/src-tauri/gen/nsis/installer-hooks.nsh`：只清理旧自启值、记录安装目录（`HKCU\Software\bluebridge-ime\InstallDir`）、安装/卸载时结束残留进程、卸载时摘掉自启值。默认**不**写 `Run` 自启，常驻与否交给界面开关。
- **CI 里用 `bundle` 而不是 `build`**：`cargo install tauri-cli --locked`（CI 用 `taiki-e/install-action` 装），然后 `cargo tauri bundle --bundles nsis --ci`。界面二进制和测试已经由前面的 `cargo build --release --bins` 产出，CLI 只把它打进安装包；`tauri build` 会重新编译一遍，没必要，`--no-bundle` 也不是“不重新编译”的意思。
  为了让普通 `cargo build` 能编译进前端资源，`build.rs` 会把 `web/` 复制到
  `src-tauri/generated-dist/` 并设置 `TAURI_ENV_FRONTEND_DIST`；用 CLI 构建时这个变量已由 CLI 提供，`build.rs` 会跳过复制。
- **`gen/nsis/` 进仓库**：`desktop/.gitignore` 排除了整个 `src-tauri/gen`（里面是 tauri 生成的 schema），而 git 不会进入被排除的目录，所以根级 `!` 规则救不回来。实际做法是提交一个嵌套的 `src-tauri/gen/.gitignore`（`*`、`!.gitignore`、`!nsis/`、`!*.nsh`、`!*.nsi`），并在这几个文件首次加入时用 `git add -f` —— 一旦被跟踪，之后的增删就正常了。
- 安装包没有代码签名，首次运行会出现 SmartScreen“未知发布者”；GitHub Release 附带的校验和与 tag 是当前唯一可信来源。

## 已知限制

- 只读控件（例如某些列表项）接受焦点但没有输入能力，`SendInput` 可能返回成功而文字被丢弃——没有用 UIA 逐个验证控件可写（跨进程 UIA 查询延迟高，会挤占 8 秒回执预算），这是这里的取舍。
- 锁屏瞬间注入的极端竞态：`SendInput` 可能静默成功但字符被系统丢弃。代码只能做锁屏/安全桌面的预检，无法事后确认。
- 蓝牙协议栈极端异常时，会话线程对 `recv` 的调用可能迟迟不返回；`stop()` 不会因此阻塞（有 3 s 上限和 `SessionTail` 兜底），但该线程会晚退出，最坏情况需重启进程。
- 目标窗口若在另一个桌面上（虚拟桌面/远程会话），焦点校验会失败并返回错误，不会盲注入。
- SDP 登记失败的机器上手机搜不到服务，但用已知通道直连仍可用（见上文第 7 点）。

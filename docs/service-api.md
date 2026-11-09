# 控制 socket 协议

前端和服务之间只有这一层接口。任何语言的前端都可以通过它驱动服务；服务本身不依赖 Tauri、窗口或 WebView。

## 位置与传输

- 路径：`$XDG_RUNTIME_DIR/bluetooth-input-bridge/control.sock`（通常为 `/run/user/<uid>/...`）。`XDG_RUNTIME_DIR` 不可用或不在 `/run/user` 下时，目录名追加 `-<uid>`。
- 每个请求是一行 UTF-8 JSON，服务回一行 JSON；请求之间互不影响，可以并发。
- `subscribe` 是长连接：先回一行初始状态，之后持续推送变化，客户端断开即取消。

## 请求

| `cmd` | 额外字段 | 说明 |
| --- | --- | --- |
| `status` | — | 读取当前状态 |
| `subscribe` | — | 首行状态 + 后续事件流 |
| `setRegistration` | `enabled: bool` | 注册/注销 BlueZ Profile（手机能否发现本机） |
| `setAutostart` | `enabled: bool` | 写入并启用/停用 systemd 用户服务 |
| `enablePairing` | — | 可发现 + 可配对 180 秒 |
| `sendTest` | `text: string` | 经同一粘贴通道注入当前焦点，不超过 16 KiB |

## 响应

普通请求：

```json
{"ok": true, "status": { ... }}
```

失败：

```json
{"ok": false, "error": "具体原因", "status": { ... }}
```

订阅事件（`subscribe` 连接上）：

```json
{"event": { ...status... }}
```

## status 字段

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `serviceRunning` | bool | 服务进程是否在运行（由前端在失联时置 false） |
| `listening` | bool | Profile 是否已发布、能否接受手机连接 |
| `registrationEnabled` | bool | 用户的注册开关（持久化） |
| `autostartEnabled` | bool | systemd 用户服务是否 enabled |
| `connectedDevice` | string? | 当前连接的设备名（BlueZ `Alias` → `Name` → MAC 回退） |
| `connectedAddress` | string? | 设备 MAC |
| `receivedCount` | number | 本次服务运行期间收到的文本条数 |
| `lastError` | string? | 最近一次错误（注入、BlueZ、systemd 等） |
| `injectionMethod` | string | 当前平台的注入方式说明 |

## 示例

```python
import json, socket

path = "/run/user/1000/bluetooth-input-bridge/control.sock"
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(path)
s.sendall(json.dumps({"cmd": "status"}).encode() + b"\n")
print(json.loads(s.recv(65536).split(b"\n")[0]))
```

```bash
# 连续观察状态
printf '{"cmd":"subscribe"}\n' | socat - UNIX-CONNECT:/run/user/1000/bluetooth-input-bridge/control.sock
```

## 持久化与相关文件

- `~/.config/bluetooth-input-bridge/service.json`：`{"registration_enabled": true}`，服务启动时读取。
- `~/.config/systemd/user/bluetooth-input-bridge.service`：开机自启开关写入的 unit。
- `~/.cache/bluetooth-input-bridge/service.log`：界面自动拉起服务时的 stdout/stderr。

## 生命周期

- 同一用户只允许一个服务实例；启动时若 socket 已存在，先探测是否可连接，连接失败才清理陈旧 socket。
- 前端启动时若订阅失败，会每 5 秒尝试拉起同目录的 `bluetooth-input-bridge-service`，并在拉起前发送离线状态。

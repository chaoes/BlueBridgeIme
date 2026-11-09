package com.example.bluetoothinputbridge.ui

import android.Manifest
import android.annotation.SuppressLint
import android.app.Application
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothSocket
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.TextRange
import com.example.bluetoothinputbridge.R
import androidx.core.content.ContextCompat
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.io.BufferedReader
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.util.UUID
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject

private val SERVICE_UUID: UUID = UUID.fromString("0000e8b6-0000-1000-8000-00805f9b34fb")
private const val MAX_TEXT_BYTES = 16 * 1024
private const val CONNECT_TIMEOUT_MILLIS = 8_000L
private const val AUTO_CONNECT_TIMEOUT_MILLIS = 5_000L
private const val AUTO_CONNECT_ATTEMPTS = 3
private const val MAX_HISTORY = 32
private const val TAG = "BridgeViewModel"

private const val KEY_LAST_ADDRESS = "last_device_address"
private const val KEY_LAST_NAME = "last_device_name"
private const val KEY_HISTORY = "device_history"

data class DeviceItem(
  val name: String,
  val address: String,
  val bonded: Boolean,
  val lastConnectedAt: Long = 0L,
)

data class BridgeState(
  val devices: List<DeviceItem> = emptyList(),
  val rememberedDevice: DeviceItem? = null,
  val scanning: Boolean = false,
  val connectedTo: String? = null,
  val connectingTo: String? = null,
  val draft: TextFieldValue = TextFieldValue(),
  val autoSend: Boolean = true,
  val delayMillis: Long = 1000,
  val pendingId: String? = null,
  val status: String = "",
  val showInput: Boolean = false,
  val bluetoothAvailable: Boolean = true,
  val bluetoothEnabled: Boolean = false,
  val permissionsGranted: Boolean = false,
)

private data class HistoryEntry(val address: String, val name: String, val at: Long)

/** 最近连接过的设备排在最前，其余按已配对、名称排序。 */
private val DEVICE_ORDER: Comparator<DeviceItem> =
  compareByDescending<DeviceItem> { it.lastConnectedAt }
    .thenByDescending { it.bonded }
    .thenBy { it.name.lowercase() }

class BridgeViewModel(application: Application) : AndroidViewModel(application) {
  private val app = application
  private val preferences = app.getSharedPreferences("bridge_preferences", Context.MODE_PRIVATE)
  private val adapter: BluetoothAdapter? = application.getSystemService(BluetoothManager::class.java)?.adapter
  private val _state = MutableStateFlow(BridgeState())
  val state = _state.asStateFlow()
  private var socket: BluetoothSocket? = null
  private var connectingSocket: BluetoothSocket? = null
  private var writer: OutputStreamWriter? = null
  private var connectionJob: Job? = null
  private var idleJob: Job? = null
  private var ackJob: Job? = null
  private var sentText: String? = null
  private var registered = false

  /** 本次前台是否还允许自动连接；每次尝试后置为 false，避免重复拨号。 */
  private var autoConnectWanted = true

  /** 用户主动断开后抑制自动连接，直到下次启动应用。 */
  private var suppressAutoConnect = false

  init {
    // 有上次连接记录时，启动即进入输入界面，随后由 refreshDevices 自动连接。
    if (rememberedFrom(emptyList()) != null) {
      _state.value = _state.value.copy(showInput = true, status = "正在准备自动连接…")
    }
  }

  private val receiver = object : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
      when (intent.action) {
        BluetoothDevice.ACTION_FOUND -> {
          val device = if (Build.VERSION.SDK_INT >= 33) {
            intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE, BluetoothDevice::class.java)
          } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra<BluetoothDevice>(BluetoothDevice.EXTRA_DEVICE)
          }
          device?.let { addDevice(it) }
        }
        BluetoothAdapter.ACTION_DISCOVERY_FINISHED -> update { it.copy(scanning = false) }
        BluetoothAdapter.ACTION_STATE_CHANGED -> refreshDevices()
      }
    }
  }

  private fun update(change: (BridgeState) -> BridgeState) { _state.value = change(_state.value) }

  private fun hasPermissions(): Boolean {
    val required = if (Build.VERSION.SDK_INT >= 31) {
      listOf(Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT)
    } else listOf(Manifest.permission.ACCESS_FINE_LOCATION)
    return required.all { ContextCompat.checkSelfPermission(app, it) == PackageManager.PERMISSION_GRANTED }
  }

  fun onAppForeground() {
    val current = _state.value
    if (!suppressAutoConnect && current.connectedTo == null && current.connectingTo == null) {
      autoConnectWanted = true
    }
    refreshDevices()
  }

  @SuppressLint("MissingPermission")
  fun refreshDevices() {
    val granted = hasPermissions()
    val enabled = if (granted) adapter?.isEnabled == true else false
    if (!registered && granted && enabled) {
      val filter = IntentFilter().apply {
        addAction(BluetoothDevice.ACTION_FOUND)
        addAction(BluetoothAdapter.ACTION_DISCOVERY_FINISHED)
        addAction(BluetoothAdapter.ACTION_STATE_CHANGED)
      }
      ContextCompat.registerReceiver(app, receiver, filter, ContextCompat.RECEIVER_EXPORTED)
      registered = true
    }
    val bonded = if (granted && enabled) {
      adapter?.bondedDevices.orEmpty().map { DeviceItem(it.name ?: "未命名设备", it.address, true) }
    } else {
      emptyList()
    }
    val devices = decorate(bonded + _state.value.devices)
    val remembered = rememberedFrom(devices)
    update {
      it.copy(
        bluetoothAvailable = adapter != null,
        bluetoothEnabled = enabled,
        permissionsGranted = granted,
        devices = devices,
        rememberedDevice = remembered,
      )
    }
    if (!granted || !enabled) {
      update { it.copy(showInput = false) }
      return
    }
    val current = _state.value
    if (autoConnectWanted && !suppressAutoConnect && remembered != null &&
      current.connectedTo == null && current.connectingTo == null
    ) {
      autoConnectWanted = false
      connect(remembered, automatic = true)
    }
  }

  @SuppressLint("MissingPermission")
  private fun addDevice(device: BluetoothDevice) {
    if (!hasPermissions()) return
    val item = DeviceItem(device.name ?: app.getString(R.string.unnamed_device), device.address, device.bondState == BluetoothDevice.BOND_BONDED)
    update { current -> current.copy(devices = decorate(current.devices + item)) }
  }

  @SuppressLint("MissingPermission")
  fun scan() {
    refreshDevices()
    if (!hasPermissions() || adapter?.isEnabled != true) return
    adapter.cancelDiscovery()
    val started = adapter.startDiscovery()
    update { it.copy(scanning = started, status = if (started) app.getString(R.string.searching_status) else app.getString(R.string.search_failed)) }
  }

  @SuppressLint("MissingPermission")
  fun connect(item: DeviceItem, automatic: Boolean = false) {
    if (!hasPermissions() || adapter?.isEnabled != true) return
    closeConnection()
    adapter.cancelDiscovery()
    val attempts = if (automatic) AUTO_CONNECT_ATTEMPTS else 1
    update {
      it.copy(
        connectingTo = item.address,
        scanning = false,
        showInput = it.showInput || automatic,
        status = if (automatic) app.getString(R.string.connect_auto_progress, item.name)
        else app.getString(R.string.connect_progress, item.name),
      )
    }
    connectionJob = viewModelScope.launch {
      var lastError: Exception? = null
      for (attempt in 1..attempts) {
        if (!isActive) return@launch
        if (attempt > 1) {
          delay(700L * (attempt - 1))
          update { it.copy(status = app.getString(R.string.connect_attempt, item.name, attempt)) }
        }
        var candidate: BluetoothSocket? = null
        try {
          withContext(Dispatchers.IO) {
            candidate = adapter.getRemoteDevice(item.address).createRfcommSocketToServiceRecord(SERVICE_UUID)
          }
          val target = candidate ?: error(app.getString(R.string.connect_create_failed))
          connectingSocket = target
          val timeout = launch {
            delay(if (automatic) AUTO_CONNECT_TIMEOUT_MILLIS else CONNECT_TIMEOUT_MILLIS)
            runCatching { target.close() }
          }
          try {
            withContext(Dispatchers.IO) { target.connect() }
          } finally {
            timeout.cancel()
          }
          if (!isActive) {
            runCatching { target.close() }
            return@launch
          }
          connectingSocket = null
          socket = target
          writer = OutputStreamWriter(target.outputStream, Charsets.UTF_8)
          val displayName = withContext(Dispatchers.IO) {
            runCatching { adapter.getRemoteDevice(item.address).name }.getOrNull()
          }?.takeIf { it.isNotBlank() } ?: item.name
          recordConnection(displayName, item.address)
          suppressAutoConnect = false
          autoConnectWanted = false
          update {
            it.copy(
              connectedTo = displayName,
              connectingTo = null,
              showInput = true,
              status = if (attempt > 1) app.getString(R.string.connected_auto, displayName)
              else app.getString(R.string.connected, displayName),
            )
          }
          scheduleAutoSend()
          try {
            readAcks(target)
          } catch (error: Exception) {
            Log.w(TAG, "连接读取结束：${error.message}")
          }
          if (socket === target) {
            socket = null
            writer = null
            update { it.copy(status = app.getString(R.string.disconnected), connectedTo = null, pendingId = null, showInput = false) }
          }
          return@launch
        } catch (cancelled: CancellationException) {
          runCatching { candidate?.close() }
          throw cancelled
        } catch (error: Exception) {
          runCatching { candidate?.close() }
          if (connectingSocket === candidate) connectingSocket = null
          lastError = error
          Log.w(TAG, "连接 ${item.address} 第 $attempt 次失败：${error.message}", error)
        }
      }
      if (!isActive) return@launch
      update {
        it.copy(
          status = failureMessage(lastError, automatic),
          connectingTo = null,
          pendingId = null,
        )
      }
    }
  }

  private fun failureMessage(error: Exception?, automatic: Boolean): String {
    val raw = error?.message.orEmpty()
    val detail = when {
      raw.contains("timeout", true) || raw.contains("read failed", true) || raw.contains("socket might closed", true) ->
        app.getString(R.string.failure_timeout)
      raw.contains("refused", true) || raw.contains("rejected", true) || raw.contains("busy", true) ->
        app.getString(R.string.failure_refused)
      raw.isBlank() -> app.getString(R.string.failure_unknown)
      else -> raw
    }
    return if (automatic) app.getString(R.string.connect_failed_auto, detail)
    else app.getString(R.string.connect_failed, detail)
  }

  private suspend fun readAcks(active: BluetoothSocket) {
    withContext(Dispatchers.IO) {
      val reader = BufferedReader(InputStreamReader(active.inputStream, Charsets.UTF_8))
      while (true) {
        val line = reader.readLine() ?: break
        if (line.length > 4096) continue
        val message = runCatching { JSONObject(line) }.getOrNull() ?: continue
        if (message.optString("type") != "ack") continue
        val id = message.optString("id")
        if (id != _state.value.pendingId) continue
        ackJob?.cancel()
        if (message.optBoolean("ok")) {
          val acknowledged = sentText.orEmpty()
          update { current ->
            current.copy(
              pendingId = null,
              draft = if (current.draft.text.startsWith(acknowledged)) {
                val remaining = current.draft.text.removePrefix(acknowledged)
                TextFieldValue(remaining, selection = TextRange(remaining.length))
              } else current.draft,
              status = app.getString(R.string.delivered),
            )
          }
          sentText = null
          scheduleAutoSend()
        } else {
          update { it.copy(pendingId = null, status = app.getString(R.string.input_failed, message.optString("error", app.getString(R.string.unknown_error)))) }
          sentText = null
        }
      }
    }
  }

  fun disconnect() {
    suppressAutoConnect = true
    autoConnectWanted = false
    closeConnection()
    update { it.copy(connectedTo = null, connectingTo = null, pendingId = null, showInput = false, status = app.getString(R.string.disconnected_manual)) }
  }

  private fun closeConnection() {
    idleJob?.cancel()
    ackJob?.cancel()
    connectionJob?.cancel()
    runCatching { connectingSocket?.close() }
    connectingSocket = null
    runCatching { socket?.close() }
    socket = null
    writer = null
    sentText = null
  }

  fun openInput() { if (_state.value.connectedTo != null) update { it.copy(showInput = true) } }

  /** 回到设备列表；正在自动连接时则取消本次连接。 */
  fun showDevices() {
    val current = _state.value
    if (current.connectedTo == null && current.connectingTo != null) {
      connectionJob?.cancel()
      runCatching { connectingSocket?.close() }
      connectingSocket = null
      update { it.copy(connectingTo = null, showInput = false, status = app.getString(R.string.auto_connect_stopped)) }
    } else {
      update { it.copy(showInput = false) }
    }
  }

  /** 输入界面里的“重新连接”，沿用上次连接过的设备。 */
  fun reconnectLast() {
    val target = _state.value.rememberedDevice ?: return
    connect(target, automatic = false)
  }

  fun setAutoSend(enabled: Boolean) { update { it.copy(autoSend = enabled) }; scheduleAutoSend() }
  fun setDelayMillis(milliseconds: Long) { update { it.copy(delayMillis = (milliseconds / 250).coerceIn(0, 12) * 250) }; scheduleAutoSend() }

  fun changeDraft(value: TextFieldValue) {
    val inFlight = sentText
    if (_state.value.pendingId != null && inFlight != null && !value.text.startsWith(inFlight)) return
    update { it.copy(draft = value) }
    scheduleAutoSend()
  }

  private fun scheduleAutoSend() {
    idleJob?.cancel()
    val current = _state.value
    if (!current.autoSend || current.connectedTo == null || current.pendingId != null ||
      current.draft.text.isEmpty() || current.draft.composition != null) return
    idleJob = viewModelScope.launch {
      delay(current.delayMillis)
      val latest = _state.value
      if (latest.draft.text == current.draft.text && latest.draft.composition == null) send()
    }
  }

  fun send() {
    val current = _state.value
    val text = current.draft.text
    if (current.connectedTo == null || current.pendingId != null || text.isEmpty() || current.draft.composition != null) return
    if (text.toByteArray(Charsets.UTF_8).size > MAX_TEXT_BYTES) {
      update { it.copy(status = app.getString(R.string.text_too_long)) }
      return
    }
    idleJob?.cancel()
    val id = UUID.randomUUID().toString()
    val frame = JSONObject().put("type", "text").put("id", id).put("text", text).toString() + "\n"
    sentText = text
    update { it.copy(pendingId = id, status = app.getString(R.string.sending)) }
    viewModelScope.launch {
      try {
        withContext(Dispatchers.IO) {
          val output = writer ?: error(app.getString(R.string.connection_closed))
          output.write(frame)
          output.flush()
        }
        ackJob = viewModelScope.launch {
          delay(8000)
          if (_state.value.pendingId == id) {
            update { it.copy(pendingId = null, status = app.getString(R.string.ack_timeout)) }
            sentText = null
          }
        }
      } catch (error: Exception) {
        val reason = error.message ?: app.getString(R.string.disconnected)
        sentText = null
        disconnect()
        update { it.copy(status = app.getString(R.string.send_failed, reason)) }
      }
    }
  }

  override fun onCleared() {
    disconnect()
    if (registered) app.unregisterReceiver(receiver)
    super.onCleared()
  }

  private fun history(): List<HistoryEntry> {
    val raw = preferences.getString(KEY_HISTORY, null) ?: return emptyList()
    return runCatching {
      val array = JSONArray(raw)
      buildList {
        for (index in 0 until array.length()) {
          val item = array.optJSONObject(index) ?: continue
          val address = item.optString("address").uppercase().takeIf { it.isNotBlank() } ?: continue
          val at = item.optLong("at")
          if (at <= 0L) continue
          add(HistoryEntry(address, item.optString("name"), at))
        }
      }
    }.getOrDefault(emptyList())
  }

  /** 把本次成功连接写入历史，更新排序与“上次连接”设备。 */
  private fun recordConnection(name: String, address: String) {
    val normalized = address.uppercase()
    val now = System.currentTimeMillis()
    val entries = (history().filterNot { it.address == normalized } + HistoryEntry(normalized, name, now))
      .sortedByDescending { it.at }
      .take(MAX_HISTORY)
    val array = JSONArray()
    entries.forEach { entry ->
      array.put(JSONObject().put("address", entry.address).put("name", entry.name).put("at", entry.at))
    }
    preferences.edit()
      .putString(KEY_HISTORY, array.toString())
      .putString(KEY_LAST_ADDRESS, normalized)
      .putString(KEY_LAST_NAME, name)
      .apply()
    val item = DeviceItem(name, normalized, bonded = false, lastConnectedAt = now)
    update { current -> current.copy(devices = decorate(current.devices + item), rememberedDevice = item) }
  }

  /** 合并已配对、已发现和历史设备，并补上最近连接时间后排序。 */
  private fun decorate(items: List<DeviceItem>): List<DeviceItem> {
    val entries = history()
    val byAddress = LinkedHashMap<String, DeviceItem>()
    fun put(item: DeviceItem) {
      val key = item.address.uppercase()
      val previous = byAddress[key]
      byAddress[key] = if (previous == null) {
        item.copy(address = key)
      } else {
        previous.copy(
          name = previous.name.takeIf { it.isNotBlank() && it != "未命名设备" } ?: item.name,
          bonded = previous.bonded || item.bonded,
          lastConnectedAt = maxOf(previous.lastConnectedAt, item.lastConnectedAt),
        )
      }
    }
    items.forEach(::put)
    entries.forEach { entry ->
      val existing = byAddress[entry.address]
      put(
        DeviceItem(
          name = existing?.name?.takeIf { it.isNotBlank() } ?: entry.name.ifBlank { "上次连接的设备" },
          address = entry.address,
          bonded = existing?.bonded ?: false,
          lastConnectedAt = entry.at,
        )
      )
    }
    return byAddress.values.sortedWith(DEVICE_ORDER)
  }

  private fun rememberedFrom(devices: List<DeviceItem>): DeviceItem? {
    val entries = history()
    val savedAddress = preferences.getString(KEY_LAST_ADDRESS, null)
      ?.uppercase()
      ?.takeIf { it.isNotBlank() }
    val address = savedAddress ?: entries.maxByOrNull { it.at }?.address ?: return null
    val entry = entries.firstOrNull { it.address == address }
    return devices.firstOrNull { it.address == address } ?: DeviceItem(
      name = entry?.name?.takeIf { it.isNotBlank() }
        ?: preferences.getString(KEY_LAST_NAME, null)?.takeIf { it.isNotBlank() }
        ?: "上次连接的设备",
      address = address,
      bonded = false,
      lastConnectedAt = entry?.at ?: 0L,
    )
  }
}
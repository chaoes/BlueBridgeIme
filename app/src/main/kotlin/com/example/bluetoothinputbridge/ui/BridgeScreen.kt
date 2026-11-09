package com.example.bluetoothinputbridge.ui

import android.text.format.DateUtils
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.example.bluetoothinputbridge.R
import kotlin.math.roundToInt
import kotlinx.coroutines.delay

private val Mint = Color(0xFF8DD3C2)
private val Delivered = Color(0xFF76D8AF)
private val Canvas = Color(0xFF10131B)
private val Panel = Color(0xFF1B2130)
private val Muted = Color(0xFFAEB8C9)

@Composable
fun BridgeScreen(
  model: BridgeViewModel,
  onRequestPermissions: () -> Unit,
  onEnableBluetooth: () -> Unit,
) {
  val state by model.state.collectAsStateWithLifecycle()
  val keyboardOpen = WindowInsets.ime.getBottom(LocalDensity.current) > 0
  val scroll = rememberScrollState()
  LaunchedEffect(state.showInput, keyboardOpen) { scroll.scrollTo(0) }
  Column(
    modifier = Modifier.fillMaxSize()
      .background(Brush.verticalGradient(listOf(Color(0xFF182B35), Canvas, Canvas)))
      .windowInsetsPadding(WindowInsets.safeDrawing)
      .imePadding()
      .verticalScroll(scroll)
      .padding(horizontal = 20.dp, vertical = if (keyboardOpen) 12.dp else 24.dp),
    verticalArrangement = Arrangement.spacedBy(if (keyboardOpen) 12.dp else 18.dp),
  ) {
    if (!keyboardOpen || !state.showInput) BrandHeader(compact = keyboardOpen)
    if (state.showInput) InputPanel(state, model, keyboardOpen)
    else DevicePanel(state, model, onRequestPermissions, onEnableBluetooth)
    Spacer(Modifier.height(12.dp))
  }
}

@Composable
private fun BrandHeader(compact: Boolean) {
  Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(14.dp)) {
    Box(
      modifier = Modifier.size(if (compact) 42.dp else 58.dp)
        .background(Brush.linearGradient(listOf(Mint, Color(0xFF72A4D9))), RoundedCornerShape(17.dp)),
      contentAlignment = Alignment.Center,
    ) { Text("⌨", color = Color(0xFF10232D), style = MaterialTheme.typography.headlineMedium) }
    Column {
      if (!compact) Text("BLUETOOTH INPUT BRIDGE", color = Mint, style = MaterialTheme.typography.labelSmall)
      Text(
        stringResource(R.string.app_name),
        color = Color.White,
        style = if (compact) MaterialTheme.typography.titleLarge else MaterialTheme.typography.headlineSmall,
      )
      if (!compact) Text(stringResource(R.string.tagline), color = Muted, style = MaterialTheme.typography.bodySmall)
    }
  }
}

@Composable
private fun DevicePanel(
  state: BridgeState,
  model: BridgeViewModel,
  onRequestPermissions: () -> Unit,
  onEnableBluetooth: () -> Unit,
) {
  PanelCard {
    Text(stringResource(R.string.connect_title), style = MaterialTheme.typography.titleLarge)
    Text(state.status.ifBlank { stringResource(R.string.status_ready) }, color = Mint, style = MaterialTheme.typography.bodyMedium)
    if (state.connectingTo != null) {
      Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        CircularProgressIndicator(Modifier.size(22.dp), strokeWidth = 2.dp)
        Text(stringResource(R.string.status_connecting_secure), color = Muted)
      }
    }
    when {
      !state.bluetoothAvailable -> Text(stringResource(R.string.status_unsupported), color = Muted)
      !state.permissionsGranted -> {
        Text(stringResource(R.string.status_permissions), color = Muted)
        Button(onClick = onRequestPermissions) { Text(stringResource(R.string.action_grant)) }
      }
      !state.bluetoothEnabled -> {
        Text(stringResource(R.string.status_bluetooth_off), color = Muted)
        Button(onClick = onEnableBluetooth) { Text(stringResource(R.string.action_enable_bluetooth)) }
      }
      else -> {
        Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
          Button(onClick = model::scan, enabled = !state.scanning && state.connectingTo == null) {
            Text(stringResource(if (state.scanning) R.string.action_searching else R.string.action_search))
          }
          if (state.connectedTo != null) OutlinedButton(onClick = model::openInput) { Text(stringResource(R.string.action_back_to_input)) }
        }
      }
    }
  }
  if (state.permissionsGranted && state.bluetoothEnabled) {
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
      Text(stringResource(R.string.device_list), style = MaterialTheme.typography.titleMedium)
      Text(stringResource(R.string.device_list_subtitle), color = Muted, style = MaterialTheme.typography.bodySmall)
    }
    if (state.devices.isEmpty()) Text(stringResource(R.string.device_list_empty), color = Muted)
    state.devices.forEach { device ->
      val lastConnected = device.lastConnectedAt > 0L
      Card(
        modifier = Modifier.fillMaxWidth().clickable(enabled = state.connectingTo == null) { model.connect(device) },
        shape = RoundedCornerShape(18.dp),
        colors = CardDefaults.cardColors(containerColor = Panel),
        border = BorderStroke(1.dp, if (lastConnected) Color(0xFF3E6B5E) else Color(0xFF303A4D)),
      ) {
        Row(
          modifier = Modifier.fillMaxWidth().padding(18.dp),
          horizontalArrangement = Arrangement.SpaceBetween,
          verticalAlignment = Alignment.CenterVertically,
        ) {
          Column(Modifier.weight(1f)) {
            Text(device.name, style = MaterialTheme.typography.titleMedium)
            Text(device.address, color = Muted, style = MaterialTheme.typography.bodySmall)
          }
          Text(
            when {
              lastConnected -> relativeTime(device.lastConnectedAt)
              device.bonded -> stringResource(R.string.bonded)
              else -> stringResource(R.string.nearby)
            },
            color = if (lastConnected) Delivered else Mint,
            style = MaterialTheme.typography.labelMedium,
          )
        }
      }
    }
    Text(stringResource(R.string.device_list_footer), color = Muted, style = MaterialTheme.typography.bodySmall)
  }
}

@Composable
private fun InputPanel(state: BridgeState, model: BridgeViewModel, keyboardOpen: Boolean) {
  val focus = remember { FocusRequester() }
  val keyboard = LocalSoftwareKeyboardController.current
  val targetName = state.connectedTo
    ?: state.connectingTo?.let { address -> state.devices.firstOrNull { it.address == address }?.name }
    ?: state.rememberedDevice?.name
  LaunchedEffect(state.connectedTo) {
    if (state.connectedTo != null) {
      delay(250)
      focus.requestFocus()
      keyboard?.show()
    }
  }

  if (!keyboardOpen) {
    PanelCard {
      Row(verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
          Text(
            when {
              state.connectedTo != null -> stringResource(R.string.state_connected)
              state.connectingTo != null -> stringResource(R.string.state_connecting)
              else -> stringResource(R.string.state_disconnected)
            },
            color = if (state.connectedTo != null) Delivered else if (state.connectingTo != null) Mint else Muted,
            style = MaterialTheme.typography.labelLarge,
          )
          Text(targetName ?: stringResource(R.string.last_device), style = MaterialTheme.typography.titleMedium)
        }
        OutlinedButton(onClick = model::showDevices) { Text(stringResource(R.string.action_devices)) }
        if (state.connectedTo != null) {
          Spacer(Modifier.width(8.dp))
          OutlinedButton(onClick = model::disconnect) { Text(stringResource(R.string.action_disconnect)) }
        }
      }
      Text(state.status.ifBlank { stringResource(R.string.status_ready) }, color = Muted, style = MaterialTheme.typography.bodySmall)
      if (state.connectedTo == null && state.connectingTo == null && state.rememberedDevice != null) {
        Button(
          onClick = model::reconnectLast,
          modifier = Modifier.fillMaxWidth(),
          colors = ButtonDefaults.buttonColors(containerColor = Mint, contentColor = Color(0xFF102A32)),
        ) { Text(stringResource(R.string.action_reconnect)) }
      }
    }
  }

  PanelCard {
    if (keyboardOpen && state.connectedTo != null) {
      Text("●  ${state.connectedTo}", color = Delivered, style = MaterialTheme.typography.labelMedium)
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
      Text(stringResource(R.string.auto_send), style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
      Switch(checked = state.autoSend, onCheckedChange = model::setAutoSend)
    }
    if (state.autoSend) {
      Text(
        if (state.delayMillis == 0L) stringResource(R.string.auto_send_immediate)
        else stringResource(R.string.auto_send_delay, delayLabel(state.delayMillis)),
        color = Mint,
        style = MaterialTheme.typography.bodyMedium,
      )
      Slider(
        value = state.delayMillis / 1000f,
        onValueChange = { model.setDelayMillis((it * 4).roundToInt() * 250L) },
        valueRange = 0f..3f,
        steps = 11,
      )
      Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
        Text(stringResource(R.string.label_now), color = Muted, style = MaterialTheme.typography.labelSmall)
        Text(stringResource(R.string.label_3s), color = Muted, style = MaterialTheme.typography.labelSmall)
      }
    }
  }

  PanelCard {
    Row(verticalAlignment = Alignment.CenterVertically) {
      Column(Modifier.weight(1f)) {
        Text(stringResource(R.string.input_title), style = MaterialTheme.typography.titleMedium)
        Text(stringResource(R.string.input_subtitle), color = Muted, style = MaterialTheme.typography.bodySmall)
      }
      if (state.pendingId != null) CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
    }
    Box(
      modifier = Modifier.fillMaxWidth()
        .background(Color(0xFF101827), RoundedCornerShape(14.dp))
        .padding(16.dp),
    ) {
      BasicTextField(
        value = state.draft,
        onValueChange = model::changeDraft,
        modifier = Modifier.fillMaxWidth().heightIn(min = if (keyboardOpen) 128.dp else 220.dp, max = 320.dp)
          .focusRequester(focus),
        textStyle = TextStyle(color = Color.White, fontSize = MaterialTheme.typography.bodyLarge.fontSize),
        cursorBrush = SolidColor(Color.White),
        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Default),
        decorationBox = { inner ->
          if (state.draft.text.isEmpty()) {
            Text(
              if (state.connectedTo == null) stringResource(R.string.input_placeholder_waiting)
              else stringResource(R.string.input_placeholder_ready),
              color = Muted,
            )
          }
          inner()
        },
      )
    }
    Button(
      onClick = model::send,
      enabled = state.connectedTo != null && state.pendingId == null && state.draft.text.isNotEmpty() &&
        state.draft.composition == null,
      modifier = Modifier.fillMaxWidth(),
      colors = ButtonDefaults.buttonColors(containerColor = Mint, contentColor = Color(0xFF102A32)),
    ) { Text(stringResource(if (state.pendingId != null) R.string.action_waiting_ack else R.string.action_send_now)) }
  }
}

@Composable
private fun PanelCard(content: @Composable ColumnScope.() -> Unit) {
  Card(
    modifier = Modifier.fillMaxWidth(),
    shape = RoundedCornerShape(20.dp),
    colors = CardDefaults.cardColors(containerColor = Panel),
    border = BorderStroke(1.dp, Color(0xFF303A4D)),
  ) {
    Column(Modifier.fillMaxWidth().padding(18.dp), verticalArrangement = Arrangement.spacedBy(12.dp), content = content)
  }
}

@Composable
private fun relativeTime(at: Long): String {
  val now = System.currentTimeMillis()
  if (now - at < DateUtils.MINUTE_IN_MILLIS) return stringResource(R.string.just_now)
  return DateUtils.getRelativeTimeSpanString(at, now, DateUtils.MINUTE_IN_MILLIS, DateUtils.FORMAT_ABBREV_RELATIVE).toString()
}

private fun delayLabel(milliseconds: Long): String =
  if (milliseconds % 1000 == 0L) "${milliseconds / 1000}" else "${milliseconds / 1000.0}"

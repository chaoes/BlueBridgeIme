package com.example.bluetoothinputbridge

import android.Manifest
import android.bluetooth.BluetoothAdapter
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import com.example.bluetoothinputbridge.ui.BridgeScreen
import com.example.bluetoothinputbridge.ui.BridgeViewModel
import com.example.bluetoothinputbridge.ui.theme.BluetoothInputBridgeTheme

class MainActivity : ComponentActivity() {
  private val model: BridgeViewModel by viewModels()

  override fun onStart() {
    super.onStart()
    model.onAppForeground()
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)
    enableEdgeToEdge()
    setContent {
      val permissions = if (Build.VERSION.SDK_INT >= 31) {
        arrayOf(Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT)
      } else {
        arrayOf(Manifest.permission.ACCESS_FINE_LOCATION)
      }
      val enableBluetooth = rememberLauncherForActivityResult(
        ActivityResultContracts.StartActivityForResult()
      ) { model.refreshDevices() }
      val requestPermissions = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions()
      ) { model.refreshDevices() }
      BluetoothInputBridgeTheme {
        BridgeScreen(
          model = model,
          onRequestPermissions = { requestPermissions.launch(permissions) },
          onEnableBluetooth = {
            enableBluetooth.launch(Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE))
          }
        )
      }
    }
  }
}

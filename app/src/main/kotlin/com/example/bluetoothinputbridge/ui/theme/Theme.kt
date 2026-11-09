/*
 * Copyright 2026 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package com.example.bluetoothinputbridge.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

private val BridgeColors = darkColorScheme(
    primary = Color(0xFF8DD3C2),
    onPrimary = Color(0xFF102A32),
    secondary = Color(0xFF91AECE),
    background = Color(0xFF10131B),
    onBackground = Color(0xFFE9ECF5),
    surface = Color(0xFF1B2130),
    onSurface = Color(0xFFE9ECF5),
    surfaceContainerLow = Color(0xFF151D2B),
    onSurfaceVariant = Color(0xFFAEB8C9),
    outline = Color(0xFF3C6070),
)

@Composable
fun BluetoothInputBridgeTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = BridgeColors,
        typography = Typography,
        content = content
    )
}


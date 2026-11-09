//! Headless Bluetooth bridge service plus its local control-socket client.
//!
//! `linux` owns the BlueZ profile, RFCOMM session and text injection. `client`
//! is the only view the Tauri shell has of it, so replacing the webview stack
//! does not require touching the service.

pub mod client;
pub mod linux;

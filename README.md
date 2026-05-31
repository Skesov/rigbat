# rigbat

System tray battery monitor for gaming peripherals.

## What it does

- Shows battery level of connected peripherals (mice, keyboards, headsets, controllers) in the system tray
- Displays a single tray icon that reflects the charge state of the primary device
- Automatically discovers connected devices on startup — no manual configuration required
- Supports multiple devices simultaneously; switch the primary device from the tray menu
- Sends desktop notifications when battery is low
- Works with wired, wireless, and Bluetooth devices

## Supported connection types

- USB HID (wired and wireless dongles)
- Bluetooth (BLE and BR/EDR)
- Linux kernel power supply (sysfs)

## Requirements

- Linux
- System tray support (StatusNotifierItem / XEmbed)

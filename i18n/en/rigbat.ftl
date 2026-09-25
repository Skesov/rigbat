# English: the reference and fallback catalogue. Add every new message here first.

## Tray menu and tooltip

tray-no-devices = No devices
tray-automatic = Automatic
tray-refresh = Refresh
tray-dashboard = Device overview…
tray-settings = Settings…
tray-quit = Quit

## A device's state, as the tray menu, tooltip and settings window read it

state-charging = charging
state-discharging = discharging
state-full = full

age-just-now = just now
age-minutes = { $count }m ago
age-hours = { $count }h ago
age-days = { $count }d ago

estimate-minutes = ~{ $count }m
estimate-hours = ~{ $count }h
estimate-over-days = >{ $count }d

entry-online = { $name }: { $percent }%  { $state }
entry-online-estimate = { $name }: { $percent }%  { $state }  { $estimate } left
entry-offline = { $name }: offline
entry-offline-retained = { $name }: { $percent }%  offline ({ $age })
entry-no-access = { $name }: no access (run rigbat doctor)

note-last-reading = last reading { $age }
note-no-access = run rigbat doctor
note-remaining = { $estimate } left

kind-mouse = mouse
kind-keyboard = keyboard
kind-headset = headset
kind-controller = controller
kind-other = other

## Desktop notification

notify-low-title = { $name } battery low
notify-low-body = { $percent }% remaining

## Settings window: shared

tab-general = General
tab-devices = Devices
button-refresh = Refresh
button-refreshing = Refreshing…
settings-title = rigbat — settings

## Settings window: General tab

group-tray = Tray
tray-icon-style = Icon style
display-icon-only = Battery icon only
display-percent-only = Percentage as text
display-percent-in-icon = Percentage inside icon
tray-per-device = One icon per device
tray-per-device-hint = Choose which on the Devices tab.
tray-primary-hint-pinned = The single icon shows { $name }. Change it on the Devices tab.
tray-primary-hint-auto = The single icon shows the connected device with the lowest charge. Pin one on the Devices tab.
button-clear = Clear

group-battery = Battery
default-low-threshold = Low battery threshold
default-poll-interval = Check every
poll-interval-hint = Checking more often drains the device's battery.
interval-seconds = { $count } s
interval-minutes = { $count } min
interval-hours = { $count } h
defaults-hint = Applies to every device that has no setting of its own. To change one device, open it on the Devices tab.

notifications-enabled = Low battery notifications

group-system = System
autostart-enabled = Start with session
autostart-managed-by-systemd = Managed by rigbat.service
autostart-systemd-disable = Disable it with: systemctl --user disable --now rigbat.service

# Bilingual on purpose: findable whichever language is active.
section-language = Language / Язык
language-system = System

about-project-page = Project page

## Settings window: Devices tab

device-search-hint = Search devices…
devices-empty = No devices recorded yet. Connect a device, then press Refresh.
devices-no-match = No devices match your search.
devices-tray-unanswered = The running tray did not answer; showing the devices it listed last.
devices-connected = Connected now
devices-seen-before = Seen before
device-seen-ago = seen { $age }
device-show-in-tray = Show in tray

presence-online = Online
presence-unreachable = Unreachable
presence-disconnected = Disconnected
presence-no-access = No access

device-pin = Show on the single icon
device-pin-hint = Applies when the tray shows one icon for all devices.
device-uses-default = The default for all devices
device-default-value = Default: { $value }
device-reset = Use the default
device-remove-title = Remove from the list
device-remove-hint = Its history goes too. It returns if it is seen again.
device-remove = Remove…
device-remove-confirm = Remove
button-cancel = Cancel

## Dashboard (left click on the tray icon)

dashboard-title = rigbat — device overview
dashboard-tray-not-running = The rigbat tray is not running.
dashboard-in-tray = in tray

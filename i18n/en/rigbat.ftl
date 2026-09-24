# English: the reference and fallback catalogue. Add every new message here first.

## Tray menu and tooltip

tray-no-devices = No devices
tray-refresh = Refresh
tray-dashboard = Device overview…
tray-settings = Settings…
tray-quit = Quit

## A device's state, as the tray menu, tooltip and settings table read it

state-charging = charging
state-discharging = discharging
state-full = full

age-just-now = just now
age-minutes = { $count }m ago
age-hours = { $count }h ago
age-days = { $count }d ago

estimate-minutes = ~{ $count }m
estimate-hours = ~{ $count }h

entry-online = { $name }: { $percent }%  { $state }
entry-online-estimate = { $name }: { $percent }%  { $state }  { $estimate } left
entry-offline = { $name }: offline
entry-offline-retained = { $name }: { $percent }%  offline ({ $age })
entry-no-access = { $name }: no access (run rigbat doctor)

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
status-saved = Changes saved.
button-close = Close
button-refresh = Refresh
button-refreshing = Refreshing…
unit-seconds-suffix = { " " }s

## Settings window: General tab

section-tray-display = Tray display
display-icon-only = Battery icon only
display-percent-only = Percentage as text
display-percent-in-icon = Percentage inside icon
tray-per-device = Show one icon per device
tray-per-device-hint = Choose which devices get an icon on the Devices tab.
tray-primary-hint-pinned = The single icon shows { $name }. Change it on the Devices tab.
tray-primary-hint-auto = The single icon shows the first connected device. Pin one on the Devices tab.
button-clear = Clear

section-defaults = Defaults for all devices
default-low-threshold = Low battery threshold
default-poll-interval = Check every
poll-interval-hint = Polling more often than this wakes the device constantly and drains its battery.
defaults-hint = Applies to every device that has no setting of its own. To change one device, select its row on the Devices tab.

section-notifications = Notifications
notifications-enabled = Low battery notifications

section-startup = Startup
autostart-enabled = Start with session
autostart-managed-by-systemd = Managed by the systemd user service. Disable it with: systemctl --user disable --now rigbat.service

# Bilingual on purpose: findable whichever language is active.
section-language = Language / Язык
language-system = System

section-about = About
about-project-page = Project page

## Settings window: Devices tab

device-search-hint = Search devices…
devices-empty = No devices recorded yet. Connect a device, then press Refresh.
devices-no-match = No devices match your search.

col-name = Name
col-type = Type
col-connection = Connection
col-charge = Charge
col-status = Status
col-first-seen = First seen
col-last-seen = Last seen
col-tray-icon = Tray icon
col-actions = Actions

presence-online = Online
presence-unreachable = Unreachable
presence-disconnected = Disconnected
presence-no-access = No access

button-delete = Delete
button-confirm = Yes
button-cancel = No

detail-use-for-single-icon = Use for the single tray icon
detail-single-icon-hint = Applies when the tray shows one icon for all devices.
detail-override-threshold = Override low battery threshold
detail-default-threshold = Use default ({ $percent }%)
detail-override-interval = Override poll interval
detail-default-interval = Use default ({ $secs } s)

## Dashboard (left click on the tray icon)

dashboard-title = rigbat — devices
dashboard-tray-not-running = The rigbat tray is not running.
dashboard-last-reading = last reading { $age }
dashboard-no-access-hint = run rigbat doctor
dashboard-in-tray = in tray

# Russian. Units are abbreviated, so no plural forms are needed.

## Tray menu and tooltip

tray-no-devices = Нет устройств
tray-automatic = Автоматически
tray-refresh = Обновить
tray-dashboard = Обзор устройств…
tray-settings = Настройки…
tray-quit = Выход

## A device's state, as the tray menu, tooltip and settings window read it

state-charging = заряжается
state-discharging = разряжается
state-full = заряжено

age-just-now = только что
age-minutes = { $count } мин назад
age-hours = { $count } ч назад
age-days = { $count } д назад

estimate-minutes = ~{ $count } мин
estimate-hours = ~{ $count } ч
estimate-over-days = >{ $count } д

entry-online = { $name }: { $percent }%  { $state }
entry-online-estimate = { $name }: { $percent }%  { $state }  осталось { $estimate }
entry-offline = { $name }: не на связи
entry-offline-retained = { $name }: { $percent }%  не на связи ({ $age })
entry-no-access = { $name }: нет доступа (запустите rigbat doctor)

note-last-reading = последние данные { $age }
note-no-access = запустите rigbat doctor
note-remaining = осталось { $estimate }

kind-mouse = мышь
kind-keyboard = клавиатура
kind-headset = гарнитура
kind-controller = геймпад
kind-other = другое

## Desktop notification

notify-low-title = { $name }: низкий заряд
notify-low-body = Осталось { $percent }%

## Settings window: shared

tab-general = Основное
tab-devices = Устройства
button-refresh = Обновить
button-refreshing = Обновление…
settings-title = rigbat — настройки

## Settings window: General tab

group-tray = Трей
tray-icon-style = Вид значка
display-icon-only = Только значок
display-percent-only = Проценты текстом
display-percent-in-icon = Проценты внутри значка
tray-per-device = Значок на каждое устройство
tray-per-device-hint = Какие именно — на вкладке «Устройства».
tray-primary-hint-pinned = Общий значок показывает { $name }. Сменить — на вкладке «Устройства».
tray-primary-hint-auto = Общий значок показывает подключённое устройство с самым низким зарядом. Закрепить — на вкладке «Устройства».
button-clear = Сбросить

group-battery = Батарея
default-low-threshold = Порог низкого заряда
default-poll-interval = Опрашивать каждые
poll-interval-hint = Более частый опрос сажает батарею устройства.
interval-seconds = { $count } с
interval-minutes = { $count } мин
interval-hours = { $count } ч
defaults-hint = Действует для всех устройств без собственных настроек. Чтобы настроить одно устройство, откройте его на вкладке «Устройства».

notifications-enabled = Уведомлять о низком заряде

group-system = Система
autostart-enabled = Запускать при входе в систему
autostart-managed-by-systemd = Управляется службой rigbat.service
autostart-systemd-disable = Отключить: systemctl --user disable --now rigbat.service

section-language = Язык / Language
language-system = Системный

about-project-page = Страница проекта

## Settings window: Devices tab

device-search-hint = Поиск устройств…
devices-empty = Устройств пока нет. Подключите устройство и нажмите «Обновить».
devices-no-match = Ничего не найдено.
devices-tray-unanswered = Запущенный трей не ответил; показаны устройства из его последнего ответа.
devices-connected = Подключены сейчас
devices-seen-before = Замечены раньше
device-seen-ago = замечено { $age }
device-show-in-tray = Показывать в трее

presence-online = На связи
presence-unreachable = Недоступно
presence-disconnected = Отключено
presence-no-access = Нет доступа

device-pin = Показывать на общем значке
device-pin-hint = Действует, когда в трее один значок на все устройства.
device-uses-default = Как для всех устройств
device-default-value = По умолчанию: { $value }
device-reset = Вернуть по умолчанию
device-remove-title = Убрать из списка
device-remove-hint = Вместе с историей. Вернётся, если появится снова.
device-remove = Удалить…
device-remove-confirm = Удалить
button-cancel = Отмена

## Dashboard (left click on the tray icon)

dashboard-title = rigbat — обзор устройств
dashboard-tray-not-running = Трей rigbat не запущен.
dashboard-in-tray = в трее

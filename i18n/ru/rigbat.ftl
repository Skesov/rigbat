# Russian. A count next to a word takes the CLDR plural forms one/few/many; a number and its
# unit are joined by a no-break space (U+00A0).

## Tray menu and tooltip

tray-no-devices = Нет устройств
tray-automatic = Автоматически
tray-refresh = Обновить
tray-dashboard = Обзор устройств
tray-settings = Настройки
tray-quit = Выход

## A device's state, as the tray menu, tooltip and settings window read it

state-charging = заряжается
state-discharging = разряжается
state-full = заряжено

age-just-now = только что
age-minutes = { $count }{" "}{ $count ->
        [one] минуту
        [few] минуты
       *[many] минут
    } назад
age-hours = { $count }{" "}{ $count ->
        [one] час
        [few] часа
       *[many] часов
    } назад
age-days = { $count }{" "}{ $count ->
        [one] день
        [few] дня
       *[many] дней
    } назад

estimate-minutes = ~{ $count }{" "}{ $count ->
        [one] минута
        [few] минуты
       *[many] минут
    }
estimate-hours = ~{ $count }{" "}{ $count ->
        [one] час
        [few] часа
       *[many] часов
    }
estimate-over-days = >{ $count }{" "}{ $count ->
        [one] дня
       *[other] дней
    }

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
notify-low-body = Осталось { $percent }%. Зарядите в ближайшее время.
notify-critical-body = Осталось { $percent }%. Зарядите сейчас.
notify-open-overview = Открыть обзор устройств

## Settings window: shared

tab-general = Основное
tab-appearance = Оформление
tab-devices = Устройства
button-refresh = Обновить
button-refreshing = Обновление…
settings-title = rigbat — настройки

## Settings window: General tab

group-tray = Трей
tray-per-device = Значок на каждое устройство
tray-per-device-hint = Какие именно — на вкладке «Устройства».
tray-primary-hint-pinned = Общий значок показывает { $name }. Сменить — на вкладке «Устройства».
tray-primary-hint-auto = Общий значок показывает подключённое устройство с самым низким зарядом. Закрепить — на вкладке «Устройства».
hide-offline-after = Скрывать отключённое устройство через
hide-offline-after-hint = Оно вернётся, когда снова выйдет на связь.
button-clear = Сбросить

group-battery = Батарея
default-low-threshold = Порог низкого заряда
default-poll-interval = Опрашивать каждые
poll-interval-hint = Более частый опрос быстрее расходует заряд устройства.
# Abbreviated: the value follows «каждые» and «через», where a spelled-out «1 минута» would not agree.
interval-seconds = { $count }{" "}с
interval-minutes = { $count }{" "}мин
interval-hours = { $count }{" "}ч
defaults-hint = Действует для всех устройств без собственных настроек. Чтобы настроить одно устройство, откройте его на вкладке «Устройства».

notifications-enabled = Уведомлять о низком заряде

group-system = Система
autostart-enabled = Запускать при входе в систему
autostart-managed-by-systemd = Управляется службой rigbat.service
autostart-systemd-disable = Отключить: systemctl --user disable --now rigbat.service

section-language = Язык / Language
language-system = Системный

about-github = GitHub
about-commit-copy = Скопировать хэш коммита
about-commit-copied = Скопировано

## Settings window: Appearance tab

group-windows = Окна
window-theme = Тема
window-theme-hint = Для этого окна и обзора устройств. Значок в трее — как в системе.
theme-system = Как в системе
theme-light = Светлая
theme-dark = Тёмная

group-colours = Цвета
appearance-palette = Палитра
appearance-palette-hint = Цвета заряда на значке, в обзоре устройств и здесь.
palette-catppuccin = Catppuccin
palette-everforest = Everforest
palette-gnome = GNOME
palette-nord = Nord

group-tray-icon = Значок в трее
tray-icon-style = Вид значка
display-icon-only = Только значок
display-device-and-battery = Тип устройства и батарейка
display-percent-only = Проценты текстом

## Settings window: Devices tab

device-search-hint = Поиск устройств
devices-empty = Устройств пока нет. Подключите устройство и нажмите «Обновить».
devices-no-match = Ничего не найдено.
devices-tray-unanswered = Запущенный трей не ответил; показаны устройства из его последнего ответа.
devices-connected = Подключены сейчас
devices-seen-before = Замечены раньше
device-seen-ago = замечено { $age }
device-show-in-tray = Показывать в трее

# Lowercase, as after "name: "; `charge_value` capitalises a word that starts its own slot.
presence-online = на связи
presence-unreachable = недоступно
presence-disconnected = отключено
presence-no-access = нет доступа

device-pin = Закрепить на значке
device-pin-hint = Действует, когда в трее один значок на все устройства.
device-uses-default = Как для всех устройств
device-default-value = По умолчанию: { $value }
device-reset = Вернуть по умолчанию
device-remove-title = Убрать из списка
device-remove-hint = Вместе с историей. Вернётся, если появится снова.
device-remove = Убрать…
device-remove-confirm = Убрать
button-cancel = Отмена

## Dashboard (left click on the tray icon)

dashboard-title = rigbat — обзор устройств
dashboard-tray-not-running = Трей rigbat не запущен.
dashboard-in-tray = в трее

# Russian. Units are abbreviated, so no plural forms are needed.

## Tray menu and tooltip

tray-no-devices = Нет устройств
tray-refresh = Обновить
tray-dashboard = Обзор устройств…
tray-settings = Настройки…
tray-quit = Выход

## A device's state, as the tray menu, tooltip and settings table read it

state-charging = заряжается
state-discharging = разряжается
state-full = заряжено

age-just-now = только что
age-minutes = { $count } мин назад
age-hours = { $count } ч назад
age-days = { $count } д назад

estimate-minutes = ~{ $count } мин
estimate-hours = ~{ $count } ч

entry-online = { $name }: { $percent }%  { $state }
entry-online-estimate = { $name }: { $percent }%  { $state }  осталось { $estimate }
entry-offline = { $name }: не на связи
entry-offline-retained = { $name }: { $percent }%  не на связи ({ $age })
entry-no-access = { $name }: нет доступа (запустите rigbat doctor)

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
status-saved = Изменения сохранены.
button-close = Закрыть
button-refresh = Обновить
button-refreshing = Обновление…
unit-seconds-suffix = { " " }с

## Settings window: General tab

section-tray-display = Отображение в трее
display-icon-only = Только иконка
display-percent-only = Проценты текстом
display-percent-in-icon = Проценты внутри иконки
tray-per-device = Отдельная иконка для каждого устройства
tray-per-device-hint = Выберите устройства на вкладке «Устройства».
tray-primary-hint-pinned = Общая иконка показывает { $name }. Сменить — на вкладке «Устройства».
tray-primary-hint-auto = Общая иконка показывает первое подключённое устройство. Закрепить — на вкладке «Устройства».
button-clear = Сбросить

section-defaults = Общие настройки
default-low-threshold = Порог низкого заряда
default-poll-interval = Опрашивать каждые
poll-interval-hint = Более частый опрос постоянно будит устройство и сажает его батарею.
defaults-hint = Действует для всех устройств без собственных настроек. Чтобы настроить одно устройство, выберите его на вкладке «Устройства».

section-notifications = Уведомления
notifications-enabled = Уведомлять о низком заряде

section-startup = Автозапуск
autostart-enabled = Запускать при входе в систему
autostart-managed-by-systemd = Управляется пользовательской службой systemd. Отключить: systemctl --user disable --now rigbat.service

section-language = Язык / Language
language-system = Системный

section-about = О программе
about-project-page = Страница проекта

## Settings window: Devices tab

device-search-hint = Поиск устройств…
devices-empty = Устройств пока нет. Подключите устройство и нажмите «Обновить».
devices-no-match = Ничего не найдено.

col-name = Название
col-type = Тип
col-connection = Связь
col-charge = Заряд
col-status = Состояние
col-first-seen = Впервые
col-last-seen = Замечено
col-tray-icon = В трее
col-actions = Действия

presence-online = На связи
presence-unreachable = Недоступно
presence-disconnected = Отключено
presence-no-access = Нет доступа

button-delete = Удалить
button-confirm = Да
button-cancel = Нет

detail-use-for-single-icon = Показывать в общей иконке
detail-single-icon-hint = Действует, когда в трее одна иконка на все устройства.
detail-override-threshold = Свой порог заряда
detail-default-threshold = По умолчанию ({ $percent }%)
detail-override-interval = Свой интервал опроса
detail-default-interval = По умолчанию ({ $secs } с)

## Dashboard (left click on the tray icon)

dashboard-title = rigbat — устройства
dashboard-tray-not-running = Трей rigbat не запущен.
dashboard-last-reading = последние данные { $age }
dashboard-no-access-hint = запустите rigbat doctor
dashboard-in-tray = в трее

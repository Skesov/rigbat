use crate::sources::{BatteryBackend, bluez::BluezBackend, sysfs::SysfsBackend};

/// Возвращает все зарегистрированные бэкенды в порядке приоритета.
/// Добавить новый вендор = +1 строка здесь.
pub fn backends() -> Vec<Box<dyn BatteryBackend>> {
    vec![Box::new(SysfsBackend), Box::new(BluezBackend)]
}

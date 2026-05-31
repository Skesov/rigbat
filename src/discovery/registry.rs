use crate::sources::{
    BatteryBackend, bluez::BluezBackend, steelseries::SteelSeriesBackend, sysfs::SysfsBackend,
};

/// Returns all registered backends in priority order.
/// Add new vendor = +1 line here.
pub fn backends() -> Vec<Box<dyn BatteryBackend>> {
    vec![
        Box::new(SysfsBackend),
        Box::new(BluezBackend),
        Box::new(SteelSeriesBackend),
    ]
}

use crate::sources::{
    BatteryBackend, bluez::BluezBackend, eightbitdo::EightBitDoBackend,
    steelseries::SteelSeriesBackend, sysfs::SysfsBackend,
};

/// Returns all registered backends in priority order.
/// Add new vendor = +1 line here.
pub fn backends() -> Vec<Box<dyn BatteryBackend>> {
    vec![
        Box::new(SysfsBackend),
        Box::new(BluezBackend),
        Box::new(SteelSeriesBackend),
        Box::new(EightBitDoBackend),
    ]
}

#[cfg(test)]
mod tests {
    use super::backends;

    #[test]
    fn backends_contains_all_expected_names() {
        let names: Vec<&'static str> = backends().iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["sysfs", "bluez", "steelseries", "eightbitdo"]);
    }
}

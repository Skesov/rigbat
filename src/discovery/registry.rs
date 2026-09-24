use crate::sources::{
    BatteryBackend, bluez::BluezBackend, eightbitdo, eightbitdo::EightBitDoBackend,
    hidraw::HidrawDevice, steelseries, steelseries::SteelSeriesBackend, sysfs::SysfsBackend,
};

/// Recognises a `/sys/class/hidraw/<node>` as one of a backend's devices.
pub type HidrawMatcher = fn(&str) -> anyhow::Result<HidrawDevice>;

/// The node matchers of the backends that open `/dev/hidraw*` — the ones a
/// missing udev rule locks out. A new hidraw backend is +1 line here too.
pub fn hidraw_matchers() -> [HidrawMatcher; 2] {
    [steelseries::match_node, eightbitdo::match_node]
}

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

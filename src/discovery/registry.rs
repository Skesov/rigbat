use crate::sources::{
    BatteryBackend, bluez::BluezBackend, eightbitdo::EightBitDoBackend, hidraw::HidrawFamily,
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

/// The device tables of the backends that open `/dev/hidraw*` — the ones a
/// missing udev rule locks out. Derived from [`backends`], so it cannot drift.
pub fn hidraw_families() -> Vec<&'static HidrawFamily> {
    backends()
        .iter()
        .filter_map(|backend| backend.hidraw_family())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backends_contains_all_expected_names() {
        let names: Vec<&'static str> = backends().iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["sysfs", "bluez", "steelseries", "eightbitdo"]);
    }

    #[test]
    fn hidraw_families_come_from_the_hidraw_backends() {
        let vendors: Vec<u16> = hidraw_families().iter().map(|f| f.vendor).collect();
        assert_eq!(vendors, [0x1038, 0x2DC8]);
    }

    #[test]
    fn no_usb_id_is_claimed_twice() {
        let ids: Vec<(u16, u16)> = hidraw_families().iter().flat_map(|f| f.usb_ids()).collect();
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:04x?}");
    }
}

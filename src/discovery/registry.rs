use crate::sources::{
    BatteryBackend, bluez::BluezBackend, eightbitdo::EightBitDoBackend, headsets,
    hidraw::HidrawFamily, kysona::KysonaBackend, steelseries::SteelSeriesBackend,
    sysfs::SysfsBackend,
};

/// Returns all registered backends in priority order.
/// Add new vendor = +1 line here.
pub fn backends() -> Vec<Box<dyn BatteryBackend>> {
    vec![
        Box::new(SysfsBackend),
        Box::new(BluezBackend),
        Box::new(SteelSeriesBackend),
        Box::new(EightBitDoBackend),
        Box::new(headsets::ARCTIS_1),
        Box::new(headsets::ARCTIS_NOVA),
        Box::new(headsets::CORSAIR_VOID),
        Box::new(headsets::LOGITECH_G533),
        Box::new(KysonaBackend),
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
        assert_eq!(
            names,
            vec![
                "sysfs",
                "bluez",
                "steelseries",
                "eightbitdo",
                "steelseries-arctis",
                "steelseries-arctis-nova",
                "corsair-void",
                "logitech-g533",
                "kysona",
            ]
        );
    }

    #[test]
    fn hidraw_families_come_from_the_hidraw_backends() {
        let vendors: Vec<Vec<u16>> = hidraw_families()
            .iter()
            .map(|f| {
                let vendors: std::collections::BTreeSet<u16> =
                    f.usb_ids().map(|(vendor, _)| vendor).collect();
                vendors.into_iter().collect()
            })
            .collect();
        assert_eq!(
            vendors,
            [
                vec![0x1038],
                vec![0x2DC8],
                vec![0x1038],
                vec![0x1038],
                vec![0x1B1C],
                vec![0x046D],
                vec![0x3554, 0x373B],
            ]
        );
    }

    #[test]
    fn no_usb_id_is_claimed_twice() {
        let ids: Vec<(u16, u16)> = hidraw_families().iter().flat_map(|f| f.usb_ids()).collect();
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:04x?}");
    }
}

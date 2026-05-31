use super::types::{BatteryReading, ChargeState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryStatus {
    Offline,
    Ok { percent: u8 },
    Low { percent: u8 },
    Charging { percent: u8 },
}

/// Классифицирует показание для primary-устройства.
/// `reading == None` означает offline (устройство не отвечает).
/// Порядок приоритета: Offline → Charging → Low → Ok.
pub fn classify(reading: Option<BatteryReading>, low_threshold: u8) -> PrimaryStatus {
    match reading {
        None => PrimaryStatus::Offline,
        Some(r) => match r.state {
            ChargeState::Charging => PrimaryStatus::Charging { percent: r.percent },
            _ if r.percent <= low_threshold => PrimaryStatus::Low { percent: r.percent },
            _ => PrimaryStatus::Ok { percent: r.percent },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::{BatteryReading, ChargeState};

    #[test]
    fn none_is_offline() {
        assert_eq!(classify(None, 20), PrimaryStatus::Offline);
    }

    #[test]
    fn charging_above_threshold_is_charging() {
        let r = BatteryReading::new(80, ChargeState::Charging);
        assert_eq!(
            classify(Some(r), 20),
            PrimaryStatus::Charging { percent: 80 }
        );
    }

    #[test]
    fn charging_below_threshold_is_charging_not_low() {
        let r = BatteryReading::new(10, ChargeState::Charging);
        assert_eq!(
            classify(Some(r), 20),
            PrimaryStatus::Charging { percent: 10 }
        );
    }

    #[test]
    fn discharging_below_threshold_is_low() {
        let r = BatteryReading::new(15, ChargeState::Discharging);
        assert_eq!(classify(Some(r), 20), PrimaryStatus::Low { percent: 15 });
    }

    #[test]
    fn discharging_above_threshold_is_ok() {
        let r = BatteryReading::new(50, ChargeState::Discharging);
        assert_eq!(classify(Some(r), 20), PrimaryStatus::Ok { percent: 50 });
    }

    #[test]
    fn at_threshold_is_low() {
        let r = BatteryReading::new(20, ChargeState::Discharging);
        assert_eq!(classify(Some(r), 20), PrimaryStatus::Low { percent: 20 });
    }

    #[test]
    fn full_high_percent_is_ok() {
        let r = BatteryReading::new(100, ChargeState::Full);
        assert_eq!(classify(Some(r), 20), PrimaryStatus::Ok { percent: 100 });
    }

    #[test]
    fn battery_reading_clamps_over_100() {
        let r = BatteryReading::new(150, ChargeState::Discharging);
        assert_eq!(r.percent, 100);
    }
}

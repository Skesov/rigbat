/// BlueZ Battery1 backend — читает заряд Bluetooth-устройств через D-Bus (zbus).
///
/// Протокол:
/// - Сервис: `org.bluez`, system bus.
/// - ObjectManager на `/` → `GetManagedObjects` → все объекты.
/// - Объект интересен, если есть интерфейс `org.bluez.Device1` с `Connected == true`
///   И присутствует интерфейс `org.bluez.Battery1`.
/// - Имя: `Device1.Alias` → `Device1.Name` → адрес из пути объекта.
/// - Заряд: `Battery1.Percentage` (u8).
/// - Charging-инфо у Battery1 нет → всегда `ChargeState::Discharging`.
use anyhow::Context as _;
use zbus::zvariant::OwnedObjectPath;

use crate::domain::{BatteryReading, ChargeState, DeviceInfo, DeviceKind};

use super::{BatteryBackend, BatterySource};

pub struct BluezBackend;

pub struct BluezSource {
    info: DeviceInfo,
    path: OwnedObjectPath,
    conn: zbus::Connection,
}

/// Выбирает имя устройства: Alias → Name → адрес из пути объекта.
pub fn device_name(alias: Option<&str>, name: Option<&str>, addr: &str) -> String {
    alias
        .filter(|s| !s.is_empty())
        .or_else(|| name.filter(|s| !s.is_empty()))
        .unwrap_or(addr)
        .to_owned()
}

#[async_trait::async_trait]
impl BatteryBackend for BluezBackend {
    fn name(&self) -> &'static str {
        "bluez"
    }

    async fn discover(&self) -> Vec<Box<dyn BatterySource>> {
        match discover_inner().await {
            Ok(sources) => sources,
            Err(e) => {
                tracing_or_eprintln(e);
                Vec::new()
            }
        }
    }
}

fn tracing_or_eprintln(e: anyhow::Error) {
    eprintln!("rigbat bluez: {e:#}");
}

async fn discover_inner() -> anyhow::Result<Vec<Box<dyn BatterySource>>> {
    let conn = zbus::Connection::system()
        .await
        .context("connecting to system D-Bus")?;

    let om = zbus::fdo::ObjectManagerProxy::builder(&conn)
        .destination("org.bluez")
        .context("setting destination")?
        .path("/")
        .context("setting path")?
        .build()
        .await
        .context("building ObjectManagerProxy")?;

    let objects = om
        .get_managed_objects()
        .await
        .context("GetManagedObjects on org.bluez")?;

    let mut sources: Vec<Box<dyn BatterySource>> = Vec::new();

    for (path, interfaces) in objects {
        let Some(device_iface) = interfaces.get("org.bluez.Device1") else {
            continue;
        };

        if !interfaces.contains_key("org.bluez.Battery1") {
            continue;
        }

        // Проверить Connected
        let connected = device_iface
            .get("Connected")
            .and_then(|v| bool::try_from(v.clone()).ok())
            .unwrap_or(false);

        if !connected {
            continue;
        }

        let alias = device_iface
            .get("Alias")
            .and_then(|v| String::try_from(v.clone()).ok());

        let name_prop = device_iface
            .get("Name")
            .and_then(|v| String::try_from(v.clone()).ok());

        // Адрес как запасной вариант имени: последний компонент пути
        // BlueZ кодирует адрес в пути: /org/bluez/hciX/dev_XX_XX_XX_XX_XX_XX
        let addr_fallback = path
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or("")
            .replace('_', ":");

        let display_name = device_name(alias.as_deref(), name_prop.as_deref(), &addr_fallback);

        let info = DeviceInfo {
            name: display_name,
            kind: DeviceKind::Other,
        };

        sources.push(Box::new(BluezSource {
            info,
            path: path.clone(),
            conn: conn.clone(),
        }));
    }

    Ok(sources)
}

#[async_trait::async_trait]
impl BatterySource for BluezSource {
    fn device(&self) -> &DeviceInfo {
        &self.info
    }

    async fn poll(&mut self) -> anyhow::Result<BatteryReading> {
        // Проверить Connected
        let props_device = zbus::fdo::PropertiesProxy::builder(&self.conn)
            .destination("org.bluez")
            .context("setting destination for Device1")?
            .path(self.path.as_ref())
            .context("setting path for Device1")?
            .build()
            .await
            .context("building PropertiesProxy for Device1")?;

        let device_iface = zbus::names::InterfaceName::try_from("org.bluez.Device1")
            .context("invalid interface name")?;
        let battery_iface = zbus::names::InterfaceName::try_from("org.bluez.Battery1")
            .context("invalid interface name")?;

        let connected_val = props_device
            .get(device_iface, "Connected")
            .await
            .context("reading Device1.Connected")?;

        let connected =
            bool::try_from(connected_val).context("parsing Device1.Connected as bool")?;

        if !connected {
            anyhow::bail!("device {} is not connected", self.info.name);
        }

        // Читать заряд (тот же PropertiesProxy, он на том же пути)
        let pct_val = props_device
            .get(battery_iface, "Percentage")
            .await
            .context("reading Battery1.Percentage")?;

        let percent = u8::try_from(pct_val).context("parsing Battery1.Percentage as u8")?;

        Ok(BatteryReading::new(percent, ChargeState::Discharging))
    }
}

#[cfg(test)]
mod tests {
    use super::device_name;

    #[test]
    fn device_name_uses_alias_first() {
        assert_eq!(
            device_name(Some("My Keyboard"), Some("KB123"), "AA:BB:CC:DD:EE:FF"),
            "My Keyboard"
        );
    }

    #[test]
    fn device_name_falls_back_to_name() {
        assert_eq!(
            device_name(None, Some("KB123"), "AA:BB:CC:DD:EE:FF"),
            "KB123"
        );
    }

    #[test]
    fn device_name_falls_back_to_addr() {
        assert_eq!(
            device_name(None, None, "AA:BB:CC:DD:EE:FF"),
            "AA:BB:CC:DD:EE:FF"
        );
    }

    #[test]
    fn device_name_empty_alias_falls_back_to_name() {
        assert_eq!(
            device_name(Some(""), Some("KB123"), "AA:BB:CC:DD:EE:FF"),
            "KB123"
        );
    }

    #[test]
    fn device_name_empty_alias_and_name_falls_back_to_addr() {
        assert_eq!(
            device_name(Some(""), Some(""), "AA:BB:CC:DD:EE:FF"),
            "AA:BB:CC:DD:EE:FF"
        );
    }
}

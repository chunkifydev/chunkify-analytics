//! Local MMDB lookups. Only continent, country and city can cross the storage boundary.
use maxminddb::{Reader, geoip2};
use serde::Serialize;
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
};

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Location {
    pub continent: Option<String>,
    pub country: Option<String>,
    pub city: Option<String>,
}

pub struct Database(Reader<Vec<u8>>);

pub fn default_path() -> std::io::Result<PathBuf> {
    Ok(std::env::current_exe()?
        .parent()
        .expect("executable directory")
        .join("../share/chunkify-analytics/dbip-city-lite.mmdb"))
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let reader = Reader::open_readfile(path).map_err(|_| {
            format!("Cannot load geolocation City MMDB at {}. Install the bundled database or set server.geoip_database / CHUNKIFY_GEOIP_DATABASE.", path.display())
        })?;
        if !reader
            .metadata()
            .database_type
            .to_ascii_lowercase()
            .contains("city")
        {
            return Err("Geolocation requires a City MMDB containing country and city data".into());
        }
        Ok(Self(reader))
    }

    pub fn lookup(&self, ip: Option<IpAddr>) -> Location {
        let Some(ip) = ip.map(|ip| ip.to_canonical()) else {
            return Location::default();
        };
        if ip.is_loopback()
            || ip.is_unspecified()
            || ip.is_multicast()
            || match ip {
                IpAddr::V4(ip) => {
                    ip.is_private()
                        || ip.is_link_local()
                        || ip.is_broadcast()
                        || ip.is_documentation()
                }
                IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unicast_link_local(),
            }
        {
            return Location::default();
        }
        match self
            .0
            .lookup(ip)
            .and_then(|result| result.decode::<geoip2::City>())
        {
            Ok(Some(record)) => Location {
                continent: record
                    .continent
                    .code
                    .filter(|value| {
                        matches!(*value, "AF" | "AN" | "AS" | "EU" | "NA" | "OC" | "SA")
                    })
                    .map(str::to_owned),
                country: record
                    .country
                    .iso_code
                    .filter(|value| !value.is_empty() && *value != "ZZ")
                    .map(str::to_owned),
                city: record
                    .city
                    .names
                    .english
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
            },
            Ok(None) => Location::default(),
            Err(_) => {
                // The reader's errors may contain the address. Do not log them.
                tracing::warn!("Geolocation lookup failed; storing unknown location");
                Location::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_location_without_retaining_the_address() {
        let db = Database::open(Path::new("tests/fixtures/GeoIP2-City-Test.mmdb")).unwrap();
        for ip in ["81.2.69.160", "::ffff:81.2.69.160"] {
            assert_eq!(
                serde_json::to_value(db.lookup(Some(ip.parse().unwrap()))).unwrap(),
                serde_json::json!({"continent":"EU", "country":"GB", "city":"London"})
            );
        }
        assert_eq!(
            serde_json::to_value(db.lookup(Some("2001:218::1".parse().unwrap()))).unwrap(),
            serde_json::json!({"continent":"AS", "country":"JP", "city":null})
        );
        for ip in [
            "127.0.0.1",
            "::1",
            "10.0.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "fc00::1",
            "fe80::1",
            "203.0.113.1",
        ] {
            assert_eq!(db.lookup(Some(ip.parse().unwrap())), Location::default());
        }
        assert_eq!(db.lookup(None), Location::default());
        assert_eq!(
            db.lookup(Some("1.1.1.1".parse().unwrap())),
            Location::default()
        );
    }

    #[test]
    fn rejects_missing_corrupt_and_country_only_databases() {
        for file in [
            "missing.mmdb",
            "Cargo.toml",
            "tests/fixtures/GeoIP2-Country-Test.mmdb",
        ] {
            assert!(Database::open(Path::new(file)).is_err());
        }
    }
}

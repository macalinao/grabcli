//! The saved delivery address, read from the `location` cookie (URL-encoded
//! JSON). grabcli always shows the delivery address on a quote so the user can
//! confirm where the food is going.

use serde::Deserialize;

use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    #[serde(default)]
    pub id: String,
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub address: String,
    #[serde(default, rename = "countryCode")]
    pub country_code: String,
    /// Unit / floor / building line (`addressDetail`), if set in the app.
    #[serde(default, rename = "addressDetail", deserialize_with = "non_empty")]
    pub address_detail: Option<String>,
    /// Saved note to the driver (`noteToDriver`), if set in the app.
    #[serde(default, rename = "noteToDriver", deserialize_with = "non_empty")]
    pub note_to_driver: Option<String>,
}

/// Missing, `null`, and blank strings all become `None`.
fn non_empty<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    let v: Option<String> = Option::deserialize(d)?;
    Ok(v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

impl Location {
    pub fn from_session(session: &Session) -> Result<Self> {
        let raw = session.cookie("location").ok_or_else(|| {
            Error::new(
                ErrorKind::AddressMissing,
                "no saved delivery address (location cookie) in the session; copy it from the \
                 browser with `grab auth import --address-only` (set an address on food.grab.com \
                 there first if it has none)",
            )
        })?;
        let decoded =
            urlencoding::decode(raw).map_err(|e| Error::parse(format!("location cookie: {e}")))?;
        serde_json::from_str(&decoded)
            .map_err(|e| Error::parse(format!("location cookie JSON: {e}")))
    }

    /// The saved driver note with optional extra notes appended (`; `-joined).
    pub fn delivery_notes(&self, extra: Option<&str>) -> String {
        let extra = extra.map(str::trim).filter(|s| !s.is_empty());
        match (self.note_to_driver.as_deref(), extra) {
            (Some(a), Some(b)) => format!("{a}; {b}"),
            (Some(a), None) => a.to_string(),
            (None, Some(b)) => b.to_string(),
            (None, None) => String::new(),
        }
    }

    /// `lat,lng` for query strings.
    pub fn latlng(&self) -> String {
        format!("{},{}", self.latitude, self.longitude)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Cookie;

    #[test]
    fn parses_url_encoded_location() -> Result<()> {
        // Synthetic coordinates and address; not a real place.
        let json = r#"{"id":"POI-EXAMPLE","latitude":1.23,"longitude":4.56,"address":"1 Example St","countryCode":"PH"}"#;
        let encoded = urlencoding::encode(json).into_owned();
        let s = Session {
            cookies: vec![Cookie {
                name: "location".into(),
                value: encoded,
                domain: "food.grab.com".into(),
                path: "/".into(),
                expires: -1.0,
            }],
            ..Default::default()
        };
        let loc = Location::from_session(&s)?;
        assert_eq!(loc.id, "POI-EXAMPLE");
        assert_eq!(loc.latlng(), "1.23,4.56");
        assert_eq!(loc.address, "1 Example St");
        assert_eq!(loc.address_detail, None);
        assert_eq!(loc.note_to_driver, None);
        Ok(())
    }

    #[test]
    fn parses_address_detail_and_driver_note() -> Result<()> {
        let json = r#"{"id":"P","latitude":1.0,"longitude":2.0,"address":"1 Example St","addressDetail":"Unit 1A, Example Tower","noteToDriver":"Leave at lobby"}"#;
        let loc: Location = serde_json::from_str(json).map_err(|e| Error::parse(e.to_string()))?;
        assert_eq!(
            loc.address_detail.as_deref(),
            Some("Unit 1A, Example Tower")
        );
        assert_eq!(loc.note_to_driver.as_deref(), Some("Leave at lobby"));
        Ok(())
    }

    #[test]
    fn null_or_blank_detail_is_none() -> Result<()> {
        let json = r#"{"latitude":1.0,"longitude":2.0,"addressDetail":null,"noteToDriver":"  "}"#;
        let loc: Location = serde_json::from_str(json).map_err(|e| Error::parse(e.to_string()))?;
        assert_eq!(loc.address_detail, None);
        assert_eq!(loc.note_to_driver, None);
        Ok(())
    }

    #[test]
    fn delivery_notes_append() -> Result<()> {
        let mut loc: Location = serde_json::from_str(r#"{"latitude":1.0,"longitude":2.0}"#)
            .map_err(|e| Error::parse(e.to_string()))?;
        assert_eq!(loc.delivery_notes(None), "");
        assert_eq!(loc.delivery_notes(Some("Ring bell")), "Ring bell");
        loc.note_to_driver = Some("Leave at lobby".into());
        assert_eq!(loc.delivery_notes(None), "Leave at lobby");
        assert_eq!(loc.delivery_notes(Some(" ")), "Leave at lobby");
        assert_eq!(
            loc.delivery_notes(Some("Ring bell")),
            "Leave at lobby; Ring bell"
        );
        Ok(())
    }

    #[test]
    fn missing_location_errors() {
        let e = Location::from_session(&Session::default()).expect_err("should error");
        assert_eq!(e.kind, ErrorKind::AddressMissing);
    }
}

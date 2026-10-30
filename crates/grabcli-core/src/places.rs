//! Saved places (Home, Work and other saved addresses) from the account's
//! address book, as food.grab.com's address picker loads them. See
//! docs/places.md.
//!
//! Listing is a read-only GET. [`SavedPlace::to_location`] builds the
//! `location` cookie value the web app writes when a saved place is picked;
//! storing it only changes grabcli's local session, nothing on Grab's side.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, ErrorKind, Result};
use crate::http::Allowlist;
use crate::location::Location;

/// `GET` the saved places (web app: `getGetFavoriteAddresses`).
pub const FAVORITES_URL: &str = "https://food.grab.com/proxy/foodweb/v1/order/geo/favorites";

/// Within this distance of the saved delivery address a place counts as the
/// same place even when the poiIDs differ.
const SAME_PLACE_M: f64 = 20.0;

/// The closest place within this distance is reported as "near" the saved
/// delivery address (e.g. the same building picked as a different POI).
const NEAR_PLACE_M: f64 = 150.0;

/// How a saved place relates to the saved delivery address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    /// Same poiID or practically the same coordinates.
    Current,
    /// The closest place, within [`NEAR_PLACE_M`], but not the same one.
    Near,
}

/// Which place (index into `places`) is the saved delivery address: an exact
/// match if any, else the closest one within [`NEAR_PLACE_M`].
pub fn current_place(places: &[SavedPlace], loc: &Location) -> Option<(usize, Match)> {
    if let Some(i) = places.iter().position(|p| p.matches(loc)) {
        return Some((i, Match::Current));
    }
    places
        .iter()
        .enumerate()
        .map(|(i, p)| (i, p.distance_m(loc)))
        .filter(|(_, d)| *d <= NEAR_PLACE_M)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| (i, Match::Near))
}

/// The allowlist for saved-place commands: the one read-only GET.
pub fn allowlist() -> Allowlist {
    let mut a = Allowlist::new();
    a.allow_exact(FAVORITES_URL);
    a
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct LatLng {
    pub latitude: f64,
    pub longitude: f64,
}

/// One saved place, as returned by the favorites endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SavedPlace {
    #[serde(rename = "poiID", default)]
    pub poi_id: String,
    /// The place name (building / POI name).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub address: String,
    pub location: LatLng,
    /// Raw label: `HOME_TAG_PRESET`, `WORK_TAG_PRESET`, a custom label, or
    /// empty.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub city: String,
    #[serde(rename = "cityID", default)]
    pub city_id: String,
    #[serde(default)]
    pub country: String,
    #[serde(rename = "countryCode", default)]
    pub country_code: String,
}

/// Parse the favorites response (`{"places":[...]}`).
pub fn parse_places(v: &Value) -> Result<Vec<SavedPlace>> {
    match v.get("places") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(p) => serde_json::from_value(p.clone())
            .map_err(|e| Error::parse(format!("saved places: {e}"))),
    }
}

impl SavedPlace {
    /// The label as the app shows it: `Home` / `Work` for the presets, the
    /// custom label otherwise, `None` if unlabeled.
    pub fn display_label(&self) -> Option<String> {
        let raw = self.label.trim();
        if raw.is_empty() {
            return None;
        }
        Some(match raw.strip_suffix("_TAG_PRESET") {
            Some(preset) => {
                let lower = preset.to_ascii_lowercase();
                let mut chars = lower.chars();
                match chars.next() {
                    Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => raw.to_string(),
                }
            }
            None => raw.to_string(),
        })
    }

    /// `name - address`, the line the web app shows and stores.
    pub fn formatted_address(&self) -> String {
        match (self.name.trim(), self.address.trim()) {
            ("", a) => a.to_string(),
            (n, "") => n.to_string(),
            (n, a) => format!("{n} - {a}"),
        }
    }

    /// Whether this is the place the saved delivery address points at: same
    /// poiID, or (failing that) practically the same coordinates.
    pub fn matches(&self, loc: &Location) -> bool {
        if !self.poi_id.is_empty() && self.poi_id == loc.id {
            return true;
        }
        self.distance_m(loc) <= SAME_PLACE_M
    }

    /// Great-circle distance to the saved delivery address, in metres.
    pub fn distance_m(&self, loc: &Location) -> f64 {
        const R: f64 = 6_371_000.0;
        let (la1, la2) = (
            self.location.latitude.to_radians(),
            loc.latitude.to_radians(),
        );
        let dla = la2 - la1;
        let dlo = (loc.longitude - self.location.longitude).to_radians();
        let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
        2.0 * R * h.sqrt().asin()
    }

    /// The `location` cookie JSON for this place, shaped like the web app's
    /// own when a saved place is picked. `unit` and `note` become
    /// `addressDetail` and `noteToDriver` (empty when `None`).
    pub fn to_location(&self, unit: Option<&str>, note: Option<&str>) -> Value {
        let addr = self.formatted_address();
        json!({
            "id": self.poi_id,
            "latitude": self.location.latitude,
            "longitude": self.location.longitude,
            "address": addr,
            "countryCode": self.country_code,
            "isAccurate": true,
            "addressDetail": unit.map(str::trim).unwrap_or_default(),
            "noteToDriver": note.map(str::trim).unwrap_or_default(),
            "city": self.city,
            "cityID": self.city_id.parse::<i64>().unwrap_or_default(),
            "displayAddress": addr,
        })
    }
}

/// Find a place by 1-based list index, poiID, label (`home`, `work`, a custom
/// label; case-insensitive) or name. Errors if nothing or several match.
pub fn find<'a>(places: &'a [SavedPlace], target: &str) -> Result<&'a SavedPlace> {
    let t = target.trim();
    if let Ok(n) = t.parse::<usize>()
        && (1..=places.len()).contains(&n)
    {
        return Ok(&places[n - 1]);
    }
    if let Some(p) = places
        .iter()
        .find(|p| !p.poi_id.is_empty() && p.poi_id == t)
    {
        return Ok(p);
    }
    let eq = |s: &str| !s.trim().is_empty() && s.trim().eq_ignore_ascii_case(t);
    for key in [
        (|p: &SavedPlace| p.display_label().unwrap_or_default()) as fn(&SavedPlace) -> String,
        |p| p.label.clone(),
        |p| p.name.clone(),
    ] {
        let hits: Vec<&SavedPlace> = places.iter().filter(|p| eq(&key(p))).collect();
        match hits.as_slice() {
            [one] => return Ok(one),
            [] => {}
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "{} saved places match {t:?}; use the number or poiID from \
                         `grab saved-places list`",
                        hits.len()
                    ),
                ));
            }
        }
    }
    Err(Error::new(
        ErrorKind::NotFound,
        format!("no saved place matches {t:?}; see `grab saved-places list`"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fake places only: no real addresses or coordinates.
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../docs/fixtures/saved_places.json"))
            .expect("fixture parses")
    }

    fn places() -> Vec<SavedPlace> {
        parse_places(&fixture()).expect("places parse")
    }

    fn loc(json: &str) -> Location {
        serde_json::from_str(json).expect("location parses")
    }

    #[test]
    fn parses_fixture() {
        let p = places();
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].poi_id, "POI-HOME-EXAMPLE");
        assert_eq!(p[0].name, "Example Tower");
        assert_eq!(p[0].address, "1 Example St, Example City");
        assert_eq!(p[0].location.latitude, 1.23);
        assert_eq!(p[0].city_id, "4");
        assert_eq!(p[0].country_code, "PH");
    }

    #[test]
    fn empty_or_missing_places() {
        assert!(parse_places(&json!({})).expect("ok").is_empty());
        assert!(
            parse_places(&json!({"places": null}))
                .expect("ok")
                .is_empty()
        );
        assert!(parse_places(&json!({"places": "x"})).is_err());
    }

    #[test]
    fn labels() {
        let p = places();
        let l: Vec<Option<String>> = p.iter().map(SavedPlace::display_label).collect();
        assert_eq!(
            l,
            vec![
                Some("Home".into()),
                Some("Work".into()),
                Some("Gym".into()),
                None
            ]
        );
    }

    #[test]
    fn formatted_address() {
        let p = places();
        assert_eq!(
            p[0].formatted_address(),
            "Example Tower - 1 Example St, Example City"
        );
    }

    #[test]
    fn matches_by_poi_or_coords() {
        let p = places();
        let by_id =
            loc(r#"{"id":"POI-HOME-EXAMPLE","latitude":9.0,"longitude":9.0,"address":"x"}"#);
        assert!(p[0].matches(&by_id));
        assert!(!p[1].matches(&by_id));
        let by_coords = loc(r#"{"id":"OTHER","latitude":1.23005,"longitude":4.56005}"#);
        assert!(p[0].matches(&by_coords));
        let far = loc(r#"{"id":"OTHER","latitude":1.24,"longitude":4.56}"#);
        assert!(!p[0].matches(&far));
    }

    #[test]
    fn current_place_exact_near_none() {
        let p = places();
        let exact = loc(r#"{"id":"POI-WORK-EXAMPLE","latitude":0.0,"longitude":0.0}"#);
        assert_eq!(current_place(&p, &exact), Some((1, Match::Current)));
        // ~110 m north of Home: near, not current.
        let near = loc(r#"{"id":"OTHER","latitude":1.231,"longitude":4.56}"#);
        assert_eq!(current_place(&p, &near), Some((0, Match::Near)));
        let none = loc(r#"{"id":"OTHER","latitude":1.3,"longitude":4.56}"#);
        assert_eq!(current_place(&p, &none), None);
    }

    #[test]
    fn distance_is_metres() {
        let p = places();
        let l = loc(r#"{"latitude":1.24,"longitude":4.56}"#);
        let d = p[0].distance_m(&l);
        assert!((d - 1112.0).abs() < 5.0, "{d}");
    }

    #[test]
    fn find_by_index_id_label_name() {
        let p = places();
        assert_eq!(find(&p, "2").expect("idx").poi_id, "POI-WORK-EXAMPLE");
        assert_eq!(
            find(&p, "POI-GYM-EXAMPLE").expect("id").poi_id,
            "POI-GYM-EXAMPLE"
        );
        assert_eq!(find(&p, "home").expect("label").poi_id, "POI-HOME-EXAMPLE");
        assert_eq!(
            find(&p, "WORK_TAG_PRESET").expect("raw").poi_id,
            "POI-WORK-EXAMPLE"
        );
        assert_eq!(
            find(&p, "example plaza").expect("name").poi_id,
            "POI-OTHER-EXAMPLE"
        );
        assert_eq!(
            find(&p, "nope").expect_err("none").kind,
            ErrorKind::NotFound
        );
        assert_eq!(find(&p, "9").expect_err("range").kind, ErrorKind::NotFound);
    }

    #[test]
    fn find_ambiguous() {
        let mut p = places();
        p[1].name = p[0].name.clone();
        assert_eq!(
            find(&p, "Example Tower").expect_err("ambiguous").kind,
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn location_cookie_roundtrips() {
        let p = places();
        let v = p[0].to_location(Some(" Unit 1A, Example Tower "), None);
        assert_eq!(v["id"], "POI-HOME-EXAMPLE");
        assert_eq!(v["cityID"], 4);
        assert_eq!(v["isAccurate"], true);
        assert_eq!(v["noteToDriver"], "");
        let back: Location = serde_json::from_value(v).expect("location");
        assert_eq!(back.latlng(), "1.23,4.56");
        assert_eq!(
            back.address_detail.as_deref(),
            Some("Unit 1A, Example Tower")
        );
        assert_eq!(back.note_to_driver, None);
        assert_eq!(back.country_code, "PH");
    }

    #[test]
    fn allowlist_is_exact() {
        let a = allowlist();
        assert!(a.is_allowed(FAVORITES_URL));
        assert!(!a.is_allowed("https://food.grab.com/proxy/foodweb/v1/order/geo/favorites/x"));
        assert!(!a.is_allowed("https://food.grab.com/proxy/foodweb/v2/order/orders"));
    }
}

//! A stable, versioned view of a live order, combined from the track and detail
//! endpoints.
//!
//! The `--json` output follows [`SCHEMA_VERSION`]; fields are additive across
//! versions so scripts can depend on them. Nothing here is specific to any
//! consumer — it's a plain data model plus a concise human rendering.

use grabcli_core::error::{Error, Result};
use grabcli_core::money::Money;
use serde::Serialize;
use serde_json::Value;

/// Bump when the `--json` tracking shape changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct LatLng {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Driver {
    pub name: Option<String>,
    pub vehicle_model: Option<String>,
    pub vehicle_plate: Option<String>,
    pub service_type: Option<String>,
    pub rating: Option<f64>,
    pub photo_url: Option<String>,
    pub location: Option<LatLng>,
    pub bearing: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stage {
    pub task_type: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Eta {
    pub min_minutes: Option<i64>,
    pub max_minutes: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Place {
    pub name: Option<String>,
    pub address: Option<String>,
    pub coordinates: Option<LatLng>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemLine {
    pub quantity: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tracking {
    pub schema: u32,
    pub order_id: String,
    pub booking_code: Option<String>,
    pub short_number: Option<String>,
    pub state: String,
    pub human_state: String,
    pub is_final: bool,
    pub cancelable: bool,
    pub stages: Vec<Stage>,
    pub eta: Eta,
    pub driver: Option<Driver>,
    pub merchant: Place,
    pub dropoff: Place,
    /// Remaining route to the destination, decoded from `driverTrack.meta.route`.
    pub route: Vec<LatLng>,
    pub route_distance_m: Option<f64>,
    pub total_php: Option<f64>,
    pub items: Vec<ItemLine>,
    pub web_url: String,
}

/// Map a raw order state to a short human phrase.
pub fn human_state(state: &str) -> &'static str {
    match state {
        "NEW" | "ALLOCATING" | "REALLOCATING" | "REALLOCATED" => "finding a driver",
        "MAX_CONFIRMING" => "confirming with the restaurant",
        "ORDER_IN_PREPARE" | "ORDER_PLACED_ON_MERCHANT" => "being prepared",
        "ORDER_EXECUTING" => "in progress",
        "READY" | "ORDER_READY" => "ready for pickup",
        "DRIVER_AT_STORE" => "driver at the restaurant",
        "PICKING_UP" => "driver heading to the restaurant",
        "FOOD_COLLECTED" => "on the way to you",
        "DRIVER_ARRIVED" => "driver has arrived",
        "SCHEDULED" => "scheduled",
        "COMPLETED" => "delivered",
        "FAILED" => "failed",
        s if s.starts_with("CANCELLED") => "cancelled",
        _ => "in progress",
    }
}

pub fn is_final(state: &str) -> bool {
    matches!(state, "COMPLETED" | "FAILED") || state.starts_with("CANCELLED")
}

fn latlng(v: &Value) -> Option<LatLng> {
    Some(LatLng {
        latitude: v.get("latitude").and_then(Value::as_f64)?,
        longitude: v.get("longitude").and_then(Value::as_f64)?,
    })
}

/// Decode `driverTrack.meta.route` — a JSON array of `{lat,lng}` points.
fn decode_route(meta: &Value) -> Vec<LatLng> {
    let Some(raw) = meta.get("route").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Ok(Value::Array(pts)) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    pts.iter()
        .filter_map(|p| {
            Some(LatLng {
                latitude: p.get("lat").and_then(Value::as_f64)?,
                longitude: p.get("lng").and_then(Value::as_f64)?,
            })
        })
        .collect()
}

/// Build the tracking model from the track response and the (optional) order
/// detail response.
pub fn build(order_id: &str, track: &Value, detail: Option<&Value>) -> Result<Tracking> {
    let state = track
        .get("state")
        .and_then(Value::as_str)
        .or_else(|| {
            detail
                .and_then(|d| d.pointer("/order/orderState"))
                .and_then(Value::as_str)
        })
        .unwrap_or("UNKNOWN")
        .to_string();

    let booking_code = track
        .get("bookingCode")
        .and_then(Value::as_str)
        .map(ToString::to_string);

    let stages = track
        .get("tasks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|t| Stage {
                    task_type: t
                        .get("taskType")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    state: t
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let dt = track.get("driverTrack");
    let eta = Eta {
        min_minutes: dt
            .and_then(|d| d.get("minETAInMin"))
            .and_then(Value::as_i64),
        max_minutes: dt
            .and_then(|d| d.get("maxETAInMin"))
            .and_then(Value::as_i64),
    };
    let route = dt
        .and_then(|d| d.get("meta"))
        .map(decode_route)
        .unwrap_or_default();
    let route_distance_m = dt
        .and_then(|d| d.pointer("/meta/distance"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok());

    // Driver details come from the detail endpoint; live position from track.
    let driver = {
        let d = detail.and_then(|d| d.pointer("/order/driver"));
        let loc = dt.and_then(|d| d.get("location")).and_then(latlng);
        let bearing = dt.and_then(|d| d.get("bearing")).and_then(Value::as_f64);
        if d.is_some() || loc.is_some() {
            Some(Driver {
                name: d
                    .and_then(|x| x.get("driverName"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                vehicle_model: d
                    .and_then(|x| x.get("vehicleModel"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                vehicle_plate: d
                    .and_then(|x| x.get("vehiclePlateNumber"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                service_type: d
                    .and_then(|x| x.get("serviceType"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                rating: d
                    .and_then(|x| x.get("driverRating"))
                    .and_then(Value::as_f64),
                photo_url: d
                    .and_then(|x| x.get("driverImageUrl"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                location: loc,
                bearing,
            })
        } else {
            None
        }
    };

    // Merchant + drop-off from the detail snapshot.
    let (merchant, dropoff, total_php, items, short_number) = match detail {
        Some(d) => {
            let o = d.get("order").unwrap_or(d);
            let sd = o.get("snapshotDetail");
            let mc = sd.and_then(|s| s.pointer("/cartWithQuote/merchantCartWithQuoteList/0"));
            let minfo = mc.and_then(|m| m.get("merchantInfoObj"));
            let merchant = Place {
                name: minfo
                    .and_then(|m| m.get("name"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                address: minfo
                    .and_then(|m| m.pointer("/location/address"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                coordinates: minfo
                    .and_then(|m| m.pointer("/location/coordinates"))
                    .and_then(latlng),
            };
            let recv = sd.and_then(|s| s.pointer("/receiver/address"));
            let dropoff = Place {
                name: recv
                    .and_then(|r| r.get("keywords"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                address: recv
                    .and_then(|r| r.get("address"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                coordinates: recv.and_then(|r| r.get("coordinates")).and_then(latlng),
            };
            let total_php = sd
                .and_then(|s| s.pointer("/cartWithQuote/totalQuoteInMin/reducedPriceInMinorUnit"))
                .and_then(Value::as_i64)
                .map(|m| Money(m).as_major_f64());
            let items = mc
                .and_then(|m| m.get("itemsV2"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|it| ItemLine {
                            quantity: it.get("quantity").and_then(Value::as_i64).unwrap_or(1),
                            name: it
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let short = o
                .get("shortOrderNumber")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            (merchant, dropoff, total_php, items, short)
        }
        None => (
            Place {
                name: None,
                address: None,
                coordinates: None,
            },
            Place {
                name: None,
                address: None,
                coordinates: None,
            },
            None,
            Vec::new(),
            None,
        ),
    };

    Ok(Tracking {
        schema: SCHEMA_VERSION,
        order_id: order_id.to_string(),
        booking_code,
        short_number,
        human_state: human_state(&state).to_string(),
        is_final: is_final(&state),
        cancelable: track
            .get("cancelable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        state,
        stages,
        eta,
        driver,
        merchant,
        dropoff,
        route,
        route_distance_m,
        total_php,
        items,
        web_url: format!("https://food.grab.com/ph/en/order/{order_id}"),
    })
}

impl Tracking {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// A concise multi-line human summary.
    pub fn human(&self) -> String {
        let mut s = String::new();
        let who = self.merchant.name.as_deref().unwrap_or("order");
        s.push_str(&format!("{who}  [{}] {}\n", self.state, self.human_state));
        s.push_str(&format!("order {}", self.order_id));
        if let Some(b) = &self.booking_code {
            s.push_str(&format!("  booking {b}"));
        }
        if let Some(n) = &self.short_number {
            s.push_str(&format!("  {n}"));
        }
        s.push('\n');
        match (self.eta.min_minutes, self.eta.max_minutes) {
            (Some(a), Some(b)) if a != b => s.push_str(&format!("ETA {a}-{b} min\n")),
            (Some(a), _) => s.push_str(&format!("ETA {a} min\n")),
            _ => {}
        }
        if let Some(d) = &self.driver {
            let name = d.name.as_deref().unwrap_or("driver");
            let veh = d.vehicle_model.as_deref().unwrap_or("");
            let plate = d.vehicle_plate.as_deref().unwrap_or("");
            s.push_str(&format!("driver {name}  {veh} {plate}"));
            if let Some(r) = d.rating {
                s.push_str(&format!("  ★{r:.0}"));
            }
            s.push('\n');
        }
        if let Some(t) = self.total_php {
            s.push_str(&format!(
                "total {}\n",
                Money((t * 100.0).round() as i64).peso()
            ));
        }
        if let Some(addr) = &self.dropoff.address {
            s.push_str(&format!("to {addr}\n"));
        }
        s.push_str(&format!("track: {}", self.web_url));
        s
    }

    /// A one-line signature of the parts worth re-printing in `--watch`.
    pub fn watch_signature(&self) -> String {
        let loc = self
            .driver
            .as_ref()
            .and_then(|d| d.location.as_ref())
            .map(|l| format!("{:.4},{:.4}", l.latitude, l.longitude))
            .unwrap_or_default();
        format!(
            "{}|{:?}|{:?}|{loc}",
            self.state, self.eta.min_minutes, self.eta.max_minutes
        )
    }
}

/// Validate an order id argument.
pub fn check_order_id(id: &str) -> Result<()> {
    if id.trim().is_empty() {
        return Err(Error::invalid("order id is required"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic track + detail (invented ids/coords/names/plate).
    fn track() -> Value {
        serde_json::json!({
            "orderID": "ORDER-EXAMPLE",
            "bookingCode": "A-EXAMPLE123",
            "state": "FOOD_COLLECTED",
            "cancelable": false,
            "tasks": [
                {"taskType": "PREPARE", "state": "COMPLETED"},
                {"taskType": "DELIVERY", "state": "FOOD_COLLECTED"}
            ],
            "driverTrack": {
                "driverID": "DX1",
                "location": {"latitude": 1.5, "longitude": 2.5},
                "minETAInMin": 10, "maxETAInMin": 12, "bearing": 90.0,
                "meta": {
                    "distance": "1234.5",
                    "route": "[{\"lat\":1.5,\"lng\":2.5},{\"lat\":1.6,\"lng\":2.6}]"
                }
            }
        })
    }

    fn detail() -> Value {
        serde_json::json!({"order": {
            "shortOrderNumber": "GF-001",
            "driver": {
                "driverName": "Example Driver", "vehicleModel": "Honda Example",
                "vehiclePlateNumber": "ABC123", "driverRating": 5, "serviceType": "BIKE",
                "driverImageUrl": "https://example.com/pic.jpg"
            },
            "snapshotDetail": {
                "receiver": {"address": {
                    "address": "1 Example St", "keywords": "Example Park",
                    "coordinates": {"latitude": 1.6, "longitude": 2.6}
                }},
                "cartWithQuote": {
                    "totalQuoteInMin": {"reducedPriceInMinorUnit": 44200},
                    "merchantCartWithQuoteList": [{
                        "merchantInfoObj": {"name": "Example Noodles - Downtown",
                            "location": {"address": "9 Market Rd",
                                "coordinates": {"latitude": 1.4, "longitude": 2.4}}},
                        "itemsV2": [{"name": "Karaage Bowl", "quantity": 1}]
                    }]
                }
            }
        }})
    }

    #[test]
    fn builds_full_model() -> Result<()> {
        let t = build("ORDER-EXAMPLE", &track(), Some(&detail()))?;
        assert_eq!(t.schema, 1);
        assert_eq!(t.state, "FOOD_COLLECTED");
        assert_eq!(t.human_state, "on the way to you");
        assert!(!t.is_final);
        assert_eq!(t.stages.len(), 2);
        assert_eq!(t.eta.min_minutes, Some(10));
        let d = t.driver.as_ref().expect("driver");
        assert_eq!(d.name.as_deref(), Some("Example Driver"));
        assert_eq!(d.vehicle_plate.as_deref(), Some("ABC123"));
        assert!(d.location.is_some());
        assert_eq!(
            t.merchant.name.as_deref(),
            Some("Example Noodles - Downtown")
        );
        assert_eq!(t.dropoff.address.as_deref(), Some("1 Example St"));
        assert_eq!(t.route.len(), 2);
        assert_eq!(t.route_distance_m, Some(1234.5));
        assert_eq!(t.total_php, Some(442.0));
        assert_eq!(t.items[0].name, "Karaage Bowl");
        assert!(t.web_url.ends_with("ORDER-EXAMPLE"));
        Ok(())
    }

    #[test]
    fn final_states() {
        assert!(is_final("COMPLETED"));
        assert!(is_final("CANCELLED_BY_USER"));
        assert!(!is_final("PICKING_UP"));
    }

    #[test]
    fn track_only_without_detail() -> Result<()> {
        let t = build("X", &track(), None)?;
        assert!(t.merchant.name.is_none());
        assert!(t.driver.is_some()); // live location still present
        Ok(())
    }
}

# Saved places

The account's saved places (Home, Work, other saved addresses) as the
food.grab.com address picker loads them. `grab saved-places list` shows them;
`grab saved-places use` points grabcli's delivery address at one.

## Endpoint

```
GET https://food.grab.com/proxy/foodweb/v1/order/geo/favorites
```

The web app calls it `getGetFavoriteAddresses` (redux action
`GET_SAVED_ADDRESSES`). It only fires from the heart icon next to the address
input, and that icon is behind the `savedAddresses` feature flag, which was off
for this account in October 2026. The endpoint answers anyway.

Headers are the same as other logged-in `proxy/foodweb` calls (e.g.
`v2/order/history`): `cookie`, `x-gfc-session`, `x-hydra-jwt`,
`x-gfc-country: PH`, `x-country-code: PH`, `x-grab-web-app-version`,
`accept: application/json, text/plain, */*`. No query parameters, no body.

The Android app uses a different endpoint,
`https://api.grab.com/api/passenger/v2/poi/favourite`, with a GrabID OAuth2
bearer token and device headers. grabcli doesn't hold those credentials and
doesn't need them, because the web endpoint above works with the food.grab.com
session.

It returns the places for every country on the account, regardless of
`x-country-code`. The web app filters them to the current country before
showing them.

## Response

Fake example data:

```json
{
  "places": [
    {
      "poiID": "POI-HOME-EXAMPLE",
      "address": "1 Example St, Example City",
      "location": { "latitude": 1.23, "longitude": 4.56 },
      "name": "Example Tower",
      "label": "HOME_TAG_PRESET",
      "city": "Example City",
      "cityID": "4",
      "country": "Philippines",
      "countryID": "2",
      "countryCode": "PH"
    }
  ]
}
```

- `label` is `HOME_TAG_PRESET`, `WORK_TAG_PRESET`, a free-text custom label, or
  `""`. grabcli shows the presets as `Home` / `Work`. Only `HOME_TAG_PRESET`
  has been seen live; `WORK_TAG_PRESET` is assumed by analogy.
- `cityID` and `countryID` are strings here but numbers in the `location`
  cookie.
- There is no unit (`addressDetail`) or driver note per place. Those live only
  in the `location` cookie.

A full fake fixture is in [fixtures/saved_places.json](fixtures/saved_places.json).

## Picking a place (`use`)

When a saved place is picked, the web app builds the `location` cookie
(URL-encoded JSON) on the client like this, with no server call:

```json
{
  "id": "<poiID>",
  "latitude": 1.23,
  "longitude": 4.56,
  "address": "<name> - <address>",
  "countryCode": "PH",
  "isAccurate": true,
  "addressDetail": "",
  "noteToDriver": "",
  "city": "Example City",
  "cityID": 4,
  "displayAddress": "<name> - <address>"
}
```

`grab saved-places use <n|poiID|label|name>` writes the same shape into the
session's `location` cookie and changes nothing on Grab's side. The cookie
keeps its domain, path and expiry. `addressDetail` and `noteToDriver` come from
`--unit` / `--note`. Without them, the current values are kept if the chosen
place is the current address, and are empty otherwise.

## Matching the current address

`list` marks the place the `location` cookie points at:

- `*`: same poiID, or within 20 m.
- `~`: otherwise, the closest place within 150 m. This is usually the same
  building saved under a different POI (for example, an entrance instead of
  the tower).

`--json` reports this as `current` / `near_current` per place.

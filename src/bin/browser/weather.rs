//! Optional sky weather. Only fixed upstreams are reachable; caches are bounded
//! and coalesce concurrent tabs. Weather failures never affect session workers.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::Mutex;

use super::runtime::Error;

const FORECAST: &str = "https://api.open-meteo.com/v1/forecast";
const LOCATIONS: &str = "https://geocoding-api.open-meteo.com/v1/search";
const FRESH: Duration = Duration::from_secs(15 * 60);
const RETRY: Duration = Duration::from_secs(60);
const CAPACITY: usize = 32;

#[derive(Clone, Copy, Deserialize)]
pub(super) struct Coordinates {
    latitude: f64,
    longitude: f64,
}

impl Coordinates {
    fn valid(self) -> bool {
        (-90.0..=90.0).contains(&self.latitude) && (-180.0..=180.0).contains(&self.longitude)
    }

    fn query(self, url: &mut Url) {
        // Weather grids do not need precise device coordinates. Rounded keys
        // also share cached forecasts between nearby browser locations.
        url.query_pairs_mut()
            .append_pair("latitude", &format!("{:.2}", self.latitude))
            .append_pair("longitude", &format!("{:.2}", self.longitude));
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Forecast {
    utc_offset_seconds: i32,
    current: Conditions,
}

#[derive(Clone, Deserialize, Serialize)]
struct Conditions {
    time: i64,
    cloud_cover_low: f64,
    cloud_cover_mid: f64,
    cloud_cover_high: f64,
    wind_speed_10m: f64,
    wind_direction_10m: f64,
}

impl Forecast {
    fn valid(&self) -> bool {
        let c = &self.current;
        [c.cloud_cover_low, c.cloud_cover_mid, c.cloud_cover_high]
            .iter()
            .all(|v| (0.0..=100.0).contains(v))
            && (0.0..=360.0).contains(&c.wind_direction_10m)
            && (0.0..=200.0).contains(&c.wind_speed_10m)
            && (-86400..=86400).contains(&self.utc_offset_seconds)
            && chrono::Utc::now().timestamp().abs_diff(c.time) < 2 * 60 * 60
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Locations {
    #[serde(default)]
    results: Vec<Location>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Location {
    name: String,
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    admin1: String,
    #[serde(default)]
    country: String,
}

struct Entry<T> {
    key: String,
    expires: Instant,
    result: Result<T, String>,
}

type Cache<T> = Mutex<VecDeque<Entry<T>>>;

pub(super) struct Weather {
    client: Result<Client, String>,
    forecasts: Cache<Forecast>,
    locations: Cache<Locations>,
}

impl Weather {
    pub(super) fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!(
                    "myco/",
                    env!("CARGO_PKG_VERSION"),
                    " (sky weather)"
                ))
                .build()
                .map_err(|e| e.to_string()),
            forecasts: Mutex::new(VecDeque::new()),
            locations: Mutex::new(VecDeque::new()),
        }
    }

    pub(super) async fn forecast(&self, coordinates: Coordinates) -> Result<Forecast, Error> {
        if !coordinates.valid() {
            return Err(Error::Invalid("Invalid weather coordinates.".into()));
        }
        let mut url = Url::parse(FORECAST).unwrap();
        coordinates.query(&mut url);
        url.query_pairs_mut()
            .append_pair("current", "cloud_cover_low,cloud_cover_mid,cloud_cover_high,wind_speed_10m,wind_direction_10m")
            .append_pair("wind_speed_unit", "ms")
            .append_pair("timeformat", "unixtime")
            .append_pair("timezone", "auto")
            .append_pair("forecast_days", "1");
        self.cached(url, &self.forecasts, Forecast::valid).await
    }

    pub(super) async fn locations(&self, query: &str) -> Result<Locations, Error> {
        let query = query.trim();
        if !(2..=80).contains(&query.chars().count()) || query.chars().any(char::is_control) {
            return Err(Error::Invalid(
                "Enter a city name between 2 and 80 characters.".into(),
            ));
        }
        let mut url = Url::parse(LOCATIONS).unwrap();
        url.query_pairs_mut()
            .append_pair("name", query)
            .append_pair("count", "5")
            .append_pair("language", "en");
        self.cached(url, &self.locations, |locations| {
            locations.results.len() <= 5
                && locations.results.iter().all(|location| {
                    Coordinates {
                        latitude: location.latitude,
                        longitude: location.longitude,
                    }
                    .valid()
                })
        })
        .await
    }

    async fn cached<T: Clone + DeserializeOwned>(
        &self,
        url: Url,
        cache: &Cache<T>,
        valid: impl Fn(&T) -> bool,
    ) -> Result<T, Error> {
        // The lock coalesces requests during a cold start or an upstream outage.
        // Forecasts and city searches have independent locks and bounded timeouts.
        let mut entries = cache.lock().await;
        entries.retain(|entry| entry.expires > Instant::now());
        if let Some(entry) = entries.iter().find(|entry| entry.key == url.as_str()) {
            return entry.result.clone().map_err(Error::Unavailable);
        }
        let result = self.fetch(url.clone(), valid).await;
        let lifetime = if result.is_ok() { FRESH } else { RETRY };
        if entries.len() >= CAPACITY {
            entries.pop_front();
        }
        entries.push_back(Entry {
            key: url.into(),
            expires: Instant::now() + lifetime,
            result: result.clone(),
        });
        result.map_err(Error::Unavailable)
    }

    async fn fetch<T: DeserializeOwned>(
        &self,
        url: Url,
        valid: impl Fn(&T) -> bool,
    ) -> Result<T, String> {
        let client = self.client.as_ref().map_err(Clone::clone)?;
        let mut response = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("Weather service unavailable: {}", e.without_url()))?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| e.without_url().to_string())?
        {
            if body.len() + chunk.len() > 128 * 1024 {
                return Err("Weather response too large.".into());
            }
            body.extend_from_slice(&chunk);
        }
        let value =
            serde_json::from_slice(&body).map_err(|e| format!("Invalid weather response: {e}"))?;
        if !valid(&value) {
            return Err("Weather data is missing, invalid, or out of date.".into());
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, http::StatusCode, routing::get};
    use serde_json::{Value, json};

    use super::*;

    fn forecast() -> Value {
        json!({"utc_offset_seconds": -25200, "current": {
            "time": chrono::Utc::now().timestamp(), "cloud_cover_low": 70,
            "cloud_cover_mid": 20, "cloud_cover_high": 40,
            "wind_speed_10m": 5, "wind_direction_10m": 250
        }})
    }

    async fn upstream(
        status: StatusCode,
        body: String,
    ) -> (Url, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let router = Router::new().route(
            "/",
            get(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                let body = body.clone();
                async move { (status, body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (url, calls, task)
    }

    #[tokio::test]
    async fn simultaneous_tabs_share_weather_and_expired_entries_refresh() {
        let weather = Weather::new();
        let (url, calls, task) = upstream(StatusCode::OK, forecast().to_string()).await;
        let (first, second) = tokio::join!(
            weather.cached(url.clone(), &weather.forecasts, Forecast::valid),
            weather.cached(url.clone(), &weather.forecasts, Forecast::valid),
        );
        assert_eq!(first.unwrap().current.cloud_cover_low, 70.0);
        assert_eq!(second.unwrap().current.cloud_cover_high, 40.0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        weather.forecasts.lock().await[0].expires = Instant::now();
        weather
            .cached(url, &weather.forecasts, Forecast::valid)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn upstream_failures_are_bounded_cached_and_retried() {
        let mut invalid = forecast();
        invalid["current"]["cloud_cover_low"] = json!(101);
        let mut stale = forecast();
        stale["current"]["time"] = json!(chrono::Utc::now().timestamp() - 7201);
        for (status, body) in [
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable".into()),
            (StatusCode::OK, "not json".into()),
            (StatusCode::OK, "x".repeat(128 * 1024 + 1)),
            (StatusCode::OK, "{}".into()),
            (StatusCode::OK, invalid.to_string()),
            (StatusCode::OK, stale.to_string()),
        ] {
            let weather = Weather::new();
            let (url, calls, task) = upstream(status, body).await;
            for _ in 0..2 {
                assert!(matches!(
                    weather
                        .cached(url.clone(), &weather.forecasts, Forecast::valid)
                        .await,
                    Err(Error::Unavailable(_))
                ));
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let mut entries = weather.forecasts.lock().await;
            assert!(entries[0].expires.duration_since(Instant::now()) <= RETRY);
            entries[0].expires = Instant::now();
            drop(entries);
            assert!(
                weather
                    .cached(url, &weather.forecasts, Forecast::valid)
                    .await
                    .is_err()
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            task.abort();
        }
    }

    #[tokio::test]
    async fn different_locations_cannot_grow_the_cache_without_bound() {
        let weather = Weather::new();
        let (mut url, _, task) = upstream(StatusCode::OK, forecast().to_string()).await;
        for i in 0..CAPACITY + 2 {
            url.set_query(Some(&format!("latitude={i}")));
            weather
                .cached(url.clone(), &weather.forecasts, Forecast::valid)
                .await
                .unwrap();
        }
        let cache = weather.forecasts.lock().await;
        assert_eq!(cache.len(), CAPACITY);
        assert!(cache.front().unwrap().key.ends_with("latitude=2"));
        task.abort();
    }

    #[tokio::test]
    async fn invalid_locations_are_rejected_before_any_upstream_request() {
        let weather = Weather::new();
        for latitude in [f64::NAN, f64::INFINITY, -91.0, 91.0] {
            assert!(matches!(
                weather
                    .forecast(Coordinates {
                        latitude,
                        longitude: 0.0
                    })
                    .await,
                Err(Error::Invalid(_))
            ));
        }
        for query in ["", "a", "city\nname", &"a".repeat(81)] {
            assert!(matches!(
                weather.locations(query).await,
                Err(Error::Invalid(_))
            ));
        }
        assert!(weather.forecasts.lock().await.is_empty());
        assert!(weather.locations.lock().await.is_empty());
    }

    #[test]
    fn device_coordinates_are_rounded_before_leaving_myco() {
        let mut url = Url::parse(FORECAST).unwrap();
        Coordinates {
            latitude: 37.77491,
            longitude: -122.41942,
        }
        .query(&mut url);
        assert_eq!(url.query().unwrap(), "latitude=37.77&longitude=-122.42");
    }
}

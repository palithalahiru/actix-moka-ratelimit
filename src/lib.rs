//! Rate limiter using a fixed window counter for arbitrary keys, backed by Moka in-memory cache for Actix Web.
//!
//! ```toml
//! [dependencies]
//! actix-web = "4"
#![doc = concat!("actix-ratelimit = \"", env!("CARGO_PKG_VERSION_MAJOR"), ".", env!("CARGO_PKG_VERSION_MINOR"),"\"")]
//! ```
//!
//! ```no_run
//! use std::{sync::Arc, time::Duration};
//! use actix_web::{dev::ServiceRequest, get, web, App, HttpServer, Responder};
//! use actix_session::SessionExt as _;
//! use actix_ratelimit::{Limiter, RateLimiter};
//!
//! #[get("/{id}/{name}")]
//! async fn index(info: web::Path<(u32, String)>) -> impl Responder {
//!     format!("Hello {}! id:{}", info.1, info.0)
//! }
//!
//! #[actix_web::main]
//! async fn main() -> std::io::Result<()> {
//!     let limiter = web::Data::new(
//!         Limiter::builder()
//!             .key_by(|req: &ServiceRequest| {
//!                 req.get_session()
//!                     .get(&"session-id")
//!                     .unwrap_or_else(|_| req.cookie(&"rate-api-id").map(|c| c.to_string()))
//!             })
//!             .limit(5000)
//!             .period(Duration::from_secs(3600)) // 60 minutes
//!             .build()
//!             .unwrap(),
//!     );
//!
//!     HttpServer::new(move || {
//!         App::new()
//!             .wrap(RateLimiter::default())
//!             .app_data(limiter.clone())
//!             .service(index)
//!     })
//!     .bind(("127.0.0.1", 8080))?
//!     .run()
//!     .await
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]
#![doc(html_logo_url = "https://actix.rs/img/logo.png")]
#![doc(html_favicon_url = "https://actix.rs/favicon.ico")]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::{borrow::Cow, fmt, sync::Arc, time::Duration};

use actix_web::dev::ServiceRequest;
use moka::future::Cache;

mod builder;
mod errors;
mod middleware;
mod status;

pub use self::{builder::Builder, errors::Error, middleware::RateLimiter, status::Status};

/// Default request limit.
pub const DEFAULT_REQUEST_LIMIT: usize = 5000;

/// Default period (in seconds).
pub const DEFAULT_PERIOD_SECS: u64 = 3600;

/// Default cookie name.
pub const DEFAULT_COOKIE_NAME: &str = "sid";

/// Default session key.
#[cfg(feature = "session")]
pub const DEFAULT_SESSION_KEY: &str = "rate-api-id";

/// Helper trait to impl Debug on GetKeyFn type
trait GetKeyFnT: Fn(&ServiceRequest) -> Option<String> {}

impl<T> GetKeyFnT for T where T: Fn(&ServiceRequest) -> Option<String> {}

/// Get key function type with auto traits
type GetKeyFn = dyn GetKeyFnT + Send + Sync;

/// Get key resolver function type
impl fmt::Debug for GetKeyFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GetKeyFn")
    }
}

/// Wrapped Get key function Trait
type GetArcBoxKeyFn = Arc<GetKeyFn>;

/// Counter data stored in cache
#[derive(Debug, Clone)]
struct CounterData {
    count: usize,
    expires_at: i64,
}

/// Rate limiter.
#[derive(Clone)]
pub struct Limiter {
    cache: Cache<String, CounterData>,
    limit: usize,
    period: Duration,
    get_key_fn: GetArcBoxKeyFn,
}

impl fmt::Debug for Limiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Limiter")
            .field("limit", &self.limit)
            .field("period", &self.period)
            .field("get_key_fn", &self.get_key_fn)
            .finish()
    }
}

impl Limiter {
    /// Construct rate limiter builder with defaults.
    #[must_use]
    pub fn builder() -> Builder {
        Builder {
            limit: DEFAULT_REQUEST_LIMIT,
            period: Duration::from_secs(DEFAULT_PERIOD_SECS),
            get_key_fn: None,
            cookie_name: Cow::Borrowed(DEFAULT_COOKIE_NAME),
            #[cfg(feature = "session")]
            session_key: Cow::Borrowed(DEFAULT_SESSION_KEY),
        }
    }

    /// Consumes one rate limit unit, returning the status.
    pub async fn count(&self, key: impl Into<String>) -> Result<Status, Error> {
        let (count, reset) = self.track(key).await?;
        let status = Status::new(count, self.limit, reset);

        if count > self.limit {
            Err(Error::LimitExceeded(status))
        } else {
            Ok(status)
        }
    }

    /// Tracks the given key in a period and returns the count and TTL for the key in seconds.
    async fn track(&self, key: impl Into<String>) -> Result<(usize, usize), Error> {
        let key = key.into();
        let now = chrono::Utc::now().timestamp();
        let period_secs = self.period.as_secs() as i64;
        let expires_at = now + period_secs;

        // Get or initialize the counter
        let counter = match self.cache.get(&key).await {
            Some(data) if data.expires_at > now => {
                // Counter exists and hasn't expired
                let new_count = data.count + 1;
                let new_data = CounterData {
                    count: new_count,
                    expires_at: data.expires_at,
                };
                self.cache.insert(key.clone(), new_data.clone()).await;
                new_data
            }
            _ => {
                // Counter doesn't exist or has expired, create new one
                let new_data = CounterData {
                    count: 1,
                    expires_at,
                };
                self.cache.insert(key.clone(), new_data.clone()).await;
                new_data
            }
        };

        let ttl = (counter.expires_at - now) as u64;
        let reset = Status::epoch_utc_plus(Duration::from_secs(ttl))?;

        Ok((counter.count, reset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_limiter() {
        let mut builder = Limiter::builder();
        let limiter = builder.build();
        assert!(limiter.is_ok());

        let limiter = limiter.unwrap();
        assert_eq!(limiter.limit, 5000);
        assert_eq!(limiter.period, Duration::from_secs(3600));
    }
}

//! The HTTP read-through layer under the supply-chain clients: an on-disk
//! cache and an in-process LRU, both keyed by request URL, and a self-throttle
//! that gates the requests that reach the network.
//!
//! [`HttpCache::get`] looks in this order: the in-process LRU, then the disk
//! entry, then the [`Transport`] (throttled). A response is kept in both
//! places for its lifetime ([`lifetime`]): the larger of its
//! `Cache-Control: max-age` and [`MIN_TTL`] (one hour), so a response that
//! names no lifetime, or a shorter one, is still kept for an hour. A response
//! marked `no-store` or `no-cache` is never kept, and a non-2xx response is an
//! error that is never remembered. A disk entry that is missing, unreadable,
//! for another URL or expired is a miss; a disk entry that cannot be written
//! still serves the response.
//!
//! Disk entries live under the store's directory, beside its graph:
//! `<store>/http-cache/<namespace>/`, one file per key (SHA-256 of the key).
//! That is the embedding cache's precedent, where the store owns its cache. The
//! namespace names the client (`deps-dev`, `osv`), so two clients never share
//! keys.
//!
//! Self-throttle: a [`Throttle`] hands out request start slots one interval
//! apart, so N rapid calls leave at the configured rate. The deps.dev API
//! publishes no rate limit, and GUAC throttles its deps.dev collector for the
//! same reason. Cache hits take no slot. Each cache gates its own network
//! calls, and the throttle never holds a lock across a request.
//!
//! The layer is HTTP-generic. [`Transport`] fetches a URL, and the cache knows
//! nothing about deps.dev. A GET is keyed on its URL. A POST (OSV's
//! `querybatch`) is keyed on its URL and the SHA-256 of its body, as the
//! synthetic key `POST <sha256(body)> <url>`, so two bodies sent to one URL
//! are two entries. The key is opaque to the disk and memory layers.
use lru::LruCache;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The freshness floor: no kept response expires sooner than this.
pub const MIN_TTL: Duration = Duration::from_secs(60 * 60);

/// Entries kept in the in-process LRU per cache.
const MEMORY_ENTRIES: usize = 512;

/// One response from the network: the status, the `Cache-Control` header if
/// any, and the body.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub cache_control: Option<String>,
    pub body: Vec<u8>,
}

/// Why a fetch produced no body.
#[derive(Debug)]
pub enum FetchError {
    /// No response: connection, TLS or timeout failure.
    Transport(String),
    /// A response with a non-2xx status.
    Status { url: String, status: u16 },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "request failed: {message}"),
            Self::Status { url, status } => write!(f, "{url} returned HTTP {status}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Fetches one URL. [`ReqwestTransport`] is the production implementation;
/// tests supply a fake, so nothing in CI reaches the network.
pub trait Transport: Send + Sync {
    fn get(&self, url: &str) -> impl Future<Output = Result<HttpResponse, FetchError>> + Send;

    /// A POST of `body` as JSON to `url`.
    fn post(
        &self,
        url: &str,
        body: &str,
    ) -> impl Future<Output = Result<HttpResponse, FetchError>> + Send;
}

/// [`Transport`] over a shared reqwest client.
#[derive(Clone)]
pub struct ReqwestTransport(pub reqwest::Client);

impl Transport for ReqwestTransport {
    async fn get(&self, url: &str) -> Result<HttpResponse, FetchError> {
        send(self.0.get(url)).await
    }

    async fn post(&self, url: &str, body: &str) -> Result<HttpResponse, FetchError> {
        let request = self
            .0
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_owned());
        send(request).await
    }
}

/// Sends `request` and reads its status, `Cache-Control` and body.
async fn send(request: reqwest::RequestBuilder) -> Result<HttpResponse, FetchError> {
    let response = request
        .send()
        .await
        .map_err(|error| FetchError::Transport(error.to_string()))?;
    let status = response.status().as_u16();
    let cache_control = response
        .headers()
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response
        .bytes()
        .await
        .map_err(|error| FetchError::Transport(error.to_string()))?
        .to_vec();
    Ok(HttpResponse {
        status,
        cache_control,
        body,
    })
}

/// A transport shared between a client and its caller (tests read its call
/// count through the same `Arc`).
impl<T: Transport> Transport for Arc<T> {
    fn get(&self, url: &str) -> impl Future<Output = Result<HttpResponse, FetchError>> + Send {
        (**self).get(url)
    }

    fn post(
        &self,
        url: &str,
        body: &str,
    ) -> impl Future<Output = Result<HttpResponse, FetchError>> + Send {
        (**self).post(url, body)
    }
}

/// Hands out request start slots one `interval` apart.
pub struct Throttle {
    interval: Duration,
    next: Mutex<Option<Instant>>,
}

impl Throttle {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::new(None),
        }
    }

    /// The start slot for a request arriving at `now`: `now` when the schedule
    /// has run down, otherwise the next free slot. Each call books the slot
    /// after it. The clock is a parameter so tests can drive it.
    pub fn reserve(&self, now: Instant) -> Instant {
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = match *next {
            Some(slot) if slot > now => slot,
            _ => now,
        };
        *next = Some(slot + self.interval);
        slot
    }

    /// Waits for this caller's slot.
    pub async fn wait(&self) {
        let slot = self.reserve(Instant::now());
        tokio::time::sleep_until(tokio::time::Instant::from_std(slot)).await;
    }
}

/// The read-through cache for one client: an LRU, a disk directory and the
/// throttle for its network calls.
pub struct HttpCache {
    dir: PathBuf,
    memory: Mutex<LruCache<String, Entry>>,
    throttle: Throttle,
}

#[derive(Clone)]
struct Entry {
    expires_at: u64,
    body: Arc<[u8]>,
}

/// The first line of a disk entry; the body follows it. `url` is the entry's
/// key: the URL for a GET, the body-keyed form for a POST.
#[derive(Serialize, Deserialize)]
struct Header {
    url: String,
    expires_at: u64,
}

impl HttpCache {
    /// The cache for the client `namespace` of the store at `store_dir`,
    /// network calls gated `min_interval` apart.
    pub fn for_store(store_dir: &Path, namespace: &str, min_interval: Duration) -> Self {
        Self {
            dir: store_dir.join("http-cache").join(namespace),
            memory: Mutex::new(LruCache::new(
                NonZeroUsize::new(MEMORY_ENTRIES).expect("a non-zero LRU size"),
            )),
            throttle: Throttle::new(min_interval),
        }
    }

    /// The body at `url`: from memory, from disk, or else fetched through
    /// `transport` (throttled) and kept for its [`lifetime`].
    pub async fn get<T: Transport>(
        &self,
        transport: &T,
        url: &str,
    ) -> Result<Arc<[u8]>, FetchError> {
        self.read_through(url, url, || transport.get(url)).await
    }

    /// The body `url` answers to a POST of `body`, cached like a GET: the
    /// entry is keyed on the body too, so each distinct body is its own entry.
    pub async fn post<T: Transport>(
        &self,
        transport: &T,
        url: &str,
        body: &str,
    ) -> Result<Arc<[u8]>, FetchError> {
        let key = format!("POST {} {url}", digest_hex(body.as_bytes()));
        self.read_through(&key, url, || transport.post(url, body))
            .await
    }

    /// The cache's one path: memory, disk, else `fetch` under the throttle.
    /// `key` names the entry; `url` names the request in a status error.
    async fn read_through<F, Fut>(
        &self,
        key: &str,
        url: &str,
        fetch: F,
    ) -> Result<Arc<[u8]>, FetchError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<HttpResponse, FetchError>>,
    {
        if let Some(entry) = self.remembered(key, now_secs()) {
            return Ok(entry.body);
        }
        let path = entry_path(&self.dir, key);
        if let Some(entry) = read_disk(&path, key, now_secs()) {
            self.remember(key, entry.clone());
            return Ok(entry.body);
        }
        self.throttle.wait().await;
        let response = fetch().await?;
        if !(200..300).contains(&response.status) {
            return Err(FetchError::Status {
                url: url.to_owned(),
                status: response.status,
            });
        }
        let body: Arc<[u8]> = response.body.into();
        if let Some(lifetime) = lifetime(response.cache_control.as_deref()) {
            let entry = Entry {
                expires_at: now_secs() + lifetime.as_secs(),
                body: body.clone(),
            };
            // A cache that cannot be written still serves the response.
            let _ = write_disk(&self.dir, &path, key, &entry);
            self.remember(key, entry);
        }
        Ok(body)
    }

    fn remembered(&self, key: &str, now: u64) -> Option<Entry> {
        let mut memory = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
        match memory.get(key) {
            Some(entry) if entry.expires_at > now => Some(entry.clone()),
            Some(_) => {
                memory.pop(key);
                None
            }
            None => None,
        }
    }

    fn remember(&self, key: &str, entry: Entry) {
        self.memory
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .put(key.to_owned(), entry);
    }
}

/// How long a response is kept, from its `Cache-Control` header: the larger of
/// `max-age` and [`MIN_TTL`]. `None` for `no-store` and `no-cache`, which ask
/// for no keeping.
pub fn lifetime(cache_control: Option<&str>) -> Option<Duration> {
    let mut max_age = 0;
    for directive in cache_control
        .unwrap_or_default()
        .split(',')
        .map(|directive| directive.trim().to_ascii_lowercase())
    {
        if directive == "no-store" || directive == "no-cache" {
            return None;
        }
        if let Some(seconds) = directive
            .strip_prefix("max-age=")
            .and_then(|seconds| seconds.parse::<u64>().ok())
        {
            max_age = seconds;
        }
    }
    Some(MIN_TTL.max(Duration::from_secs(max_age)))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn entry_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{}.http", digest_hex(key.as_bytes())))
}

/// The lowercase hexadecimal SHA-256 of `bytes`.
fn digest_hex(bytes: &[u8]) -> String {
    graphicle_memory::text::sha256(bytes)
}

/// A disk entry for `key` that is still fresh. Anything else is a miss.
fn read_disk(path: &Path, key: &str, now: u64) -> Option<Entry> {
    let bytes = std::fs::read(path).ok()?;
    let split = bytes.iter().position(|byte| *byte == b'\n')?;
    let header: Header = serde_json::from_slice(&bytes[..split]).ok()?;
    (header.url == key && header.expires_at > now).then(|| Entry {
        expires_at: header.expires_at,
        body: bytes[split + 1..].into(),
    })
}

/// Writes the entry through a temporary file, so a reader never sees half of it.
fn write_disk(dir: &Path, path: &Path, key: &str, entry: &Entry) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut bytes = serde_json::to_vec(&Header {
        url: key.to_owned(),
        expires_at: entry.expires_at,
    })
    .map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    bytes.extend_from_slice(&entry.body);
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(temp, path)
}

/// The wire the supply-chain reads run over (the osv bulk query and the
/// deps.dev read-through layer): the network in production; in tests a
/// fixture, a stub that panics on any request, or a store with no network.
/// One thread-local slot holds the wire a test installed, so every reader
/// shares one install and one fallback mechanism.
pub(crate) mod wire {
    #[cfg(not(test))]
    use super::ReqwestTransport;
    #[cfg(test)]
    use super::test_transport::FixtureTransport;
    use super::{FetchError, HttpResponse, Transport};
    use crate::Result;
    #[cfg(test)]
    use std::cell::RefCell;
    #[cfg(test)]
    use std::sync::Arc;

    /// The transport a query runs over.
    #[derive(Clone)]
    pub enum Wire {
        #[cfg(not(test))]
        Network(ReqwestTransport),
        #[cfg(test)]
        Fixture(Arc<FixtureTransport>),
        #[cfg(test)]
        Panic(Arc<PanicTransport>),
        /// A store with no network: every request fails with this message.
        #[cfg(test)]
        Refused(&'static str),
    }

    /// What a test thread gets when it installed no wire: a stub that panics
    /// with the message, or a store with no network that refuses with it.
    /// Production never reads the message, so the lint is allowed there.
    #[cfg_attr(not(test), allow(dead_code))]
    #[derive(Clone, Copy)]
    pub(crate) enum Fallback {
        Panic(&'static str),
        Refused(&'static str),
    }

    impl Transport for Wire {
        async fn get(&self, url: &str) -> std::result::Result<HttpResponse, FetchError> {
            match self {
                #[cfg(not(test))]
                Self::Network(transport) => transport.get(url).await,
                #[cfg(test)]
                Self::Fixture(transport) => transport.get(url).await,
                #[cfg(test)]
                Self::Panic(transport) => transport.get(url).await,
                #[cfg(test)]
                Self::Refused(message) => Err(refused(message)),
            }
        }

        async fn post(
            &self,
            url: &str,
            body: &str,
        ) -> std::result::Result<HttpResponse, FetchError> {
            match self {
                #[cfg(not(test))]
                Self::Network(transport) => transport.post(url, body).await,
                #[cfg(test)]
                Self::Fixture(transport) => transport.post(url, body).await,
                #[cfg(test)]
                Self::Panic(transport) => transport.post(url, body).await,
                #[cfg(test)]
                Self::Refused(message) => Err(refused(message)),
            }
        }
    }

    #[cfg(test)]
    fn refused(message: &str) -> FetchError {
        FetchError::Transport(message.to_owned())
    }

    #[cfg(test)]
    thread_local! {
        static INSTALLED: RefCell<Option<Wire>> = const { RefCell::new(None) };
    }

    /// Serve this thread's queries from `fixture`.
    #[cfg(test)]
    pub(crate) fn install_fixture(fixture: Arc<FixtureTransport>) {
        INSTALLED.with(|slot| *slot.borrow_mut() = Some(Wire::Fixture(fixture)));
    }

    /// Fail this thread's queries by panicking with `message`, and return the
    /// counter that reads how many requests reached the stub.
    #[cfg(test)]
    pub(crate) fn install_panic(message: &'static str) -> Arc<PanicTransport> {
        let transport = Arc::new(PanicTransport::new(message));
        INSTALLED.with(|slot| *slot.borrow_mut() = Some(Wire::Panic(transport.clone())));
        transport
    }

    /// Fail this thread's queries as a store with no network does: each
    /// request is a connection failure carrying `message`.
    #[cfg(test)]
    pub(crate) fn install_refused(message: &'static str) {
        INSTALLED.with(|slot| *slot.borrow_mut() = Some(Wire::Refused(message)));
    }

    /// The wire of the next query, built on the calling thread: in tests the
    /// installed one, else the fallback; in production the shared reqwest
    /// client.
    #[cfg(test)]
    pub(crate) fn wire(fallback: Fallback) -> Result<Wire> {
        Ok(INSTALLED
            .with(|slot| slot.borrow().clone())
            .unwrap_or(match fallback {
                Fallback::Panic(message) => Wire::Panic(Arc::new(PanicTransport::new(message))),
                Fallback::Refused(message) => Wire::Refused(message),
            }))
    }

    /// The wire of the next query: the shared reqwest client.
    #[cfg(not(test))]
    pub(crate) fn wire(_fallback: Fallback) -> Result<Wire> {
        Ok(Wire::Network(ReqwestTransport(crate::model::client()?)))
    }

    /// A transport that panics on any request. It names the URL it was asked
    /// for, and counts the requests that reached it, so a test can assert zero.
    #[cfg(test)]
    pub struct PanicTransport {
        calls: std::sync::atomic::AtomicUsize,
        message: &'static str,
    }

    #[cfg(test)]
    impl PanicTransport {
        fn new(message: &'static str) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                message,
            }
        }

        /// The number of requests that reached this stub.
        pub fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn refuse(&self, url: &str) -> ! {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("{}: {url}", self.message)
        }
    }

    #[cfg(test)]
    impl Transport for PanicTransport {
        async fn get(&self, url: &str) -> std::result::Result<HttpResponse, FetchError> {
            self.refuse(url)
        }

        async fn post(
            &self,
            url: &str,
            _body: &str,
        ) -> std::result::Result<HttpResponse, FetchError> {
            self.refuse(url)
        }
    }
}

/// A fake transport for tests: canned responses by URL, and a call count.
/// An unknown URL answers 404.
#[cfg(test)]
pub mod test_transport {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct FixtureTransport {
        routes: HashMap<String, HttpResponse>,
        calls: AtomicUsize,
        posted: Mutex<Vec<String>>,
    }

    impl FixtureTransport {
        pub fn new(routes: impl IntoIterator<Item = (String, HttpResponse)>) -> Self {
            Self {
                routes: routes.into_iter().collect(),
                calls: AtomicUsize::new(0),
                posted: Mutex::new(Vec::new()),
            }
        }

        /// The number of requests that reached this transport.
        pub fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// The bodies POSTed to this transport, in order.
        pub fn posted(&self) -> Vec<String> {
            self.posted
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn answer(&self, url: &str) -> HttpResponse {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.routes.get(url).cloned().unwrap_or(HttpResponse {
                status: 404,
                cache_control: None,
                body: Vec::new(),
            })
        }
    }

    /// A 200 response carrying `body`, with `max-age=3600`.
    pub fn json(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            cache_control: Some("public, max-age=3600".to_owned()),
            body: body.as_bytes().to_vec(),
        }
    }

    impl Transport for FixtureTransport {
        async fn get(&self, url: &str) -> Result<HttpResponse, FetchError> {
            Ok(self.answer(url))
        }

        async fn post(&self, url: &str, body: &str) -> Result<HttpResponse, FetchError> {
            self.posted
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(body.to_owned());
            Ok(self.answer(url))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_transport::{FixtureTransport, json};
    use super::*;
    use tempfile::TempDir;

    const URL: &str = "https://api.example.test/v1/thing";

    fn cache(store: &TempDir, interval: Duration) -> HttpCache {
        HttpCache::for_store(store.path(), "test", interval)
    }

    #[test]
    fn lifetime_honors_max_age_above_a_one_hour_floor() {
        assert_eq!(lifetime(None), Some(MIN_TTL));
        assert_eq!(
            lifetime(Some("public, max-age=7200")),
            Some(Duration::from_secs(7200))
        );
        assert_eq!(lifetime(Some("max-age=60")), Some(MIN_TTL));
        assert_eq!(lifetime(Some("max-age=garbage")), Some(MIN_TTL));
        assert_eq!(lifetime(Some("no-store")), None);
        assert_eq!(lifetime(Some("private, NO-CACHE")), None);
    }

    #[test]
    fn throttle_reserves_slots_one_interval_apart() {
        let interval = Duration::from_millis(100);
        let throttle = Throttle::new(interval);
        let start = Instant::now();
        let slots: Vec<Instant> = (0..4).map(|_| throttle.reserve(start)).collect();
        for (index, slot) in slots.iter().enumerate() {
            assert_eq!(*slot, start + interval * index as u32);
        }
        // A caller that arrives after the schedule has run down starts at once.
        let later = start + Duration::from_secs(10);
        assert_eq!(throttle.reserve(later), later);
        assert_eq!(throttle.reserve(later), later + interval);
    }

    #[tokio::test]
    async fn rapid_calls_are_gated_to_the_configured_rate() {
        let store = TempDir::new().unwrap();
        let cache = cache(&store, Duration::from_millis(40));
        let transport =
            FixtureTransport::new((0..4).map(|index| (format!("{URL}/{index}"), json("{}"))));
        let started = Instant::now();
        for index in 0..4 {
            cache
                .get(&transport, &format!("{URL}/{index}"))
                .await
                .unwrap();
        }
        // Lower bound only: a sleep never returns early, so this cannot flake.
        assert!(started.elapsed() >= Duration::from_millis(120));
        assert_eq!(transport.calls(), 4);
    }

    #[tokio::test]
    async fn second_call_is_served_from_cache_without_a_transport_call() {
        let store = TempDir::new().unwrap();
        let transport = FixtureTransport::new([(URL.to_owned(), json("{\"a\":1}"))]);
        let cache = cache(&store, Duration::ZERO);
        let first = cache.get(&transport, URL).await.unwrap();
        let second = cache.get(&transport, URL).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(transport.calls(), 1);
        // A fresh cache over the same store finds the disk entry.
        let reopened = cache_in(&store);
        assert_eq!(reopened.get(&transport, URL).await.unwrap(), first);
        assert_eq!(transport.calls(), 1);
    }

    fn cache_in(store: &TempDir) -> HttpCache {
        cache(store, Duration::ZERO)
    }

    #[tokio::test]
    async fn disk_entry_lives_under_the_store_and_records_max_age() {
        let store = TempDir::new().unwrap();
        let transport = FixtureTransport::new([(
            URL.to_owned(),
            HttpResponse {
                status: 200,
                cache_control: Some("public, max-age=7200".to_owned()),
                body: b"{}".to_vec(),
            },
        )]);
        cache(&store, Duration::ZERO)
            .get(&transport, URL)
            .await
            .unwrap();
        let path = entry_path(&store.path().join("http-cache").join("test"), URL);
        let bytes = std::fs::read(&path).unwrap();
        let split = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        let header: Header = serde_json::from_slice(&bytes[..split]).unwrap();
        assert_eq!(header.url, URL);
        let lifetime = header.expires_at - now_secs();
        assert!((7190..=7200).contains(&lifetime), "lifetime {lifetime}");
        assert_eq!(&bytes[split + 1..], b"{}");
    }

    #[tokio::test]
    async fn expired_disk_entry_is_fetched_again() {
        let store = TempDir::new().unwrap();
        let dir = store.path().join("http-cache").join("test");
        let stale = Entry {
            expires_at: 1,
            body: b"old".to_vec().into(),
        };
        write_disk(&dir, &entry_path(&dir, URL), URL, &stale).unwrap();
        let transport = FixtureTransport::new([(URL.to_owned(), json("new"))]);
        let body = cache(&store, Duration::ZERO)
            .get(&transport, URL)
            .await
            .unwrap();
        assert_eq!(&body[..], b"new");
        assert_eq!(transport.calls(), 1);
    }

    #[tokio::test]
    async fn errors_and_no_store_responses_are_never_kept() {
        let store = TempDir::new().unwrap();
        let cache = cache(&store, Duration::ZERO);
        let missing = FixtureTransport::new([]);
        let error = cache.get(&missing, URL).await.unwrap_err();
        assert!(matches!(error, FetchError::Status { status: 404, .. }));
        cache.get(&missing, URL).await.unwrap_err();
        assert_eq!(missing.calls(), 2);

        let private = FixtureTransport::new([(
            URL.to_owned(),
            HttpResponse {
                status: 200,
                cache_control: Some("no-store".to_owned()),
                body: b"{}".to_vec(),
            },
        )]);
        cache.get(&private, URL).await.unwrap();
        cache.get(&private, URL).await.unwrap();
        assert_eq!(private.calls(), 2);
    }

    #[tokio::test]
    async fn posts_are_cached_per_body_and_never_collide_with_gets() {
        let store = TempDir::new().unwrap();
        let cache = cache(&store, Duration::ZERO);
        let transport = FixtureTransport::new([(URL.to_owned(), json("{\"a\":1}"))]);
        let first = cache.post(&transport, URL, "{\"q\":1}").await.unwrap();
        // The same body is served from the cache; a fresh cache reads disk.
        let again = cache.post(&transport, URL, "{\"q\":1}").await.unwrap();
        assert_eq!(first, again);
        assert_eq!(
            cache_in(&store)
                .post(&transport, URL, "{\"q\":1}")
                .await
                .unwrap(),
            first
        );
        assert_eq!(transport.calls(), 1);
        // Another body to the same URL is another request and another entry.
        cache.post(&transport, URL, "{\"q\":2}").await.unwrap();
        assert_eq!(transport.calls(), 2);
        assert_eq!(transport.posted(), ["{\"q\":1}", "{\"q\":2}"]);
        // The GET at the same URL keeps its own entry, keyed on the URL alone.
        cache.get(&transport, URL).await.unwrap();
        assert_eq!(transport.calls(), 3);
        cache.get(&transport, URL).await.unwrap();
        assert_eq!(transport.calls(), 3);
    }

    #[tokio::test]
    async fn a_failed_post_is_never_kept() {
        let store = TempDir::new().unwrap();
        let cache = cache(&store, Duration::ZERO);
        let missing = FixtureTransport::new([]);
        let error = cache.post(&missing, URL, "{}").await.unwrap_err();
        assert!(matches!(error, FetchError::Status { status: 404, .. }));
        cache.post(&missing, URL, "{}").await.unwrap_err();
        assert_eq!(missing.calls(), 2);
    }
}

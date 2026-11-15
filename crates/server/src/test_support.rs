use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode, request::Builder},
    response::Response,
};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::{net::TcpListener, sync::Semaphore};
use tower::ServiceExt;

use crate::{
    app::{self, Addresses, AppState, Timeouts},
    budget::{Clock, DEFAULT_HISTORY_DAYS},
    config::{ApiKey, Config, Consumer, Token},
    redis_store::RedisStore,
};

pub(crate) const NOV_3_2026_20H: u64 = 1_793_736_000_000;
pub(crate) const TOKEN: &str = "app-token-0123456789abcdef0123456789";
pub(crate) const KEY: &str = "test-curseforge-key";
pub(crate) const USER_AGENT: &str = "test-gateway/0 (https://example.com)";
pub(crate) const TEST_REDIS_ENV: &str = "CONTENT_GATEWAY_TEST_REDIS_URL";
const UNREACHABLE: &str = "http://127.0.0.1:9";

pub(crate) struct TestClock(AtomicU64);

impl TestClock {
    pub(crate) fn at(ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(ms)))
    }

    pub(crate) fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) base: String,
    pub(crate) method: String,
    pub(crate) uri: String,
    pub(crate) key: Option<String>,
    pub(crate) user_agent: Option<String>,
    pub(crate) body: String,
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(&'static str, &'static str)>,
    pub(crate) body: String,
}

pub(crate) fn reply(status: u16, body: impl Into<String>) -> Reply {
    Reply {
        status,
        headers: vec![("content-type", "application/json; charset=utf-8")],
        body: body.into(),
    }
}

pub(crate) struct Mock {
    pub(crate) base: String,
    pub(crate) seen: Arc<Mutex<Vec<Seen>>>,
}

pub(crate) struct Gate(Arc<Semaphore>);

impl Gate {
    pub(crate) fn open(&self) {
        self.0.close();
    }
}

impl Mock {
    pub(crate) async fn start(
        delay: Duration,
        answer: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static,
    ) -> Self {
        Self::serve(delay, None, answer).await
    }

    pub(crate) async fn held(
        answer: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static,
    ) -> (Self, Gate) {
        let gate = Arc::new(Semaphore::new(0));
        let mock = Self::serve(Duration::ZERO, Some(gate.clone()), answer).await;
        (mock, Gate(gate))
    }

    async fn serve(
        delay: Duration,
        gate: Option<Arc<Semaphore>>,
        answer: impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let answer = Arc::new(answer);
        let calls = Arc::new(AtomicUsize::new(0));
        let recorded = seen.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = base.clone();
        let mock = Router::new().fallback(move |request: Request<Body>| {
            let (recorded, answer, calls) = (recorded.clone(), answer.clone(), calls.clone());
            let (base, gate) = (served.clone(), gate.clone());
            async move {
                let (parts, body) = request.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                let seen = Seen {
                    base,
                    method: parts.method.to_string(),
                    uri: parts.uri.to_string(),
                    key: parts
                        .headers
                        .get("x-api-key")
                        .map(|value| value.to_str().unwrap().to_owned()),
                    user_agent: parts
                        .headers
                        .get("user-agent")
                        .map(|value| value.to_str().unwrap().to_owned()),
                    body: String::from_utf8(body.to_vec()).unwrap(),
                };
                recorded.lock().unwrap().push(seen.clone());
                tokio::time::sleep(delay).await;
                if let Some(gate) = gate {
                    let _closed = gate.acquire().await;
                }
                let reply = answer(calls.fetch_add(1, Ordering::SeqCst), &seen);
                let mut response = Response::builder().status(reply.status);
                for (name, value) in reply.headers {
                    response = response.header(name, value);
                }
                let body = if seen.method == "HEAD" {
                    Body::empty()
                } else {
                    Body::from(reply.body)
                };
                response.body(body).unwrap()
            }
        });
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        Self { base, seen }
    }

    pub(crate) fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

pub(crate) async fn until(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..5_000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("waited 5 s for {what}");
}

pub(crate) fn addresses_at(base: &str) -> Addresses {
    Addresses {
        curseforge_api: format!("{base}/v1"),
        modrinth_api: format!("{base}/v2"),
        curseforge_cdn: base.to_owned(),
        mojang_meta: base.to_owned(),
        neoforge_maven: base.to_owned(),
        forge_files: base.to_owned(),
        fabric_meta: base.to_owned(),
    }
}

pub(crate) fn consumer(name: &str, token: &str, pools: &[&str]) -> Consumer {
    Consumer {
        name: name.to_owned(),
        token: Token::new(token),
        pools: pools.iter().map(|pool| (*pool).to_owned()).collect(),
    }
}

pub(crate) fn config(consumers: Vec<Consumer>, limit: u32) -> Config {
    Config {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        consumers,
        budget_per_minute: limit,
        modrinth_per_minute: limit,
        budget_history_days: DEFAULT_HISTORY_DAYS,
        redis_url: None,
        curseforge_api_key: ApiKey::new(KEY),
        user_agent: USER_AGENT.to_owned(),
    }
}

pub(crate) fn test_state(consumers: Vec<Consumer>) -> Arc<AppState> {
    Arc::new(
        AppState::new(
            &config(consumers, 300),
            &addresses_at(UNREACHABLE),
            Timeouts::default(),
            TestClock::at(NOV_3_2026_20H),
            None,
        )
        .unwrap(),
    )
}

pub(crate) struct Gateway {
    pub(crate) router: Router,
    pub(crate) state: Arc<AppState>,
    pub(crate) clock: Arc<TestClock>,
}

pub(crate) struct GatewayWith {
    pub(crate) limit: u32,
    pub(crate) timeouts: Timeouts,
    pub(crate) redis: Option<Arc<RedisStore>>,
}

impl Default for GatewayWith {
    fn default() -> Self {
        Self {
            limit: 300,
            timeouts: Timeouts::default(),
            redis: None,
        }
    }
}

pub(crate) fn gateway(mock: &Mock) -> Gateway {
    gateway_with(mock, GatewayWith::default())
}

pub(crate) fn gateway_with(mock: &Mock, with: GatewayWith) -> Gateway {
    let clock = TestClock::at(NOV_3_2026_20H);
    let state = Arc::new(
        AppState::new(
            &config(vec![consumer("app", TOKEN, &["launcher"])], with.limit),
            &addresses_at(&mock.base),
            with.timeouts,
            clock.clone(),
            with.redis,
        )
        .unwrap(),
    );
    Gateway {
        router: app::router(state.clone()),
        state,
        clock,
    }
}

pub(crate) struct Got {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

impl Got {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.body.to_vec()).unwrap()
    }

    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

pub(crate) async fn collect(response: Response) -> Got {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Got {
        status,
        headers,
        body,
    }
}

pub(crate) async fn call(router: &Router, request: Request<Body>) -> Got {
    collect(router.clone().oneshot(request).await.unwrap()).await
}

pub(crate) async fn send(router: &Router, request: Builder, body: &str) -> Got {
    let request = request
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    call(router, request).await
}

pub(crate) async fn get(router: &Router, uri: &str) -> Got {
    send(router, Request::get(uri), "").await
}

pub(crate) async fn post_json(router: &Router, uri: &str, body: &str) -> Got {
    send(
        router,
        Request::post(uri).header("content-type", "application/json"),
        body,
    )
    .await
}

pub(crate) struct TestRedis {
    url: String,
    prefix: String,
}

impl TestRedis {
    pub(crate) fn from_env() -> Option<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let url = std::env::var(TEST_REDIS_ENV)
            .ok()
            .filter(|url| !url.trim().is_empty())?;
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let prefix = format!(
            "content-gateway-test:{}:{started}:{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        Some(Self { url, prefix })
    }

    pub(crate) fn required() -> Self {
        Self::from_env()
            .unwrap_or_else(|| panic!("{TEST_REDIS_ENV} must name a Redis to run this test"))
    }

    pub(crate) fn open(&self) -> Arc<RedisStore> {
        Arc::new(RedisStore::open(&self.url, self.prefix.clone()).unwrap())
    }

    pub(crate) async fn forget(&self) {
        let mut connection = redis::Client::open(self.url.as_str())
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap();
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(format!("{}:*", self.prefix))
            .query_async(&mut connection)
            .await
            .unwrap();
        if !keys.is_empty() {
            redis::cmd("DEL")
                .arg(keys)
                .query_async::<()>(&mut connection)
                .await
                .unwrap();
        }
    }
}

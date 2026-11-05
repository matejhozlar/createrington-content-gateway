use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
    response::Response,
};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::net::TcpListener;
use tower::ServiceExt;

use crate::{
    app::{self, AppState},
    budget::{Budget, TestClock},
    cache::AnswerCache,
    config::{ApiKey, Consumer, Token},
    curseforge, modrinth,
    redis_store::RedisStore,
    resolve::{self, Cdn},
    upstream::{Source, Upstream},
    versions::{self, Meta},
};

pub(crate) const NOV_3_2026_20H: u64 = 1_793_736_000_000;
pub(crate) const TOKEN: &str = "app-token-0123456789abcdef0123456789";
pub(crate) const KEY: &str = "test-curseforge-key";
pub(crate) const USER_AGENT: &str = "test-gateway/0 (https://example.com)";

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

impl Mock {
    pub(crate) async fn start(
        delay: Duration,
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
            let base = served.clone();
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

pub(crate) struct Gateway {
    pub(crate) router: Router,
    pub(crate) state: Arc<AppState>,
    pub(crate) clock: Arc<TestClock>,
}

pub(crate) fn gateway(mock: &Mock, limit: u32, timeout: Duration) -> Gateway {
    gateway_on(mock, limit, timeout, None)
}

pub(crate) fn gateway_on(
    mock: &Mock,
    limit: u32,
    timeout: Duration,
    redis: Option<Arc<RedisStore>>,
) -> Gateway {
    gateway_with(mock, limit, timeout, redis, &["launcher"])
}

fn gateway_with(
    mock: &Mock,
    limit: u32,
    timeout: Duration,
    redis: Option<Arc<RedisStore>>,
    pools: &[&str],
) -> Gateway {
    let clock = TestClock::at(NOV_3_2026_20H);
    let consumers = vec![Consumer {
        name: "app".to_owned(),
        token: Token::new(TOKEN),
        pools: pools.iter().map(|pool| (*pool).to_owned()).collect(),
    }];
    let state = Arc::new(AppState {
        budget: Budget::new(
            limit,
            pools.iter().map(|pool| ("app", *pool)),
            clock.clone(),
            redis.clone(),
        ),
        modrinth_budget: Budget::new(
            limit,
            pools.iter().map(|pool| ("app", *pool)),
            clock.clone(),
            redis.clone(),
        )
        .named("modrinth", "Modrinth"),
        upstream: Upstream::new(
            Source::CurseForge,
            format!("{}/v1", mock.base),
            Some(ApiKey::new(KEY)),
            curseforge::USER_AGENT,
            timeout,
        )
        .unwrap(),
        modrinth: Upstream::new(
            Source::Modrinth,
            format!("{}/v2", mock.base),
            None,
            USER_AGENT,
            modrinth::TIMEOUT,
        )
        .unwrap(),
        cdn: Cdn::new(mock.base.clone(), resolve::CDN_TIMEOUT).unwrap(),
        meta: Meta::new(
            mock.base.clone(),
            mock.base.clone(),
            mock.base.clone(),
            mock.base.clone(),
            USER_AGENT,
            versions::TIMEOUT,
        )
        .unwrap(),
        cache: AnswerCache::new(redis),
        clock: clock.clone(),
        consumers,
    });
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

    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

pub(crate) async fn send(
    router: &Router,
    request: axum::http::request::Builder,
    body: &str,
) -> Got {
    let request = request
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Got {
        status,
        headers,
        body,
    }
}

pub(crate) async fn get(router: &Router, uri: &str) -> Got {
    send(router, Request::get(uri), "").await
}

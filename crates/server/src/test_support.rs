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
    curseforge::Upstream,
    redis_store::RedisStore,
};

pub(crate) const NOV_3_2026_20H: u64 = 1_793_736_000_000;
pub(crate) const TOKEN: &str = "app-token-0123456789abcdef0123456789";
pub(crate) const KEY: &str = "test-curseforge-key";

#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) uri: String,
    pub(crate) key: Option<String>,
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
        let mock = Router::new().fallback(move |request: Request<Body>| {
            let (recorded, answer, calls) = (recorded.clone(), answer.clone(), calls.clone());
            async move {
                let (parts, body) = request.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                let seen = Seen {
                    method: parts.method.to_string(),
                    uri: parts.uri.to_string(),
                    key: parts
                        .headers
                        .get("x-api-key")
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
                response.body(Body::from(reply.body)).unwrap()
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
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
    let clock = TestClock::at(NOV_3_2026_20H);
    let consumers = vec![Consumer {
        name: "app".to_owned(),
        token: Token::new(TOKEN),
        pools: vec!["launcher".to_owned()],
    }];
    let state = Arc::new(AppState {
        budget: Budget::new(limit, [("app", "launcher")], clock.clone(), redis.clone()),
        upstream: Upstream::new(mock.base.clone(), ApiKey::new(KEY), timeout).unwrap(),
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

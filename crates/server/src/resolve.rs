use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use axum::{
    Json,
    body::Bytes,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_LENGTH},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::{sync::Semaphore, task::JoinSet};
use utoipa::ToSchema;

use crate::{
    app::AppState,
    auth::Pool,
    batch::{self, Batch},
    cache::{Answer, Entry},
    error::ApiError,
    modrinth,
    passthrough::{self, HOUR_MS, MINUTE_MS},
};

pub const CURSEFORGE_CDN: &str = "https://mediafilez.forgecdn.net";
pub const CDN_TIMEOUT: Duration = Duration::from_secs(10);
const CDN_WAIT: Duration = Duration::from_secs(20);
/// The longest a `POST /v1/resolve` can take: CurseForge's timeout, then Modrinth's, then the CDN wait. The client's
/// timeout for `resolve()` is set above it.
pub const LONGEST: Duration = Duration::from_secs(40);
const CDN_AT_ONCE: usize = 8;
const CDN_FOUND_MS: u64 = HOUR_MS;
const CDN_NOT_FOUND_MS: u64 = 10 * MINUTE_MS;
const MODRINTH_CDN_HOST: &str = "cdn.modrinth.com";
const SHA1_ALGO: u8 = 1;
const MOST_FILES: usize = 1000;

/// The client that checks built addresses on CurseForge's CDN with HEAD requests.
pub struct Cdn {
    client: reqwest::Client,
    base: String,
}

impl Cdn {
    pub fn new(base: impl Into<String>, timeout: Duration) -> reqwest::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent(modrinth::DEFAULT_USER_AGENT)
                .build()?,
            base: base.into(),
        })
    }

    /// The address of a file on the CDN, built from its id and name the way CurseForge's own links are.
    pub fn url(&self, file_id: u64, file_name: &str) -> String {
        format!(
            "{}/files/{}/{}/{}",
            self.base,
            file_id / 1000,
            file_id % 1000,
            encoded(file_name)
        )
    }

    async fn head(&self, url: &str) -> Result<(StatusCode, Option<u64>), reqwest::Error> {
        let response = self.client.head(url).send().await?;
        let length = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()?.parse().ok());
        Ok((response.status(), length))
    }
}

fn encoded(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The body of `POST /v1/resolve`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolveRequest {
    /// CurseForge file ids, up to 1000.
    #[schema(example = json!([4567890, 4567891]))]
    pub file_ids: Vec<u64>,
    /// Whether a file's built address on CurseForge's CDN may be checked and given when neither CurseForge nor
    /// Modrinth has a link. On unless set to false.
    #[serde(default = "yes")]
    #[schema(default = true)]
    pub cdn: bool,
}

fn yes() -> bool {
    true
}

/// Where a file's download address comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FileSource {
    /// The link CurseForge's API gives for the file.
    Curseforge,
    /// The copy of the file Modrinth hosts, found by its SHA-1.
    Modrinth,
    /// The file's built address on CurseForge's CDN, which answered a HEAD with the file's size.
    CurseforgeCdn,
}

/// What `POST /v1/resolve` answers for each file asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedFile {
    pub file_id: u64,
    /// Where `url` comes from, or null when no source has the file.
    pub source: Option<FileSource>,
    /// The address to download the file from, or null when no source has it.
    pub url: Option<String>,
    /// The file's SHA-1 as CurseForge lists it, null when CurseForge does not know the file.
    pub sha1: Option<String>,
    /// The file's name as CurseForge lists it, null when CurseForge does not know the file.
    pub file_name: Option<String>,
    /// The file's size in bytes as CurseForge lists it, null when CurseForge does not know the file.
    pub size: Option<u64>,
    /// True when a source could not be asked for the file, so that a null `source` is worth asking again later
    /// rather than remembering.
    pub retry: bool,
}

/// What `POST /v1/resolve` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Resolved {
    /// One entry per file id asked for, in the order asked, each id once.
    pub files: Vec<ResolvedFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileRow {
    id: u64,
    file_name: Option<String>,
    file_length: Option<u64>,
    download_url: Option<String>,
    #[serde(default)]
    hashes: Vec<FileHash>,
}

#[derive(Deserialize)]
struct FileHash {
    value: String,
    algo: u8,
}

#[derive(Deserialize)]
struct Version {
    #[serde(default)]
    files: Vec<VersionFile>,
}

#[derive(Deserialize)]
struct VersionFile {
    hashes: VersionHashes,
    url: String,
}

#[derive(Deserialize)]
struct VersionHashes {
    sha1: Option<String>,
}

struct Known {
    file_id: u64,
    file_name: Option<String>,
    size: Option<u64>,
    sha1: Option<String>,
    download_url: Option<String>,
}

struct Candidate {
    file_id: u64,
    file_name: String,
    size: u64,
}

/// A download address for each CurseForge file: CurseForge's own link, else Modrinth's copy of the file found by
/// its SHA-1, else the file's built address on CurseForge's CDN when a HEAD answers 200 with the file's size.
///
/// The files are read from the cache like `POST /v1/curseforge/mods/files`, and the CurseForge calls for the ones
/// not kept count against the budget. Modrinth answers and CDN verdicts are kept for an hour, misses for 10
/// minutes. No bytes pass through the gateway: the consumer downloads from the address given. A null `source` with
/// `retry` false means no source has the file; `retry` true means a source could not be asked this time. At worst
/// a call takes 40 s: 10 s for CurseForge, 10 s for Modrinth and 20 s for the CDN checks.
#[utoipa::path(
    post,
    path = "/v1/resolve",
    tag = "gateway",
    security(("bearer" = [])),
    params(
        ("X-Budget-Pool" = Option<String>, Header, description = "The pool the CurseForge calls draw from. Required when the token has more than one pool"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks every source again instead of answering from the cache"),
    ),
    request_body(content = ResolveRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "An address per file, in the order asked", body = Resolved),
        (status = 400, description = "`INVALID_BODY`, `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 413, description = "The files need more CurseForge calls than a whole minute allows, `REQUEST_TOO_LARGE`; split them", body = ApiError),
        (status = 502, description = "CurseForge could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 503, description = "The CurseForge call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429", body = ApiError, headers(("Retry-After" = u64, description = "Seconds until the next call can go out"))),
        (status = 504, description = "CurseForge did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
    )
)]
pub async fn resolve(
    State(state): State<Arc<AppState>>,
    pool: Pool,
    headers: HeaderMap,
    body: Result<Json<ResolveRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return ApiError::invalid_body(
            "a JSON object with `fileIds`, a list of CurseForge file ids",
        )
        .into_response();
    };
    if request.file_ids.is_empty() {
        return ApiError::invalid_body("`fileIds` must list at least one file id").into_response();
    }
    let mut seen = HashSet::new();
    let mut ids: Vec<u64> = Vec::new();
    for id in &request.file_ids {
        if seen.insert(*id) {
            ids.push(*id);
        }
        if ids.len() > MOST_FILES {
            return ApiError::invalid_body("`fileIds` may list at most 1000 file ids")
                .into_response();
        }
    }
    let forced = passthrough::wants_fresh(&headers);

    let rows = match batch::rows(
        state.clone(),
        pool.clone(),
        Batch::Files,
        ids.clone(),
        forced,
    )
    .await
    {
        Ok(rows) => rows,
        Err(response) => return *response,
    };
    let known: HashMap<u64, Known> = rows
        .rows
        .iter()
        .filter_map(|(id, row)| {
            let row: FileRow = serde_json::from_slice(row).ok()?;
            Some((
                *id,
                Known {
                    file_id: row.id,
                    file_name: row.file_name,
                    size: row.file_length,
                    sha1: row
                        .hashes
                        .iter()
                        .find(|hash| hash.algo == SHA1_ALGO)
                        .and_then(|hash| modrinth::sha1_of(&hash.value)),
                    download_url: row.download_url,
                },
            ))
        })
        .collect();

    let wanted: Vec<&Known> = ids
        .iter()
        .filter_map(|id| known.get(id))
        .filter(|file| file.download_url.is_none())
        .collect();
    let hashes: Vec<String> = wanted.iter().filter_map(|file| file.sha1.clone()).collect();
    let (on_modrinth, modrinth_failed) = if hashes.is_empty() {
        (HashMap::new(), false)
    } else {
        match modrinth::lookup(&state, &pool, hashes, forced).await {
            Ok(looked) => (
                looked
                    .known
                    .into_iter()
                    .filter_map(|known| Some((known.hash.clone(), modrinth_url(&known)?)))
                    .collect::<HashMap<String, String>>(),
                false,
            ),
            Err(failure) => {
                tracing::warn!(
                    "Modrinth could not be asked for {} files: {failure:?}",
                    wanted.len()
                );
                (HashMap::new(), true)
            }
        }
    };

    let on_cdn = if request.cdn {
        let candidates: Vec<Candidate> = wanted
            .iter()
            .filter(|file| {
                file.sha1
                    .as_ref()
                    .is_none_or(|sha1| !on_modrinth.contains_key(sha1))
            })
            .filter_map(|file| {
                Some(Candidate {
                    file_id: file.file_id,
                    file_name: file.file_name.clone()?,
                    size: file.size?,
                })
            })
            .collect();
        check_cdn(&state, &candidates, forced).await
    } else {
        HashMap::new()
    };

    let files = ids
        .iter()
        .map(|id| {
            let Some(file) = known.get(id) else {
                return ResolvedFile {
                    file_id: *id,
                    source: None,
                    url: None,
                    sha1: None,
                    file_name: None,
                    size: None,
                    retry: false,
                };
            };
            let (found, retry) = if let Some(url) = &file.download_url {
                (Some((FileSource::Curseforge, url.clone())), false)
            } else if let Some(url) = file.sha1.as_ref().and_then(|sha1| on_modrinth.get(sha1)) {
                (Some((FileSource::Modrinth, url.clone())), false)
            } else {
                let cdn = if request.cdn && file.file_name.is_some() && file.size.is_some() {
                    on_cdn.get(id)
                } else {
                    Some(&None)
                };
                match cdn {
                    Some(Some(url)) => (Some((FileSource::CurseforgeCdn, url.clone())), false),
                    Some(None) => (None, modrinth_failed && file.sha1.is_some()),
                    None => (None, true),
                }
            };
            let (source, url) =
                found.map_or((None, None), |(source, url)| (Some(source), Some(url)));
            ResolvedFile {
                file_id: *id,
                source,
                url,
                sha1: file.sha1.clone(),
                file_name: file.file_name.clone(),
                size: file.size,
                retry,
            }
        })
        .collect();
    Json(Resolved { files }).into_response()
}

fn modrinth_url(known: &modrinth::Known) -> Option<String> {
    let version: Version = serde_json::from_slice(known.version.as_ref()?).ok()?;
    version
        .files
        .into_iter()
        .find(|file| {
            file.hashes
                .sha1
                .as_ref()
                .is_some_and(|sha1| sha1.eq_ignore_ascii_case(&known.hash))
                && is_modrinth_cdn(&file.url)
        })
        .map(|file| file.url)
}

fn is_modrinth_cdn(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|url| url.scheme() == "https" && url.host_str() == Some(MODRINTH_CDN_HOST))
}

fn cdn_key(file_id: u64, size: u64) -> String {
    format!("cdn:{file_id}:{size}")
}

/// Each candidate's address on the CDN when it answers a HEAD with the file's size, `None` when it does not have
/// the file, absent when it could not be asked within the wait.
async fn check_cdn(
    state: &Arc<AppState>,
    candidates: &[Candidate],
    forced: bool,
) -> HashMap<u64, Option<String>> {
    let mut verdicts = HashMap::with_capacity(candidates.len());
    if candidates.is_empty() {
        return verdicts;
    }
    let keys: Vec<String> = candidates
        .iter()
        .map(|file| cdn_key(file.file_id, file.size))
        .collect();
    let now = state.clock.now_ms();
    let kept = state.cache.lookup_many(&keys, now).await;
    let mut to_check = Vec::new();
    for (file, kept) in candidates.iter().zip(kept) {
        match kept.filter(|entry| !forced && entry.is_fresh(now)) {
            Some(entry) if entry.answer.status == StatusCode::OK => {
                verdicts.insert(
                    file.file_id,
                    String::from_utf8(entry.answer.body.to_vec()).ok(),
                );
            }
            Some(_) => {
                verdicts.insert(file.file_id, None);
            }
            None => to_check.push((file.file_id, file.file_name.clone(), file.size)),
        }
    }
    if to_check.is_empty() {
        return verdicts;
    }

    let at_once = Arc::new(Semaphore::new(CDN_AT_ONCE));
    let mut checks = JoinSet::new();
    for (file_id, file_name, size) in to_check {
        let (state, at_once) = (state.clone(), at_once.clone());
        checks.spawn(async move {
            let _slot = at_once.acquire().await;
            (file_id, head(&state, file_id, &file_name, size).await)
        });
    }
    let collect = async {
        while let Some(joined) = checks.join_next().await {
            if let Ok((file_id, Some(verdict))) = joined {
                verdicts.insert(file_id, verdict);
            }
        }
    };
    if tokio::time::timeout(CDN_WAIT, collect).await.is_err() {
        tracing::warn!("CurseForge's CDN could not be asked about every file within {CDN_WAIT:?}");
    }
    verdicts
}

async fn head(
    state: &AppState,
    file_id: u64,
    file_name: &str,
    size: u64,
) -> Option<Option<String>> {
    let url = state.cdn.url(file_id, file_name);
    let verdict = match state.cdn.head(&url).await {
        Ok((StatusCode::OK, Some(length))) if length == size => Some(url),
        Ok((StatusCode::OK, length)) => {
            tracing::warn!(
                "CurseForge's CDN lists {} for file {file_id}, CurseForge's API lists {size} bytes",
                length.map_or("no size".to_owned(), |length| length.to_string())
            );
            None
        }
        Ok((StatusCode::FORBIDDEN | StatusCode::NOT_FOUND, _)) => None,
        Ok((status, _)) => {
            tracing::warn!("CurseForge's CDN answered {status} for file {file_id}");
            return None;
        }
        Err(error) => {
            tracing::warn!("CurseForge's CDN could not be asked about file {file_id}: {error}");
            return None;
        }
    };
    let (answer, fresh_ms) = match &verdict {
        Some(url) => (
            Answer {
                status: StatusCode::OK,
                content_type: Some(HeaderValue::from_static("text/plain")),
                body: Bytes::from(url.clone()),
            },
            CDN_FOUND_MS,
        ),
        None => (
            Answer {
                status: StatusCode::NOT_FOUND,
                content_type: None,
                body: Bytes::new(),
            },
            CDN_NOT_FOUND_MS,
        ),
    };
    state.cache.insert(
        &cdn_key(file_id, size),
        Arc::new(Entry::new(answer, state.clock.now_ms(), fresh_ms)),
    );
    Some(verdict)
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use serde_json::{Value, json};

    use super::*;
    use crate::{curseforge::TIMEOUT, test_support::*};

    const JEI_SHA1: &str = "3e1dd9fb5ea2f0dd4ffcc2b1a7d0bb1a1a9d6a8e";
    const SODIUM_SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
    const CREATE_SHA1: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";

    fn file(id: u64, name: &str, size: u64, sha1: &str, url: Option<&str>) -> Value {
        json!({
            "id": id, "modId": id / 10, "fileName": name, "fileLength": size,
            "downloadUrl": url,
            "hashes": [{ "value": sha1.to_ascii_uppercase(), "algo": 1 }, { "value": "md5md5", "algo": 2 }]
        })
    }

    fn files_answer(files: &[Value]) -> String {
        json!({ "data": files }).to_string()
    }

    fn modrinth_answer(found: &[(&str, &str)]) -> String {
        let mut body = serde_json::Map::new();
        for (sha1, name) in found {
            body.insert(
                (*sha1).to_owned(),
                json!({ "id": "v", "files": [
                    { "hashes": { "sha1": sha1 }, "url": format!("https://cdn.modrinth.com/data/p/versions/v/{name}"), "filename": name, "size": 1 },
                    { "hashes": { "sha1": "ffffffffffffffffffffffffffffffffffffffff" }, "url": "https://cdn.modrinth.com/data/p/versions/v/other.jar", "filename": "other.jar", "size": 1 }
                ] }),
            );
        }
        Value::Object(body).to_string()
    }

    fn cdn_reply(status: u16, length: &'static str) -> Reply {
        Reply {
            status,
            headers: vec![("content-length", length)],
            body: String::new(),
        }
    }

    async fn resolve(gateway: &Gateway, body: &str) -> Got {
        send(
            &gateway.router,
            Request::post("/v1/resolve").header("content-type", "application/json"),
            body,
        )
        .await
    }

    #[tokio::test]
    async fn takes_curseforge_then_modrinth_then_the_cdn_and_says_when_nothing_has_a_file() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                files_answer(&[
                    file(
                        1001,
                        "jei.jar",
                        10,
                        JEI_SHA1,
                        Some("https://edge.forgecdn.net/jei.jar"),
                    ),
                    file(1002, "sodium.jar", 20, SODIUM_SHA1, None),
                    file(1003, "create 1.0(a).jar", 30, CREATE_SHA1, None),
                    file(
                        1004,
                        "gone.jar",
                        40,
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        None,
                    ),
                ]),
            ),
            "/v2/version_files" => reply(200, modrinth_answer(&[(SODIUM_SHA1, "sodium.jar")])),
            "/files/1/3/create%201.0%28a%29.jar" => cdn_reply(200, "30"),
            "/files/1/4/gone.jar" => cdn_reply(403, "0"),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let got = resolve(&gateway, r#"{"fileIds":[1001,1002,1003,1004,9999,1001]}"#).await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.header("x-cache"), None);
        let files = got.json()["files"].clone();
        assert_eq!(files.as_array().unwrap().len(), 5);
        assert_eq!(files[0]["source"], "curseforge");
        assert_eq!(files[0]["url"], "https://edge.forgecdn.net/jei.jar");
        assert_eq!(files[0]["sha1"], JEI_SHA1);
        assert_eq!(files[0]["fileName"], "jei.jar");
        assert_eq!(files[0]["size"], 10);
        assert_eq!(files[1]["source"], "modrinth");
        assert_eq!(
            files[1]["url"],
            "https://cdn.modrinth.com/data/p/versions/v/sodium.jar"
        );
        assert_eq!(files[2]["source"], "curseforge-cdn");
        assert_eq!(
            files[2]["url"],
            format!("{}/files/1/3/create%201.0%28a%29.jar", mock.base)
        );
        assert_eq!(files[3]["source"], Value::Null);
        assert_eq!(files[3]["url"], Value::Null);
        assert_eq!(files[3]["retry"], false);
        assert_eq!(files[4]["fileId"], 9999);
        assert_eq!(files[4]["source"], Value::Null);
        assert_eq!(files[4]["sha1"], Value::Null);
        assert_eq!(files[4]["retry"], false);

        let seen = mock.seen();
        let modrinth = seen
            .iter()
            .find(|seen| seen.uri == "/v2/version_files")
            .unwrap();
        let asked: Value = serde_json::from_str(&modrinth.body).unwrap();
        assert_eq!(
            asked["hashes"],
            json!([
                SODIUM_SHA1,
                CREATE_SHA1,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ])
        );
        let heads: Vec<&Seen> = seen.iter().filter(|seen| seen.method == "HEAD").collect();
        assert_eq!(heads.len(), 2);
        assert_eq!(gateway.state.budget.snapshot().await.used, 1);

        let again = resolve(&gateway, r#"{"fileIds":[1003,1004]}"#).await;
        assert_eq!(again.json()["files"][0]["source"], "curseforge-cdn");
        assert_eq!(mock.calls(), seen.len());
    }

    #[tokio::test]
    async fn the_cdn_is_skipped_when_asked_and_a_wrong_size_is_no_answer() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                files_answer(&[
                    file(1003, "create.jar", 30, CREATE_SHA1, None),
                    file(1005, "short.jar", 50, SODIUM_SHA1, None),
                ]),
            ),
            "/v2/version_files" => reply(200, "{}"),
            "/files/1/5/short.jar" => cdn_reply(200, "49"),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let off = resolve(&gateway, r#"{"fileIds":[1003],"cdn":false}"#).await;
        assert_eq!(off.json()["files"][0]["source"], Value::Null);
        assert_eq!(off.json()["files"][0]["retry"], false);
        assert!(mock.seen().iter().all(|seen| seen.method != "HEAD"));

        let wrong = resolve(&gateway, r#"{"fileIds":[1005]}"#).await;
        assert_eq!(wrong.json()["files"][0]["source"], Value::Null);
        assert_eq!(wrong.json()["files"][0]["retry"], false);
        resolve(&gateway, r#"{"fileIds":[1005]}"#).await;
        assert_eq!(
            mock.seen()
                .iter()
                .filter(|seen| seen.method == "HEAD")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_modrinth_failure_asks_the_cdn_and_marks_the_rest_for_a_retry() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                files_answer(&[
                    file(1003, "create.jar", 30, CREATE_SHA1, None),
                    file(1006, "lost.jar", 60, SODIUM_SHA1, None),
                ]),
            ),
            "/v2/version_files" => reply(500, "{}"),
            "/files/1/3/create.jar" => cdn_reply(200, "30"),
            "/files/1/6/lost.jar" => cdn_reply(404, "0"),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let got = resolve(&gateway, r#"{"fileIds":[1003,1006]}"#).await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.json()["files"][0]["source"], "curseforge-cdn");
        assert_eq!(got.json()["files"][0]["retry"], false);
        assert_eq!(got.json()["files"][1]["source"], Value::Null);
        assert_eq!(got.json()["files"][1]["retry"], true);
    }

    #[tokio::test]
    async fn a_curseforge_failure_is_the_passthrough_s_answer() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(500, "{}")).await;
        let failing = gateway(&mock, 300, TIMEOUT);
        let got = resolve(&failing, r#"{"fileIds":[1003]}"#).await;
        assert_eq!(got.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(got.header("x-cache"), Some("miss"));

        let refused = gateway(&mock, 1, TIMEOUT);
        resolve(&refused, r#"{"fileIds":[1003]}"#).await;
        let got = resolve(&refused, r#"{"fileIds":[1004]}"#).await;
        assert_eq!(got.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(got.json()["code"], "BUDGET_SPENT");
    }

    #[tokio::test]
    async fn refuses_a_bad_body() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        for body in ["{}", r#"{"fileIds":[]}"#, r#"{"fileIds":["x"]}"#, "nope"] {
            let got = resolve(&gateway, body).await;
            assert_eq!(got.status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(got.json()["code"], "INVALID_BODY", "{body}");
        }
        let many: Vec<String> = (0..1001).map(|id| id.to_string()).collect();
        let got = resolve(&gateway, &format!(r#"{{"fileIds":[{}]}}"#, many.join(","))).await;
        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert_eq!(mock.calls(), 0);
    }

    #[test]
    fn the_longest_call_is_the_three_sources_in_a_row() {
        assert_eq!(TIMEOUT + modrinth::TIMEOUT + CDN_WAIT, LONGEST);
    }

    #[tokio::test]
    async fn a_file_without_a_name_or_size_keeps_its_link_and_skips_the_cdn() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                json!({ "data": [
                    { "id": 1007, "modId": 100, "downloadUrl": "https://edge.forgecdn.net/bare.jar", "hashes": [] },
                    { "id": 1008, "modId": 100, "fileLength": 80, "hashes": [{ "value": CREATE_SHA1, "algo": 1 }] },
                ] })
                .to_string(),
            ),
            "/v2/version_files" => reply(200, "{}"),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let got = resolve(&gateway, r#"{"fileIds":[1007,1008]}"#).await;
        let files = got.json()["files"].clone();
        assert_eq!(files[0]["source"], "curseforge");
        assert_eq!(files[0]["fileName"], Value::Null);
        assert_eq!(files[0]["size"], Value::Null);
        assert_eq!(files[1]["source"], Value::Null);
        assert_eq!(files[1]["sha1"], CREATE_SHA1);
        assert_eq!(files[1]["size"], 80);
        assert_eq!(files[1]["retry"], false);
        assert!(mock.seen().iter().all(|seen| seen.method != "HEAD"));
    }

    #[test]
    fn builds_the_cdn_address_like_curseforge_does() {
        let cdn = Cdn::new(CURSEFORGE_CDN, CDN_TIMEOUT).unwrap();
        assert_eq!(
            cdn.url(4567890, "Create-1.20.1 (v0.5.1f)!'*.jar"),
            "https://mediafilez.forgecdn.net/files/4567/890/Create-1.20.1%20%28v0.5.1f%29%21%27%2A.jar"
        );
        assert_eq!(
            cdn.url(999, "ü.jar"),
            "https://mediafilez.forgecdn.net/files/0/999/%C3%BC.jar"
        );
    }
}

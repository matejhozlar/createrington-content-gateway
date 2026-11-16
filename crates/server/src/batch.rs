use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, de::IgnoredAny};
use serde_json::value::RawValue;

use crate::{
    app::AppState,
    auth::Pool,
    cache::{Answer, Entry},
    clock::{HOUR_MS, MINUTE_MS},
    error::ApiError,
    load::{self, PerId},
    passthrough,
    upstream::Source,
};

const MOST_IDS: usize = 1000;

/// A CurseForge batch route answered per id: `POST /mods`, `POST /mods/files` and `POST /fingerprints`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Batch {
    Mods,
    Files,
    Fingerprints,
    MinecraftFingerprints,
}

const MINECRAFT: &str = "432";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModIds {
    #[serde(rename = "modIds")]
    ids: Vec<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIds {
    #[serde(rename = "fileIds")]
    ids: Vec<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FingerprintIds {
    fingerprints: Vec<u64>,
}

#[derive(Deserialize)]
struct Matches<'a> {
    #[serde(borrow)]
    data: MatchLists<'a>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchLists<'a> {
    is_cache_built: bool,
    #[serde(borrow)]
    exact_matches: Vec<&'a RawValue>,
    #[serde(borrow)]
    partial_matches: Vec<&'a RawValue>,
    partial_match_fingerprints: HashMap<String, IgnoredAny>,
    #[serde(borrow, default)]
    unmatched_fingerprints: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Match {
    file: MatchedFile,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchedFile {
    file_fingerprint: u64,
}

#[derive(Deserialize)]
struct DataRows<'a> {
    #[serde(borrow)]
    data: Vec<&'a RawValue>,
}

#[derive(Deserialize)]
struct Single<'a> {
    #[serde(borrow)]
    data: &'a RawValue,
}

#[derive(Deserialize)]
struct RowIds {
    id: u64,
    #[serde(rename = "modId")]
    mod_id: Option<u64>,
}

impl Batch {
    pub fn of(segments: &[&str]) -> Option<Self> {
        match segments {
            ["mods"] => Some(Self::Mods),
            ["mods", "files"] => Some(Self::Files),
            ["fingerprints"] => Some(Self::Fingerprints),
            ["fingerprints", MINECRAFT] => Some(Self::MinecraftFingerprints),
            _ => None,
        }
    }

    /// The ids of a body that holds only them, as asked. `None` for any other body.
    pub fn ids(self, body: &[u8]) -> Option<Vec<u64>> {
        let ids = match self {
            Self::Mods => serde_json::from_slice::<ModIds>(body).ok()?.ids,
            Self::Files => serde_json::from_slice::<FileIds>(body).ok()?.ids,
            Self::Fingerprints | Self::MinecraftFingerprints => {
                serde_json::from_slice::<FingerprintIds>(body)
                    .ok()?
                    .fingerprints
            }
        };
        (!ids.is_empty()).then_some(ids)
    }

    pub fn found_ms(self) -> u64 {
        match self {
            Self::Mods | Self::Fingerprints | Self::MinecraftFingerprints => HOUR_MS,
            Self::Files => 10 * MINUTE_MS,
        }
    }

    fn is_fingerprints(self) -> bool {
        matches!(self, Self::Fingerprints | Self::MinecraftFingerprints)
    }

    fn field(self) -> &'static str {
        match self {
            Self::Mods => "modIds",
            Self::Files => "fileIds",
            Self::Fingerprints | Self::MinecraftFingerprints => "fingerprints",
        }
    }
}

impl PerId for Batch {
    type Id = u64;
    const SOURCE: Source = Source::CurseForge;
    const PER_CALL: usize = 100;
    const KNOWS_NONE_AS_404: bool = true;

    /// The cache key of one id, shared with `GET /mods/{id}` and `GET /mods/{modId}/files/{id}`.
    fn key(self, id: &u64) -> String {
        match self {
            Self::Mods => format!("mod:{id}"),
            Self::Files => format!("file:{id}"),
            Self::Fingerprints => format!("fingerprint:{id}"),
            Self::MinecraftFingerprints => format!("fingerprint:{MINECRAFT}:{id}"),
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Mods => "mods",
            Self::Files => "mods/files",
            Self::Fingerprints => "fingerprints",
            Self::MinecraftFingerprints => "fingerprints/432",
        }
    }

    fn body(self, ids: &[u64]) -> Bytes {
        let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
        format!("{{\"{}\":[{}]}}", self.field(), ids.join(",")).into()
    }

    fn split(self, answer: &Answer, now_ms: u64) -> Option<Vec<(u64, Entry)>> {
        let rows = if self.is_fingerprints() {
            split_matches(answer)?
        } else {
            split_rows(answer)?
        };
        Some(
            rows.into_iter()
                .map(|(id, row, owner)| {
                    let entry = kept_row(self, &row, owner, &answer.content_type, now_ms);
                    (id, entry)
                })
                .collect(),
        )
    }

    fn not_found_ms(self) -> u64 {
        match self {
            Self::Mods => 5 * MINUTE_MS,
            Self::Files | Self::Fingerprints | Self::MinecraftFingerprints => 10 * MINUTE_MS,
        }
    }
}

/// The object under `data` of a single answer, a slice of its body, with its project when it is a file.
pub fn row_of(body: &Bytes) -> Option<(Bytes, Option<u64>)> {
    let single: Single<'_> = serde_json::from_slice(body).ok()?;
    let owner = serde_json::from_str::<RowIds>(single.data.get())
        .ok()
        .and_then(|ids| ids.mod_id);
    Some((body.slice_ref(single.data.get().as_bytes()), owner))
}

/// The answer to a batch of ids from the per-id cache: the rows CurseForge knows, joined in its shape.
pub async fn answer(
    state: Arc<AppState>,
    pool: Pool,
    batch: Batch,
    asked: Vec<u64>,
    forced: bool,
) -> Response {
    let Some(ids) = load::distinct(asked.iter().copied(), MOST_IDS) else {
        return ApiError::invalid_body(&format!(
            "a list of at most {MOST_IDS} ids under `{}`",
            batch.field()
        ))
        .into_response();
    };
    let given = match load::rows(&state, &pool, batch, ids, forced).await {
        Ok(given) => given,
        Err(failure) => return passthrough::failed(failure, &pool, Source::CurseForge),
    };
    let (rows, cache) = (given.value, given.cache);
    if batch.is_fingerprints() {
        return passthrough::respond(&Answer::json(fingerprint_answer(&rows, &asked)), cache);
    }
    if rows.is_empty() {
        return passthrough::respond(&Answer::not_found(), cache);
    }
    let mut body =
        Vec::with_capacity(rows.iter().map(|(_, row)| row.len() + 1).sum::<usize>() + 11);
    body.extend_from_slice(b"{\"data\":[");
    join(&mut body, rows.iter().map(|(_, row)| row.as_ref()));
    body.extend_from_slice(b"]}");
    passthrough::respond(&Answer::json(body), cache)
}

fn join<'a>(body: &mut Vec<u8>, parts: impl Iterator<Item = &'a [u8]>) {
    for (index, part) in parts.enumerate() {
        if index > 0 {
            body.push(b',');
        }
        body.extend_from_slice(part);
    }
}

fn fingerprint_answer(rows: &[(u64, Bytes)], asked: &[u64]) -> Vec<u8> {
    let numbers = |ids: &mut dyn Iterator<Item = u64>| {
        ids.map(|id| id.to_string()).collect::<Vec<_>>().join(",")
    };
    let mut body =
        Vec::with_capacity(rows.iter().map(|(_, row)| row.len() + 1).sum::<usize>() + 256);
    body.extend_from_slice(b"{\"data\":{\"isCacheBuilt\":true,\"exactMatches\":[");
    join(&mut body, rows.iter().map(|(_, row)| row.as_ref()));
    body.extend_from_slice(b"],\"exactFingerprints\":[");
    body.extend_from_slice(numbers(&mut rows.iter().map(|(id, _)| *id)).as_bytes());
    body.extend_from_slice(
        b"],\"partialMatches\":[],\"partialMatchFingerprints\":{},\"installedFingerprints\":[",
    );
    body.extend_from_slice(numbers(&mut asked.iter().copied()).as_bytes());
    body.extend_from_slice(b"],\"unmatchedFingerprints\":null}}");
    body
}

fn split_rows(answer: &Answer) -> Option<Vec<(u64, Bytes, Option<u64>)>> {
    let rows: DataRows<'_> = serde_json::from_slice(&answer.body).ok()?;
    rows.data
        .iter()
        .map(|row| {
            let ids: RowIds = serde_json::from_str(row.get()).ok()?;
            Some((
                ids.id,
                answer.body.slice_ref(row.get().as_bytes()),
                ids.mod_id,
            ))
        })
        .collect()
}

fn split_matches(answer: &Answer) -> Option<Vec<(u64, Bytes, Option<u64>)>> {
    let matches: Matches<'_> = serde_json::from_slice(&answer.body).ok()?;
    let lists = matches.data;
    let plain = lists.is_cache_built
        && lists.partial_matches.is_empty()
        && lists.partial_match_fingerprints.is_empty()
        && lists.unmatched_fingerprints.is_none();
    if !plain {
        return None;
    }
    let mut grouped: Vec<(u64, Vec<&[u8]>)> = Vec::new();
    for raw in lists.exact_matches {
        let fingerprint = serde_json::from_str::<Match>(raw.get())
            .ok()?
            .file
            .file_fingerprint;
        match grouped.iter_mut().find(|(known, _)| *known == fingerprint) {
            Some((_, rows)) => rows.push(raw.get().as_bytes()),
            None => grouped.push((fingerprint, vec![raw.get().as_bytes()])),
        }
    }
    Some(
        grouped
            .into_iter()
            .map(|(fingerprint, rows)| {
                let mut row = Vec::new();
                join(&mut row, rows.into_iter());
                (fingerprint, Bytes::from(row), None)
            })
            .collect(),
    )
}

fn kept_row(
    batch: Batch,
    row: &Bytes,
    owner: Option<u64>,
    content_type: &Option<HeaderValue>,
    now: u64,
) -> Entry {
    if batch.is_fingerprints() {
        return Entry {
            row: Some(row.clone()),
            ..Entry::new(
                Answer {
                    status: StatusCode::OK,
                    content_type: content_type.clone(),
                    body: row.clone(),
                },
                now,
                batch.found_ms(),
            )
        };
    }
    let mut body = Vec::with_capacity(row.len() + 9);
    body.extend_from_slice(b"{\"data\":");
    body.extend_from_slice(row);
    body.push(b'}');
    let body = Bytes::from(body);
    let row = body.slice(8..body.len() - 1);
    Entry {
        row: Some(row),
        owner,
        ..Entry::new(
            Answer {
                status: StatusCode::OK,
                content_type: content_type.clone(),
                body,
            },
            now,
            batch.found_ms(),
        )
    }
}

#[cfg(test)]
mod tests;

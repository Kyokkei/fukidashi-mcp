//! Deterministic manga ingress for the local Fukidashi pipeline.
//!
//! MangaDex is the only native provider in v2. A direct URL is delegated to
//! an already provisioned gallery-dl executable, with its output constrained
//! to an owned staging directory before images are imported into a fresh
//! server-managed job.

use std::{
    cmp::Ordering,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
#[cfg(test)]
use image::{ImageFormat, RgbImage};
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use std::io::Cursor;
use tempfile::{Builder as TempDirBuilder, TempDir};
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore, time::sleep};
use uuid::Uuid;

use crate::{
    config::Config,
    workflow::{IngressPageFile, Registration, Workflow},
};

const MANGADEX_BASE_URL: &str = "https://api.mangadex.org/";
const USER_AGENT: &str = "fukidashi-mcp/0.1 manga-ingress";
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
#[cfg(test)]
const MAX_IMAGE_PIXELS: u64 = 100_000_000;
#[cfg(test)]
const MAX_IMAGE_DIMENSION: u32 = 12_000;
const MAX_PAGES: usize = 500;
const MAX_RETRIES: usize = 3;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_TOTAL_IMAGE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_DIRECT_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_DIRECT_OUTPUT_FILES: usize = 500;
const MAX_FEED_PAGE_SIZE: usize = 100;
const MAX_LATEST_TIE_PAGES: usize = 2;
const OUTPUT_SCAN_INTERVAL: Duration = Duration::from_millis(250);
// A large gallery-dl chapter may legitimately take many minutes on a
// constrained connection. Keep a finite ceiling while still killing the
// helper and cleaning its owned staging directory on timeout.
const DIRECT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchMangaRequest {
    /// auto and mangadex are accepted; auto currently resolves to MangaDex.
    #[serde(default = "default_auto_source")]
    #[schemars(schema_with = "search_source_schema")]
    pub source: String,
    pub query: String,
    #[serde(default = "default_search_limit")]
    #[schemars(schema_with = "search_limit_schema")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PullChapterRequest {
    /// auto, mangadex, or direct.
    #[serde(default = "default_auto_source")]
    #[schemars(schema_with = "pull_source_schema")]
    pub source: String,
    /// Exact MangaDex manga UUID when using the native provider.
    #[serde(default)]
    pub manga_id: Option<String>,
    /// Exact MangaDex chapter UUID.
    #[serde(default)]
    pub chapter_id: Option<String>,
    /// Exact chapter number/string. Ambiguous matches are returned as an
    /// error with candidates instead of being guessed.
    #[serde(default, alias = "chapter_string")]
    pub chapter: Option<String>,
    /// Select the highest chapter value from the MangaDex feed. This requires
    /// manga_id and cannot be combined with chapter_id or chapter.
    #[schemars(schema_with = "latest_schema")]
    pub latest: bool,
    /// Exact translated language code used for MangaDex feed filtering.
    #[serde(default)]
    pub translated_language: Option<String>,
    /// "full" selects MangaDex's data list; "data_saver" explicitly selects
    /// its dataSaver list. Legacy JSON booleans are accepted as aliases.
    #[serde(
        default = "default_data_mode",
        deserialize_with = "deserialize_data_mode"
    )]
    #[schemars(schema_with = "data_mode_schema")]
    pub data_saver: String,
    /// Explicit http(s) URL for the gallery-dl mode.
    #[serde(default, alias = "direct_url")]
    pub url: Option<String>,
    /// Optional human-readable label for a direct import. It is sanitized by
    /// the workflow and never controls an output path.
    #[serde(default, alias = "title")]
    #[schemars(length(max = 96))]
    pub job_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PullChapterRequestWire {
    #[serde(default = "default_auto_source")]
    source: String,
    #[serde(default)]
    manga_id: Option<String>,
    #[serde(default)]
    chapter_id: Option<String>,
    #[serde(default, alias = "chapter_string")]
    chapter: Option<String>,
    latest: Option<bool>,
    #[serde(default)]
    translated_language: Option<String>,
    #[serde(
        default = "default_data_mode",
        deserialize_with = "deserialize_data_mode"
    )]
    data_saver: String,
    #[serde(default, alias = "direct_url")]
    url: Option<String>,
    #[serde(default, alias = "title")]
    job_name: Option<String>,
}

impl<'de> Deserialize<'de> for PullChapterRequest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = PullChapterRequestWire::deserialize(deserializer)?;
        Ok(Self {
            source: wire.source,
            manga_id: wire.manga_id,
            chapter_id: wire.chapter_id,
            chapter: wire.chapter,
            latest: wire.latest.unwrap_or(false),
            translated_language: wire.translated_language,
            data_saver: wire.data_saver,
            url: wire.url,
            job_name: wire.job_name,
        })
    }
}

fn default_auto_source() -> String {
    "auto".to_owned()
}

fn default_search_limit() -> usize {
    10
}

fn search_limit_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "integer",
        "minimum": 1,
        "maximum": 20,
        "default": 10
    })
}

fn default_data_mode() -> String {
    "full".to_owned()
}

fn deserialize_data_mode<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Bool(true) => Ok("data_saver".to_owned()),
        Value::Bool(false) => Ok("full".to_owned()),
        Value::String(value) => match value.as_str() {
            "full" | "data" => Ok("full".to_owned()),
            "data_saver" | "dataSaver" => Ok("data_saver".to_owned()),
            _ => Err(serde::de::Error::custom(
                "data_saver must be full or data_saver",
            )),
        },
        _ => Err(serde::de::Error::custom(
            "data_saver must be a boolean compatibility value or a full/data_saver string",
        )),
    }
}

fn data_mode_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["full", "data_saver"],
        "default": "full"
    })
}

fn search_source_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "string", "enum": ["auto", "mangadex"]})
}

fn pull_source_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "string", "enum": ["auto", "mangadex", "direct"]})
}

fn latest_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "boolean",
        "default": false,
        "description": "Set true for the highest MangaDex chapter; omitted means false."
    })
}

#[derive(Debug, Clone)]
pub struct MangaDexClient {
    http: Client,
    base_url: Url,
    allow_insecure_local: bool,
}

impl MangaDexClient {
    pub fn new() -> Result<Self> {
        Self::with_base_url(MANGADEX_BASE_URL)
    }

    /// Build a client for the production API or for a loopback mock server in
    /// tests. Plain HTTP is accepted only for loopback hosts.
    pub fn with_base_url(raw: &str) -> Result<Self> {
        let base_url = Url::parse(raw).context("parse MangaDex API base URL")?;
        if base_url.host_str().is_none() || !base_url.path().ends_with('/') {
            bail!("MangaDex API base URL must have a host and trailing slash");
        }
        let allow_insecure_local =
            base_url.scheme() == "http" && base_url.host_str().is_some_and(is_loopback_host);
        if base_url.scheme() != "https" && !allow_insecure_local {
            bail!("MangaDex API and image hosts must use HTTPS");
        }
        let http = Client::builder()
            .redirect(Policy::custom(move |attempt| {
                if attempt.previous().len() >= 3 {
                    return attempt.stop();
                }
                if validate_download_base(attempt.url(), allow_insecure_local).is_ok() {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .user_agent(USER_AGENT)
            .build()
            .context("build bounded MangaDex HTTP client")?;
        Ok(Self {
            http,
            base_url,
            allow_insecure_local,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    fn uses_data_saver(request: &PullChapterRequest) -> bool {
        request.data_saver == "data_saver"
    }

    pub async fn search(&self, request: &SearchMangaRequest) -> Result<Value> {
        validate_search_request(request)?;
        let mut url = self.endpoint("manga")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("title", request.query.trim());
            query.append_pair("limit", &request.limit.to_string());
            query.append_pair("offset", "0");
        }
        let body = self.get_json(url).await?;
        let data = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("MangaDex search response has no data array"))?;
        let results = data
            .iter()
            .map(parse_manga_result)
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({
            "provider": "mangadex",
            "query": request.query.trim(),
            "results": results,
        }))
    }

    /// Resolve an exact MangaDex chapter without downloading its pages.
    pub async fn resolve_chapter(&self, request: &PullChapterRequest) -> Result<Value> {
        let resolved = self.resolve_chapter_record(request).await?;
        Ok(resolved.summary)
    }

    pub async fn pull_mangadex(
        &self,
        config: &Config,
        request: &PullChapterRequest,
        workflow: &Workflow,
    ) -> Result<Value> {
        validate_mangadex_request(request)?;
        let chapter = self.resolve_chapter_record(request).await?;
        let manga_title = self.manga_title(&chapter.manga_id).await?;
        if chapter.external_url.is_some() {
            return Ok(external_release_response(&chapter, manga_title.as_deref()));
        }
        let use_data_saver = Self::uses_data_saver(request);
        let at_home = self.at_home(&chapter.id).await?;
        let page_key = if use_data_saver { "dataSaver" } else { "data" };
        let Some(file_names) = at_home.get(page_key).and_then(Value::as_array) else {
            return Ok(unavailable_release_response(
                &chapter,
                Some(manga_title.as_deref().unwrap_or_default()),
                format!("MangaDex at-home response has no {page_key} page list"),
            ));
        };
        if file_names.is_empty() {
            return Ok(unavailable_release_response(
                &chapter,
                Some(manga_title.as_deref().unwrap_or_default()),
                format!("MangaDex at-home response returned no {page_key} pages"),
            ));
        }
        if file_names.len() > MAX_PAGES {
            bail!("MangaDex chapter page count must be between 1 and {MAX_PAGES}");
        }
        let base = at_home
            .get("baseUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("MangaDex at-home response has no baseUrl"))?;
        let base = Url::parse(base).context("parse MangaDex at-home baseUrl")?;
        validate_download_base(&base, self.allow_insecure_local)?;
        let hash = at_home
            .get("chapter")
            .and_then(|value| value.get("hash"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("MangaDex at-home response has no chapter hash"))?;
        let mut page_urls = Vec::with_capacity(file_names.len());
        for file_name in file_names {
            let file_name = file_name
                .as_str()
                .ok_or_else(|| anyhow!("MangaDex page name is not a string"))?;
            validate_page_name(file_name)?;
            let mut url = base.clone();
            let trimmed_path = url.path().trim_end_matches('/').to_owned();
            url.set_path(if trimmed_path.is_empty() {
                "/"
            } else {
                &trimmed_path
            });
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| anyhow!("MangaDex at-home URL cannot carry path segments"))?;
                segments
                    .push(if use_data_saver { "data-saver" } else { "data" })
                    .push(hash)
                    .push(file_name);
            }
            page_urls.push((file_name.to_owned(), url));
        }
        page_urls.sort_by(|left, right| natural_cmp(&left.0, &right.0));
        let staging = create_provider_staging(config)?;
        let pages = self
            .download_pages_to_staging(page_urls, staging.path())
            .await?;
        let chapter_label = chapter
            .chapter
            .as_deref()
            .or(chapter.title.as_deref())
            .unwrap_or("unknown")
            .trim();
        let title_label = manga_title.as_deref().unwrap_or(&chapter.manga_id);
        let job_name = request.job_name.as_deref().unwrap_or("").trim();
        let slug = if job_name.is_empty() {
            format!("{title_label}-ch-{chapter_label}")
        } else {
            job_name.to_owned()
        };
        let provenance = json!({
            "provider": "mangadex",
            "manga_id": chapter.manga_id,
            "chapter_id": chapter.id,
            "chapter": chapter.chapter,
            "translated_language": request.translated_language,
            "data_saver": request.data_saver,
            "title": chapter.title,
            "manga_title": manga_title,
            "job_name": slug,
        });
        let registration =
            workflow.import_ingress_files(&slug, staging.path(), pages, provenance.clone())?;
        imported_response("mangadex", registration, provenance)
    }

    async fn resolve_chapter_record(
        &self,
        request: &PullChapterRequest,
    ) -> Result<ResolvedChapter> {
        validate_mangadex_request(request)?;
        if request.latest {
            return self.resolve_latest_chapter_record(request).await;
        }
        if let Some(chapter_id) = request.chapter_id.as_deref() {
            let id = validate_uuid(chapter_id, "chapter_id")?;
            let expected_manga_id = request
                .manga_id
                .as_deref()
                .map(|manga_id| validate_uuid(manga_id, "manga_id"))
                .transpose()?;
            let url = self.endpoint(&format!("chapter/{id}"))?;
            let body = self.get_json(url).await?;
            let data = body
                .get("data")
                .ok_or_else(|| anyhow!("MangaDex chapter response has no data"))?;
            let chapter = parse_chapter(data, expected_manga_id.as_deref())?;
            if let Some(expected) = request.chapter.as_deref()
                && chapter.chapter.as_deref() != Some(expected)
            {
                bail!(
                    "chapter_id {id} does not have the exact requested chapter string {expected:?}"
                );
            }
            if let Some(expected) = request.translated_language.as_deref()
                && chapter.translated_language.as_deref() != Some(expected)
            {
                bail!(
                    "chapter_id {id} translated_language mismatch: requested {expected:?}, MangaDex returned {:?}",
                    chapter.translated_language
                );
            }
            return Ok(chapter);
        }
        let manga_id = request
            .manga_id
            .as_deref()
            .ok_or_else(|| anyhow!("manga_id or exact chapter_id is required"))?;
        let manga_id = validate_uuid(manga_id, "manga_id")?;
        let mut url = self.endpoint(&format!("manga/{manga_id}/feed"))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("limit", "100");
            query.append_pair("offset", "0");
            query.append_pair("order[chapter]", "asc");
            query.append_pair("includes[]", "scanlation_group");
            if let Some(chapter) = request.chapter.as_deref() {
                query.append_pair("chapter", chapter);
            }
            if let Some(language) = request.translated_language.as_deref() {
                validate_language(language)?;
                query.append_pair("translatedLanguage[]", language);
            }
        }
        let body = self.get_json(url).await?;
        let entries = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("MangaDex feed response has no data array"))?;
        let mut chapters = entries
            .iter()
            .filter_map(|entry| parse_chapter(entry, Some(&manga_id)).ok())
            .filter(|chapter| {
                request
                    .chapter
                    .as_deref()
                    .is_none_or(|wanted| chapter.chapter.as_deref() == Some(wanted))
            })
            .collect::<Vec<_>>();
        chapters.sort_by(|left, right| left.id.cmp(&right.id));
        match chapters.as_slice() {
            [] => bail!("no MangaDex chapter matched the exact requested identifiers"),
            [chapter] => Ok(chapter.clone()),
            _ => {
                let candidates = chapters
                    .iter()
                    .map(|chapter| chapter.summary.clone())
                    .collect::<Vec<_>>();
                bail!(
                    "MangaDex chapter selection is ambiguous; provide chapter_id or a translated_language filter; candidates={}",
                    serde_json::to_string(&candidates)?
                );
            }
        }
    }

    async fn resolve_latest_chapter_record(
        &self,
        request: &PullChapterRequest,
    ) -> Result<ResolvedChapter> {
        let manga_id = request
            .manga_id
            .as_deref()
            .ok_or_else(|| anyhow!("latest=true requires manga_id"))?;
        let manga_id = validate_uuid(manga_id, "manga_id")?;
        let mut chapters = Vec::new();
        let mut offset = 0usize;
        let mut continuation_pages = 0usize;
        loop {
            let mut url = self.endpoint(&format!("manga/{manga_id}/feed"))?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", &MAX_FEED_PAGE_SIZE.to_string());
                query.append_pair("offset", &offset.to_string());
                query.append_pair("order[chapter]", "desc");
                query.append_pair("includes[]", "scanlation_group");
                if let Some(language) = request.translated_language.as_deref() {
                    validate_language(language)?;
                    query.append_pair("translatedLanguage[]", language);
                }
            }
            let body = self.get_json(url).await?;
            let entries = body
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("MangaDex feed response has no data array"))?;
            if entries.is_empty() {
                break;
            }
            let page_chapters = entries
                .iter()
                .map(|entry| parse_chapter(entry, Some(&manga_id)))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter(|chapter| {
                    request
                        .translated_language
                        .as_deref()
                        .is_none_or(|language| {
                            chapter.translated_language.as_deref() == Some(language)
                        })
                })
                .collect::<Vec<_>>();
            let current_max = max_chapter_value(&chapters);
            let page_max = page_chapters
                .iter()
                .map(|chapter| chapter.chapter.as_deref().unwrap_or_default())
                .max_by(|left, right| compare_chapter_values(left, right));
            if current_max.is_some_and(|current_max| {
                page_max
                    .is_none_or(|page_max| compare_chapter_values(page_max, current_max).is_lt())
            }) {
                break;
            }
            chapters.extend(page_chapters);
            if entries.len() < MAX_FEED_PAGE_SIZE
                || continuation_pages >= MAX_LATEST_TIE_PAGES
                || !latest_page_needs_tie_continuation(&chapters)
            {
                break;
            }
            continuation_pages += 1;
            offset = offset
                .checked_add(MAX_FEED_PAGE_SIZE)
                .ok_or_else(|| anyhow!("MangaDex feed pagination overflowed"))?;
        }
        chapters.sort_by(compare_latest_chapters);
        chapters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no MangaDex chapter matched the requested manga/language"))
    }

    async fn at_home(&self, chapter_id: &str) -> Result<Value> {
        self.get_json(self.endpoint(&format!("at-home/server/{chapter_id}"))?)
            .await
    }

    async fn manga_title(&self, manga_id: &str) -> Result<Option<String>> {
        let body = self
            .get_json(self.endpoint(&format!("manga/{manga_id}"))?)
            .await?;
        let attributes = body
            .get("data")
            .and_then(|data| data.get("attributes"))
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("MangaDex manga response has no attributes"))?;
        Ok(attributes
            .get("title")
            .and_then(Value::as_object)
            .and_then(pick_title))
    }

    async fn download_pages_to_staging(
        &self,
        pages: Vec<(String, Url)>,
        staging_root: &Path,
    ) -> Result<Vec<IngressPageFile>> {
        self.download_pages_to_staging_with_limit(pages, staging_root, MAX_TOTAL_IMAGE_BYTES)
            .await
    }

    async fn download_pages_to_staging_with_limit(
        &self,
        pages: Vec<(String, Url)>,
        staging_root: &Path,
        total_limit: u64,
    ) -> Result<Vec<IngressPageFile>> {
        let semaphore = Arc::new(Semaphore::new(4));
        let total_bytes = Arc::new(AtomicU64::new(0));
        let mut set = tokio::task::JoinSet::new();
        for (index, (name, url)) in pages.into_iter().enumerate() {
            let permit = semaphore.clone();
            let client = self.clone();
            let total_bytes = total_bytes.clone();
            let staging_path = staging_root.join(format!("{index:04}-{name}"));
            set.spawn(async move {
                let _permit = permit
                    .acquire_owned()
                    .await
                    .map_err(|_| anyhow!("download concurrency gate closed"))?;
                let bytes = client.get_bytes(url, MAX_IMAGE_BYTES).await?;
                reserve_download_bytes(&total_bytes, bytes.len(), total_limit)?;
                tokio::fs::write(&staging_path, &bytes)
                    .await
                    .with_context(|| format!("stage downloaded page {}", staging_path.display()))?;
                Ok::<_, anyhow::Error>((
                    index,
                    IngressPageFile {
                        original_name: name,
                        path: staging_path,
                    },
                ))
            });
        }
        let mut result = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok(page)) => result.push(page),
                Ok(Err(error)) => {
                    set.abort_all();
                    return Err(error);
                }
                Err(error) => {
                    set.abort_all();
                    return Err(anyhow!("MangaDex page download task failed: {error}"));
                }
            }
        }
        result.sort_by_key(|(index, _)| *index);
        Ok(result.into_iter().map(|(_, page)| page).collect())
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path)
            .with_context(|| format!("construct MangaDex endpoint {path}"))
    }

    async fn get_json(&self, url: Url) -> Result<Value> {
        let bytes = self.get_bytes(url, MAX_JSON_BYTES).await?;
        serde_json::from_slice(&bytes).context("parse MangaDex JSON response")
    }

    async fn get_bytes(&self, url: Url, max_bytes: usize) -> Result<Vec<u8>> {
        validate_download_base(&url, self.allow_insecure_local)?;
        let mut last_status = None;
        for attempt in 0..MAX_RETRIES {
            let response = self
                .http
                .get(url.clone())
                .send()
                .await
                .with_context(|| format!("request {}", url))?;
            validate_download_base(response.url(), self.allow_insecure_local)
                .context("validate final MangaDex response URL")?;
            let status = response.status();
            if status.is_success() {
                return read_response_limited(response, max_bytes).await;
            }
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|seconds| Duration::from_secs(seconds.min(5)));
            let body = read_response_limited(response, 16 * 1024)
                .await
                .unwrap_or_default();
            last_status = Some((status, String::from_utf8_lossy(&body).into_owned()));
            if !(status.as_u16() == 429 || status.is_server_error()) || attempt + 1 >= MAX_RETRIES {
                break;
            }
            sleep(retry_delay(attempt, retry_after)).await;
        }
        let (status, body) = last_status.unwrap_or_else(|| {
            (
                reqwest::StatusCode::REQUEST_TIMEOUT,
                "no response".to_owned(),
            )
        });
        bail!("HTTP {} from {}: {}", status, url, body.trim())
    }
}

#[derive(Debug, Clone)]
struct ResolvedChapter {
    id: String,
    manga_id: String,
    chapter: Option<String>,
    title: Option<String>,
    translated_language: Option<String>,
    external_url: Option<String>,
    published_at: Option<String>,
    readable_at: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    summary: Value,
}

fn compare_latest_chapters(left: &ResolvedChapter, right: &ResolvedChapter) -> Ordering {
    compare_chapter_values(
        right.chapter.as_deref().unwrap_or_default(),
        left.chapter.as_deref().unwrap_or_default(),
    )
    .then_with(|| language_preference(left).cmp(&language_preference(right)))
    .then_with(|| {
        compare_optional_desc(left.published_at.as_deref(), right.published_at.as_deref())
    })
    .then_with(|| compare_optional_desc(left.readable_at.as_deref(), right.readable_at.as_deref()))
    .then_with(|| compare_optional_desc(left.created_at.as_deref(), right.created_at.as_deref()))
    .then_with(|| compare_optional_desc(left.updated_at.as_deref(), right.updated_at.as_deref()))
    .then_with(|| left.id.cmp(&right.id))
}

fn language_preference(chapter: &ResolvedChapter) -> (u8, &str) {
    match chapter.translated_language.as_deref() {
        Some("en") => (0, "en"),
        Some(language) => (1, language),
        None => (2, ""),
    }
}

fn compare_optional_desc(left: Option<&str>, right: Option<&str>) -> Ordering {
    right.unwrap_or_default().cmp(left.unwrap_or_default())
}

fn max_chapter_value(chapters: &[ResolvedChapter]) -> Option<&str> {
    chapters
        .iter()
        .map(|chapter| chapter.chapter.as_deref().unwrap_or_default())
        .max_by(|left, right| compare_chapter_values(left, right))
}

fn min_chapter_value(chapters: &[ResolvedChapter]) -> Option<&str> {
    chapters
        .iter()
        .map(|chapter| chapter.chapter.as_deref().unwrap_or_default())
        .min_by(|left, right| compare_chapter_values(left, right))
}

fn latest_page_needs_tie_continuation(chapters: &[ResolvedChapter]) -> bool {
    let Some(maximum) = max_chapter_value(chapters) else {
        return false;
    };
    let Some(minimum) = min_chapter_value(chapters) else {
        return false;
    };
    compare_chapter_values(minimum, maximum).is_eq()
}

fn compare_chapter_values(left: &str, right: &str) -> Ordering {
    match (parse_numeric_chapter(left), parse_numeric_chapter(right)) {
        (Some(left), Some(right)) => compare_numeric_chapters(&left, &right),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => left.trim().cmp(right.trim()),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct NumericChapter {
    integer: String,
    fraction: String,
}

fn parse_numeric_chapter(raw: &str) -> Option<NumericChapter> {
    let trimmed = raw.trim();
    let mut parts = trimmed.split('.');
    let integer = parts.next()?;
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || integer.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let fraction = fraction.trim_end_matches('0');
    Some(NumericChapter {
        integer: integer.to_owned(),
        fraction: fraction.to_owned(),
    })
}

fn compare_numeric_chapters(left: &NumericChapter, right: &NumericChapter) -> Ordering {
    left.integer
        .len()
        .cmp(&right.integer.len())
        .then_with(|| left.integer.cmp(&right.integer))
        .then_with(|| {
            let length = left.fraction.len().max(right.fraction.len());
            (0..length)
                .map(|index| {
                    left.fraction
                        .as_bytes()
                        .get(index)
                        .copied()
                        .unwrap_or(b'0')
                        .cmp(
                            &right
                                .fraction
                                .as_bytes()
                                .get(index)
                                .copied()
                                .unwrap_or(b'0'),
                        )
                })
                .find(|ordering| *ordering != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        })
}

fn parse_manga_result(value: &Value) -> Result<Value> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("MangaDex search item has no stable id"))?;
    let attributes = value
        .get("attributes")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("MangaDex search item has no attributes"))?;
    let titles = attributes
        .get("title")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let display_title = pick_title(&titles).unwrap_or_else(|| id.to_owned());
    let alternate_titles = attributes
        .get("altTitles")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_object())
                .flat_map(|item| {
                    item.iter().filter_map(|(language, title)| {
                        title.as_str().map(|title| {
                            json!({
                                "language": language,
                                "title": title,
                            })
                        })
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(json!({
        "manga_id": id,
        "title": display_title,
        "original_language": attributes.get("originalLanguage").cloned().unwrap_or(Value::Null),
        "status": attributes.get("status").cloned().unwrap_or(Value::Null),
        "alternate_titles": alternate_titles,
        "suggested_next_step": {
            "tool": "fukidashi_pull_chapter",
            "arguments": {
                "source": "mangadex",
                "manga_id": id,
                "latest": true,
            },
            "description": "Resolve the highest chapter value, including external releases, without silently falling back to an older chapter."
        },
    }))
}

fn pick_title(titles: &serde_json::Map<String, Value>) -> Option<String> {
    ["en", "ja-ro", "ja", "zh", "ko"]
        .into_iter()
        .find_map(|language| titles.get(language).and_then(Value::as_str))
        .or_else(|| titles.values().find_map(Value::as_str))
        .map(str::to_owned)
}

fn parse_chapter(value: &Value, expected_manga_id: Option<&str>) -> Result<ResolvedChapter> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("MangaDex chapter has no stable id"))?;
    let attributes = value
        .get("attributes")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("MangaDex chapter has no attributes"))?;
    let manga_id = value
        .get("relationships")
        .and_then(Value::as_array)
        .and_then(|relationships| {
            relationships.iter().find_map(|relationship| {
                (relationship.get("type").and_then(Value::as_str) == Some("manga"))
                    .then(|| relationship.get("id").and_then(Value::as_str))
                    .flatten()
            })
        })
        .or(expected_manga_id)
        .ok_or_else(|| anyhow!("MangaDex chapter has no manga relationship"))?;
    if let Some(expected) = expected_manga_id
        && manga_id != expected
    {
        bail!("MangaDex chapter does not belong to manga_id {expected}");
    }
    let chapter = attributes
        .get("chapter")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = attributes
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let translated_language = attributes
        .get("translatedLanguage")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let external_url = attributes
        .get("externalUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let published_at = chapter_date_attribute(attributes, "publishAt");
    let readable_at = chapter_date_attribute(attributes, "readableAt");
    let created_at = chapter_date_attribute(attributes, "createdAt");
    let updated_at = chapter_date_attribute(attributes, "updatedAt");
    Ok(ResolvedChapter {
        id: id.to_owned(),
        manga_id: manga_id.to_owned(),
        chapter: chapter.clone(),
        title: title.clone(),
        translated_language,
        external_url: external_url.clone(),
        published_at: published_at.clone(),
        readable_at: readable_at.clone(),
        created_at: created_at.clone(),
        updated_at: updated_at.clone(),
        summary: json!({
            "chapter_id": id,
            "manga_id": manga_id,
            "chapter": chapter,
            "title": title,
            "translated_language": attributes.get("translatedLanguage").cloned().unwrap_or(Value::Null),
            "external_url": external_url,
            "published_at": published_at,
            "readable_at": readable_at,
            "created_at": created_at,
            "updated_at": updated_at,
        }),
    })
}

fn chapter_date_attribute(
    attributes: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<String> {
    attributes
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub async fn search_manga(request: SearchMangaRequest) -> Result<Value> {
    MangaDexClient::new()?.search(&request).await
}

pub async fn pull_chapter(
    config: &Config,
    workflow: &Workflow,
    request: PullChapterRequest,
) -> Result<Value> {
    let source = normalize_source(&request.source, request.url.is_some())?;
    match source {
        "mangadex" => {
            MangaDexClient::new()?
                .pull_mangadex(config, &request, workflow)
                .await
        }
        "direct" => pull_direct(config, workflow, &request).await,
        _ => unreachable!("source is validated"),
    }
}

async fn pull_direct(
    config: &Config,
    workflow: &Workflow,
    request: &PullChapterRequest,
) -> Result<Value> {
    validate_direct_request(request)?;
    let url = validate_direct_url(request.url.as_deref().unwrap_or_default())?;
    let helper = config.gallery_dl_path().ok_or_else(|| {
        anyhow!("gallery-dl is not installed; provision it or set FUKIDASHI_GALLERY_DL")
    })?;
    if !helper.is_file() {
        bail!(
            "configured gallery-dl helper is not a regular file: {}",
            helper.display()
        );
    }
    config.ensure_runtime_dirs()?;
    let staging = config
        .temp_dir()
        .join(format!(".fukidashi-direct-{}", Uuid::new_v4().simple()));
    create_owned_staging(&staging, &config.temp_dir())?;
    let result = async {
        run_gallery_dl(&helper, &staging, &url).await?;
        let files = collect_ingress_files(&staging)?;
        if files.is_empty() {
            bail!("gallery-dl completed without image files");
        }
        let pages = files
            .into_iter()
            .map(|file| {
                let name = file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("gallery-dl output has an invalid filename"))?;
                Ok(IngressPageFile {
                    original_name: name.to_owned(),
                    path: file,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let job_name = request
            .job_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| direct_job_label(&url));
        let provenance = json!({
            "provider": "direct",
            "url": url.as_str(),
            "helper": helper,
            "job_name": job_name,
        });
        let registration = workflow.import_ingress_files(
            &job_name,
            staging.as_path(),
            pages,
            provenance.clone(),
        )?;
        imported_response("direct", registration, provenance)
    }
    .await;
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn validate_search_request(request: &SearchMangaRequest) -> Result<()> {
    if !matches!(request.source.as_str(), "auto" | "mangadex") {
        bail!("source must be auto or mangadex for manga search");
    }
    let query = request.query.trim();
    if query.is_empty() || query.len() > 256 {
        bail!("query must contain between 1 and 256 characters");
    }
    if !(1..=20).contains(&request.limit) {
        bail!("limit must be between 1 and 20");
    }
    Ok(())
}

fn validate_mangadex_request(request: &PullChapterRequest) -> Result<()> {
    if request.url.is_some() {
        bail!("url is only accepted for source=direct");
    }
    if request.latest && request.manga_id.is_none() {
        bail!("latest=true requires manga_id");
    }
    if request.latest && request.chapter_id.is_some() {
        bail!("latest=true cannot be combined with chapter_id");
    }
    if request.latest && request.chapter.is_some() {
        bail!("latest=true cannot be combined with chapter");
    }
    if request.manga_id.is_none() && request.chapter_id.is_none() {
        bail!("manga_id or exact chapter_id is required for MangaDex ingress");
    }
    if request.chapter.is_some() && request.chapter_id.is_none() && request.manga_id.is_none() {
        bail!("chapter requires manga_id so the MangaDex feed can be resolved exactly");
    }
    if request
        .chapter
        .as_deref()
        .is_some_and(|chapter| chapter.trim().is_empty() || chapter.len() > 128)
    {
        bail!("chapter must be a non-empty exact string of at most 128 bytes");
    }
    if let Some(language) = request.translated_language.as_deref() {
        validate_language(language)?;
    }
    match request.data_saver.as_str() {
        "full" | "data_saver" => {}
        _ => bail!("data_saver must be full or data_saver"),
    }
    validate_job_name(request.job_name.as_deref())?;
    Ok(())
}

fn validate_direct_request(request: &PullChapterRequest) -> Result<()> {
    if request.manga_id.is_some()
        || request.chapter_id.is_some()
        || request.chapter.is_some()
        || request.latest
        || request.translated_language.is_some()
    {
        bail!("MangaDex identifiers cannot be combined with source=direct");
    }
    if request.url.is_none() {
        bail!("source=direct requires an explicit url");
    }
    validate_job_name(request.job_name.as_deref())?;
    Ok(())
}

fn validate_job_name(job_name: Option<&str>) -> Result<()> {
    let Some(job_name) = job_name else {
        return Ok(());
    };
    let trimmed = job_name.trim();
    if trimmed.is_empty() || trimmed.len() > 96 {
        bail!("job_name must contain between 1 and 96 bytes");
    }
    if trimmed
        .chars()
        .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        bail!("job_name must be a label without path separators or control characters");
    }
    Ok(())
}

fn normalize_source(source: &str, has_url: bool) -> Result<&'static str> {
    match source {
        "auto" if has_url => Ok("direct"),
        "auto" => Ok("mangadex"),
        "mangadex" => Ok("mangadex"),
        "direct" => Ok("direct"),
        _ => bail!("source must be auto, mangadex, or direct"),
    }
}

fn validate_uuid(raw: &str, label: &str) -> Result<String> {
    let value = uuid::Uuid::parse_str(raw).with_context(|| format!("{label} must be a UUID"))?;
    Ok(value.hyphenated().to_string())
}

fn validate_language(language: &str) -> Result<()> {
    if language.len() < 2
        || language.len() > 16
        || language
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '-'))
    {
        bail!("translated_language must be an exact language code");
    }
    Ok(())
}

fn validate_download_base(url: &Url, allow_insecure_local: bool) -> Result<()> {
    if url.scheme() != "https"
        && !(allow_insecure_local
            && url.scheme() == "http"
            && url.host_str().is_some_and(is_loopback_host))
    {
        bail!("download URL must use HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("download URL cannot contain credentials");
    }
    Ok(())
}

fn create_provider_staging(config: &Config) -> Result<TempDir> {
    config.ensure_runtime_dirs()?;
    TempDirBuilder::new()
        .prefix(".fukidashi-mangadex-")
        .tempdir_in(config.temp_dir())
        .context("create MangaDex ingress staging in configured temp")
}

fn reserve_download_bytes(counter: &AtomicU64, amount: usize, limit: u64) -> Result<()> {
    let amount = u64::try_from(amount).context("download byte count exceeds u64")?;
    loop {
        let current = counter.load(AtomicOrdering::Relaxed);
        let next = current
            .checked_add(amount)
            .ok_or_else(|| anyhow!("MangaDex chapter download byte count overflowed"))?;
        if next > limit {
            bail!(
                "MangaDex chapter downloads exceed the configured total byte limit ({limit} bytes)"
            );
        }
        if counter
            .compare_exchange_weak(
                current,
                next,
                AtomicOrdering::Relaxed,
                AtomicOrdering::Relaxed,
            )
            .is_ok()
        {
            return Ok(());
        }
    }
}

fn validate_direct_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).context("parse direct ingress URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("direct ingress URL must be an absolute http or https URL");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("direct ingress URL cannot contain credentials");
    }
    Ok(url)
}

fn direct_job_label(url: &Url) -> String {
    let host = url.host_str().unwrap_or("direct");
    let path_label = url
        .path_segments()
        .into_iter()
        .flatten()
        .rfind(|segment| !segment.is_empty())
        .unwrap_or("gallery");
    format!("{host}-{path_label}")
}

fn validate_page_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 240
        || name.chars().any(|ch| {
            ch.is_control() || matches!(ch, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
    {
        bail!("remote page name is not a safe single filename: {name:?}");
    }
    Ok(())
}

#[cfg(test)]
fn decode_page(bytes: &[u8]) -> Result<RgbImage> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        bail!("image file exceeds the per-page byte limit");
    }
    let format = image::guess_format(bytes).context("identify ingress image format")?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        bail!("ingress image format must be PNG, JPEG, or WebP");
    }
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let (width, height) = reader.into_dimensions()?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        bail!("ingress image dimensions exceed configured limits");
    }
    Ok(image::load_from_memory_with_format(bytes, format)?.to_rgb8())
}

async fn read_response_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("HTTP response exceeds the configured byte limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            bail!("HTTP response exceeds the configured byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn retry_delay(attempt: usize, retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or_else(|| Duration::from_millis(200_u64.saturating_mul(1_u64 << attempt.min(3))))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "[::1]"
        || host == "::1"
}

fn natural_cmp(left: &str, right: &str) -> Ordering {
    let left = left.to_ascii_lowercase();
    let right = right.to_ascii_lowercase();
    let (mut li, mut ri) = (0, 0);
    let lb = left.as_bytes();
    let rb = right.as_bytes();
    while li < lb.len() && ri < rb.len() {
        if lb[li].is_ascii_digit() && rb[ri].is_ascii_digit() {
            let ls = li;
            let rs = ri;
            while li < lb.len() && lb[li].is_ascii_digit() {
                li += 1;
            }
            while ri < rb.len() && rb[ri].is_ascii_digit() {
                ri += 1;
            }
            let ltrim = &left[ls..li].trim_start_matches('0');
            let rtrim = &right[rs..ri].trim_start_matches('0');
            let ltrim = if ltrim.is_empty() { "0" } else { ltrim };
            let rtrim = if rtrim.is_empty() { "0" } else { rtrim };
            if ltrim.len() != rtrim.len() {
                return ltrim.len().cmp(&rtrim.len());
            }
            if ltrim != rtrim {
                return ltrim.cmp(rtrim);
            }
        } else if lb[li] != rb[ri] {
            return lb[li].cmp(&rb[ri]);
        } else {
            li += 1;
            ri += 1;
        }
    }
    left.len().cmp(&right.len())
}

fn imported_response(provider: &str, registration: Registration, source: Value) -> Result<Value> {
    let job_id = registration
        .job_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("published job has no UTF-8 id"))?;
    Ok(json!({
        "provider": provider,
        "job_id": job_id,
        "job_path": registration.job_dir,
        "page_count": registration.expected_pages.len(),
        "source": source,
        "next_step": {
            "tool": "fukidashi_translation_start",
            "arguments": {"job_id": job_id},
        },
    }))
}

fn external_release_response(chapter: &ResolvedChapter, manga_title: Option<&str>) -> Value {
    let mut response = chapter.summary.as_object().cloned().unwrap_or_default();
    response.insert("provider".to_owned(), json!("mangadex"));
    response.insert("status".to_owned(), json!("external_release"));
    response.insert("imported".to_owned(), json!(false));
    response.insert(
        "manga_title".to_owned(),
        manga_title
            .filter(|title| !title.is_empty())
            .map_or(Value::Null, |title| json!(title)),
    );
    let mut next_step = json!({
        "tool": "fukidashi_pull_chapter",
        "arguments": {
            "source": "direct",
        },
        "description": "This selected MangaDex release has no native hosted pages. Use source=direct only with an explicit supported http(s) URL you already have or obtain; this external URL is a candidate only. Do not substitute an older chapter."
    });
    if let Some(candidate) = chapter
        .external_url
        .as_deref()
        .and_then(validated_external_candidate)
    {
        next_step["candidate_url"] = json!(candidate);
    }
    response.insert("next_step".to_owned(), next_step);
    Value::Object(response)
}

fn validated_external_candidate(raw: &str) -> Option<String> {
    validate_direct_url(raw).ok().map(|url| url.to_string())
}

fn unavailable_release_response(
    chapter: &ResolvedChapter,
    manga_title: Option<&str>,
    reason: String,
) -> Value {
    let mut response = chapter.summary.as_object().cloned().unwrap_or_default();
    response.insert("provider".to_owned(), json!("mangadex"));
    response.insert("status".to_owned(), json!("unavailable"));
    response.insert("imported".to_owned(), json!(false));
    response.insert(
        "manga_title".to_owned(),
        manga_title
            .filter(|title| !title.is_empty())
            .map_or(Value::Null, |title| json!(title)),
    );
    response.insert("reason".to_owned(), json!(reason));
    response.insert(
        "next_step".to_owned(),
        json!({
            "description": "The selected release has no usable MangaDex page list. Report this exact release or provide an explicit direct URL; never substitute an older chapter."
        }),
    );
    Value::Object(response)
}

pub fn gallery_dl_args(staging: &Path, url: &Url) -> Vec<OsString> {
    vec![
        OsString::from("--config-ignore"),
        OsString::from("--no-input"),
        OsString::from("--windows-filenames"),
        OsString::from("--directory"),
        staging.as_os_str().to_owned(),
        OsString::from(url.as_str()),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StagingUsage {
    bytes: u64,
    files: usize,
}

fn staging_usage_exceeded(
    usage: StagingUsage,
    max_bytes: u64,
    max_files: usize,
) -> Option<&'static str> {
    if usage.bytes > max_bytes {
        Some("gallery-dl staging exceeds the configured byte limit")
    } else if usage.files > max_files {
        Some("gallery-dl staging exceeds the configured file-count limit")
    } else {
        None
    }
}

fn scan_staging_usage(root: &Path) -> Result<StagingUsage> {
    reject_link_or_reparse(root)
        .with_context(|| format!("inspect gallery-dl staging root {}", root.display()))?;
    if !std::fs::metadata(root)?.is_dir() {
        bail!(
            "gallery-dl staging root is not a directory: {}",
            root.display()
        );
    }
    let mut usage = StagingUsage { bytes: 0, files: 0 };
    scan_staging_usage_inner(root, &mut usage)?;
    Ok(usage)
}

fn scan_staging_usage_inner(directory: &Path, usage: &mut StagingUsage) -> Result<()> {
    reject_link_or_reparse(directory)?;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            bail!(
                "gallery-dl staging contains a symlink or reparse point: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            scan_staging_usage_inner(&path, usage)?;
        } else if metadata.is_file() {
            usage.files = usage
                .files
                .checked_add(1)
                .ok_or_else(|| anyhow!("gallery-dl staging file count overflowed"))?;
            usage.bytes = usage
                .bytes
                .checked_add(metadata.len())
                .ok_or_else(|| anyhow!("gallery-dl staging byte count overflowed"))?;
        } else {
            bail!(
                "gallery-dl staging contains a non-regular file: {}",
                path.display()
            );
        }
    }
    Ok(())
}

async fn run_gallery_dl(helper: &Path, staging: &Path, url: &Url) -> Result<()> {
    let mut command = Command::new(helper);
    command
        .args(gallery_dl_args(staging, url))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("start gallery-dl helper {}", helper.display()))?;
    let stderr = child.stderr.take();
    let mut stderr_task = Some(tokio::spawn(read_child_stderr(stderr)));
    let deadline = Instant::now() + DIRECT_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().context("poll gallery-dl")? {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_gallery_child(&mut child, &mut stderr_task).await;
            bail!(
                "gallery-dl timed out after {} seconds",
                DIRECT_TIMEOUT.as_secs()
            );
        }
        sleep(OUTPUT_SCAN_INTERVAL).await;
        let scan_root = staging.to_owned();
        let usage = tokio::task::spawn_blocking(move || scan_staging_usage(&scan_root))
            .await
            .map_err(|error| anyhow!("gallery-dl output watcher failed: {error}"))?;
        let usage = match usage {
            Ok(usage) => usage,
            Err(error) => {
                terminate_gallery_child(&mut child, &mut stderr_task).await;
                return Err(error.context("inspect gallery-dl output while it was running"));
            }
        };
        if let Some(reason) =
            staging_usage_exceeded(usage, MAX_DIRECT_OUTPUT_BYTES, MAX_DIRECT_OUTPUT_FILES)
        {
            terminate_gallery_child(&mut child, &mut stderr_task).await;
            bail!("{reason}");
        }
    };
    let stderr = stderr_task
        .take()
        .expect("gallery-dl stderr task is present until process exit")
        .await
        .map_err(|error| anyhow!("gallery-dl stderr reader failed: {error}"))?;
    if !status.success() {
        bail!(
            "gallery-dl exited with {status}: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(())
}

async fn terminate_gallery_child(
    child: &mut tokio::process::Child,
    stderr_task: &mut Option<tokio::task::JoinHandle<Vec<u8>>>,
) {
    let _ = child.kill().await;
    let _ = child.wait().await;
    if let Some(task) = stderr_task.take() {
        let _ = task.await;
    }
}

async fn read_child_stderr(stderr: Option<tokio::process::ChildStderr>) -> Vec<u8> {
    let Some(mut stderr) = stderr else {
        return Vec::new();
    };
    let mut output = Vec::with_capacity(MAX_STDERR_BYTES.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    while let Ok(read) = stderr.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        let remaining = MAX_STDERR_BYTES.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    output
}

fn create_owned_staging(path: &Path, parent: &Path) -> Result<()> {
    if !path.starts_with(parent) || path == parent {
        bail!("ingress staging directory escaped its configured temp root");
    }
    reject_link_or_reparse(parent)
        .with_context(|| format!("inspect ingress temp root {}", parent.display()))?;
    if path.exists() {
        bail!("ingress staging path already exists");
    }
    std::fs::create_dir(path)?;
    reject_link_or_reparse(path)?;
    Ok(())
}

fn collect_ingress_files(root: &Path) -> Result<Vec<PathBuf>> {
    let root = std::fs::canonicalize(root)
        .with_context(|| format!("resolve gallery-dl staging root {}", root.display()))?;
    let mut files = Vec::new();
    collect_ingress_files_inner(&root, &root, &mut files)?;
    files.sort_by(|left, right| natural_cmp(&left.to_string_lossy(), &right.to_string_lossy()));
    if files.len() > MAX_PAGES {
        bail!("gallery-dl output exceeds {MAX_PAGES} pages");
    }
    Ok(files)
}

fn collect_ingress_files_inner(
    root: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    reject_link_or_reparse(directory)?;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            bail!(
                "gallery-dl output contains a symlink or reparse point: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            collect_ingress_files_inner(root, &path, files)?;
        } else if metadata.is_file() {
            let canonical = std::fs::canonicalize(&path)?;
            if !canonical.starts_with(root) {
                bail!(
                    "gallery-dl output escaped its staging directory: {}",
                    path.display()
                );
            }
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if !matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp"
            ) {
                bail!("gallery-dl produced a non-image file: {}", path.display());
            }
            files.push(path);
        } else {
            bail!(
                "gallery-dl output contains a non-regular file: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn reject_link_or_reparse(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        bail!(
            "ingress path contains a symlink or reparse point: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::Arc,
        thread::JoinHandle,
    };

    fn mock_server<F>(requests: usize, handler: F) -> (String, JoinHandle<()>)
    where
        F: Fn(&str, &str) -> (u16, Vec<u8>) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        let thread = std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let path = read_request_path(&mut stream);
                let base = format!("http://{address}/api/");
                let (status, body) = handler(&path, &base);
                let reason = if status == 200 { "OK" } else { "ERROR" };
                let header = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        (format!("http://{address}/api/"), thread)
    }

    fn read_request_path(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if bytes.len() > 16 * 1024 {
                break;
            }
        }
        String::from_utf8_lossy(&bytes)
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_owned()
    }

    fn chapter_json(id: &str, manga_id: &str, number: &str) -> Value {
        json!({
            "id": id,
            "type": "chapter",
            "attributes": {
                "chapter": number,
                "title": format!("Chapter {number}"),
                "translatedLanguage": "en"
            },
            "relationships": [{"type": "manga", "id": manga_id}]
        })
    }

    fn png_bytes(color: [u8; 3]) -> Vec<u8> {
        let image = RgbImage::from_pixel(3, 4, image::Rgb(color));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn test_config(root: &Path) -> Config {
        Config {
            storage_root: root.to_owned(),
            models_dir: root.join("models"),
            ort_dylib: None,
            config_file: root.join("config.toml"),
            configured_jobs_dir: None,
            configured_cache_dir: None,
            configured_temp_dir: Some(root.join("configured-temp")),
            configured_runtime_dir: None,
            configured_exports_dir: None,
            configured_font_dirs: Vec::new(),
            configured_provider: None,
            storage_source: "test".to_owned(),
            models_source: "test".to_owned(),
            ort_source: "test".to_owned(),
        }
    }

    #[test]
    fn source_and_url_validation_are_explicit() {
        assert!(validate_direct_url("file:///tmp/manga").is_err());
        assert!(validate_direct_url("https://example.test/gallery?a=1&b=2").is_ok());
        assert!(normalize_source("auto", true).unwrap() == "direct");
        assert!(normalize_source("auto", false).unwrap() == "mangadex");
    }

    #[test]
    fn data_mode_defaults_to_full_and_accepts_legacy_boolean_aliases() {
        let base = serde_json::json!({
            "source": "mangadex",
            "manga_id": "11111111-1111-1111-1111-111111111111"
        });
        let full: PullChapterRequest = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(full.data_saver, "full");
        let mut legacy = base;
        legacy["data_saver"] = Value::Bool(true);
        let data_saver: PullChapterRequest = serde_json::from_value(legacy).unwrap();
        assert_eq!(data_saver.data_saver, "data_saver");
        assert!(
            serde_json::from_value::<PullChapterRequest>(serde_json::json!({
                "source": "mangadex",
                "manga_id": "11111111-1111-1111-1111-111111111111",
                "data_saver": "small"
            }))
            .is_err()
        );
    }

    #[test]
    fn latest_defaults_to_false_and_conflicts_with_exact_selectors() {
        let base = serde_json::json!({
            "source": "mangadex",
            "manga_id": "11111111-1111-1111-1111-111111111111"
        });
        let request: PullChapterRequest = serde_json::from_value(base).unwrap();
        assert!(!request.latest);

        let mut latest = serde_json::json!({
            "source": "mangadex",
            "manga_id": "11111111-1111-1111-1111-111111111111",
            "latest": true,
            "chapter": "1192"
        });
        let request: PullChapterRequest = serde_json::from_value(latest.clone()).unwrap();
        assert!(validate_mangadex_request(&request).is_err());
        latest["chapter"] = Value::Null;
        latest["manga_id"] = Value::Null;
        let request: PullChapterRequest = serde_json::from_value(latest).unwrap();
        assert!(validate_mangadex_request(&request).is_err());
    }

    #[tokio::test]
    async fn latest_does_not_exhaust_a_full_first_page_when_boundary_is_lower() {
        let manga_id = "23232323-2323-2323-2323-232323232323";
        let latest_id = "24242424-2424-2424-2424-242424242424";
        let older_id = "25252525-2525-2525-2525-252525252525";
        let latest = chapter_json(latest_id, manga_id, "1192");
        let older = chapter_json(older_id, manga_id, "1191");
        let mut entries = vec![latest];
        entries.extend(std::iter::repeat_with(|| older.clone()).take(99));
        let body = serde_json::to_vec(&json!({"data": entries})).unwrap();
        let (base, thread) = mock_server(1, move |path, _base| {
            assert!(path.starts_with("/api/manga/23232323-2323-2323-2323-232323232323/feed?"));
            assert!(
                path.contains("order%5Bchapter%5D=desc") || path.contains("order[chapter]=desc")
            );
            assert!(path.contains("limit=100"));
            assert!(path.contains("offset=0"));
            (200, body.clone())
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let resolved = client
            .resolve_chapter(&PullChapterRequest {
                source: "mangadex".into(),
                manga_id: Some(manga_id.into()),
                chapter_id: None,
                chapter: None,
                latest: true,
                translated_language: None,
                data_saver: "full".into(),
                url: None,
                job_name: None,
            })
            .await
            .unwrap();
        thread.join().unwrap();
        assert_eq!(resolved["chapter"], "1192");
        assert_eq!(resolved["chapter_id"], latest_id);
    }

    #[test]
    fn external_url_is_only_a_direct_candidate_when_safe() {
        assert_eq!(
            validated_external_candidate("https://example.test/chapter/1192"),
            Some("https://example.test/chapter/1192".to_owned())
        );
        assert!(validated_external_candidate("http://user:pass@example.test/chapter").is_none());
        assert!(validated_external_candidate("file:///chapter/1192").is_none());
    }

    #[test]
    fn gallery_arguments_keep_url_as_one_argument() {
        let url = validate_direct_url("https://example.test/gallery?a=1&b=2").unwrap();
        let args = gallery_dl_args(Path::new("C:/owned/stage"), &url);
        assert_eq!(args.len(), 6);
        assert_eq!(
            args[5],
            OsString::from("https://example.test/gallery?a=1&b=2")
        );
        assert_eq!(args[0], OsString::from("--config-ignore"));
        assert!(args.contains(&OsString::from("--no-input")));
        assert!(args.contains(&OsString::from("--windows-filenames")));
    }

    #[test]
    fn native_download_budget_rejects_low_limit_without_large_allocation() {
        let used = AtomicU64::new(0);
        reserve_download_bytes(&used, 7, 10).unwrap();
        let error = reserve_download_bytes(&used, 4, 10).unwrap_err();
        assert!(error.to_string().contains("total byte limit"));
        assert_eq!(used.load(AtomicOrdering::Relaxed), 7);
    }

    #[test]
    fn provider_staging_is_created_under_configured_temp() {
        let directory = tempfile::tempdir().unwrap();
        let config = test_config(directory.path());
        let staging = create_provider_staging(&config).unwrap();
        assert!(staging.path().starts_with(config.temp_dir()));
        assert!(
            staging
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".fukidashi-mangadex-")
        );
    }

    #[test]
    fn staging_usage_counts_partial_files_and_enforces_watcher_decision() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("stage");
        create_owned_staging(&staging, directory.path()).unwrap();
        std::fs::write(staging.join("page-001.png.part"), [1_u8, 2, 3]).unwrap();
        std::fs::create_dir(staging.join("nested")).unwrap();
        std::fs::write(staging.join("nested/page-002.webp"), [4_u8, 5]).unwrap();
        let usage = scan_staging_usage(&staging).unwrap();
        assert_eq!(usage, StagingUsage { bytes: 5, files: 2 });
        assert_eq!(
            staging_usage_exceeded(usage, 4, 2),
            Some("gallery-dl staging exceeds the configured byte limit")
        );
        assert_eq!(
            staging_usage_exceeded(StagingUsage { bytes: 5, files: 3 }, 100, 2),
            Some("gallery-dl staging exceeds the configured file-count limit")
        );
    }

    #[test]
    fn native_urls_require_https_and_reject_credentials() {
        assert!(
            validate_download_base(&Url::parse("http://example.test/image.png").unwrap(), false)
                .is_err()
        );
        assert!(
            validate_download_base(
                &Url::parse("https://user:pass@example.test/image.png").unwrap(),
                false
            )
            .is_err()
        );
        assert!(
            validate_download_base(
                &Url::parse("http://127.0.0.1:1234/image.png").unwrap(),
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn natural_sort_orders_page_numbers() {
        let mut names = vec!["page10.jpg", "page2.jpg", "page1.jpg"];
        names.sort_by(|left, right| natural_cmp(left, right));
        assert_eq!(names, ["page1.jpg", "page2.jpg", "page10.jpg"]);
    }

    #[test]
    fn decode_rejects_unsupported_format() {
        assert!(decode_page(b"plain text").is_err());
    }

    #[test]
    fn staged_output_rejects_traversal_and_non_image_files() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("stage");
        create_owned_staging(&staging, directory.path()).unwrap();
        std::fs::write(staging.join("metadata.json"), b"{}").unwrap();
        let error = collect_ingress_files(&staging).unwrap_err();
        assert!(error.to_string().contains("non-image"));
    }

    #[cfg(unix)]
    #[test]
    fn staged_output_rejects_symlinked_files() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("stage");
        create_owned_staging(&staging, directory.path()).unwrap();
        let outside = directory.path().join("outside.png");
        std::fs::write(&outside, png_bytes([1, 2, 3])).unwrap();
        std::os::unix::fs::symlink(&outside, staging.join("page.png")).unwrap();
        let error = collect_ingress_files(&staging).unwrap_err();
        assert!(error.to_string().contains("symlink"));
    }

    #[tokio::test]
    async fn search_parses_native_mangadex_metadata() {
        let manga_id = "11111111-1111-1111-1111-111111111111";
        let body = serde_json::to_vec(&json!({
            "data": [{
                "id": manga_id,
                "attributes": {
                    "title": {"ja": "原題", "en": "English title"},
                    "altTitles": [{"vi": "Tên Việt"}, {"ja-ro": "Gendai"}],
                    "originalLanguage": "ja",
                    "status": "ongoing"
                }
            }]
        }))
        .unwrap();
        let (base, thread) = mock_server(1, move |path, _base| {
            assert!(path.starts_with("/api/manga?"));
            (200, body.clone())
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let response = client
            .search(&SearchMangaRequest {
                source: "auto".into(),
                query: "title".into(),
                limit: 2,
            })
            .await
            .unwrap();
        thread.join().unwrap();
        assert_eq!(response["provider"], "mangadex");
        assert_eq!(response["results"][0]["manga_id"], manga_id);
        assert_eq!(response["results"][0]["title"], "English title");
        assert_eq!(response["results"][0]["original_language"], "ja");
        assert_eq!(
            response["results"][0]["alternate_titles"][0]["language"],
            "vi"
        );
        assert_eq!(
            response["results"][0]["suggested_next_step"]["arguments"]["latest"],
            true
        );
    }

    #[tokio::test]
    async fn chapter_resolution_reports_ambiguity_instead_of_guessing() {
        let manga_id = "22222222-2222-2222-2222-222222222222";
        let first = chapter_json("33333333-3333-3333-3333-333333333333", manga_id, "1");
        let second = chapter_json("44444444-4444-4444-4444-444444444444", manga_id, "2");
        let body = serde_json::to_vec(&json!({"data": [first, second]})).unwrap();
        let (base, thread) = mock_server(1, move |path, _base| {
            assert!(path.starts_with("/api/manga/22222222-2222-2222-2222-222222222222/feed?"));
            (200, body.clone())
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let error = client
            .resolve_chapter(&PullChapterRequest {
                source: "mangadex".into(),
                manga_id: Some(manga_id.into()),
                chapter_id: None,
                chapter: None,
                latest: false,
                translated_language: Some("en".into()),
                data_saver: "full".into(),
                url: None,
                job_name: None,
            })
            .await
            .unwrap_err();
        thread.join().unwrap();
        assert!(error.to_string().contains("ambiguous"));
        assert!(
            error
                .to_string()
                .contains("33333333-3333-3333-3333-333333333333")
        );
    }

    #[tokio::test]
    async fn exact_chapter_id_validates_language_without_querying_the_endpoint() {
        let manga_id = "99999999-9999-9999-9999-999999999999";
        let chapter_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let mut chapter = chapter_json(chapter_id, manga_id, "7");
        chapter["attributes"]["translatedLanguage"] = Value::String("ja".to_owned());
        let body = serde_json::to_vec(&json!({"data": chapter})).unwrap();
        let (base, thread) = mock_server(1, move |path, _base| {
            assert_eq!(path, "/api/chapter/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
            (200, body.clone())
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let error = client
            .resolve_chapter(&PullChapterRequest {
                source: "mangadex".into(),
                manga_id: Some(manga_id.into()),
                chapter_id: Some(chapter_id.into()),
                chapter: None,
                latest: false,
                translated_language: Some("en".into()),
                data_saver: "full".into(),
                url: None,
                job_name: None,
            })
            .await
            .unwrap_err();
        thread.join().unwrap();
        let message = error.to_string();
        assert!(message.contains("translated_language mismatch"));
        assert!(message.contains("requested \"en\""));
        assert!(message.contains("\"ja\""));
    }

    #[tokio::test]
    async fn latest_selects_external_newer_chapter_without_falling_back_or_importing() {
        let manga_id = "12121212-1212-1212-1212-121212121212";
        let latest_id = "13131313-1313-1313-1313-131313131313";
        let older_id = "14141414-1414-1414-1414-141414141414";
        let mut latest = chapter_json(latest_id, manga_id, "1192");
        latest["attributes"]["translatedLanguage"] = Value::String("ja".to_owned());
        latest["attributes"]["externalUrl"] =
            Value::String("https://example.test/releases/1192".to_owned());
        latest["attributes"]["publishAt"] = Value::String("2026-09-08T00:00:00+00:00".to_owned());
        let older = chapter_json(older_id, manga_id, "1191");
        let body = serde_json::to_vec(&json!({"data": [latest, older]})).unwrap();
        let (base, thread) = mock_server(2, move |path, _base| {
            if path.contains("order%5Bchapter%5D=desc") || path.contains("order[chapter]=desc") {
                assert!(path.contains("offset=0"));
                return (200, body.clone());
            }
            if path == "/api/manga/12121212-1212-1212-1212-121212121212" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "data": {"id": manga_id, "attributes": {"title": {"en": "Latest External"}}}
                    }))
                    .unwrap(),
                );
            }
            panic!("unexpected MangaDex request path {path}");
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(directory.path().join("jobs")).unwrap();
        let response = client
            .pull_mangadex(
                &test_config(directory.path()),
                &PullChapterRequest {
                    source: "mangadex".into(),
                    manga_id: Some(manga_id.into()),
                    chapter_id: None,
                    chapter: None,
                    latest: true,
                    translated_language: None,
                    data_saver: "full".into(),
                    url: None,
                    job_name: None,
                },
                &workflow,
            )
            .await
            .unwrap();
        thread.join().unwrap();
        assert_eq!(response["status"], "external_release");
        assert_eq!(response["imported"], false);
        assert_eq!(response["chapter_id"], latest_id);
        assert_eq!(response["chapter"], "1192");
        assert_eq!(response["translated_language"], "ja");
        assert_eq!(
            response["external_url"],
            "https://example.test/releases/1192"
        );
        assert_eq!(response["next_step"]["arguments"]["source"], "direct");
        assert!(response["next_step"]["arguments"].get("url").is_none());
        assert_eq!(
            response["next_step"]["candidate_url"],
            response["external_url"]
        );
        assert_eq!(response["manga_title"], "Latest External");
        assert_eq!(std::fs::read_dir(workflow.root()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn latest_hosted_chapter_imports_the_selected_release() {
        let manga_id = "15151515-1515-1515-1515-151515151515";
        let latest_id = "16161616-1616-1616-1616-161616161616";
        let older_id = "17171717-1717-1717-1717-171717171717";
        let latest = chapter_json(latest_id, manga_id, "1192");
        let older = chapter_json(older_id, manga_id, "1191");
        let feed = serde_json::to_vec(&json!({"data": [latest, older]})).unwrap();
        let image = png_bytes([15, 25, 35]);
        let (base, thread) = mock_server(4, move |path, request_base| {
            if path.starts_with("/api/manga/15151515-1515-1515-1515-151515151515/feed?") {
                assert!(
                    path.contains("order%5Bchapter%5D=desc")
                        || path.contains("order[chapter]=desc")
                );
                return (200, feed.clone());
            }
            if path == "/api/manga/15151515-1515-1515-1515-151515151515" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "data": {"id": manga_id, "attributes": {"title": {"en": "Latest Hosted"}}}
                    }))
                    .unwrap(),
                );
            }
            if path == "/api/at-home/server/16161616-1616-1616-1616-161616161616" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "baseUrl": format!("{request_base}images/"),
                        "chapter": {"hash": "latest-hash"},
                        "data": ["page1.png"]
                    }))
                    .unwrap(),
                );
            }
            if path == "/api/images/data/latest-hash/page1.png" {
                return (200, image.clone());
            }
            panic!("unexpected MangaDex request path {path}");
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(directory.path().join("jobs")).unwrap();
        let response = client
            .pull_mangadex(
                &test_config(directory.path()),
                &PullChapterRequest {
                    source: "mangadex".into(),
                    manga_id: Some(manga_id.into()),
                    chapter_id: None,
                    chapter: None,
                    latest: true,
                    translated_language: None,
                    data_saver: "full".into(),
                    url: None,
                    job_name: None,
                },
                &workflow,
            )
            .await
            .unwrap();
        thread.join().unwrap();
        assert_eq!(response["page_count"], 1);
        assert_eq!(response["source"]["chapter_id"], latest_id);
        assert_eq!(response["source"]["chapter"], "1192");
        assert!(
            response["job_id"]
                .as_str()
                .unwrap()
                .starts_with("Latest_Hosted-ch-1192--")
        );
    }

    #[tokio::test]
    async fn exact_external_chapter_returns_structured_non_import() {
        let manga_id = "18181818-1818-1818-1818-181818181818";
        let chapter_id = "19191919-1919-1919-1919-191919191919";
        let mut chapter = chapter_json(chapter_id, manga_id, "1192");
        chapter["attributes"]["externalUrl"] =
            Value::String("https://example.test/one-piece/1192".to_owned());
        let body = serde_json::to_vec(&json!({"data": chapter})).unwrap();
        let (base, thread) = mock_server(2, move |path, _base| {
            if path == "/api/chapter/19191919-1919-1919-1919-191919191919" {
                return (200, body.clone());
            }
            if path == "/api/manga/18181818-1818-1818-1818-181818181818" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "data": {"id": manga_id, "attributes": {"title": {"en": "Exact External"}}}
                    }))
                    .unwrap(),
                );
            }
            panic!("unexpected MangaDex request path {path}");
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(directory.path().join("jobs")).unwrap();
        let response = client
            .pull_mangadex(
                &test_config(directory.path()),
                &PullChapterRequest {
                    source: "mangadex".into(),
                    manga_id: Some(manga_id.into()),
                    chapter_id: Some(chapter_id.into()),
                    chapter: None,
                    latest: false,
                    translated_language: None,
                    data_saver: "full".into(),
                    url: None,
                    job_name: None,
                },
                &workflow,
            )
            .await
            .unwrap();
        thread.join().unwrap();
        assert_eq!(response["status"], "external_release");
        assert_eq!(response["imported"], false);
        assert_eq!(response["chapter_id"], chapter_id);
        assert_eq!(
            response["external_url"],
            "https://example.test/one-piece/1192"
        );
        assert_eq!(response["manga_title"], "Exact External");
        assert!(response["next_step"]["arguments"].get("url").is_none());
        assert_eq!(
            response["next_step"]["candidate_url"],
            response["external_url"]
        );
        assert_eq!(std::fs::read_dir(workflow.root()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn empty_at_home_pages_return_unavailable_without_substituting_an_older_release() {
        let manga_id = "20202020-2020-2020-2020-202020202020";
        let latest_id = "21212121-2121-2121-2121-212121212121";
        let older_id = "22222222-2222-2222-2222-222222222222";
        let latest = chapter_json(latest_id, manga_id, "1192");
        let older = chapter_json(older_id, manga_id, "1191");
        let feed = serde_json::to_vec(&json!({"data": [latest, older]})).unwrap();
        let (base, thread) = mock_server(3, move |path, request_base| {
            if path.starts_with("/api/manga/20202020-2020-2020-2020-202020202020/feed?") {
                return (200, feed.clone());
            }
            if path == "/api/manga/20202020-2020-2020-2020-202020202020" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "data": {"id": manga_id, "attributes": {"title": {"en": "Empty Hosted"}}}
                    }))
                    .unwrap(),
                );
            }
            if path == "/api/at-home/server/21212121-2121-2121-2121-212121212121" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "baseUrl": format!("{request_base}images/"),
                        "chapter": {"hash": "empty-hash"},
                        "data": [],
                        "dataSaver": ["page1.png"]
                    }))
                    .unwrap(),
                );
            }
            panic!("unexpected MangaDex request path {path}");
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(directory.path().join("jobs")).unwrap();
        let response = client
            .pull_mangadex(
                &test_config(directory.path()),
                &PullChapterRequest {
                    source: "mangadex".into(),
                    manga_id: Some(manga_id.into()),
                    chapter_id: None,
                    chapter: None,
                    latest: true,
                    translated_language: None,
                    data_saver: "full".into(),
                    url: None,
                    job_name: None,
                },
                &workflow,
            )
            .await
            .unwrap();
        thread.join().unwrap();
        assert_eq!(response["status"], "unavailable");
        assert_eq!(response["imported"], false);
        assert_eq!(response["chapter_id"], latest_id);
        assert_eq!(response["chapter"], "1192");
        assert!(
            response["reason"]
                .as_str()
                .unwrap()
                .contains("no data pages")
        );
        assert_eq!(std::fs::read_dir(workflow.root()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn at_home_pages_are_naturally_sorted_and_imported_as_pending_job() {
        let manga_id = "55555555-5555-5555-5555-555555555555";
        let chapter_id = "66666666-6666-6666-6666-666666666666";
        let chapter = chapter_json(chapter_id, manga_id, "1");
        let feed = serde_json::to_vec(&json!({"data": [chapter]})).unwrap();
        let image_two = png_bytes([20, 30, 40]);
        let image_ten = png_bytes([50, 60, 70]);
        let (base, thread) = mock_server(5, move |path, base| {
            if path.starts_with("/api/manga/55555555-5555-5555-5555-555555555555/feed?") {
                return (200, feed.clone());
            }
            if path == "/api/manga/55555555-5555-5555-5555-555555555555" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "data": {"id": manga_id, "attributes": {"title": {"en": "One Piece"}}}
                    }))
                    .unwrap(),
                );
            }
            if path == "/api/at-home/server/66666666-6666-6666-6666-666666666666" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "baseUrl": format!("{base}images/"),
                        "chapter": {"hash": "hash"},
                        "data": ["page10.png", "page2.png"]
                    }))
                    .unwrap(),
                );
            }
            if path == "/api/images/data/hash/page2.png" {
                return (200, image_two.clone());
            }
            if path == "/api/images/data/hash/page10.png" {
                return (200, image_ten.clone());
            }
            (404, b"missing".to_vec())
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(directory.path().join("jobs")).unwrap();
        let config = test_config(directory.path());
        let response = client
            .pull_mangadex(
                &config,
                &PullChapterRequest {
                    source: "mangadex".into(),
                    manga_id: Some(manga_id.into()),
                    chapter_id: None,
                    chapter: Some("1".into()),
                    latest: false,
                    translated_language: Some("en".into()),
                    data_saver: "full".into(),
                    url: None,
                    job_name: None,
                },
                &workflow,
            )
            .await
            .unwrap();
        thread.join().unwrap();
        let job = PathBuf::from(response["job_path"].as_str().unwrap());
        let pending = workflow.next_pending_page(&job).unwrap().unwrap();
        assert_eq!(pending.page_number, 1);
        assert_eq!(pending.total_pages, 2);
        assert_eq!(response["next_step"]["tool"], "fukidashi_translation_start");
        let provenance: Value =
            serde_json::from_slice(&std::fs::read(job.join("ingress.json")).unwrap()).unwrap();
        assert_eq!(provenance["pages"][0]["original_name"], "page2.png");
        assert_eq!(provenance["pages"][1]["original_name"], "page10.png");
        assert!(job.join("source/0001.png").is_file());
        assert!(
            job.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("One_Piece-ch-1--")
        );
    }

    #[tokio::test]
    async fn at_home_data_saver_uses_official_data_saver_route_and_order() {
        let manga_id = "77777777-7777-7777-7777-777777777777";
        let chapter_id = "88888888-8888-8888-8888-888888888888";
        let chapter = chapter_json(chapter_id, manga_id, "2");
        let feed = serde_json::to_vec(&json!({"data": [chapter]})).unwrap();
        let image_one = png_bytes([80, 90, 100]);
        let image_three = png_bytes([110, 120, 130]);
        let (base, thread) = mock_server(5, move |path, base| {
            if path.starts_with("/api/manga/77777777-7777-7777-7777-777777777777/feed?") {
                return (200, feed.clone());
            }
            if path == "/api/manga/77777777-7777-7777-7777-777777777777" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "data": {"id": manga_id, "attributes": {"title": {"en": "Chainsaw Title"}}}
                    }))
                    .unwrap(),
                );
            }
            if path == "/api/at-home/server/88888888-8888-8888-8888-888888888888" {
                return (
                    200,
                    serde_json::to_vec(&json!({
                        "baseUrl": format!("{base}images/"),
                        "chapter": {"hash": "save-hash"},
                        "data": ["page99.png"],
                        "dataSaver": ["page3.png", "page1.png"]
                    }))
                    .unwrap(),
                );
            }
            if path == "/api/images/data-saver/save-hash/page1.png" {
                return (200, image_one.clone());
            }
            if path == "/api/images/data-saver/save-hash/page3.png" {
                return (200, image_three.clone());
            }
            panic!("unexpected MangaDex request path {path}");
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(directory.path().join("jobs")).unwrap();
        let config = test_config(directory.path());
        let response = client
            .pull_mangadex(
                &config,
                &PullChapterRequest {
                    source: "mangadex".into(),
                    manga_id: Some(manga_id.into()),
                    chapter_id: None,
                    chapter: Some("2".into()),
                    latest: false,
                    translated_language: Some("en".into()),
                    data_saver: "data_saver".into(),
                    url: None,
                    job_name: None,
                },
                &workflow,
            )
            .await
            .unwrap();
        thread.join().unwrap();
        let job = PathBuf::from(response["job_path"].as_str().unwrap());
        let provenance: Value =
            serde_json::from_slice(&std::fs::read(job.join("ingress.json")).unwrap()).unwrap();
        assert_eq!(provenance["pages"][0]["original_name"], "page1.png");
        assert_eq!(provenance["pages"][1]["original_name"], "page3.png");
        assert!(
            job.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("Chainsaw_Title-ch-2--")
        );
    }

    #[test]
    fn file_import_publishes_readable_distinct_labels_and_cleans_failed_jobs() {
        let directory = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(directory.path().join("jobs")).unwrap();
        let first_stage = tempfile::tempdir().unwrap();
        let first_page = first_stage.path().join("page-1.png");
        std::fs::write(&first_page, png_bytes([1, 2, 3])).unwrap();
        let first = workflow
            .import_ingress_files(
                "One Piece - Chapter 1",
                first_stage.path(),
                vec![IngressPageFile {
                    original_name: "page-1.png".into(),
                    path: first_page,
                }],
                json!({"provider": "test"}),
            )
            .unwrap();
        let second_stage = tempfile::tempdir().unwrap();
        let second_page = second_stage.path().join("page-1.png");
        std::fs::write(&second_page, png_bytes([4, 5, 6])).unwrap();
        let second = workflow
            .import_ingress_files(
                "Chainsaw Man - Chapter 1",
                second_stage.path(),
                vec![IngressPageFile {
                    original_name: "page-1.png".into(),
                    path: second_page,
                }],
                json!({"provider": "test"}),
            )
            .unwrap();
        let first_name = first.job_dir.file_name().unwrap().to_string_lossy();
        let second_name = second.job_dir.file_name().unwrap().to_string_lossy();
        assert!(
            first_name.starts_with("One_Piece_-_Chapter_1--"),
            "actual first job name: {first_name}"
        );
        assert!(
            second_name.starts_with("Chainsaw_Man_-_Chapter_1--"),
            "actual second job name: {second_name}"
        );
        assert_ne!(first_name, second_name);

        let failed_stage = tempfile::tempdir().unwrap();
        let good_page = failed_stage.path().join("good.png");
        let bad_page = failed_stage.path().join("bad.png");
        std::fs::write(&good_page, png_bytes([7, 8, 9])).unwrap();
        std::fs::write(&bad_page, b"not an image").unwrap();
        assert!(
            workflow
                .import_ingress_files(
                    "Broken Gallery",
                    failed_stage.path(),
                    vec![
                        IngressPageFile {
                            original_name: "good.png".into(),
                            path: good_page,
                        },
                        IngressPageFile {
                            original_name: "bad.png".into(),
                            path: bad_page,
                        },
                    ],
                    json!({"provider": "test"}),
                )
                .is_err()
        );
        let published = std::fs::read_dir(workflow.root())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .collect::<Vec<_>>();
        assert_eq!(published.len(), 2);
        assert!(published.iter().all(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with("Broken_Gallery")
        }));
    }
}

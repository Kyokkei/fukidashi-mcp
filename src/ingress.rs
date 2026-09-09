//! Deterministic manga ingress for the local Fukidashi pipeline.
//!
//! MangaDex is the only native provider in v1. A direct URL is delegated to
//! an already provisioned gallery-dl executable, with its output constrained
//! to an owned staging directory before images are imported into a fresh
//! server-managed job.

use std::{
    cmp::Ordering,
    ffi::OsString,
    io::Cursor,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use image::{ImageFormat, RgbImage};
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::Semaphore,
    time::{sleep, timeout},
};
use uuid::Uuid;

use crate::{
    config::Config,
    workflow::{Registration, Workflow},
};

const MANGADEX_BASE_URL: &str = "https://api.mangadex.org/";
const USER_AGENT: &str = "fukidashi-mcp/0.1 manga-ingress";
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 1024 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 100_000_000;
const MAX_IMAGE_DIMENSION: u32 = 12_000;
const MAX_PAGES: usize = 500;
const MAX_RETRIES: usize = 3;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const DIRECT_TIMEOUT: Duration = Duration::from_secs(120);

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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
            .redirect(Policy::limited(3))
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
        request: &PullChapterRequest,
        workflow: &Workflow,
    ) -> Result<Value> {
        validate_mangadex_request(request)?;
        let chapter = self.resolve_chapter_record(request).await?;
        let use_data_saver = Self::uses_data_saver(request);
        let at_home = self.at_home(&chapter.id).await?;
        let file_names = at_home
            .get(if use_data_saver { "dataSaver" } else { "data" })
            .and_then(Value::as_array)
            .ok_or_else(|| {
                anyhow!(
                    "MangaDex at-home response has no {} page list",
                    if use_data_saver { "dataSaver" } else { "data" }
                )
            })?;
        if file_names.is_empty() || file_names.len() > MAX_PAGES {
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
                segments.push(hash).push(file_name);
            }
            page_urls.push((file_name.to_owned(), url));
        }
        page_urls.sort_by(|left, right| natural_cmp(&left.0, &right.0));
        let downloads = self.download_pages(page_urls).await?;
        let mut total_bytes = 0_usize;
        let mut pages = Vec::with_capacity(downloads.len());
        for (name, bytes) in downloads {
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > MAX_TOTAL_IMAGE_BYTES {
                bail!("MangaDex chapter exceeds the total image byte limit");
            }
            pages.push((name, decode_page(&bytes)?));
        }
        let provenance = json!({
            "provider": "mangadex",
            "manga_id": chapter.manga_id,
            "chapter_id": chapter.id,
            "chapter": chapter.chapter,
            "translated_language": request.translated_language,
            "data_saver": request.data_saver,
            "title": chapter.title,
        });
        let registration =
            workflow.import_ingress_pages(&chapter.slug, pages, provenance.clone())?;
        imported_response("mangadex", registration, provenance)
    }

    async fn resolve_chapter_record(
        &self,
        request: &PullChapterRequest,
    ) -> Result<ResolvedChapter> {
        validate_mangadex_request(request)?;
        if let Some(chapter_id) = request.chapter_id.as_deref() {
            let id = validate_uuid(chapter_id, "chapter_id")?;
            let expected_manga_id = request
                .manga_id
                .as_deref()
                .map(|manga_id| validate_uuid(manga_id, "manga_id"))
                .transpose()?;
            let mut url = self.endpoint(&format!("chapter/{id}"))?;
            if let Some(language) = request.translated_language.as_deref() {
                validate_language(language)?;
                url.query_pairs_mut()
                    .append_pair("translatedLanguage[]", language);
            }
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

    async fn at_home(&self, chapter_id: &str) -> Result<Value> {
        self.get_json(self.endpoint(&format!("at-home/server/{chapter_id}"))?)
            .await
    }

    async fn download_pages(&self, pages: Vec<(String, Url)>) -> Result<Vec<(String, Vec<u8>)>> {
        let semaphore = Arc::new(Semaphore::new(4));
        let mut set = tokio::task::JoinSet::new();
        for (name, url) in pages {
            let permit = semaphore.clone();
            let client = self.clone();
            set.spawn(async move {
                let _permit = permit
                    .acquire_owned()
                    .await
                    .map_err(|_| anyhow!("download concurrency gate closed"))?;
                let bytes = client.get_bytes(url, MAX_IMAGE_BYTES).await?;
                Ok::<_, anyhow::Error>((name, bytes))
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
        result.sort_by(|left, right| natural_cmp(&left.0, &right.0));
        Ok(result)
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
    slug: String,
    summary: Value,
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
    let slug = title
        .as_deref()
        .or(chapter.as_deref())
        .unwrap_or("manga")
        .to_owned();
    Ok(ResolvedChapter {
        id: id.to_owned(),
        manga_id: manga_id.to_owned(),
        chapter: chapter.clone(),
        title: title.clone(),
        slug,
        summary: json!({
            "chapter_id": id,
            "manga_id": manga_id,
            "chapter": chapter,
            "title": title,
            "translated_language": attributes.get("translatedLanguage").cloned().unwrap_or(Value::Null),
        }),
    })
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
                .pull_mangadex(&request, workflow)
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
        let mut total = 0_usize;
        let mut pages = Vec::with_capacity(files.len());
        for file in files {
            let bytes = std::fs::read(&file)
                .with_context(|| format!("read gallery-dl output {}", file.display()))?;
            total = total.saturating_add(bytes.len());
            if total > MAX_TOTAL_IMAGE_BYTES {
                bail!("gallery-dl output exceeds the total image byte limit");
            }
            let name = file
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("gallery-dl output has an invalid filename"))?;
            pages.push((name.to_owned(), decode_page(&bytes)?));
        }
        let provenance = json!({
            "provider": "direct",
            "url": url.as_str(),
            "helper": helper,
        });
        let registration = workflow.import_ingress_pages("direct", pages, provenance.clone())?;
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
    Ok(())
}

fn validate_direct_request(request: &PullChapterRequest) -> Result<()> {
    if request.manga_id.is_some()
        || request.chapter_id.is_some()
        || request.chapter.is_some()
        || request.translated_language.is_some()
    {
        bail!("MangaDex identifiers cannot be combined with source=direct");
    }
    if request.url.is_none() {
        bail!("source=direct requires an explicit url");
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

fn validate_page_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.chars().any(|ch| matches!(ch, '/' | '\\'))
    {
        bail!("remote page name is not a safe single filename: {name:?}");
    }
    Ok(())
}

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

pub fn gallery_dl_args(staging: &Path, url: &Url) -> Vec<OsString> {
    vec![
        OsString::from("--config-ignore"),
        OsString::from("--directory"),
        staging.as_os_str().to_owned(),
        OsString::from(url.as_str()),
    ]
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
    let stderr_task = tokio::spawn(read_child_stderr(stderr));
    let status = match timeout(DIRECT_TIMEOUT, child.wait()).await {
        Ok(status) => status.context("wait for gallery-dl")?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stderr_task.await;
            bail!(
                "gallery-dl timed out after {} seconds",
                DIRECT_TIMEOUT.as_secs()
            );
        }
    };
    let stderr = stderr_task
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
    fn gallery_arguments_keep_url_as_one_argument() {
        let url = validate_direct_url("https://example.test/gallery?a=1&b=2").unwrap();
        let args = gallery_dl_args(Path::new("C:/owned/stage"), &url);
        assert_eq!(args.len(), 4);
        assert_eq!(
            args[3],
            OsString::from("https://example.test/gallery?a=1&b=2")
        );
        assert_eq!(args[0], OsString::from("--config-ignore"));
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
                translated_language: Some("en".into()),
                data_saver: "full".into(),
                url: None,
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
    async fn at_home_pages_are_naturally_sorted_and_imported_as_pending_job() {
        let manga_id = "55555555-5555-5555-5555-555555555555";
        let chapter_id = "66666666-6666-6666-6666-666666666666";
        let chapter = chapter_json(chapter_id, manga_id, "1");
        let feed = serde_json::to_vec(&json!({"data": [chapter]})).unwrap();
        let image_two = png_bytes([20, 30, 40]);
        let image_ten = png_bytes([50, 60, 70]);
        let (base, thread) = mock_server(4, move |path, base| {
            if path.starts_with("/api/manga/55555555-5555-5555-5555-555555555555/feed?") {
                return (200, feed.clone());
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
            if path == "/api/images/hash/page2.png" {
                return (200, image_two.clone());
            }
            if path == "/api/images/hash/page10.png" {
                return (200, image_ten.clone());
            }
            (404, b"missing".to_vec())
        });
        let client = MangaDexClient::with_base_url(&base).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let workflow = Workflow::new(directory.path().join("jobs")).unwrap();
        let response = client
            .pull_mangadex(
                &PullChapterRequest {
                    source: "mangadex".into(),
                    manga_id: Some(manga_id.into()),
                    chapter_id: None,
                    chapter: Some("1".into()),
                    translated_language: Some("en".into()),
                    data_saver: "full".into(),
                    url: None,
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
    }
}

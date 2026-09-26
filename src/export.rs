//! Safe, ordered project export to ZIP, EPUB, or a standalone HTML document.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;
use zip::CompressionMethod;

const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;
// Managed jobs may contain a complete long-form chapter whose rendered pages
// exceed the legacy aggregate input budget. Keep a separate hard ceiling for
// those server-owned roots while retaining the smaller ad-hoc export limit.
const MAX_MANAGED_INPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Project {
    schema_version: u32,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    glossary: Option<serde_json::Value>,
    pages: Vec<Page>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Page {
    id: String,
    image_path: String,
    #[serde(default)]
    rendered_image_path: Option<String>,
    #[serde(default)]
    bubbles: Vec<Bubble>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Bubble {
    id: String,
    text: String,
    #[serde(default)]
    translation: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

struct ResolvedPage {
    id: String,
    extra: serde_json::Map<String, serde_json::Value>,
    ext: String,
    page_name: String,
    bubbles: Vec<Bubble>,
    source: PathBuf,
}

/// Export the manifest-listed pages in their explicit order.
pub fn export_project(project_dir: &Path, format: &str) -> Result<serde_json::Value> {
    export_project_to(project_dir, format, None)
}

/// Export using an optional shared exports directory. The no-argument wrapper
/// keeps the legacy job-local destination for callers that do not have a
/// configured storage namespace.
pub fn export_project_to(
    project_dir: &Path,
    format: &str,
    configured_output_dir: Option<&Path>,
) -> Result<serde_json::Value> {
    let root = fs::canonicalize(project_dir)
        .with_context(|| format!("resolve project root {}", project_dir.display()))?;
    let max_input_bytes = max_input_bytes(&root);
    crate::editor::export_gate(&root)?;
    let manifest_path = root.join("project.json");
    let manifest_bytes = fs::read(&manifest_path).context("read project.json")?;
    if manifest_bytes.len() as u64 > max_input_bytes {
        bail!("project manifest exceeds export limit");
    }
    let project: Project = serde_json::from_slice(&manifest_bytes).context("parse project.json")?;
    if project.schema_version != 1 {
        bail!(
            "unsupported project schema_version {}",
            project.schema_version
        );
    }
    if project.pages.is_empty() {
        bail!("project contains no pages");
    }
    let pages = resolve_pages(&root, project.pages.clone(), max_input_bytes)?;
    let output_dir = configured_output_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("fukidashi-output"));
    fs::create_dir_all(&output_dir).context("create export directory")?;
    let (extension, epub_layout) = match format {
        "zip" => ("zip", false),
        "epub" => ("epub", true),
        "html_monolith" => ("html", false),
        other => bail!("unsupported export format {other:?}; expected zip, epub, or html_monolith"),
    };
    if format == "html_monolith" {
        let data = make_html(&project, &pages)?;
        if data.len() as u64 > MAX_ARCHIVE_BYTES {
            bail!("export exceeds {} byte limit", MAX_ARCHIVE_BYTES);
        }
        let output = output_dir.join(format!(
            "fukidashi-{}.{}",
            Uuid::new_v4().simple(),
            extension
        ));
        let temporary =
            tempfile::NamedTempFile::new_in(&output_dir).context("create atomic export")?;
        fs::write(temporary.path(), &data).context("write export")?;
        temporary
            .persist(&output)
            .map_err(|e| anyhow!("promote export: {}", e.error))?;
        let absolute = fs::canonicalize(&output).unwrap_or(output);
        return Ok(
            serde_json::json!({"format": format, "output_path": absolute, "pages": pages.len(), "bytes": data.len()}),
        );
    }
    // Stream the archive straight into a temp file in the output directory:
    // a large book never sits in RAM as a whole-archive `Vec<u8>`. Page
    // bytes stream one file at a time through the single shared packaging
    // core in `crate::approval`; only small generated descriptors (manifest,
    // OPF/nav XHTML) are held in memory.
    let output = output_dir.join(format!(
        "fukidashi-{}.{}",
        Uuid::new_v4().simple(),
        extension
    ));
    let temporary = tempfile::NamedTempFile::new_in(&output_dir).context("create atomic export")?;
    write_export_archive(temporary.path(), &project, &pages, epub_layout)?;
    let bytes = fs::metadata(temporary.path())
        .context("measure export")?
        .len();
    if bytes > MAX_ARCHIVE_BYTES {
        bail!("export exceeds {} byte limit", MAX_ARCHIVE_BYTES);
    }
    if bytes == 0 {
        bail!("export produced an empty archive");
    }
    temporary
        .persist(&output)
        .map_err(|e| anyhow!("promote export: {}", e.error))?;
    let absolute = fs::canonicalize(&output).unwrap_or(output);
    Ok(
        serde_json::json!({"format": format, "output_path": absolute, "pages": pages.len(), "bytes": bytes}),
    )
}

fn max_input_bytes(root: &Path) -> u64 {
    if root.join("job.json").is_file() || root.join(".fukidashi-job.json").is_file() {
        MAX_MANAGED_INPUT_BYTES
    } else {
        MAX_INPUT_BYTES
    }
}

fn resolve_pages(root: &Path, pages: Vec<Page>, max_input_bytes: u64) -> Result<Vec<ResolvedPage>> {
    let mut total = 0u64;
    pages
        .into_iter()
        .enumerate()
        .map(|(index, page)| {
            if page.id.is_empty() || page.id.len() > 128 {
                bail!("page {index} has an invalid id");
            }
            let image_path = Path::new(&page.image_path);
            let candidate = if image_path.is_absolute() {
                let rendered = page.rendered_image_path.as_deref().ok_or_else(|| {
                    anyhow!(
                        "page {index} has an absolute source path but no managed rendered_image_path"
                    )
                })?;
                let rendered = Path::new(rendered);
                if rendered
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
                {
                    bail!("page {index} rendered_image_path escapes the project root");
                }
                if rendered.is_absolute() {
                    rendered.to_path_buf()
                } else {
                    root.join(rendered)
                }
            } else {
                if image_path.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                }) {
                    bail!("page {index} image_path escapes the project root");
                }
                root.join(image_path)
            };
            let source = fs::canonicalize(&candidate)
                .with_context(|| format!("resolve page {} image", page.id))?;
            if !crate::workflow::path_is_within_public(&source, root)? {
                bail!("page {} image is outside project root", page.id);
            }
            let metadata = fs::symlink_metadata(&candidate).context("inspect page image path")?;
            if metadata.file_type().is_symlink() {
                bail!("page {} image path is a symlink", page.id);
            }
            let size = fs::metadata(&source)?.len();
            total = total
                .checked_add(size)
                .ok_or_else(|| anyhow!("export size overflow"))?;
            if total > max_input_bytes {
                bail!("project assets exceed export input limit");
            }
            let ext = source
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("bin")
                .to_ascii_lowercase();
            let ext = match ext.as_str() {
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg" => ext,
                _ => "bin".to_owned(),
            };
            Ok(ResolvedPage {
                id: page.id,
                extra: page.extra,
                ext,
                page_name: format!("page-{index:04}"),
                bubbles: page.bubbles,
                source,
            })
        })
        .collect()
}

fn export_manifest(project: &Project, pages: &[ResolvedPage]) -> Result<Vec<u8>> {
    let exported_pages = pages
        .iter()
        .map(|page| {
            let mut value = page.extra.clone();
            value.insert("id".into(), serde_json::json!(page.id));
            value.insert(
                "image_path".into(),
                serde_json::json!(format!("pages/{}.{}", page.page_name, page.ext)),
            );
            value.insert("bubbles".into(), serde_json::json!(page.bubbles));
            serde_json::Value::Object(value)
        })
        .collect::<Vec<_>>();
    let mut value = project.extra.clone();
    value.insert(
        "schema_version".into(),
        serde_json::json!(project.schema_version),
    );
    if let Some(title) = &project.title {
        value.insert("title".into(), serde_json::json!(title));
    }
    if let Some(language) = &project.language {
        value.insert("language".into(), serde_json::json!(language));
    }
    if let Some(glossary) = &project.glossary {
        value.insert("glossary".into(), glossary.clone());
    }
    value.insert("pages".into(), serde_json::Value::Array(exported_pages));
    Ok(serde_json::to_vec_pretty(&serde_json::Value::Object(
        value,
    ))?)
}

/// Owned archive entry so generated descriptors (manifest, OPF/nav XHTML)
/// can sit beside streamed page files in one ordered entry list.
enum OwnedEntry {
    File {
        name: String,
        method: CompressionMethod,
        path: PathBuf,
    },
    Bytes {
        name: String,
        method: CompressionMethod,
        bytes: Vec<u8>,
    },
}

/// The single ordered entry plan for ZIP and EPUB production packaging.
/// Natural page order, every expected page exactly once; ZIP concatenation
/// is never used. Both the file-streaming production path and the in-memory
/// test helper below build from this one plan.
fn export_archive_plan(
    project: &Project,
    pages: &[ResolvedPage],
    epub_layout: bool,
) -> Result<Vec<OwnedEntry>> {
    if pages.is_empty() {
        bail!("no cached pages to package");
    }
    if epub_layout {
        let mut entries = Vec::new();
        entries.push(OwnedEntry::Bytes {
            name: "mimetype".to_owned(),
            method: CompressionMethod::Stored,
            bytes: b"application/epub+zip".to_vec(),
        });
        entries.push(OwnedEntry::Bytes {
            name: "META-INF/container.xml".to_owned(),
            method: CompressionMethod::Deflated,
            bytes: br#"<?xml version="1.0" encoding="UTF-8"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#.to_vec(),
        });
        let mut manifest = String::new();
        let mut spine = String::new();
        let mut nav = String::new();
        for (index, page) in pages.iter().enumerate() {
            let id = format!("p{index:04}");
            let image_id = format!("img{index:04}");
            manifest.push_str(&format!("<item id=\"{id}\" href=\"pages/{name}.xhtml\" media-type=\"application/xhtml+xml\"/><item id=\"{image_id}\" href=\"images/{name}.{ext}\" media-type=\"{mime}\"/>", id=xml_attr(&id), name=page.page_name, ext=xml_attr(&page.ext), mime=media_type(&page.ext)));
            spine.push_str(&format!("<itemref idref=\"{id}\"/>"));
            nav.push_str(&format!(
                "<li><a href=\"pages/{name}.xhtml\">Page {}</a></li>",
                index + 1,
                name = page.page_name
            ));
            entries.push(OwnedEntry::File {
                name: format!("OEBPS/images/{}.{}", page.page_name, page.ext),
                method: CompressionMethod::Deflated,
                path: page.source.clone(),
            });
            entries.push(OwnedEntry::Bytes {
                name: format!("OEBPS/pages/{}.xhtml", page.page_name),
                method: CompressionMethod::Deflated,
                bytes: page_xhtml(page, &project.language).into_bytes(),
            });
        }
        let title = xml_escape(project.title.as_deref().unwrap_or("Fukidashi project"));
        let opf = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">urn:uuid:{}</dc:identifier><dc:title>{title}</dc:title><dc:language>{}</dc:language></metadata><manifest>{manifest}<item id="nav" properties="nav" href="nav.xhtml" media-type="application/xhtml+xml"/></manifest><spine>{spine}</spine></package>"#,
            Uuid::new_v4(),
            xml_escape(project.language.as_deref().unwrap_or("und"))
        );
        entries.push(OwnedEntry::Bytes {
            name: "OEBPS/content.opf".to_owned(),
            method: CompressionMethod::Deflated,
            bytes: opf.into_bytes(),
        });
        let nav_xhtml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE html><html xmlns="http://www.w3.org/1999/xhtml"><head><title>{title}</title></head><body><nav epub:type="toc" xmlns:epub="http://www.idpf.org/2007/ops"><ol>{nav}</ol></nav></body></html>"#
        );
        entries.push(OwnedEntry::Bytes {
            name: "OEBPS/nav.xhtml".to_owned(),
            method: CompressionMethod::Deflated,
            bytes: nav_xhtml.into_bytes(),
        });
        return Ok(entries);
    }
    let mut entries = Vec::new();
    entries.push(OwnedEntry::Bytes {
        name: "project.json".to_owned(),
        method: CompressionMethod::Deflated,
        bytes: export_manifest(project, pages)?,
    });
    for page in pages {
        entries.push(OwnedEntry::File {
            name: format!("pages/{}.{}", page.page_name, page.ext),
            method: CompressionMethod::Deflated,
            path: page.source.clone(),
        });
    }
    Ok(entries)
}

fn as_archive_entries(entries: &[OwnedEntry]) -> Vec<crate::approval::ArchiveEntry<'_>> {
    entries
        .iter()
        .map(|entry| match entry {
            OwnedEntry::File { name, method, path } => crate::approval::ArchiveEntry::File {
                name: name.clone(),
                method: *method,
                path,
            },
            OwnedEntry::Bytes {
                name,
                method,
                bytes,
            } => crate::approval::ArchiveEntry::Bytes {
                name: name.clone(),
                method: *method,
                bytes,
            },
        })
        .collect()
}

/// Production packaging: stream the single ordered entry plan into a temp
/// file via the shared `crate::approval` archive core, then fsync. The
/// caller renames into place; a packaging failure leaves the render
/// cache/checkpoint intact and publishes no partial archive.
fn write_export_archive(
    tmp_path: &Path,
    project: &Project,
    pages: &[ResolvedPage],
    epub_layout: bool,
) -> Result<()> {
    let plan = export_archive_plan(project, pages, epub_layout)?;
    let entries = as_archive_entries(&plan);
    crate::approval::write_archive_to_path(tmp_path, &entries)
}

#[cfg(test)]
fn make_zip(project: &Project, pages: &[ResolvedPage], epub_layout: bool) -> Result<Vec<u8>> {
    // In-memory helper for tests. Production streams to a file through
    // `write_export_archive`; both share `export_archive_plan` and the
    // `crate::approval` writer core.
    let plan = export_archive_plan(project, pages, epub_layout)?;
    let entries = as_archive_entries(&plan);
    let cursor =
        crate::approval::write_archive_entries(std::io::Cursor::new(Vec::new()), &entries)?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
fn make_epub(project: &Project, pages: &[ResolvedPage]) -> Result<Vec<u8>> {
    make_zip(project, pages, true)
}

fn page_xhtml(page: &ResolvedPage, language: &Option<String>) -> String {
    let text = page
        .bubbles
        .iter()
        .map(|b| {
            format!(
                "<p data-bubble-id=\"{}\">{}</p>",
                xml_attr(&b.id),
                xml_escape(b.translation.as_deref().unwrap_or(&b.text))
            )
        })
        .collect::<String>();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><html xmlns="http://www.w3.org/1999/xhtml" lang="{}"><head><title>{}</title></head><body><img src="../images/{}.{}" alt=""/>{}</body></html>"#,
        xml_attr(language.as_deref().unwrap_or("und")),
        xml_escape(&page.page_name),
        page.page_name,
        xml_attr(&page.ext),
        text
    )
}

fn make_html(project: &Project, pages: &[ResolvedPage]) -> Result<Vec<u8>> {
    let title = html_escape(project.title.as_deref().unwrap_or("Fukidashi project"));
    let mut body = String::new();
    for (index, page) in pages.iter().enumerate() {
        let mime = media_type(&page.ext);
        let bytes = fs::read(&page.source)
            .with_context(|| format!("read page image {}", page.source.display()))?;
        let src = format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        body.push_str(&format!("<section data-page=\"{index}\"><img src=\"{src}\" alt=\"Page {}\"><div class=\"dialogue\">", index + 1));
        for bubble in &page.bubbles {
            body.push_str(&format!(
                "<p data-bubble-id=\"{}\">{}</p>",
                html_escape(&bubble.id),
                html_escape(bubble.translation.as_deref().unwrap_or(&bubble.text))
            ));
        }
        body.push_str("</div></section>");
    }
    Ok(format!("<!doctype html><meta charset=\"utf-8\"><title>{title}</title><style>body{{background:#222;color:#eee;font:16px sans-serif}}section{{max-width:1000px;margin:2rem auto}}img{{max-width:100%;height:auto}}.dialogue{{white-space:pre-wrap}}</style>{body}").into_bytes())
}

fn media_type(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        _ => "image/png",
    }
}
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn xml_attr(value: &str) -> String {
    xml_escape(value)
}
fn html_escape(value: &str) -> String {
    xml_escape(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn managed_export_budget_is_larger_but_still_bounded() {
        let ad_hoc = tempdir().unwrap();
        assert_eq!(max_input_bytes(ad_hoc.path()), MAX_INPUT_BYTES);

        fs::write(ad_hoc.path().join("job.json"), b"{}").unwrap();
        assert_eq!(max_input_bytes(ad_hoc.path()), MAX_MANAGED_INPUT_BYTES);

        fs::remove_file(ad_hoc.path().join("job.json")).unwrap();
        fs::write(ad_hoc.path().join(".fukidashi-job.json"), b"{}").unwrap();
        assert_eq!(max_input_bytes(ad_hoc.path()), MAX_MANAGED_INPUT_BYTES);
    }

    fn three_page_project(dir: &tempfile::TempDir) -> PathBuf {
        for index in 0..3 {
            fs::write(
                dir.path().join(format!("page-{index}.png")),
                format!("page-bytes-{index}"),
            )
            .unwrap();
        }
        let manifest = serde_json::json!({
            "schema_version": 1,
            "title": "Order",
            "pages": (0..3).map(|index| serde_json::json!({
                "id": format!("p{index}"),
                "image_path": format!("page-{index}.png"),
                "bubbles": [],
            })).collect::<Vec<_>>(),
        });
        fs::write(
            dir.path().join("project.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        dir.path().to_path_buf()
    }

    fn zip_names(bytes: &[u8]) -> Vec<String> {
        let cursor = std::io::Cursor::new(bytes);
        let mut zip = zip::ZipArchive::new(cursor).unwrap();
        (0..zip.len())
            .map(|index| zip.by_index(index).unwrap().name().to_owned())
            .collect()
    }

    #[test]
    fn file_streamed_export_keeps_natural_order_exactly_once() {
        let dir = tempdir().unwrap();
        let root = three_page_project(&dir);
        let out = tempdir().unwrap();
        let value = export_project_to(&root, "zip", Some(out.path())).unwrap();
        assert_eq!(value["pages"], 3);
        let path = PathBuf::from(value["output_path"].as_str().unwrap());
        assert!(path.is_file());
        let bytes = fs::read(&path).unwrap();
        assert_eq!(
            zip_names(&bytes),
            vec![
                "project.json",
                "pages/page-0000.png",
                "pages/page-0001.png",
                "pages/page-0002.png",
            ]
        );
        // The in-memory helper shares the same entry plan and writer core.
        let manifest_bytes = fs::read(root.join("project.json")).unwrap();
        let project: Project = serde_json::from_slice(&manifest_bytes).unwrap();
        let pages = resolve_pages(&root, project.pages.clone(), MAX_INPUT_BYTES).unwrap();
        let memory = make_zip(&project, &pages, false).unwrap();
        assert_eq!(zip_names(&memory), zip_names(&bytes));
        let epub = make_epub(&project, &pages).unwrap();
        let names = zip_names(&epub);
        assert!(names.contains(&"OEBPS/images/page-0000.png".to_owned()));
        assert!(names.contains(&"OEBPS/pages/page-0002.xhtml".to_owned()));
    }

    #[test]
    fn export_failure_publishes_no_partial_archive() {
        let dir = tempdir().unwrap();
        let root = three_page_project(&dir);
        fs::remove_file(root.join("page-1.png")).unwrap();
        let out = tempdir().unwrap();
        assert!(export_project_to(&root, "zip", Some(out.path())).is_err());
        let published: Vec<_> = fs::read_dir(out.path()).unwrap().collect();
        assert!(published.is_empty());
    }
}

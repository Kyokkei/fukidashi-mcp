use fukidashi_mcp::export::export_project;
use image::{ImageBuffer, Rgb};
use serde_json::json;
use std::fs;
use std::io::Read;
use tempfile::tempdir;
use zip::{CompressionMethod, ZipArchive};

fn fixture() -> tempfile::TempDir {
    let dir = tempdir().expect("temp project");
    let page_a = ImageBuffer::<Rgb<u8>, _>::from_pixel(8, 8, Rgb([255, 255, 255]));
    let page_b = ImageBuffer::<Rgb<u8>, _>::from_pixel(8, 8, Rgb([200, 200, 200]));
    page_a.save(dir.path().join("a.png")).expect("page a");
    page_b.save(dir.path().join("b.png")).expect("page b");
    let manifest = json!({
        "schema_version": 1,
        "title": "Punctuation <& 漫画",
        "language": "vi",
        "pages": [
            {"id":"second", "image_path":"b.png", "bubbles":[{"id":"b2","bbox":{"x1":0.0,"y1":0.0,"x2":3.0,"y2":3.0},"confidence":1.0,"reading_order":0,"text":"B", "translation":"<two>&"}]},
            {"id":"first", "image_path":"a.png", "bubbles":[{"id":"b1","bbox":{"x1":0.0,"y1":0.0,"x2":3.0,"y2":3.0},"confidence":1.0,"reading_order":0,"text":"A", "translation":"one"}]}
        ]
    });
    fs::write(
        dir.path().join("project.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    dir
}

#[test]
fn zip_preserves_manifest_order_and_escapes_html_export() {
    let dir = fixture();
    let result = export_project(dir.path(), "zip").expect("zip export");
    let zip_path = result["output_path"].as_str().unwrap();
    let file = fs::File::open(zip_path).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
    assert_eq!(archive.by_index(0).unwrap().name(), "project.json");
    assert_eq!(archive.by_index(1).unwrap().name(), "pages/page-0000.png");
    assert_eq!(archive.by_index(2).unwrap().name(), "pages/page-0001.png");
    let mut manifest = String::new();
    archive
        .by_name("project.json")
        .unwrap()
        .read_to_string(&mut manifest)
        .unwrap();
    let project: fukidashi_mcp::domain::Project = serde_json::from_str(&manifest).unwrap();
    project.validate().unwrap();

    let html = export_project(dir.path(), "html_monolith").expect("html export");
    let html_text = fs::read_to_string(html["output_path"].as_str().unwrap()).unwrap();
    assert!(html_text.contains("&lt;two&gt;&amp;"));
    assert!(html_text.contains("data-page=\"0\""));
}

#[test]
fn epub_starts_with_uncompressed_mimetype_and_has_ordered_spine() {
    let dir = fixture();
    let result = export_project(dir.path(), "epub").expect("epub export");
    let file = fs::File::open(result["output_path"].as_str().unwrap()).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
    {
        let first = archive.by_index(0).unwrap();
        assert_eq!(first.name(), "mimetype");
        assert_eq!(first.compression(), CompressionMethod::Stored);
    }
    let mut mimetype = String::new();
    archive
        .by_name("mimetype")
        .unwrap()
        .read_to_string(&mut mimetype)
        .unwrap();
    assert_eq!(mimetype, "application/epub+zip");
    assert!(archive.by_name("META-INF/container.xml").is_ok());
    let mut opf = String::new();
    archive
        .by_name("OEBPS/content.opf")
        .unwrap()
        .read_to_string(&mut opf)
        .unwrap();
    assert!(opf.find("page-0000.xhtml").unwrap() < opf.find("page-0001.xhtml").unwrap());
}

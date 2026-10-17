//! `extract_images`: embedded images from Office, ODF, EPUB and HTML documents.

mod common;

use common::*;
use serde_json::{Value, json};

const OPTS: fn() -> Value = || json!({"extract_images": true, "title_method": "local", "summary_method": "local"});

fn image_refs(content: &str) -> Vec<String> {
    content.match_indices("](images/").map(|(i, _)| content[i + 2..].split(')').next().unwrap().to_string()).collect()
}

#[tokio::test]
async fn docx_images_are_extracted_once_and_referenced_in_place() {
    let s = server().await;
    let p = s.write("report.docx", &fixtures::docx_with_pictures());
    let (st, v) = s.convert_sync(&p, OPTS()).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    let c = d["content"].as_str().unwrap();
    let images = d["images"].as_array().unwrap();
    assert_eq!(images.len(), 1, "identical pictures kept once, the icon skipped: {images:?}");
    let img = &images[0];
    let path = img["path"].as_str().unwrap();
    assert!(path.starts_with("images/") && path.ends_with(".png"));
    assert_eq!((img["width"].as_u64(), img["height"].as_u64(), img["format"].as_str()), (Some(640), Some(480), Some("png")));
    assert_eq!(img["alt"], "Revenue chart");
    // In place, after the paragraph before it; the repeat points at the same file.
    assert!(c.contains(&format!("Revenue grew.\n\n![Revenue chart]({path})")), "{c}");
    assert_eq!(image_refs(c), [path, path], "{c}");
    assert_eq!(d["statistics"]["image_count"], 1);
    assert!(!d["warnings"].to_string().contains("not transcribed"));
    // The file is stored and served (token required).
    let file = path.trim_start_matches("images/");
    assert_eq!(std::fs::read(s.dir.join("images").join(file)).unwrap(), fixtures::picture(640, 480, 1));
    assert_eq!(s.client.get(format!("{}/_/images/{file}", s.url)).send().await.unwrap().status(), 401);
    let r = s.client.get(format!("{}/_/images/{file}", s.url)).header("x-access-token", TOKEN).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/png");
    assert!(r.headers()["content-security-policy"].to_str().unwrap().contains("sandbox"));
    assert_eq!(r.bytes().await.unwrap().to_vec(), fixtures::picture(640, 480, 1));
    for bad in ["../doc.sqlite", "0123456789abcdef.exe", "zz23456789abcdef.png"] {
        let r = s.client.get(format!("{}/_/images/{bad}", s.url)).header("x-access-token", TOKEN).send().await.unwrap();
        assert_eq!(r.status(), 404, "{bad}");
    }
}

#[tokio::test]
async fn off_by_default() {
    let s = server().await;
    let p = s.write("report.docx", &fixtures::docx_with_pictures());
    let (_, v) = s.convert_sync(&p, json!({"title_method": "local", "summary_method": "local"})).await;
    assert_eq!(v["data"]["images"], json!([]));
    assert!(!v["data"]["content"].as_str().unwrap().contains("images/"));
    assert!(v["data"]["warnings"].to_string().contains("not transcribed"));
}

#[tokio::test]
async fn pptx_xlsx_and_html() {
    let s = server().await;
    let p = s.write("deck.pptx", &fixtures::pptx_with_picture());
    let (st, v) = s.convert_sync(&p, OPTS()).await;
    assert_eq!(st, 200, "{v}");
    let img = &v["data"]["images"][0];
    assert_eq!(img["format"], "jpg");
    assert_eq!(img["location"], "slide 2");
    assert_eq!(img["alt"], "Team photo");
    let c = v["data"]["content"].as_str().unwrap();
    let slide2 = c.find("## Slide 2").unwrap();
    assert!(c[slide2..].contains("![Team photo](images/"), "{c}");

    let p = s.write("book.xlsx", &fixtures::xlsx_with_picture());
    let (_, v) = s.convert_sync(&p, OPTS()).await;
    assert_eq!(v["data"]["images"][0]["location"], "workbook", "{v}");
    assert!(v["data"]["content"].as_str().unwrap().contains("## Images\n\n![image](images/"));

    let p = s.write("page.html", &fixtures::html_with_data_image());
    let (_, v) = s.convert_sync(&p, OPTS()).await;
    let c = v["data"]["content"].as_str().unwrap();
    assert!(c.contains("Before ![Company logo](images/") && c.contains(" after."), "{c}");
    assert!(c.contains("[image: Remote]"), "external images are not fetched: {c}");
    assert_eq!(v["data"]["images"].as_array().unwrap().len(), 1);
}

#[cfg(feature = "odf-epub")]
#[tokio::test]
async fn odt_and_epub() {
    let s = server().await;
    let p = s.write("notes.odt", &fixtures::odt_with_picture());
    let (st, v) = s.convert_sync(&p, OPTS()).await;
    assert_eq!(st, 200, "{v}");
    assert!(v["data"]["content"].as_str().unwrap().contains("See ![image](images/"), "{v}");
    assert_eq!(v["data"]["images"][0]["width"], 400);

    let p = s.write("book.epub", &fixtures::epub_with_picture());
    let (st, v) = s.convert_sync(&p, OPTS()).await;
    assert_eq!(st, 200, "{v}");
    assert!(v["data"]["content"].as_str().unwrap().contains("![Cover art](images/"), "{v}");
    assert_eq!(v["data"]["images"][0]["height"], 700);
}

#[tokio::test]
async fn destination_gets_an_images_folder_and_cache_hits_keep_them() {
    let s = server().await;
    let p = s.write("report.docx", &fixtures::docx_with_pictures());
    let out = s.dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let folder = format!("{}/", out.display());
    let (_, v) = s.rpc("convert", json!({"source": p, "destination": folder}), OPTS()).await;
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "completed", "{j}");
    let r = &j["result"];
    let file = r["images"][0]["path"].as_str().unwrap().trim_start_matches("images/").to_string();
    assert_eq!(r["images_dir"], format!("{}images/", folder), "{r}");
    // The Markdown's relative references resolve next to it.
    let md = std::fs::read_to_string(out.join("report.docx.md")).unwrap();
    assert!(md.contains(&format!("](images/{file})")));
    assert!(out.join("images").join(&file).exists());
    // A repeat is a cache hit and still publishes the images, into another folder.
    s.flush().await;
    let out2 = s.dir.join("out2");
    std::fs::create_dir_all(&out2).unwrap();
    let (_, v) = s.rpc("convert", json!({"source": p, "destination": format!("{}/", out2.display())}), OPTS()).await;
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["result"]["cache_hit"], true, "{j}");
    assert!(out2.join("images").join(&file).exists());
    // If the stored image is gone, the cached result is not used: the document is converted again.
    s.flush().await;
    std::fs::remove_file(s.dir.join("images").join(&file)).unwrap();
    let (_, v) = s.convert_sync(&p, OPTS()).await;
    assert_eq!(v["data"]["cache_hit"], false);
    assert!(s.dir.join("images").join(&file).exists(), "stored again");
}

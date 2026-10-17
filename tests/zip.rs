//! `create_zip`, `zip_only` and `job.zip`: the Markdown, summary and images in one zip.

mod common;

use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;

const OPTS: fn() -> Value = || json!({"extract_images": true, "title_method": "local", "summary_method": "local"});

/// Entry name -> contents.
fn entries(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut b = Vec::new();
            f.read_to_end(&mut b).unwrap();
            (f.name().to_string(), b)
        })
        .collect()
}

fn folder(s: &TestServer, name: &str) -> (std::path::PathBuf, String) {
    let out = s.dir.join(name);
    std::fs::create_dir_all(&out).unwrap();
    let f = format!("{}/", out.display());
    (out, f)
}

#[tokio::test]
async fn create_zip_next_to_the_markdown() {
    let s = server().await;
    let p = s.write("report.docx", &fixtures::docx_with_pictures());
    let (out, dest) = folder(&s, "out");
    let mut o = OPTS();
    o["create_zip"] = json!(true);
    let (st, v) = s.rpc("convert", json!({"source": p, "destination": dest}), o).await;
    assert_eq!(st, 202, "{v}");
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "completed", "{j}");
    let r = &j["result"];
    let zip_path = out.join("report.docx.zip");
    assert_eq!(r["zip_file"], zip_path.display().to_string());
    assert_eq!(r["zip_bytes"].as_u64(), Some(std::fs::metadata(&zip_path).unwrap().len()));
    assert_eq!(j["zip"]["status"], "completed");
    let e = entries(&std::fs::read(&zip_path).unwrap());
    let image = r["images"][0]["path"].as_str().unwrap();
    assert_eq!(e["report.docx.md"], std::fs::read(out.join("report.docx.md")).unwrap(), "the same Markdown");
    assert!(!String::from_utf8_lossy(&e["summary.md"]).trim().is_empty());
    assert_eq!(e[image], fixtures::picture(640, 480, 1));
    assert_eq!(e.len(), 3, "{:?}", e.keys());
    // The usual files are still written.
    assert!(out.join("report.docx.docv.json").exists() && out.join(image).exists());
}

#[tokio::test]
async fn zip_only_writes_just_the_zip_and_the_result() {
    let s = server().await;
    let p = s.write("report.docx", &fixtures::docx_with_pictures());
    let (out, dest) = folder(&s, "out");
    let mut o = OPTS();
    o["zip_only"] = json!(true);
    o["gen_summary"] = json!(false);
    let (_, v) = s.rpc("convert", json!({"source": p, "destination": dest}), o).await;
    let j = s.wait_job(v["data"]["job_id"].as_str().unwrap()).await;
    assert_eq!(j["status"], "completed", "{j}");
    let r = &j["result"];
    assert!(!out.join("report.docx.md").exists() && !out.join("images").exists());
    assert!(out.join("report.docx.docv.json").exists());
    assert_eq!(r["md_file"], out.join("report.docx.md").display().to_string(), "the path is kept");
    assert_eq!((r["md_bytes"].as_u64(), &r["images_dir"]), (Some(0), &Value::Null), "0 bytes: not written");
    let e = entries(&std::fs::read(out.join("report.docx.zip")).unwrap());
    assert!(e.contains_key("report.docx.md") && !e.contains_key("summary.md"), "no summary, no summary.md: {:?}", e.keys());
    assert_eq!(e.keys().filter(|k| k.starts_with("images/")).count(), 1);
}

#[tokio::test]
async fn job_zip_builds_it_later() {
    let s = server().await;
    let p = s.write("report.docx", &fixtures::docx_with_pictures());
    let (st, v) = s.rpc("convert", json!({"source": p}), OPTS()).await;
    assert_eq!(st, 202, "{v}");
    let job_id = v["data"]["job_id"].as_str().unwrap().to_string();
    let j = s.wait_job(&job_id).await;
    // Always reported; 0 bytes means not made yet.
    let url = format!("/_/jobs/{job_id}/zip");
    assert_eq!((&j["result"]["zip_file"], j["result"]["zip_bytes"].as_u64()), (&json!(url), Some(0)), "{j}");
    assert!(j["zip"].is_null());
    let get = || s.client.get(format!("{}{url}", s.url)).header("x-access-token", TOKEN).send();
    assert_eq!(get().await.unwrap().status(), 404);

    let (st, v) = s.rpc("job.zip", json!({"job_id": job_id}), json!({})).await;
    assert_eq!(st, 202, "{v}");
    assert_eq!((v["data"]["status"].as_str(), v["data"]["zip_file"].as_str()), (Some("running"), Some(url.as_str())));
    let mut info = Value::Null;
    for _ in 0..200 {
        let (_, g) = s.rpc("job.get", json!({"job_id": job_id, "fields": ["status"]}), json!({})).await;
        info = g["data"]["zip"].clone();
        if info["status"] == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(info["status"], "completed", "{info}");
    let r = get().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/zip");
    assert_eq!(r.headers()["content-disposition"], "attachment; filename=\"report.docx.zip\"");
    let bytes = r.bytes().await.unwrap();
    assert_eq!(info["zip_bytes"].as_u64(), Some(bytes.len() as u64));
    let e = entries(&bytes);
    assert!(e.contains_key("report.docx.md") && e.contains_key("summary.md"), "{:?}", e.keys());
    // Asking again reports the zip that exists.
    let (st, v) = s.rpc("job.zip", json!({"job_id": job_id}), json!({})).await;
    assert_eq!((st, &v["data"]["zip_bytes"]), (200, &info["zip_bytes"]), "{v}");
    // The download needs the token.
    assert_eq!(s.client.get(format!("{}{url}", s.url)).send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn zip_errors() {
    let s = server().await;
    let p = s.write("report.docx", &fixtures::docx(1));
    let (st, v) = s.convert_sync(&p, json!({"create_zip": true})).await;
    assert_eq!((st, v["error"]["code"].as_str()), (400, Some("invalid_options")), "{v}");
    let (st, v) = s.rpc("job.zip", json!({"job_id": "0190f1d2-0000-7000-8000-000000000000"}), json!({})).await;
    assert_eq!((st, v["error"]["code"].as_str()), (404, Some("job_not_found")), "{v}");
    // A sync conversion has no job and no zip.
    let (_, v) = s.convert_sync(&p, json!({"title_method": "local", "summary_method": "local"})).await;
    assert_eq!((&v["data"]["zip_file"], &v["data"]["zip_bytes"]), (&Value::Null, &json!(0)));
}

#[cfg(feature = "webhooks")]
#[tokio::test]
async fn job_zip_sends_a_webhook() {
    use std::sync::{Arc, Mutex};
    let hits: Arc<Mutex<Vec<Value>>> = Default::default();
    let h2 = hits.clone();
    let hook = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |body: axum::body::Bytes| {
            let h = h2.clone();
            async move {
                h.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
                "ok"
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, hook).await.unwrap() });

    let s = server().await;
    let p = s.write("report.docx", &fixtures::docx_with_pictures());
    let (_, v) = s.rpc("convert", json!({"source": p, "webhook": {"url": url}}), OPTS()).await;
    let job_id = v["data"]["job_id"].as_str().unwrap().to_string();
    s.wait_job(&job_id).await;
    s.rpc("job.zip", json!({"job_id": job_id}), json!({})).await;
    for _ in 0..200 {
        if hits.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let hits = hits.lock().unwrap().clone();
    let finished = hits.iter().find(|h| h["event"] == "job.finished").expect("job.finished");
    assert_eq!(finished["zip_bytes"], 0, "{finished}");
    let zip = hits.iter().find(|h| h["event"] == "zip.completed").expect("zip.completed");
    assert_eq!(zip["job_id"], job_id.as_str());
    assert_eq!(zip["zip_file"], finished["zip_file"], "the same path as announced before");
    assert!(zip["zip_bytes"].as_u64().unwrap() > 0, "{zip}");
    assert_ne!(zip["event_id"], finished["event_id"]);
}

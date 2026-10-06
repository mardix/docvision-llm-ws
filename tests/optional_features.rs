//! Tests for optional features; each is compiled only when its feature is enabled.

#![allow(unused_imports)]

mod common;

use common::*;
use serde_json::json;

#[cfg(feature = "odf-epub")]
#[tokio::test]
async fn odf_and_epub() {
    let s = server().await;
    let local = json!({"summary_method": "local", "title_method": "local"});
    let p = s.write("a.odt", &fixtures::odt());
    let (st, v) = s.convert_sync(&p, local.clone()).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    assert!(c.contains("# Heading One"));
    assert!(c.contains("Hello  world with [a link](https://example.org)."), "{c}");
    assert!(c.contains("- Item A"));
    assert!(c.contains("| x | y |"));
    assert_eq!(v["data"]["title"], "Odf Title");
    let p = s.write("a.ods", &fixtures::ods());
    let (st, v) = s.convert_sync(&p, local.clone()).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    assert!(c.contains("## Budget") && c.contains("| Paper | 3 |"), "{c}");
    assert!(c.len() < 2000, "repeated empty rows/cells are not expanded");
    let p = s.write("a.odp", &fixtures::odp());
    let (st, v) = s.convert_sync(&p, local.clone()).await;
    assert_eq!(st, 200, "{v}");
    assert!(v["data"]["content"].as_str().unwrap().contains("## Slide 1: Intro"));
    assert_eq!(v["data"]["statistics"]["slide_count"], 2);
    let p = s.write("a.epub", &fixtures::epub());
    let (st, v) = s.convert_sync(&p, local).await;
    assert_eq!(st, 200, "{v}");
    let c = v["data"]["content"].as_str().unwrap();
    assert!(c.find("# Chapter 1").unwrap() < c.find("# Chapter 2").unwrap(), "spine order");
    assert_eq!(v["data"]["title"], "Epub Book");
}

#[cfg(not(feature = "odf-epub"))]
#[tokio::test]
async fn odf_without_feature_is_rejected() {
    let s = server().await;
    let p = s.write("a.odt", &fixtures::odt());
    let (st, v) = s.convert_sync(&p, json!({})).await;
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["error"]["code"], "feature_not_compiled");
}

#[cfg(feature = "lang-detect")]
#[tokio::test]
async fn local_language_detection() {
    let s = server().await;
    let en = "# Report\n\nThe quick brown fox jumps over the lazy dog. This document is written in plain English and describes the results of the quarterly review in detail.\n";
    let p = s.write("en.md", en.as_bytes());
    let (st, v) = s.convert_sync(&p, json!({"detect_language": true, "gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["data"]["language"], "en");
    assert_eq!(v["data"]["language_detection"]["method"], "local_whatlang");
    assert_eq!(v["data"]["llm"]["totals"]["calls"], 0);
    let mixed = format!(
        "{en}\n## Teil\n\nDies ist ein ausführlicher deutscher Absatz über die Ergebnisse der vierteljährlichen Überprüfung und die nächsten Schritte für das Team.\n\n## Partie\n\nCeci est un long paragraphe en français qui décrit les résultats de la revue trimestrielle et les prochaines étapes.\n"
    );
    let p = s.write("mixed.md", mixed.as_bytes());
    let (_, v) = s.convert_sync(&p, json!({"detect_language": true, "gen_summary": false, "gen_title": false, "chunk_size": 16})).await;
    assert_eq!(v["data"]["language"], "mul", "{}", v["data"]["language_detection"]);
}

#[cfg(feature = "translate")]
#[tokio::test]
async fn translation_preserves_original_and_structure() {
    let s = server().await;
    let md = "# Title\n\nParagraph one.\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n```\ncode\n```\n";
    let p = s.write("t.md", md.as_bytes());
    let (st, v) = s.convert_sync(&p, json!({"translate_to": "fr", "gen_summary": false, "gen_title": false})).await;
    assert_eq!(st, 200, "{v}");
    let d = &v["data"];
    assert_eq!(d["content"], md, "original content untouched");
    assert_eq!(d["translated_language"], "fr");
    assert_eq!(d["translated_content"], md, "mock echoes; structure preserved");
    assert!(d["translated_statistics"]["total_words"].as_u64().unwrap() > 0);
    assert_eq!(d["feature_status"]["translation"]["status"], "completed");
    // A failed section means no translation at all (never partial-as-complete).
    *s.mock.p5xx.lock().unwrap() = 1.0;
    let (st, v) = s.convert_sync(&p, json!({"translate_to": "de", "gen_summary": false, "gen_title": false, "cache": "bypass"})).await;
    assert_eq!(st, 200, "{v}");
    assert!(v["data"]["translated_content"].is_null());
    assert_eq!(v["data"]["feature_status"]["translation"]["status"], "failed");
}

//! Property tests: chunking never loses or reorders content; DSN parsing round-trips.

use docvision_llm_ws::markdown::{analyze, apply_overlap, normalize};
use docvision_llm_ws::source::parse_s3;
use proptest::prelude::*;

fn markdown_line() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z0-9 ,.]{0,80}".prop_map(|s| s),
        "[a-z ]{1,30}".prop_map(|s| format!("# {s}")),
        "[a-z ]{1,30}".prop_map(|s| format!("### {s}")),
        "[a-z0-9]{1,8}".prop_map(|s| format!("| {s} | {s} |")),
        Just("```".to_string()),
        Just(String::new()),
        "[\\p{L}\\p{N} ]{0,40}".prop_map(|s| s),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn chunks_cover_content_in_order(lines in prop::collection::vec(markdown_line(), 0..200), target in 16u32..400) {
        let md = normalize(&lines.join("\n"));
        let (stats, chunks) = analyze(&md, target, &[]);
        prop_assert_eq!(stats.content_bytes as usize, md.len());
        if md.is_empty() {
            prop_assert!(chunks.is_empty());
        } else {
            prop_assert_eq!(chunks.first().unwrap().start, 0);
            prop_assert_eq!(chunks.last().unwrap().end, md.len());
            for w in chunks.windows(2) {
                prop_assert_eq!(w[0].end, w[1].start, "contiguous, ordered ranges");
            }
            let rebuilt: String = chunks.iter().map(|c| &md[c.start..c.end]).collect();
            prop_assert_eq!(rebuilt, md.clone());
        }
        prop_assert_eq!(stats.total_characters as usize, md.chars().count());
    }

    #[test]
    fn overlap_only_repeats_the_previous_chunk(lines in prop::collection::vec(markdown_line(), 0..200), target in 32u32..400, pct in 0u32..90) {
        let md = normalize(&lines.join("\n"));
        let (_, mut chunks) = analyze(&md, target, &[]);
        let base = chunks.clone();
        apply_overlap(&md, &mut chunks, target * pct / 100, &[]);
        prop_assert_eq!(chunks.first().map(|c| c.overlap).unwrap_or(0), 0);
        for (i, c) in chunks.iter().enumerate() {
            prop_assert_eq!((c.start, c.end), (base[i].start, base[i].end), "base ranges unchanged");
            if i > 0 {
                prop_assert!(c.start - c.overlap >= base[i - 1].start, "overlap stays within the previous chunk");
                prop_assert!(md.is_char_boundary(c.start - c.overlap));
            }
        }
    }

    #[test]
    fn normalize_is_idempotent(lines in prop::collection::vec(markdown_line(), 0..100)) {
        let once = normalize(&lines.join("\r\n"));
        prop_assert_eq!(normalize(&once), once.clone());
    }

    #[test]
    fn dsn_roundtrip(
        ak in "[A-Z0-9]{4,20}",
        sk in "[a-zA-Z0-9/+:@?#%= ]{1,40}",
        bucket in "[a-z0-9][a-z0-9-]{2,30}",
        key in "[a-zA-Z0-9_ ./-]{1,60}",
        region in prop::option::of("[a-z]{2}-[a-z]{4,9}-[1-3]"),
    ) {
        let key = key.trim_start_matches('/').to_string();
        prop_assume!(!key.is_empty() && !key.split('/').any(|s| s == "." || s == ".."));
        let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
        let path: String = key.split('/').map(enc).collect::<Vec<_>>().join("/");
        let mut dsn = format!("s3://{}:{}@{}/{}", enc(&ak), enc(&sk), bucket, path);
        if let Some(r) = &region {
            dsn.push_str(&format!("?region={r}"));
        }
        let loc = parse_s3(&dsn).unwrap();
        prop_assert_eq!(loc.access_key.unwrap().0, ak);
        prop_assert_eq!(loc.secret_key.unwrap().0, sk);
        prop_assert_eq!(loc.bucket, bucket);
        prop_assert_eq!(loc.key, key);
        prop_assert_eq!(loc.region, region);
    }
}

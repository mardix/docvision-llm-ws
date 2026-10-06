//! Generated document fixtures (no binary files in the repo).

#![allow(dead_code)]

use std::io::Write;

fn zip(entries: &[(&str, String)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

/// DOCX with a title, `sections` sections of paragraphs, a bullet list, a numbered list,
/// a table and a hyperlink.
pub fn docx(sections: usize) -> Vec<u8> {
    let mut body = String::new();
    body.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr><w:r><w:t>Quarterly Report</w:t></w:r></w:p>"#);
    for s in 0..sections {
        body.push_str(&format!(r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Section {s}</w:t></w:r></w:p>"#));
        for p in 0..10 {
            body.push_str(&format!(
                r#"<w:p><w:r><w:t xml:space="preserve">Paragraph {p} of section {s} has </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold text</w:t></w:r><w:r><w:t xml:space="preserve"> and enough words to look like a real document paragraph &amp; more.</w:t></w:r></w:p>"#
            ));
        }
    }
    body.push_str(
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Bullet one</w:t></w:r></w:p>"#,
    );
    body.push_str(
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Bullet two</w:t></w:r></w:p>"#,
    );
    body.push_str(r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>Step one</w:t></w:r></w:p>"#);
    body.push_str(r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>Step two</w:t></w:r></w:p>"#);
    body.push_str(r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Value</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>Alpha</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>42</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#);
    body.push_str(r#"<w:p><w:r><w:t xml:space="preserve">See </w:t></w:r><w:hyperlink r:id="rIdLink"><w:r><w:t>the website</w:t></w:r></w:hyperlink><w:r><w:t>.</w:t></w:r></w:p>"#);
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {W}><w:body>{body}<w:sectPr/></w:body></w:document>"#
    );
    let styles = format!(
        r#"<?xml version="1.0"?><w:styles {W}><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style></w:styles>"#
    );
    let numbering = format!(
        r#"<?xml version="1.0"?><w:numbering {W}><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num></w:numbering>"#
    );
    let rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdLink" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/" TargetMode="External"/></Relationships>"#;
    let core = r#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Docx Meta Title</dc:title></cp:coreProperties>"#;
    zip(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#.into(),
        ),
        ("word/document.xml", doc),
        ("word/styles.xml", styles),
        ("word/numbering.xml", numbering),
        ("word/_rels/document.xml.rels", rels.into()),
        ("docProps/core.xml", core.into()),
    ])
}

/// XLSX with two sheets; the first has `rows` rows, shared strings, numbers and a formula.
pub fn xlsx(rows: usize) -> Vec<u8> {
    let workbook = r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sales" sheetId="1" r:id="rId1"/><sheet name="Notes" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
    let rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="worksheet" Target="/xl/worksheets/sheet2.xml"/></Relationships>"#;
    let shared = r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>Region</t></si><si><t>Revenue</t></si><si><r><t>North</t></r><r><t xml:space="preserve"> East</t></r></si></sst>"#;
    let mut sd = String::from(
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="inlineStr"><is><t>Total</t></is></c></row>"#,
    );
    for i in 0..rows {
        let r = i + 2;
        sd.push_str(&format!(
            r#"<row r="{r}"><c r="A{r}" t="s"><v>2</v></c><c r="B{r}"><v>{}</v></c><c r="C{r}"><f>B{r}*2</f><v>{}</v></c></row>"#,
            i * 10,
            i * 20
        ));
    }
    let sheet1 = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:C{}"/><sheetData>{sd}</sheetData></worksheet>"#,
        rows + 1
    );
    let sheet2 = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="B1" t="inlineStr"><is><t>Only B</t></is></c></row></sheetData></worksheet>"#;
    zip(&[
        ("[Content_Types].xml", "<Types/>".into()),
        ("xl/workbook.xml", workbook.into()),
        ("xl/_rels/workbook.xml.rels", rels.into()),
        ("xl/sharedStrings.xml", shared.into()),
        ("xl/worksheets/sheet1.xml", sheet1),
        ("xl/worksheets/sheet2.xml", sheet2.into()),
    ])
}

/// PPTX with `n` slides; slide order comes from presentation.xml (reversed part names).
pub fn pptx(n: usize) -> Vec<u8> {
    let ns = r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let mut ids = String::new();
    let mut rels =
        String::from(r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
    let mut entries: Vec<(String, String)> = Vec::new();
    for i in 0..n {
        // Part names are deliberately reversed to prove ordering follows sldIdLst.
        let part = n - i;
        ids.push_str(&format!(r#"<p:sldId id="{}" r:id="rIdS{i}"/>"#, 256 + i));
        rels.push_str(&format!(r#"<Relationship Id="rIdS{i}" Type="slide" Target="slides/slide{part}.xml"/>"#));
        let slide = format!(
            r#"<?xml version="1.0"?><p:sld {ns}><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Title {}</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>Point A of slide {}</a:t></a:r></a:p><a:p><a:pPr lvl="1"/><a:r><a:t>Sub point</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
            i + 1,
            i + 1
        );
        entries.push((format!("ppt/slides/slide{part}.xml"), slide));
    }
    rels.push_str("</Relationships>");
    let pres = format!(r#"<?xml version="1.0"?><p:presentation {ns}><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"#);
    let mut all: Vec<(&str, String)> =
        vec![("[Content_Types].xml", "<Types/>".into()), ("ppt/presentation.xml", pres), ("ppt/_rels/presentation.xml.rels", rels)];
    for (k, v) in &entries {
        all.push((k.as_str(), v.clone()));
    }
    zip(&all)
}

/// Minimal page-tree PDF with `pages` empty pages (for LLM-path tests).
pub fn pdf(pages: usize) -> Vec<u8> {
    let mut s = String::from("%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
    s.push_str(&format!("2 0 obj << /Type /Pages /Kids [{}] /Count {} >> endobj\n", kids.join(" "), pages));
    for i in 0..pages {
        s.push_str(&format!("{} 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >> endobj\n", i + 3));
    }
    s.push_str("trailer << /Size 10 /Root 1 0 R >>\n%%EOF\n");
    s.into_bytes()
}

/// A real PDF with extractable text on every page except those in `blank` (scanned-like).
#[cfg(feature = "pdf-native")]
pub fn text_pdf(pages: usize, blank: &[usize]) -> Vec<u8> {
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {"Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica"});
    let resources_id = doc.add_object(dictionary! {"Font" => dictionary! {"F1" => font_id}});
    let mut kids = Vec::new();
    for p in 0..pages {
        let ops = if blank.contains(&(p + 1)) {
            vec![]
        } else {
            vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new(
                    "Tj",
                    vec![Object::string_literal(format!("This is native text on page {} with plenty of words to count.", p + 1))],
                ),
                Operation::new("ET", vec![]),
            ]
        };
        let content = Content { operations: ops };
        let cid = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page = doc.add_object(dictionary! {"Type" => "Page", "Parent" => pages_id, "Contents" => cid, "Resources" => resources_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()]});
        kids.push(page.into());
    }
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! {"Type" => "Pages", "Kids" => kids, "Count" => pages as i64}));
    let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages_id});
    doc.trailer.set("Root", catalog);
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

pub fn png(w: u32, h: u32) -> Vec<u8> {
    // Signature + IHDR is all the header check needs.
    let mut v = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
    v
}

pub fn legacy_doc() -> Vec<u8> {
    let mut v = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    v.extend_from_slice(&[0u8; 504]);
    v
}

pub const HTML: &str = "<!doctype html><html><head><title>Html Title</title><style>p{}</style></head><body><h1>Main</h1><p>Hello <em>world</em> &amp; friends.</p><table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table></body></html>";

const ODF_NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:xlink="http://www.w3.org/1999/xlink""#;

fn odf(mime: &str, body: &str) -> Vec<u8> {
    zip(&[
        ("mimetype", mime.into()),
        ("content.xml", format!(r#"<?xml version="1.0"?><office:document-content {ODF_NS}><office:body>{body}</office:body></office:document-content>"#)),
        ("meta.xml", r#"<?xml version="1.0"?><office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><office:meta><dc:title>Odf Title</dc:title></office:meta></office:document-meta>"#.into()),
    ])
}

pub fn odt() -> Vec<u8> {
    odf(
        "application/vnd.oasis.opendocument.text",
        r#"<office:text><text:h text:outline-level="1">Heading One</text:h><text:p>Hello<text:s text:c="2"/>world with <text:a xlink:href="https://example.org">a link</text:a>.</text:p><text:list><text:list-item><text:p>Item A</text:p></text:list-item></text:list><table:table table:name="T"><table:table-row><table:table-cell><text:p>x</text:p></table:table-cell><table:table-cell><text:p>y</text:p></table:table-cell></table:table-row></table:table></office:text>"#,
    )
}

pub fn ods() -> Vec<u8> {
    odf(
        "application/vnd.oasis.opendocument.spreadsheet",
        r#"<office:spreadsheet><table:table table:name="Budget"><table:table-row><table:table-cell><text:p>Item</text:p></table:table-cell><table:table-cell><text:p>Cost</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>Paper</text:p></table:table-cell><table:table-cell><text:p>3</text:p></table:table-cell><table:table-cell table:number-columns-repeated="1000"/></table:table-row><table:table-row table:number-rows-repeated="100000"><table:table-cell table:number-columns-repeated="1000"/></table:table-row></table:table></office:spreadsheet>"#,
    )
}

pub fn odp() -> Vec<u8> {
    odf(
        "application/vnd.oasis.opendocument.presentation",
        r#"<office:presentation><draw:page draw:name="Intro"><draw:frame><draw:text-box><text:p>Welcome slide</text:p></draw:text-box></draw:frame></draw:page><draw:page draw:name="page2"><draw:frame><draw:text-box><text:p>Second</text:p></draw:text-box></draw:frame></draw:page></office:presentation>"#,
    )
}

pub fn epub() -> Vec<u8> {
    zip(&[
        ("mimetype", "application/epub+zip".into()),
        ("META-INF/container.xml", r#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#.into()),
        ("OEBPS/content.opf", r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata><dc:title>Epub Book</dc:title></metadata><manifest><item id="c2" href="ch2.xhtml" media-type="application/xhtml+xml"/><item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#.into()),
        ("OEBPS/ch1.xhtml", "<html><body><h1>Chapter 1</h1><p>First.</p></body></html>".into()),
        ("OEBPS/ch2.xhtml", "<html><body><h1>Chapter 2</h1><p>Second.</p></body></html>".into()),
    ])
}

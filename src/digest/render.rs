use chrono::NaiveDate;
use pulldown_cmark::{html, Options, Parser};

const TEMPLATE: &str = include_str!("../../templates/email.html");

pub fn markdown_to_html(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(md, opts);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// E-mailové klienty spolehlivě stylují jen inline styly — dosadíme je do tagů
/// vygenerovaných z Markdownu.
fn inline_styles(html: &str) -> String {
    const MAP: [(&str, &str); 10] = [
        ("<h1>", "<h1 style=\"font-size:22px;margin:0 0 14px;color:#111;\">"),
        ("<h2>", "<h2 style=\"font-size:17px;margin:22px 0 8px;color:#111;border-bottom:1px solid #eceef1;padding-bottom:4px;\">"),
        ("<h3>", "<h3 style=\"font-size:15px;margin:16px 0 6px;color:#111;\">"),
        ("<table>", "<table style=\"border-collapse:collapse;width:100%;margin:8px 0 14px;font-size:14px;\">"),
        ("<th>", "<th style=\"text-align:left;border-bottom:2px solid #d0d4da;padding:5px 10px 5px 0;\">"),
        ("<td>", "<td style=\"border-bottom:1px solid #eceef1;padding:5px 10px 5px 0;vertical-align:top;\">"),
        ("<ul>", "<ul style=\"margin:6px 0 12px;padding-left:22px;\">"),
        ("<li>", "<li style=\"margin:3px 0;\">"),
        ("<blockquote>", "<blockquote style=\"margin:8px 0;padding:2px 14px;border-left:3px solid #d0d4da;color:#555;\">"),
        ("<code>", "<code style=\"background:#f4f5f7;border-radius:3px;padding:1px 5px;font-size:13px;\">"),
    ];
    let mut out = html.to_string();
    for (from, to) in MAP {
        out = out.replace(from, to);
    }
    out
}

/// Celý e-mail: Markdown → HTML → inline styly → šablona.
pub fn render_email(md: &str, date: NaiveDate) -> String {
    let body = inline_styles(&markdown_to_html(md));
    TEMPLATE
        .replace("{{BODY}}", &body)
        .replace("{{DATE}}", &date.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_tables_and_headers() {
        let html = markdown_to_html("# Titul\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("<h1>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("<td>1</td>"));
    }

    #[test]
    fn email_has_inline_styles_and_date() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 17).unwrap();
        let out = render_email("# Test\n\n- bod\n", d);
        assert!(out.contains("<h1 style="));
        assert!(out.contains("<li style="));
        assert!(out.contains("2026-07-17"));
        assert!(!out.contains("{{BODY}}"));
        assert!(!out.contains("{{DATE}}"));
    }
}

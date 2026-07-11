use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::model::Client;
use crate::pricing::{CatalogFreshness, MeasureStatus};
use crate::report::{
    QueryCoverageStatus, ReportDocument, ReportRow, ReportSummary, summarize_rows,
};

/// Render a complete offline report. The renderer intentionally never reads
/// `ReportRow::event_ids`, so canonical/session/source identifiers cannot leak
/// into the shareable artifact by default.
pub fn render_report_html(document: &ReportDocument) -> String {
    let summary = summarize_rows(&document.rows);
    let mut html = String::with_capacity(32_000);
    html.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
    html.push_str("<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n");
    html.push_str("<meta name=\"color-scheme\" content=\"light\">\n");
    let _ = writeln!(
        html,
        "<title>Token Ledger — {} to {}</title>",
        escape_html(&document.query.requested_start_date),
        escape_html(&document.query.requested_end_date_inclusive)
    );
    html.push_str(STYLE);
    html.push_str("</head>\n<body>\n<a class=\"skip\" href=\"#report\">Skip to report</a>\n");
    html.push_str("<div class=\"sheet\"><div class=\"hazard\" aria-hidden=\"true\"></div>\n");
    html.push_str("<header class=\"masthead\">\n<div class=\"mast-copy\">\n");
    html.push_str("<p class=\"eyebrow\">LOCAL INFERENCE ACCOUNTING / PRIVATE BY DESIGN</p>\n");
    html.push_str("<h1><span>Token</span><span>Ledger</span></h1>\n");
    html.push_str("<p class=\"lede\">A measured view of locally persisted Claude Code and OpenAI Codex usage. Estimates are API list-price equivalents—not provider invoices.</p>\n</div>\n");
    let _ = writeln!(
        html,
        "<div class=\"issue-stamp\" aria-label=\"Report date range\"><span>REPORT WINDOW</span><strong>{}</strong><i>through</i><strong>{}</strong><small>{}</small></div>",
        escape_html(&document.query.requested_start_date),
        escape_html(&document.query.requested_end_date_inclusive),
        escape_html(&document.query.timezone)
    );
    html.push_str("</header>\n<main id=\"report\">\n");

    render_scope(document, &mut html);
    if document.coverage.provisional {
        let _ = writeln!(
            html,
            "<aside class=\"snapshot-alert\" role=\"status\"><b>PROVISIONAL SNAPSHOT</b><span>{} source(s) were active or volatile at the as-of boundary. Totals may move on the next scan.</span></aside>",
            format_count(document.coverage.active_or_volatile_source_count)
        );
    }
    render_metrics(&summary, &mut html);
    render_rows(document, &mut html);
    render_coverage(document, &mut html);

    html.push_str("</main>\n<footer>\n<div><b>READ THIS REPORT CORRECTLY.</b> Coverage reflects readable local persistence. Deleted, remote, expired, ephemeral, or other-machine activity may be absent.</div>\n");
    html.push_str("<div class=\"privacy-mark\"><span aria-hidden=\"true\">◆</span> SHARE-SAFE DEFAULT<br><small>No prompts, paths, event IDs, session IDs, or source IDs are embedded.</small></div>\n");
    let _ = writeln!(
        html,
        "<div class=\"generated\">Generated {}<br>As of {}<br>Catalog {} / {}</div>",
        escape_html(&document.generated_at_utc.to_rfc3339()),
        escape_html(
            &document
                .coverage
                .as_of
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "unavailable".to_string())
        ),
        escape_html(&document.catalog.revision),
        freshness_label(document.catalog.freshness)
    );
    html.push_str("</footer>\n</div>\n</body>\n</html>\n");
    html
}

pub fn write_report_html(document: &ReportDocument, path: Option<&Path>) -> Result<String> {
    let html = render_report_html(document);
    if let Some(path) = path {
        std::fs::write(path, &html)
            .with_context(|| format!("failed to write HTML report {}", path.display()))?;
    }
    Ok(html)
}

fn render_scope(document: &ReportDocument, html: &mut String) {
    html.push_str("<section class=\"scope\" aria-labelledby=\"scope-title\"><div><p class=\"section-no\">00 / SCOPE</p><h2 id=\"scope-title\">What is counted</h2></div><div class=\"scope-chips\">\n");
    let _ = write!(
        html,
        "<span><b>GROUPED</b>{}</span><span><b>CLIENTS</b>{}</span><span><b>MODELS</b>{}</span>",
        escape_html(&document.query.group_by.join(" + ")),
        escape_html(&filter_label(&document.query.client_filters)),
        escape_html(&filter_label(&document.query.model_filters)),
    );
    html.push_str("</div></section>\n");
}

fn render_metrics(summary: &ReportSummary, html: &mut String) {
    let (price, price_note) = summary_price(summary);
    html.push_str("<section class=\"metrics\" aria-label=\"Report totals\">\n");
    metric(
        html,
        "01",
        "Requests",
        &format_count(summary.requests),
        "canonical usage events",
    );
    metric(
        html,
        "02",
        "Input tokens",
        &format_count(summary.input_tokens_total),
        &format!(
            "{} cached / {} uncached",
            format_count(summary.input_tokens_cached),
            format_count(summary.input_tokens_uncached)
        ),
    );
    metric(
        html,
        "03",
        "Output tokens",
        &format_count(summary.output_tokens_total),
        &format!(
            "{} cache-write tokens",
            format_count(summary.cache_write_tokens)
        ),
    );
    metric(html, "04", "API equivalent", &price, &price_note);
    html.push_str("</section>\n");
}

fn metric(html: &mut String, number: &str, label: &str, value: &str, note: &str) {
    let _ = writeln!(
        html,
        "<article class=\"metric\"><span class=\"metric-no\">{number}</span><p>{}</p><strong>{}</strong><small>{}</small></article>",
        escape_html(label),
        escape_html(value),
        escape_html(note)
    );
}

fn render_rows(document: &ReportDocument, html: &mut String) {
    html.push_str("<section class=\"breakdown\" aria-labelledby=\"breakdown-title\"><div class=\"section-head\"><div><p class=\"section-no\">05 / BREAKDOWN</p><h2 id=\"breakdown-title\">Usage register</h2></div><p>Every row is an aggregate. Drilldown identifiers remain in the private CLI and are intentionally excluded here.</p></div>\n");
    if document.rows.is_empty() {
        html.push_str("<div class=\"empty\"><b>NO MATCHING LOCAL EVENTS</b><p>This is not automatically a verified zero. Review the coverage register below.</p></div></section>\n");
        return;
    }
    html.push_str("<div class=\"table-wrap\"><table><caption>Aggregated token usage and API-equivalent estimates</caption><thead><tr><th scope=\"col\">Day / client</th><th scope=\"col\">Model</th><th scope=\"col\" class=\"num\">Req.</th><th scope=\"col\" class=\"num\">Uncached</th><th scope=\"col\" class=\"num\">Cached</th><th scope=\"col\" class=\"num\">Writes</th><th scope=\"col\" class=\"num\">Output</th><th scope=\"col\" class=\"num\">USD est.</th><th scope=\"col\">Evidence</th></tr></thead><tbody>\n");
    for row in &document.rows {
        render_row(row, html);
    }
    html.push_str("</tbody></table></div>\n");
    if document.rows.iter().any(|row| row.unpriced_events > 0) {
        html.push_str("<p class=\"warning\"><b>PRICE GAP</b> Unpriced events are excluded from known USD subtotals and are never treated as $0.</p>\n");
    }
    if document
        .rows
        .iter()
        .any(|row| row.api_equivalent_usd_status == MeasureStatus::Bounded)
    {
        html.push_str("<p class=\"warning\"><b>FINITE RANGE</b> USD endpoints cover documented routing/tier scenarios or a source-counter invariant; neither endpoint is an invoice.</p>\n");
    }
    html.push_str("</section>\n");
}

fn render_row(row: &ReportRow, html: &mut String) {
    let writes = row
        .cache_write_5m_tokens
        .saturating_add(row.cache_write_1h_tokens)
        .saturating_add(row.cache_write_unknown_tokens);
    let (price, price_class) = row_price(row);
    let evidence = if row.api_equivalent_usd_status == MeasureStatus::Bounded {
        "USD bounded".to_string()
    } else if row.api_equivalent_usd_status == MeasureStatus::Partial {
        "USD partial".to_string()
    } else if row.api_equivalent_usd_status == MeasureStatus::Unpriced {
        "USD unpriced".to_string()
    } else if row.unpriced_events > 0 {
        format!("{} unpriced", row.unpriced_events)
    } else if row.partial_events > 0 {
        format!("{} partial", row.partial_events)
    } else {
        row.quality.clone()
    };
    let _ = writeln!(
        html,
        "<tr><td><b>{}</b><small>{}</small></td><td class=\"model\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num price {price_class}\">{}</td><td><span class=\"quality\">{}</span>{}</td></tr>",
        escape_html(row.day.as_deref().unwrap_or("all dates")),
        escape_html(row.client.as_deref().unwrap_or("all clients")),
        escape_html(row.model.as_deref().unwrap_or("all models")),
        format_count(row.requests),
        format_count(row.input_tokens_uncached),
        format_count(row.input_tokens_cached),
        format_count(writes),
        format_count(row.output_tokens_total),
        escape_html(&price),
        escape_html(&evidence),
        row_details(row),
    );
}

fn row_details(row: &ReportRow) -> String {
    let mut notes = Vec::new();
    if let Some(unit) = row.provider_unit_name.as_deref() {
        notes.push(format!(
            "{} {unit} ({})",
            html_measure_amount(
                row.provider_unit_status,
                row.provider_unit_lower_bound.as_deref(),
                row.provider_unit_upper_bound.as_deref(),
                ""
            ),
            html_status_label(row.provider_unit_status)
        ));
    } else {
        for (unit, amount) in &row.provider_units {
            notes.push(format!("{amount} {unit}"));
        }
        for (unit, amount) in &row.known_provider_units {
            notes.push(format!("≥{amount} {unit}"));
        }
    }
    notes.extend(row.warnings.iter().cloned());
    if notes.is_empty() {
        return String::new();
    }
    let mut html = String::from("<details><summary>notes</summary><ul>");
    for note in notes {
        let _ = write!(html, "<li>{}</li>", escape_html(&note));
    }
    html.push_str("</ul></details>");
    html
}

fn render_coverage(document: &ReportDocument, html: &mut String) {
    html.push_str("<section class=\"coverage\" aria-labelledby=\"coverage-title\"><div class=\"section-head\"><div><p class=\"section-no\">06 / COVERAGE</p><h2 id=\"coverage-title\">Evidence register</h2></div><p>Local evidence is bounded. A blank interval does not prove that a provider recorded no usage.</p></div><div class=\"coverage-grid\">\n");
    for assessment in &document.query_coverage {
        let label = coverage_label(assessment.status);
        let class_name = coverage_class(assessment.status);
        let _ = writeln!(
            html,
            "<article><div><span class=\"lamp {class_name}\" aria-hidden=\"true\"></span><b>{}</b></div><strong>{}</strong><p>{} matching event(s)</p><small>{}</small></article>",
            client_label(assessment.client),
            label,
            format_count(assessment.matching_event_count),
            escape_html(&assessment.note)
        );
    }
    html.push_str("</div></section>\n");
}

fn summary_price(summary: &ReportSummary) -> (String, String) {
    if summary.requests == 0 {
        return ("—".to_string(), "no matching local events".to_string());
    }
    (
        html_measure_amount(
            summary.api_equivalent_usd_status,
            summary.api_equivalent_usd_lower_bound.as_deref(),
            summary.api_equivalent_usd_upper_bound.as_deref(),
            "$",
        ),
        match summary.api_equivalent_usd_status {
            MeasureStatus::Exact => "exact at catalog rates".to_string(),
            MeasureStatus::Bounded => "finite scenario range".to_string(),
            MeasureStatus::Partial => format!(
                "lower bound · {} partial / {} unpriced",
                summary.partial_events, summary.unpriced_events
            ),
            MeasureStatus::Unpriced => {
                format!("{} event(s) need rate coverage", summary.unpriced_events)
            }
            MeasureStatus::Unavailable => "not applicable".to_string(),
        },
    )
}

fn row_price(row: &ReportRow) -> (String, &'static str) {
    (
        html_measure_amount(
            row.api_equivalent_usd_status,
            row.api_equivalent_usd_lower_bound.as_deref(),
            row.api_equivalent_usd_upper_bound.as_deref(),
            "$",
        ),
        match row.api_equivalent_usd_status {
            MeasureStatus::Exact => "exact",
            MeasureStatus::Bounded => "bounded",
            MeasureStatus::Partial => "bounded",
            MeasureStatus::Unpriced | MeasureStatus::Unavailable => "missing",
        },
    )
}

fn html_measure_amount(
    status: MeasureStatus,
    lower: Option<&str>,
    upper: Option<&str>,
    prefix: &str,
) -> String {
    match status {
        MeasureStatus::Exact => lower
            .map(|value| format!("{prefix}{value}"))
            .unwrap_or_else(|| "—".to_string()),
        MeasureStatus::Bounded => match (lower, upper) {
            (Some(lower), Some(upper)) => format!("{prefix}{lower}–{prefix}{upper}"),
            (Some(lower), None) => format!("≥{prefix}{lower}"),
            _ => "BOUNDED".to_string(),
        },
        MeasureStatus::Partial => lower
            .map(|value| format!("≥{prefix}{value}"))
            .unwrap_or_else(|| "PARTIAL".to_string()),
        MeasureStatus::Unpriced => "UNPRICED".to_string(),
        MeasureStatus::Unavailable => "N/A".to_string(),
    }
}

fn html_status_label(status: MeasureStatus) -> &'static str {
    match status {
        MeasureStatus::Exact => "exact",
        MeasureStatus::Bounded => "bounded",
        MeasureStatus::Partial => "partial",
        MeasureStatus::Unpriced => "unpriced",
        MeasureStatus::Unavailable => "unavailable",
    }
}

fn filter_label(values: &[String]) -> String {
    if values.is_empty() {
        "ALL".to_string()
    } else {
        values.join(" + ")
    }
}

fn client_label(client: Client) -> &'static str {
    match client {
        Client::ClaudeCode => "CLAUDE CODE",
        Client::OpenaiCodex => "OPENAI CODEX",
    }
}

fn coverage_label(status: QueryCoverageStatus) -> &'static str {
    match status {
        QueryCoverageStatus::NoSources => "NO SOURCES",
        QueryCoverageStatus::NoObservations => "NO OBSERVATIONS",
        QueryCoverageStatus::MatchingEvents => "MATCHED",
        QueryCoverageStatus::NoEventsWithinObservedWindow => "NO MATCH IN WINDOW",
        QueryCoverageStatus::OutsideObservedWindow => "OUTSIDE WINDOW",
    }
}

fn coverage_class(status: QueryCoverageStatus) -> &'static str {
    match status {
        QueryCoverageStatus::MatchingEvents => "ok",
        QueryCoverageStatus::NoEventsWithinObservedWindow => "watch",
        QueryCoverageStatus::NoSources
        | QueryCoverageStatus::NoObservations
        | QueryCoverageStatus::OutsideObservedWindow => "stop",
    }
}

fn freshness_label(value: CatalogFreshness) -> &'static str {
    match value {
        CatalogFreshness::Fresh => "fresh",
        CatalogFreshness::Stale => "stale",
        CatalogFreshness::FutureDated => "future-dated",
    }
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

const STYLE: &str = r#"<style>
:root{--paper:#ece6d8;--paper-hi:#f7f2e8;--ink:#171713;--muted:#5c5a51;--rule:#171713;--red:#db3a28;--blue:#1554d1;--yellow:#f0bd2d;--green:#23865c}
*{box-sizing:border-box}
html{background:#292823;color:var(--ink);font-family:Georgia,'Times New Roman',serif}
body{margin:0;padding:34px;background:radial-gradient(circle at 10% 0,#4a4942 0,transparent 26%),#292823}
.skip{position:fixed;left:12px;top:-60px;background:#fff;color:#000;padding:12px 18px;z-index:20;font:700 14px Consolas,monospace}.skip:focus{top:12px}
.sheet{position:relative;max-width:1440px;margin:auto;background-color:var(--paper);background-image:linear-gradient(rgba(23,23,19,.055) 1px,transparent 1px),linear-gradient(90deg,rgba(23,23,19,.055) 1px,transparent 1px);background-size:28px 28px;border:2px solid #080806;box-shadow:16px 18px 0 rgba(0,0,0,.34);overflow:hidden}
.hazard{height:14px;border-bottom:2px solid var(--rule);background:repeating-linear-gradient(135deg,var(--yellow) 0 18px,var(--ink) 18px 36px)}
.masthead{display:grid;grid-template-columns:minmax(0,1fr) 270px;gap:56px;padding:50px 54px 38px;border-bottom:4px solid var(--rule);background:linear-gradient(112deg,rgba(255,255,255,.32),transparent 50%)}
.eyebrow,.section-no{margin:0 0 12px;font:700 12px/1 Consolas,'Courier New',monospace;letter-spacing:.18em;text-transform:uppercase}
h1{margin:0;font-family:Impact,'Arial Narrow',sans-serif;font-weight:900;font-size:clamp(72px,11vw,156px);line-height:.7;letter-spacing:-.045em;text-transform:uppercase}
h1 span{display:block}h1 span:last-child{color:var(--red);margin-left:clamp(42px,11vw,150px);text-shadow:4px 4px 0 var(--ink);-webkit-text-stroke:1px var(--ink)}
.lede{max-width:690px;margin:36px 0 0;padding-left:18px;border-left:8px solid var(--blue);font-size:19px;line-height:1.45}
.issue-stamp{align-self:end;padding:18px;border:2px solid var(--rule);box-shadow:7px 7px 0 var(--blue);transform:rotate(1.4deg);background:var(--paper-hi);font-family:Consolas,'Courier New',monospace;text-align:center}
.issue-stamp span,.issue-stamp small{display:block;font-size:11px;letter-spacing:.13em}.issue-stamp strong{display:block;font-size:22px;margin:8px 0}.issue-stamp i{font-family:Georgia,serif;font-size:13px}
main{padding:0 54px 54px}.scope{display:grid;grid-template-columns:210px 1fr;gap:34px;align-items:start;padding:34px 0;border-bottom:2px solid var(--rule)}
h2{margin:0;font-family:Impact,'Arial Narrow',sans-serif;font-size:38px;line-height:1;text-transform:uppercase;letter-spacing:.02em}
.scope-chips{display:grid;grid-template-columns:repeat(3,1fr);border:2px solid var(--rule)}.scope-chips span{min-width:0;padding:14px 16px;font:700 13px/1.35 Consolas,monospace;overflow-wrap:anywhere}.scope-chips span+span{border-left:2px solid var(--rule)}.scope-chips b{display:block;margin-bottom:7px;color:var(--red);font-size:10px;letter-spacing:.14em}
.snapshot-alert{display:flex;gap:18px;align-items:center;margin:22px 0 -18px;padding:13px 16px;border:2px solid var(--red);background:rgba(219,58,40,.1);font:12px/1.4 Consolas,monospace}.snapshot-alert b{flex:none;color:var(--red);letter-spacing:.08em}
.metrics{display:grid;grid-template-columns:repeat(4,1fr);margin:40px 0 56px;border:2px solid var(--rule);background:var(--paper-hi)}.metric{position:relative;min-width:0;padding:25px 20px 22px;overflow:hidden}.metric+ .metric{border-left:2px solid var(--rule)}.metric-no{position:absolute;right:7px;top:-13px;color:rgba(21,84,209,.18);font:900 76px Impact,sans-serif}.metric p{position:relative;margin:0 0 15px;font:700 11px Consolas,monospace;letter-spacing:.12em;text-transform:uppercase}.metric strong{position:relative;display:block;font:900 clamp(24px,3vw,43px)/1 Impact,'Arial Narrow',sans-serif;letter-spacing:.01em;overflow-wrap:anywhere}.metric small{position:relative;display:block;margin-top:12px;color:var(--muted);font:12px/1.35 Consolas,monospace}
.section-head{display:grid;grid-template-columns:minmax(250px,1fr) minmax(260px,430px);gap:36px;align-items:end;margin-bottom:18px}.section-head>p{margin:0;padding:12px 14px;border-left:6px solid var(--red);background:rgba(247,242,232,.7);line-height:1.4}
.table-wrap{overflow-x:auto;border:2px solid var(--rule);background:rgba(247,242,232,.82)}table{width:100%;min-width:1050px;border-collapse:collapse;font:13px/1.25 Consolas,'Courier New',monospace}caption{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)}th{padding:12px 10px;background:var(--ink);color:var(--paper-hi);font-size:10px;letter-spacing:.08em;text-transform:uppercase;text-align:left}td{padding:15px 10px;border-top:1px solid rgba(23,23,19,.35);vertical-align:top}tbody tr:first-child td{border-top:0}tbody tr:nth-child(even){background:rgba(21,84,209,.045)}tbody tr:hover{background:rgba(240,189,45,.16)}td b,td small{display:block}td small{margin-top:5px;color:var(--muted)}td.model{max-width:230px;font-weight:700;overflow-wrap:anywhere}.num{text-align:right;font-variant-numeric:tabular-nums}.price{font-weight:900}.price.bounded{color:#9a351f}.price.missing{color:var(--red)}.quality{display:inline-block;padding:3px 6px;border:1px solid currentColor;font-size:10px;text-transform:uppercase}.warning{margin:16px 0 0;padding:13px 16px;border:2px solid var(--red);background:rgba(219,58,40,.08);font:13px/1.4 Consolas,monospace}.warning b{margin-right:9px;color:var(--red)}details{margin-top:7px}summary{cursor:pointer;color:var(--blue);font-size:11px}details ul{margin:7px 0 0;padding-left:18px;max-width:250px;white-space:normal}.empty{border:2px solid var(--rule);padding:38px;text-align:center;background:var(--paper-hi)}.empty b{font:42px Impact,sans-serif}.empty p{margin-bottom:0}
.coverage{margin-top:62px}.coverage-grid{display:grid;grid-template-columns:repeat(2,1fr);gap:16px}.coverage article{padding:20px;border:2px solid var(--rule);background:var(--paper-hi);box-shadow:5px 5px 0 rgba(23,23,19,.18)}.coverage article>div{display:flex;gap:10px;align-items:center;font:700 12px Consolas,monospace;letter-spacing:.08em}.lamp{width:12px;height:12px;border:2px solid var(--ink);border-radius:50%;background:var(--red)}.lamp.ok{background:var(--green)}.lamp.watch{background:var(--yellow)}.coverage article>strong{display:block;margin-top:22px;font:32px Impact,sans-serif}.coverage article p{font:12px Consolas,monospace}.coverage article small{display:block;color:var(--muted);line-height:1.35}
footer{display:grid;grid-template-columns:1.5fr 1fr auto;gap:28px;padding:28px 54px;border-top:4px solid var(--rule);background:var(--ink);color:var(--paper-hi);font:12px/1.5 Consolas,monospace}.privacy-mark{color:var(--yellow)}.privacy-mark span{color:var(--red)}.generated{text-align:right;color:#bbb8ad}
@media (max-width:900px){body{padding:0}.sheet{border:0;box-shadow:none}.masthead{grid-template-columns:1fr;padding:36px 24px}.issue-stamp{max-width:300px}main{padding:0 24px 34px}.scope{grid-template-columns:1fr}.scope-chips,.metrics{grid-template-columns:1fr 1fr}.scope-chips span:nth-child(3){border-left:0;border-top:2px solid var(--rule)}.metric:nth-child(3){border-left:0;border-top:2px solid var(--rule)}.metric:nth-child(4){border-top:2px solid var(--rule)}.section-head{grid-template-columns:1fr}.coverage-grid{grid-template-columns:1fr}footer{grid-template-columns:1fr}.generated{text-align:left}}
@media (max-width:540px){h1{font-size:68px}.scope-chips,.metrics{grid-template-columns:1fr}.scope-chips span+span,.metric+ .metric{border-left:0;border-top:2px solid var(--rule)}.metric:nth-child(3){border-top:2px solid var(--rule)}}
@media (prefers-reduced-motion:no-preference){.metric{transition:transform .18s ease,background-color .18s ease}.metric:hover{transform:translateY(-3px);background:rgba(240,189,45,.12)}.issue-stamp{transition:transform .2s ease}.issue-stamp:hover{transform:rotate(0deg) translateY(-2px)}}
@media print{@page{size:landscape;margin:10mm}html,body{background:#fff}body{padding:0}.sheet{max-width:none;border:1px solid #000;box-shadow:none;background:#fff}.hazard{background:#ddd}.masthead{padding:24px 30px}.masthead h1{font-size:76px}.lede{font-size:14px;margin-top:20px}main{padding:0 30px 24px}.metrics{margin:22px 0 30px}.metric{padding:14px}.metric strong{font-size:25px}.section-head{margin-top:10px}.coverage{break-before:page;margin-top:20px}details:not([open]){display:none}footer{padding:18px 30px;background:#fff;color:#000;border-top:2px solid #000}.privacy-mark,.generated{color:#000}}
</style>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_blocks_markup_and_formats_counts() {
        assert_eq!(
            escape_html("<script a='b'>&\""),
            "&lt;script a=&#39;b&#39;&gt;&amp;&quot;"
        );
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(12_345_678), "12,345,678");
        assert_eq!(
            html_measure_amount(MeasureStatus::Bounded, Some("60"), Some("66"), "$"),
            "$60–$66"
        );
        assert_eq!(
            html_measure_amount(MeasureStatus::Unavailable, None, None, ""),
            "N/A"
        );
    }
}

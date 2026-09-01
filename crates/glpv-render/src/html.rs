//! Self-contained interactive HTML viewer: the UI source from `ui/` inlined
//! with the graph JSON. The result is one file that works from `file://`,
//! a web server, or published as an artifact page.

use glpv_core::model::Graph;

const APP_CSS: &str = include_str!("../../../ui/app.css");
/// The evaluator mirror is embedded ahead of the app in one script scope
/// (no bundler): the same file is what `ui/test/parity.test.mjs` loads.
const EVAL_JS: &str = include_str!("../../../ui/eval.js");
const APP_JS: &str = include_str!("../../../ui/app.js");
/// Base64 of the canonical Rust evaluator compiled to wasm (see
/// `scripts/build-wasm.sh`). Empty = viewer uses its JS fallback evaluator.
const EVAL_WASM_B64: &str = include_str!("../../../ui/eval-wasm.b64");

pub fn render_html(graph: &Graph) -> String {
    let title = graph
        .pipelines
        .first()
        .map(|p| format!("{} pipeline map", p.project.path))
        .unwrap_or_else(|| "glpv pipeline map".to_string());
    let json = serde_json::to_string(graph)
        .expect("graph serializes")
        // Keep the JSON island safe inside <script>: neutralise `</` (closes
        // the tag) and `<!--` (opens an HTML comment) using escapes that are
        // themselves valid JSON, so the island still parses.
        .replace("</", "<\\/")
        .replace("<!--", "<\\u0021--");

    let wasm = EVAL_WASM_B64.trim();
    let wasm_island = if wasm.is_empty() {
        String::new()
    } else {
        format!("<script type=\"application/wasm;base64\" id=\"glpv-eval-wasm\">{wasm}</script>\n")
    };
    format!(
        "<title>{title}</title>\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <style>\n{APP_CSS}\n</style>\n\
         <div id=\"app\"></div>\n\
         <script type=\"application/json\" id=\"glpv-graph\">{json}</script>\n\
         {wasm_island}\
         <script>\n{EVAL_JS}\n{APP_JS}\n</script>\n",
        title = html_escape(&title),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_graph() -> Graph {
        serde_json::from_str(
            r#"{"schema_version":1,"generated_at":"","tool":{"name":"glpv","version":"0","args":[]},
                "scenarios":[],"pipelines":[],"trigger_edges":[],"include_files":[],
                "include_edges":[],"diagnostics":[],"sources":[]}"#,
        )
        .unwrap()
    }

    // The evaluator mirror and the app share one <script>, and the wasm
    // island is present whenever a build is embedded.
    #[test]
    fn render_html_embeds_scripts_and_wasm_island() {
        let html = render_html(&empty_graph());
        assert_eq!(
            html.matches("<script>").count(),
            1,
            "one plain script element"
        );
        let eval_at = html.find("function evalIf(").expect("eval.js embedded");
        let app_at = html.find("function buildScene(").expect("app.js embedded");
        assert!(eval_at < app_at, "eval.js precedes app.js");
        assert!(html.contains(r#"<script type="application/json" id="glpv-graph">"#));
        assert_eq!(
            html.contains(r#"id="glpv-eval-wasm""#),
            !EVAL_WASM_B64.trim().is_empty()
        );
        assert!(html.starts_with("<title>glpv pipeline map</title>"));
    }

    // The script-island escapes must stay valid JSON: gitlab-org/gitlab's CI
    // scripts contain literal `<!-- ... -->` and `</script>` in job commands.
    #[test]
    fn script_island_escapes_stay_valid_json() {
        let hostile = "check <!-- categories --> and </script> markers";
        let json = serde_json::to_string(&serde_json::json!({ "s": hostile }))
            .unwrap()
            .replace("</", "<\\/")
            .replace("<!--", "<\\u0021--");
        assert!(!json.contains("</"));
        assert!(!json.contains("<!--"));
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back["s"], hostile);
    }
}

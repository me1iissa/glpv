//! Self-contained interactive HTML viewer: the UI source from `ui/` inlined
//! with the graph JSON. The result is one file that works from `file://`,
//! a web server, or published as an artifact page.

use glpv_core::model::Graph;

const APP_CSS: &str = include_str!("../../../ui/app.css");
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
         <script>\n{APP_JS}\n</script>\n",
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

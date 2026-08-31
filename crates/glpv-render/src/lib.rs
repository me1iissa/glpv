//! Graph renderers. JSON is canonical; DOT and Mermaid are static exports;
//! HTML is the interactive viewer (rules simulation included).

mod common;
mod dot;
mod html;
mod mermaid;

pub use dot::render_dot;
pub use html::render_html;
pub use mermaid::render_mermaid;

use glpv_core::model::Graph;

pub fn render_json(graph: &Graph) -> String {
    let mut s = serde_json::to_string_pretty(graph).expect("graph serializes");
    s.push('\n');
    s
}

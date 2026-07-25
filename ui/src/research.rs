use crate::bindings::invoke;
use crate::dto::{ResearchEdge, ResearchGraph, ResearchNode};
use crate::i18n::{t, Locale};
use leptos::*;
use std::collections::HashMap;
use wasm_bindgen::JsValue;

/// Node kinds in display order, paired with their i18n key. Closed set — it
/// mirrors `wisp_store::ResearchNodeKind`, so every stored node lands in a
/// section here.
const KINDS: [(&str, &str); 5] = [
    ("decision", "graph.kind.decision"),
    ("paper", "graph.kind.paper"),
    ("data_asset", "graph.kind.data_asset"),
    ("run", "graph.kind.run"),
    ("artifact", "graph.kind.artifact"),
];

/// A node plus the outgoing links rendered under it, as `(relation, target label)`.
pub(super) struct GraphRow {
    pub(super) node: ResearchNode,
    pub(super) links: Vec<(String, String)>,
}

/// Bucket nodes by kind and hang each node's outgoing edges off it.
///
/// Both endpoints are guaranteed to name a node in the same project — the store
/// counts them before inserting an edge (`Store::save_research_edge`) and every
/// delete path drops edges before their nodes — so no edge goes unrendered.
pub(super) fn group_graph(graph: &ResearchGraph) -> Vec<(&'static str, Vec<GraphRow>)> {
    let titles: HashMap<&str, &str> = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.title.as_str()))
        .collect();
    let label = |id: &str| {
        titles
            .get(id)
            .map(|t| (*t).to_string())
            .unwrap_or_else(|| id.to_string())
    };

    let mut outgoing: HashMap<&str, Vec<&ResearchEdge>> = HashMap::new();
    for edge in &graph.edges {
        outgoing
            .entry(edge.source_id.as_str())
            .or_default()
            .push(edge);
    }

    KINDS
        .into_iter()
        .filter_map(|(kind, label_key)| {
            let rows = graph
                .nodes
                .iter()
                .filter(|node| node.kind == kind)
                .map(|node| GraphRow {
                    node: node.clone(),
                    links: outgoing
                        .get(node.id.as_str())
                        .map(|edges| {
                            edges
                                .iter()
                                .map(|edge| (edge.relation.clone(), label(&edge.target_id)))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect::<Vec<_>>();
            (!rows.is_empty()).then_some((label_key, rows))
        })
        .collect()
}

/// Read-only view of the project's research graph — the decisions, papers and
/// data assets the agent records through the `research_graph` tool.
///
/// ponytail: a grouped list, not a canvas. Printing each node's outgoing edges
/// inline carries the structure without a layout engine. Reach for a real graph
/// renderer when a project outgrows a scrollable list.
#[component]
pub(super) fn ResearchGraphPane(locale: Locale, graph: ResearchGraph) -> impl IntoView {
    if graph.nodes.is_empty() && graph.edges.is_empty() {
        return view! {
            <div class="rp-empty">
                <span class="rp-empty-icon"></span>
                <div class="rp-empty-title">{t(locale, "graph.empty.title")}</div>
                <p>{t(locale, "graph.empty.body")}</p>
            </div>
        }
        .into_view();
    }
    let sections = group_graph(&graph);
    view! {
        <div class="rp-graph">
            <div class="context-list-pane">
                {sections.into_iter().map(|(label_key, rows)| view! {
                    <section class="control-section">
                        <div class="control-section-head">
                            <span>{t(locale, label_key)}</span>
                            <span class="control-count">{rows.len().to_string()}</span>
                        </div>
                        {rows.into_iter().map(|row| view! {
                            <article class="context-card graph-node">
                                <div class="graph-node-title">{row.node.title}</div>
                                {row.node.ref_id
                                    .filter(|id| !id.trim().is_empty())
                                    .map(|id| view! { <div class="graph-node-ref">{id}</div> })}
                                {(!row.links.is_empty()).then(|| view! {
                                    <ul class="graph-node-links">
                                        {row.links.into_iter().map(|(relation, target)| view! {
                                            <li>
                                                <span class="graph-rel">{relation}</span>
                                                <span class="graph-target">{target}</span>
                                            </li>
                                        }).collect_view()}
                                    </ul>
                                })}
                            </article>
                        }).collect_view()}
                    </section>
                }).collect_view()}
            </div>
        </div>
    }
    .into_view()
}

pub(super) fn refresh_research_graph(graph: RwSignal<ResearchGraph>) {
    spawn_local(async move {
        let value = invoke("get_research_graph", JsValue::UNDEFINED).await;
        if let Ok(next) = serde_wasm_bindgen::from_value::<ResearchGraph>(value) {
            graph.set(next);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, kind: &str, title: &str) -> ResearchNode {
        ResearchNode {
            id: id.into(),
            kind: kind.into(),
            title: title.into(),
            ref_id: None,
        }
    }

    fn edge(source: &str, target: &str, relation: &str) -> ResearchEdge {
        ResearchEdge {
            source_id: source.into(),
            target_id: target.into(),
            relation: relation.into(),
        }
    }

    #[test]
    fn groups_by_kind_and_resolves_link_targets() {
        let graph = ResearchGraph {
            nodes: vec![
                node("d1", "decision", "Use DESeq2"),
                node("a1", "data_asset", "counts.tsv"),
            ],
            edges: vec![edge("d1", "a1", "uses")],
        };
        let sections = group_graph(&graph);
        // Decision sorts before data_asset, matching KINDS order.
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "graph.kind.decision");
        assert_eq!(sections[1].0, "graph.kind.data_asset");
        assert_eq!(
            sections[0].1[0].links,
            vec![("uses".to_string(), "counts.tsv".to_string())]
        );
        // The link hangs off the source only; the target repeats nothing.
        assert!(sections[1].1[0].links.is_empty());
    }
}

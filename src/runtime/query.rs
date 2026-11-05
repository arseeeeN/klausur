//! Querying documents: CSS selectors over HTML and XML, and JSONPath over any value.

use super::Failure;
use super::value::{from_json, map_of, to_json, truncate};
use rhai::{Array, Dynamic, Engine, EvalAltResult, Map};
use scraper::selector::{CssLocalName, CssString, NonTSPseudoClass, PseudoElement, Simple};
use scraper::{ElementRef, Html};
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::BloomFilter;
use selectors::matching::{
    ElementSelectorFlags, MatchingContext, MatchingForInvalidation, MatchingMode,
    NeedsSelectorFlags, QuirksMode, SelectorCaches,
};
use selectors::parser::{ParseRelative, SelectorImpl, SelectorList};
use selectors::{Element, OpaqueElement};

type R<T> = Result<T, Box<EvalAltResult>>;

pub fn register(engine: &mut Engine) {
    engine.register_fn("html_select", |html: &str, css: &str| {
        html_select(html, css)
    });
    engine.register_fn("html_text", |html: &str| {
        let doc = Html::parse_document(html);
        collapse(&html_text(doc.root_element()))
    });
    engine.register_fn("xml_select", |xml: &str, css: &str| xml_select(xml, css));
    engine.register_fn("json_ld", |html: &str| json_ld(html));
    engine.register_fn("json_path", |value: Dynamic, path: &str| {
        json_path(&value, path)
    });
}

fn fail(message: String) -> Box<EvalAltResult> {
    Failure::new(message).into_err()
}

fn parse_selector(css: &str) -> R<SelectorList<Simple>> {
    let mut input = cssparser::ParserInput::new(css);
    let mut parser = cssparser::Parser::new(&mut input);
    SelectorList::parse(&scraper::selector::Parser, &mut parser, ParseRelative::No).map_err(|e| {
        let kind = scraper::error::SelectorErrorKind::from(e);
        fail(format!("`{css}` isn't a valid CSS selector: {kind}"))
    })
}

fn matches<E: Element<Impl = Simple>>(list: &SelectorList<Simple>, element: &E) -> bool {
    let mut caches = SelectorCaches::default();
    let mut context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        QuirksMode::NoQuirks,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    list.slice()
        .iter()
        .any(|s| selectors::matching::matches_selector(s, 0, None, element, &mut context))
}

/// Collapse runs of whitespace into single spaces and trim.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// --- HTML ----------------------------------------------------------------------------------

/// Elements whose content isn't visible text.
const HIDDEN: &[&str] = &["script", "style", "noscript", "template", "head"];

/// Elements that start a new line of text, so `<p>a</p><p>b</p>` reads "a b", not "ab".
const BLOCKS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "td",
    "th",
    "title",
    "tr",
    "ul",
];

fn html_text(element: ElementRef) -> String {
    let mut out = String::new();
    push_html_text(element, &mut out);
    out
}

fn push_html_text(element: ElementRef, out: &mut String) {
    for child in element.children() {
        if let Some(text) = child.value().as_text() {
            out.push_str(text);
        } else if let Some(el) = ElementRef::wrap(child) {
            let name = el.value().name();
            if HIDDEN.contains(&name) {
                continue;
            }
            let block = BLOCKS.contains(&name);
            if block {
                out.push(' ');
            }
            push_html_text(el, out);
            if block {
                out.push(' ');
            }
        }
    }
}

fn html_select(html: &str, css: &str) -> R<Array> {
    let list = parse_selector(css)?;
    let doc = Html::parse_document(html);
    Ok(doc
        .root_element()
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|el| matches(&list, el))
        .map(|el| {
            let attrs: Map = el
                .value()
                .attrs()
                .map(|(k, v)| (k.into(), v.to_string().into()))
                .collect();
            map_of([
                ("tag", el.value().name().to_string().into()),
                ("text", collapse(&html_text(el)).into()),
                ("attrs", attrs.into()),
                // `html` is the outer HTML; it came first, so it stays as an alias.
                ("html", el.html().into()),
                ("outer_html", el.html().into()),
                ("inner_html", el.inner_html().into()),
            ])
            .into()
        })
        .collect())
}

// --- XML -----------------------------------------------------------------------------------

/// A roxmltree element that `selectors` can match. Namespaces are ignored: `item` matches
/// `<atom:item>`, and attributes are matched by their local name.
#[derive(Clone, Copy)]
struct XmlEl<'a, 'input> {
    node: roxmltree::Node<'a, 'input>,
    /// One byte per node, so every node has a distinct address for [`OpaqueElement`].
    ids: &'a [u8],
}

impl std::fmt::Debug for XmlEl<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{}>", self.node.tag_name().name())
    }
}

impl<'a, 'input> XmlEl<'a, 'input> {
    fn wrap(&self, node: Option<roxmltree::Node<'a, 'input>>) -> Option<Self> {
        node.map(|node| XmlEl {
            node,
            ids: self.ids,
        })
    }
}

impl Element for XmlEl<'_, '_> {
    type Impl = Simple;

    fn opaque(&self) -> OpaqueElement {
        OpaqueElement::new(&self.ids[self.node.id().get_usize()])
    }
    fn parent_element(&self) -> Option<Self> {
        self.wrap(self.node.parent_element())
    }
    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }
    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }
    fn is_pseudo_element(&self) -> bool {
        false
    }
    fn prev_sibling_element(&self) -> Option<Self> {
        self.wrap(self.node.prev_sibling_element())
    }
    fn next_sibling_element(&self) -> Option<Self> {
        self.wrap(self.node.next_sibling_element())
    }
    fn first_element_child(&self) -> Option<Self> {
        self.wrap(self.node.first_element_child())
    }
    fn is_html_element_in_html_document(&self) -> bool {
        false
    }
    fn has_local_name(&self, name: &CssLocalName) -> bool {
        self.node.tag_name().name() == &*name.0
    }
    fn has_namespace(&self, _: &<Simple as SelectorImpl>::BorrowedNamespaceUrl) -> bool {
        true
    }
    fn is_same_type(&self, other: &Self) -> bool {
        self.node.tag_name() == other.node.tag_name()
    }
    fn attr_matches(
        &self,
        _: &NamespaceConstraint<&<Simple as SelectorImpl>::NamespaceUrl>,
        name: &CssLocalName,
        operation: &AttrSelectorOperation<&CssString>,
    ) -> bool {
        self.node
            .attributes()
            .any(|a| a.name() == &*name.0 && operation.eval_str(a.value()))
    }
    fn match_non_ts_pseudo_class(
        &self,
        _: &NonTSPseudoClass,
        _: &mut MatchingContext<Simple>,
    ) -> bool {
        false
    }
    fn match_pseudo_element(&self, _: &PseudoElement, _: &mut MatchingContext<Simple>) -> bool {
        false
    }
    fn apply_selector_flags(&self, _: ElementSelectorFlags) {}
    fn is_link(&self) -> bool {
        false
    }
    fn is_html_slot_element(&self) -> bool {
        false
    }
    fn has_id(&self, id: &CssLocalName, case: CaseSensitivity) -> bool {
        self.node
            .attribute("id")
            .is_some_and(|v| case.eq(v.as_bytes(), id.0.as_bytes()))
    }
    fn has_class(&self, name: &CssLocalName, case: CaseSensitivity) -> bool {
        self.node.attribute("class").is_some_and(|v| {
            v.split_whitespace()
                .any(|c| case.eq(c.as_bytes(), name.0.as_bytes()))
        })
    }
    fn has_custom_state(&self, _: &CssLocalName) -> bool {
        false
    }
    fn imported_part(&self, _: &CssLocalName) -> Option<CssLocalName> {
        None
    }
    fn is_part(&self, _: &CssLocalName) -> bool {
        false
    }
    fn is_empty(&self) -> bool {
        !self
            .node
            .children()
            .any(|c| c.is_element() || c.text().is_some_and(|t| !t.is_empty()))
    }
    fn is_root(&self) -> bool {
        self.node.parent().is_some_and(|p| p.is_root())
    }
    fn add_element_unique_hashes(&self, _: &mut BloomFilter) -> bool {
        false
    }
}

fn xml_select(xml: &str, css: &str) -> R<Array> {
    let list = parse_selector(css)?;
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(xml, options).map_err(|e| {
        fail(format!(
            "not valid XML: {e}\n  in: {}",
            truncate(xml.trim(), 120)
        ))
    })?;
    let ids = vec![0u8; doc.descendants().count() + 1];
    Ok(doc
        .descendants()
        .filter(|n| n.is_element())
        .map(|node| XmlEl { node, ids: &ids })
        .filter(|el| matches(&list, el))
        .map(|el| {
            let node = el.node;
            let attrs: Map = node
                .attributes()
                .map(|a| (a.name().into(), a.value().to_string().into()))
                .collect();
            let text: String = node
                .descendants()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect();
            map_of([
                ("tag", node.tag_name().name().to_string().into()),
                ("text", collapse(&text).into()),
                ("attrs", attrs.into()),
                ("xml", xml[node.range()].to_string().into()),
                ("outer_xml", xml[node.range()].to_string().into()),
                ("inner_xml", inner_xml(xml, node).into()),
            ])
            .into()
        })
        .collect())
}

/// The source between an element's start and end tags.
fn inner_xml(xml: &str, node: roxmltree::Node) -> String {
    match (node.first_child(), node.last_child()) {
        (Some(first), Some(last)) => xml[first.range().start..last.range().end].to_string(),
        _ => String::new(),
    }
}

/// The JSON-LD of a page: every `<script type="application/ld+json">`, parsed. A script that
/// holds an array contributes its items.
fn json_ld(html: &str) -> R<Array> {
    let list = parse_selector("script[type='application/ld+json']")?;
    let doc = Html::parse_document(html);
    let mut out = Array::new();
    for (i, el) in doc
        .root_element()
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|el| matches(&list, el))
        .enumerate()
    {
        // The raw text: `text` in `html_select` collapses whitespace, which changes strings.
        let raw: String = el.text().collect();
        let value: serde_json::Value = serde_json::from_str(raw.trim()).map_err(|e| {
            fail(format!(
                "JSON-LD script {} isn't valid JSON: {e}\n  in: {}",
                i + 1,
                truncate(raw.trim(), 120)
            ))
        })?;
        match value {
            serde_json::Value::Array(items) => out.extend(items.iter().map(from_json)),
            v => out.push(from_json(&v)),
        }
    }
    Ok(out)
}

// --- JSONPath ------------------------------------------------------------------------------

/// Every value `path` selects in `value` (RFC 9535). The leading `$` may be left out:
/// `"..url"` and `"items[0]"` work too.
fn json_path(value: &Dynamic, path: &str) -> R<Array> {
    let full = if path.starts_with('$') {
        path.to_string()
    } else if path.starts_with('.') || path.starts_with('[') {
        format!("${path}")
    } else {
        format!("$.{path}")
    };
    let query = serde_json_path::JsonPath::parse(&full)
        .map_err(|e| fail(format!("`{path}` isn't a valid JSONPath: {e}")))?;
    let json = to_json(value).map_err(fail)?;
    Ok(query
        .query(&json)
        .all()
        .into_iter()
        .map(from_json)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(found: &Array, field: &str) -> Vec<String> {
        found
            .iter()
            .map(|m| m.clone().cast::<Map>()[field].to_string())
            .collect()
    }

    #[test]
    fn html_selectors_and_text() {
        let html = r#"<html><head><title>T</title><style>p{}</style></head><body>
            <main><p class="a">One <b>bold</b></p><p id="x">Two</p></main>
            <script>var hidden = 1</script><a href="/next">next</a></body></html>"#;
        let found = html_select(html, "main p").unwrap();
        assert_eq!(tags(&found, "text"), ["One bold", "Two"]);
        assert_eq!(tags(&html_select(html, "p.a b").unwrap(), "tag"), ["b"]);
        let link = html_select(html, "a[href^='/']").unwrap()[0]
            .clone()
            .cast::<Map>();
        assert_eq!(
            link["attrs"].clone().cast::<Map>()["href"].to_string(),
            "/next"
        );
        let doc = Html::parse_document(html);
        assert_eq!(
            collapse(&html_text(doc.root_element())),
            "One bold Two next"
        );
        assert!(html_select(html, "p[").is_err());
    }

    #[test]
    fn xml_selectors_ignore_namespaces() {
        let xml = r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom" xmlns:x="urn:x">
              <entry id="1"><title>First</title><link href="/1"/></entry>
              <entry id="2"><x:title>Second</x:title><x:link x:href="/2"/></entry>
            </feed>"#;
        assert_eq!(
            tags(&xml_select(xml, "entry > title").unwrap(), "text"),
            ["First", "Second"]
        );
        assert_eq!(
            tags(&xml_select(xml, "link[href='/2']").unwrap(), "tag"),
            ["link"]
        );
        assert_eq!(
            tags(&xml_select(xml, "[id='2'] title").unwrap(), "xml"),
            ["<x:title>Second</x:title>"]
        );
        assert_eq!(xml_select(xml, "entry:nth-child(2)").unwrap().len(), 1);
        assert_eq!(xml_select(xml, "link:empty").unwrap().len(), 2);
        assert!(xml_select("<a>", "a").is_err());
    }

    #[test]
    fn inner_and_outer_markup() {
        let html = r#"<div id="d"><p>One <b>bold</b></p></div>"#;
        let d = html_select(html, "#d").unwrap();
        assert_eq!(tags(&d, "inner_html"), ["<p>One <b>bold</b></p>"]);
        assert_eq!(
            tags(&d, "outer_html"),
            [r#"<div id="d"><p>One <b>bold</b></p></div>"#]
        );
        assert_eq!(tags(&d, "html"), tags(&d, "outer_html"));
        let xml = "<a><b x='1'>t<c/></b><e/></a>";
        let b = xml_select(xml, "b").unwrap();
        assert_eq!(tags(&b, "inner_xml"), ["t<c/>"]);
        assert_eq!(tags(&b, "outer_xml"), ["<b x='1'>t<c/></b>"]);
        assert_eq!(tags(&xml_select(xml, "e").unwrap(), "inner_xml"), [""]);
    }

    #[test]
    fn json_ld_parses_every_script() {
        let html = r#"<html><head>
            <script type="application/ld+json">{"@type": "Organization", "name": "A  B"}</script>
            <script type="application/ld+json">[{"@type": "Product"}, {"@type": "Offer"}]</script>
            <script>var x = 1</script></head></html>"#;
        let found = json_ld(html).unwrap();
        let types: Vec<String> = found
            .iter()
            .map(|v| v.clone().cast::<Map>()["@type"].to_string())
            .collect();
        assert_eq!(types, ["Organization", "Product", "Offer"]);
        // Whitespace inside strings is kept, unlike `html_select(…).text`.
        assert_eq!(found[0].clone().cast::<Map>()["name"].to_string(), "A  B");
        let broken = r#"<script type="application/ld+json">{"a": }</script>"#;
        let e = match *json_ld(broken).unwrap_err() {
            EvalAltResult::ErrorRuntime(v, _) => v.cast::<Failure>().message,
            other => other.to_string(),
        };
        assert!(e.contains("JSON-LD script 1 isn't valid JSON"), "{e}");
    }

    #[test]
    fn json_paths() {
        let json: serde_json::Value = serde_json::json!({
            "a": {"url": "x"}, "b": [{"url": "y"}, {"url": "z", "n": 2}]
        });
        let v = from_json(&json);
        let urls = |p| tags_of(json_path(&v, p).unwrap());
        fn tags_of(a: Array) -> Vec<String> {
            a.iter().map(|d| d.to_string()).collect()
        }
        assert_eq!(urls("$..url"), ["x", "y", "z"]);
        assert_eq!(urls("..url"), ["x", "y", "z"]);
        assert_eq!(urls("b[?@.n > 1].url"), ["z"]);
        assert_eq!(urls("a.url"), ["x"]);
        assert!(json_path(&v, "$[").is_err());
    }
}

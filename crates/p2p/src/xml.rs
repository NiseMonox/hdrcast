//! Just enough XML for what routers send (device descriptions, SOAP answers and faults): elements and text.
//!
//! The text comes from any device on the LAN that answers a search, so the reader does nothing that could be
//! turned against this PC: a document type declaration is skipped whole, never processed, which leaves no
//! entity but the five predefined ones and the numeric ones (anything else is an error, as it was with .NET's
//! `DtdProcessing.Ignore`); nothing is ever fetched; nesting is limited.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XmlError(pub &'static str);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Element {
    /// As written, with its prefix ("s:Body").
    pub name: String,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Element(Element),
    Text(String),
}

const MAX_DEPTH: usize = 128;

impl Element {
    /// The name without its namespace prefix.
    pub fn local_name(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }

    /// All text inside, in document order (`XmlNode.InnerText`).
    pub fn inner_text(&self) -> String {
        let mut text = String::new();
        self.collect_text(&mut text);
        text
    }

    fn collect_text(&self, into: &mut String) {
        for child in &self.children {
            match child {
                Node::Text(t) => into.push_str(t),
                Node::Element(e) => e.collect_text(into),
            }
        }
    }

    pub fn child_elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|node| match node {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    /// The text of the first direct child element with this local name.
    pub fn child_text(&self, local_name: &str) -> Option<String> {
        self.child_elements().find(|e| e.local_name() == local_name).map(Element::inner_text)
    }

    /// This element and everything below it with this local name, in document order
    /// (`//*[local-name()='…']`).
    pub fn descendants<'a>(&'a self, local_name: &'a str) -> Vec<&'a Element> {
        let mut found = Vec::new();
        self.collect(local_name, &mut found);
        found
    }

    fn collect<'a>(&'a self, local_name: &str, into: &mut Vec<&'a Element>) {
        if self.local_name() == local_name {
            into.push(self);
        }
        for child in self.child_elements() {
            child.collect(local_name, into);
        }
    }

    pub fn first(&self, local_name: &str) -> Option<&Element> {
        if self.local_name() == local_name {
            return Some(self);
        }
        self.child_elements().find_map(|child| child.first(local_name))
    }

    /// The text of the first element with this local name anywhere in the document.
    pub fn first_text(&self, local_name: &str) -> Option<String> {
        self.first(local_name).map(Element::inner_text)
    }
}

/// The document's root element.
pub(crate) fn parse(xml: &str) -> Result<Element, XmlError> {
    let mut reader = Reader { text: xml.strip_prefix('\u{feff}').unwrap_or(xml), at: 0 };
    let mut open: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;

    while reader.at < reader.text.len() {
        if !reader.rest().starts_with('<') {
            let raw = reader.take_until("<").unwrap_or_else(|| reader.take_rest());
            match open.last_mut() {
                Some(parent) => {
                    let text = decode_text(raw)?;
                    // whitespace between elements is formatting, not content
                    if !text.trim().is_empty() {
                        parent.children.push(Node::Text(text));
                    }
                }
                None if raw.trim().is_empty() => {}
                None => return Err(XmlError("text outside the root element")),
            }
            continue;
        }

        if reader.skip("<?") {
            reader.take_until("?>").ok_or(XmlError("unterminated processing instruction"))?;
            reader.skip("?>");
        } else if reader.skip("<!--") {
            reader.take_until("-->").ok_or(XmlError("unterminated comment"))?;
            reader.skip("-->");
        } else if reader.skip("<![CDATA[") {
            let text = reader.take_until("]]>").ok_or(XmlError("unterminated CDATA section"))?;
            reader.skip("]]>");
            let parent = open.last_mut().ok_or(XmlError("text outside the root element"))?;
            if !text.trim().is_empty() {
                parent.children.push(Node::Text(text.to_string()));
            }
        } else if reader.rest().starts_with("<!DOCTYPE") {
            if root.is_some() || !open.is_empty() {
                return Err(XmlError("a document type declaration after the root element started"));
            }
            reader.skip_doctype()?;
        } else if reader.skip("</") {
            let name = reader.take_until(">").ok_or(XmlError("unterminated end tag"))?.trim_end();
            reader.skip(">");
            let element = open.pop().ok_or(XmlError("an end tag without a start tag"))?;
            if element.name != name {
                return Err(XmlError("an end tag that does not match its start tag"));
            }
            match open.last_mut() {
                Some(parent) => parent.children.push(Node::Element(element)),
                None => root = Some(element),
            }
        } else {
            reader.skip("<");
            if root.is_some() {
                return Err(XmlError("more than one root element"));
            }
            let (name, self_closing) = reader.start_tag()?;
            let element = Element { name, children: Vec::new() };
            if self_closing {
                match open.last_mut() {
                    Some(parent) => parent.children.push(Node::Element(element)),
                    None => root = Some(element),
                }
            } else {
                if open.len() >= MAX_DEPTH {
                    return Err(XmlError("elements nested too deeply"));
                }
                open.push(element);
            }
        }
    }

    if !open.is_empty() {
        return Err(XmlError("an element that is never closed"));
    }
    root.ok_or(XmlError("no root element"))
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    fn skip(&mut self, prefix: &str) -> bool {
        let found = self.rest().starts_with(prefix);
        if found {
            self.at += prefix.len();
        }
        found
    }

    /// Everything up to (not including) `end`; `None`, consuming nothing, when `end` never comes.
    fn take_until(&mut self, end: &str) -> Option<&'a str> {
        let rest = self.rest();
        let found = rest.find(end)?;
        self.at += found;
        Some(&rest[..found])
    }

    fn take_rest(&mut self) -> &'a str {
        let rest = self.rest();
        self.at = self.text.len();
        rest
    }

    /// After "<": the element's name and whether the tag closes itself. Attributes are checked for form and
    /// otherwise ignored.
    fn start_tag(&mut self) -> Result<(String, bool), XmlError> {
        let rest = self.rest();
        let name_end = rest.find(|c: char| c.is_whitespace() || c == '/' || c == '>').ok_or(XmlError("unterminated start tag"))?;
        let name = &rest[..name_end];
        if name.is_empty() || name.contains(['<', '&', '"', '\'', '=']) {
            return Err(XmlError("a start tag without a proper name"));
        }
        self.at += name_end;
        loop {
            self.at += self.rest().len() - self.rest().trim_start().len();
            if self.skip("/>") {
                return Ok((name.to_string(), true));
            }
            if self.skip(">") {
                return Ok((name.to_string(), false));
            }
            // an attribute: name = "value" or 'value'
            let rest = self.rest();
            let equals = rest.find('=').ok_or(XmlError("unterminated start tag"))?;
            if rest[..equals].trim_end().is_empty() || rest[..equals].contains(['<', '>', '/', '"', '\'']) {
                return Err(XmlError("a malformed attribute"));
            }
            self.at += equals + 1;
            self.at += self.rest().len() - self.rest().trim_start().len();
            let quote = self.rest().chars().next().filter(|c| matches!(c, '"' | '\'')).ok_or(XmlError("an attribute value without quotes"))?;
            self.at += 1;
            let value = self.take_until(&quote.to_string()).ok_or(XmlError("an unterminated attribute value"))?;
            if value.contains('<') {
                return Err(XmlError("'<' in an attribute value"));
            }
            self.at += 1;
        }
    }

    /// Skips "<!DOCTYPE … >" including an internal subset in [ ], reading none of it.
    fn skip_doctype(&mut self) -> Result<(), XmlError> {
        let mut in_subset = false;
        let mut quote: Option<char> = None;
        let rest = self.rest();
        for (offset, c) in rest.char_indices() {
            match (quote, c) {
                (Some(q), _) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '"' | '\'') => quote = Some(c),
                (None, '[') => in_subset = true,
                (None, ']') => in_subset = false,
                (None, '>') if !in_subset => {
                    self.at += offset + 1;
                    return Ok(());
                }
                _ => {}
            }
        }
        Err(XmlError("unterminated document type declaration"))
    }
}

/// Character data with its references resolved: the five predefined entities and numeric references.
fn decode_text(raw: &str) -> Result<String, XmlError> {
    if !raw.contains('&') {
        return Ok(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let end = after.find(';').ok_or(XmlError("'&' that starts no reference"))?;
        let name = &after[..end];
        out.push(match name {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(hex) = name.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(decimal) = name.strip_prefix('#') {
                    decimal.parse::<u32>().ok()
                } else {
                    // a declared entity would need the document type declaration, which is never read
                    return Err(XmlError("a reference to an undeclared entity"));
                };
                code.and_then(char::from_u32).ok_or(XmlError("a malformed character reference"))?
            }
        });
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_text_and_namespaces() {
        let root = parse(
            "<?xml version=\"1.0\"?>\r\n<!-- a comment -->\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:x='1 > 0'>\r\n  \
             <s:Body><u:R xmlns:u=\"x\"><A> 1 &amp; 2 &#65;&#x42;</A><B/><C><![CDATA[<raw>&]]></C><D></D></u:R></s:Body>\r\n</s:Envelope>\r\n<!-- after -->\r\n",
        )
        .unwrap();
        assert_eq!(root.name, "s:Envelope");
        assert_eq!(root.local_name(), "Envelope");
        let r = root.first("R").unwrap();
        assert_eq!(r.name, "u:R");
        let names: Vec<&str> = r.child_elements().map(|e| e.local_name()).collect();
        assert_eq!(names, ["A", "B", "C", "D"]);
        assert_eq!(r.child_text("A").as_deref(), Some(" 1 & 2 AB"));
        assert_eq!(r.child_text("B").as_deref(), Some(""));
        assert_eq!(r.child_text("C").as_deref(), Some("<raw>&"));
        assert_eq!(r.child_text("E"), None);
        assert_eq!(root.first_text("A").as_deref(), Some(" 1 & 2 AB"));
        assert_eq!(root.inner_text(), " 1 & 2 AB<raw>&", "formatting whitespace is not content");
        assert_eq!(root.descendants("Body").len(), 1);
        assert_eq!(root.descendants("nothing").len(), 0);
    }

    #[test]
    fn the_first_match_is_the_first_in_document_order() {
        let root = parse("<r><a><x>deep</x></a><x>shallow</x></r>").unwrap();
        assert_eq!(root.first_text("x").as_deref(), Some("deep"));
        let all: Vec<String> = root.descendants("x").iter().map(|e| e.inner_text()).collect();
        assert_eq!(all, ["deep", "shallow"]);
        assert_eq!(parse("<x>self</x>").unwrap().first_text("x").as_deref(), Some("self"));
    }

    #[test]
    fn a_document_type_declaration_is_skipped_unread() {
        let dtd = "<!DOCTYPE root [<!ENTITY x SYSTEM \"file:///c:/windows/win.ini\"><!ENTITY y \"]>\"> <!-- > -->]>";
        assert_eq!(parse(&format!("<?xml version=\"1.0\"?>{dtd}<root><a>ok</a></root>")).unwrap().first_text("a").as_deref(), Some("ok"));
        assert_eq!(parse(&format!("{dtd}<root>&x;</root>")), Err(XmlError("a reference to an undeclared entity")));
        assert_eq!(parse(&format!("{dtd}<root>&y;</root>")), Err(XmlError("a reference to an undeclared entity")));
        assert_eq!(parse("<!DOCTYPE root SYSTEM \"http://example.net/x.dtd\"><root/>").unwrap().name, "root");
        assert!(parse("<!DOCTYPE root [<root/>").is_err());
        assert!(parse("<root><!DOCTYPE root></root>").is_err());
    }

    #[test]
    fn what_is_not_well_formed_is_an_error_not_a_guess() {
        for bad in [
            "",
            "   ",
            "not xml",
            "<a>",
            "<a></b>",
            "</a>",
            "<a/><b/>",
            "<a/>trailing",
            "<a><b></a></b>",
            "<a b=c/>",
            "<a b=\"c/>",
            "<a b/>",
            "<a>&nbsp;</a>",
            "<a>&</a>",
            "<a>& b;</a>",
            "<a>&#xD800;</a>",
            "<a>&#;</a>",
            "<a><!-- never ends</a>",
            "<a><![CDATA[never ends</a>",
            "<?xml never ends",
            "< a/>",
            "<a",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
        let deep = format!("{}{}", "<a>".repeat(MAX_DEPTH + 1), "</a>".repeat(MAX_DEPTH + 1));
        assert_eq!(parse(&deep), Err(XmlError("elements nested too deeply")));
        let fine = format!("{}{}", "<a>".repeat(MAX_DEPTH), "</a>".repeat(MAX_DEPTH));
        assert!(parse(&fine).is_ok());
        assert_eq!(parse("\u{feff}<a/>").unwrap().name, "a", "a byte order mark is not content");
    }
}

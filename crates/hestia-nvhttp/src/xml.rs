//! A small, forgiving XML reader for GameStream responses.
//!
//! Host responses are flat documents such as
//! `<root status_code="200"><hostname>pc</hostname>...</root>`, with the app
//! list as repeated `<App>` children. This parser builds a tiny element tree,
//! decodes the standard entities and skips declarations, comments and
//! processing instructions. It is not a general-purpose XML parser.

use std::fmt;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlError(pub String);

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed XML: {}", self.0)
    }
}

impl std::error::Error for XmlError {}

impl Element {
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Trimmed text of the first child with this name.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }

    /// Depth-first search for the first descendant with this name.
    pub fn find(&self, name: &str) -> Option<&Element> {
        for child in &self.children {
            if child.name == name {
                return Some(child);
            }
            if let Some(found) = child.find(name) {
                return Some(found);
            }
        }
        None
    }
}

/// Parses a document and returns its root element.
pub fn parse(input: &str) -> Result<Element, XmlError> {
    let mut parser = Parser { src: input, pos: 0 };
    parser.skip_misc();
    let root = parser.element()?;
    Ok(root)
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn err<T>(&self, msg: &str) -> Result<T, XmlError> {
        Err(XmlError(format!("{msg} at byte {}", self.pos)))
    }

    fn skip_ws(&mut self) {
        let trimmed = self.rest().trim_start();
        self.pos = self.src.len() - trimmed.len();
    }

    /// Skips whitespace, `<?...?>`, `<!--...-->` and `<!DOCTYPE ...>`.
    fn skip_misc(&mut self) {
        loop {
            self.skip_ws();
            let rest = self.rest();
            let skip_to = if rest.starts_with("<?") {
                rest.find("?>").map(|i| i + 2)
            } else if rest.starts_with("<!--") {
                rest.find("-->").map(|i| i + 3)
            } else if rest.starts_with("<!") && !rest.starts_with("<![CDATA[") {
                rest.find('>').map(|i| i + 1)
            } else {
                None
            };
            match skip_to {
                Some(n) => self.pos += n,
                None => return,
            }
        }
    }

    fn name(&mut self) -> Result<String, XmlError> {
        let rest = self.rest();
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/' || c == '=')
            .unwrap_or(rest.len());
        if end == 0 {
            return self.err("expected a name");
        }
        self.pos += end;
        Ok(rest[..end].to_owned())
    }

    fn element(&mut self) -> Result<Element, XmlError> {
        if !self.rest().starts_with('<') {
            return self.err("expected '<'");
        }
        self.pos += 1;
        let mut el = Element {
            name: self.name()?,
            ..Default::default()
        };

        // Attributes
        loop {
            self.skip_ws();
            let rest = self.rest();
            if rest.starts_with("/>") {
                self.pos += 2;
                return Ok(el);
            }
            if rest.starts_with('>') {
                self.pos += 1;
                break;
            }
            if rest.is_empty() {
                return self.err("unterminated start tag");
            }
            let key = self.name()?;
            self.skip_ws();
            if !self.rest().starts_with('=') {
                // Attribute without a value; keep it as an empty string.
                el.attributes.push((key, String::new()));
                continue;
            }
            self.pos += 1;
            self.skip_ws();
            let quote = match self.rest().chars().next() {
                Some(q @ ('"' | '\'')) => q,
                _ => return self.err("expected a quoted attribute value"),
            };
            self.pos += 1;
            let Some(end) = self.rest().find(quote) else {
                return self.err("unterminated attribute value");
            };
            let value = decode_entities(&self.rest()[..end]);
            self.pos += end + 1;
            el.attributes.push((key, value));
        }

        // Content
        loop {
            let rest = self.rest();
            if rest.is_empty() {
                return self.err(&format!("unterminated element <{}>", el.name));
            }
            if rest.starts_with("</") {
                self.pos += 2;
                let close = self.name()?;
                if close != el.name {
                    return self.err(&format!("mismatched </{close}> for <{}>", el.name));
                }
                self.skip_ws();
                if !self.rest().starts_with('>') {
                    return self.err("expected '>'");
                }
                self.pos += 1;
                return Ok(el);
            }
            if rest.starts_with("<![CDATA[") {
                let Some(end) = rest.find("]]>") else {
                    return self.err("unterminated CDATA");
                };
                el.text.push_str(&rest[9..end]);
                self.pos += end + 3;
                continue;
            }
            if rest.starts_with("<!--") || rest.starts_with("<?") {
                self.skip_misc();
                continue;
            }
            if rest.starts_with('<') {
                let child = self.element()?;
                el.children.push(child);
                continue;
            }
            let end = rest.find('<').unwrap_or(rest.len());
            el.text.push_str(&decode_entities(&rest[..end]));
            self.pos += end;
        }
    }
}

fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';') else {
            break;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serverinfo_like() {
        let doc = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200"><hostname>Desk &amp; Couch</hostname><appversion>7.1.431.-1</appversion>
<SupportedDisplayMode><DisplayMode><Width>1920</Width><Height>1080</Height><RefreshRate>60</RefreshRate></DisplayMode></SupportedDisplayMode>
<empty/><PairStatus>1</PairStatus></root>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.name, "root");
        assert_eq!(root.attribute("status_code"), Some("200"));
        assert_eq!(root.child_text("hostname"), Some("Desk & Couch"));
        assert_eq!(root.child_text("empty"), Some(""));
        assert_eq!(root.find("Width").map(|e| e.text.as_str()), Some("1920"));
        assert_eq!(root.child_text("PairStatus"), Some("1"));
    }

    #[test]
    fn app_list() {
        let doc = "<root status_code=\"200\"><App><IsHdrSupported>0</IsHdrSupported><AppTitle>Desktop</AppTitle><UUID>a-b</UUID><ID>881448767</ID></App><App><AppTitle/><ID>2</ID></App></root>";
        let root = parse(doc).unwrap();
        let apps: Vec<_> = root.children_named("App").collect();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].child_text("AppTitle"), Some("Desktop"));
        assert_eq!(apps[1].child_text("AppTitle"), Some(""));
    }

    #[test]
    fn errors_and_entities() {
        assert!(parse("<root><a></b></root>").is_err());
        assert!(parse("<root>").is_err());
        assert_eq!(decode_entities("&#65;&#x42;&lt;&bogus;"), "AB<&bogus;");
        let root = parse("<root status_message='It&apos;s busy'/>").unwrap();
        assert_eq!(root.attribute("status_message"), Some("It's busy"));
    }
}

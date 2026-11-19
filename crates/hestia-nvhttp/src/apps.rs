//! `/applist`.

use crate::xml::Element;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub id: u32,
    pub title: String,
    /// Apollo/Hermes stable app UUID; empty on plain Sunshine.
    pub uuid: Option<String>,
    pub hdr_supported: bool,
}

pub fn parse_app_list(root: &Element) -> Vec<AppInfo> {
    root.children_named("App")
        .filter_map(|app| {
            let id = app.child_text("ID")?.parse().ok()?;
            Some(AppInfo {
                id,
                title: app.child_text("AppTitle").unwrap_or_default().to_owned(),
                uuid: app.child_text("UUID").filter(|u| !u.is_empty()).map(str::to_owned),
                hdr_supported: app.child_text("IsHdrSupported") == Some("1"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses() {
        let doc = "<root status_code=\"200\"><App><IsHdrSupported>1</IsHdrSupported><AppTitle>Steam Big Picture</AppTitle><UUID>u-1</UUID><IDX>1</IDX><ID>42</ID></App><App><AppTitle>bad</AppTitle></App></root>";
        let apps = super::parse_app_list(&crate::xml::parse(doc).unwrap());
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].id, 42);
        assert!(apps[0].hdr_supported);
        assert_eq!(apps[0].uuid.as_deref(), Some("u-1"));
    }
}

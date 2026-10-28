//! The `filex --pick` protocol, mirrored from `filex::picker` so the DLL
//! stays free of the filex crate's dependencies. Field names must match
//! `filex::picker::{PickRequest, PickResponse}`; the filex crate has a test
//! that parses [`sample_request`]'s output to keep the two in step.

use serde_json::{Value, json};
use std::path::PathBuf;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Open,
    Save,
    Folder,
}

/// Everything the dialog was told before `Show`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub mode: Option<Mode>,
    pub multiple: bool,
    pub title: Option<String>,
    pub ok_label: Option<String>,
    pub folder: Option<PathBuf>,
    pub file_name: Option<String>,
    /// (display name, "*.txt;*.md") pairs, as COMDLG_FILTERSPEC holds them.
    pub filters: Vec<(String, String)>,
    /// Zero-based (COM's file type index is one-based).
    pub filter_index: usize,
    pub default_extension: Option<String>,
    pub confirm_overwrite: bool,
    /// The host's owner window (HWND), if `Show` was given one.
    pub owner: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Picked {
        paths: Vec<PathBuf>,
        filter_index: usize,
    },
    Cancelled,
}

impl Request {
    pub fn to_json(&self) -> String {
        let mode = match self.mode.unwrap_or(Mode::Open) {
            Mode::Open => "open_file",
            Mode::Save => "save_file",
            Mode::Folder => "pick_folder",
        };
        let filters: Vec<Value> = self
            .filters
            .iter()
            .map(|(name, spec)| {
                let patterns: Vec<&str> = spec
                    .split(';')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .collect();
                json!({ "name": name, "patterns": patterns })
            })
            .collect();
        json!({
            "version": PROTOCOL_VERSION,
            "mode": mode,
            "multiple": self.multiple,
            "title": self.title,
            "ok_label": self.ok_label,
            "folder": self.folder,
            "file_name": self.file_name,
            "filters": filters,
            "filter_index": self.filter_index,
            "default_extension": self.default_extension,
            "confirm_overwrite": self.confirm_overwrite,
            "owner": self.owner,
        })
        .to_string()
    }
}

/// Parse filex's stdout. `None` for anything unexpected, which the caller
/// treats as "fall back to the system dialog".
pub fn parse_response(stdout: &str) -> Option<Response> {
    let line = stdout.lines().find(|l| !l.trim().is_empty())?;
    let value: Value = serde_json::from_str(line).ok()?;
    match value.get("result")?.as_str()? {
        "cancelled" => Some(Response::Cancelled),
        "picked" => {
            let paths: Vec<PathBuf> = value
                .get("paths")?
                .as_array()?
                .iter()
                .map(|p| p.as_str().map(PathBuf::from))
                .collect::<Option<_>>()?;
            if paths.is_empty() {
                return None;
            }
            let filter_index = value.get("filter_index")?.as_u64()? as usize;
            Some(Response::Picked {
                paths,
                filter_index,
            })
        }
        _ => None,
    }
}

/// A representative request, for the cross-crate protocol test.
pub fn sample_request() -> String {
    Request {
        mode: Some(Mode::Save),
        multiple: false,
        title: Some("Export".into()),
        ok_label: None,
        folder: Some(PathBuf::from("C:/Users")),
        file_name: Some("report".into()),
        filters: vec![("Text (*.txt;*.md)".into(), "*.txt; *.md".into())],
        filter_index: 0,
        default_extension: Some("txt".into()),
        confirm_overwrite: true,
        owner: Some(0x1234),
    }
    .to_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_specs_split_into_patterns() {
        let json: Value = serde_json::from_str(&sample_request()).unwrap();
        assert_eq!(json["mode"], "save_file");
        assert_eq!(json["filters"][0]["patterns"], json!(["*.txt", "*.md"]));
        assert_eq!(json["version"], PROTOCOL_VERSION);
    }

    #[test]
    fn responses_parse_and_garbage_does_not() {
        assert_eq!(
            parse_response("{\"result\":\"cancelled\"}\n"),
            Some(Response::Cancelled)
        );
        assert_eq!(
            parse_response(r#"{"result":"picked","paths":["C:/a.txt"],"filter_index":2}"#),
            Some(Response::Picked {
                paths: vec![PathBuf::from("C:/a.txt")],
                filter_index: 2
            })
        );
        assert_eq!(parse_response(""), None);
        assert_eq!(parse_response("panicked at ..."), None);
        assert_eq!(
            parse_response(r#"{"result":"picked","paths":[],"filter_index":0}"#),
            None
        );
    }
}

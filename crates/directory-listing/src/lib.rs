//! Transport-independent directory listings from HTML autoindexes and nginx JSON.
#![doc = include_str!("../README.md")]
use percent_encoding::percent_decode_str;
use scraper::{Html, Selector};
use serde::Deserialize;
use std::collections::HashSet;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub url: Url,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// The document explicitly advertises truncation. False is not a guarantee
    /// that the server supplied its entire directory.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("listing URL must be an HTTP directory URL ending in a slash")]
    InvalidBaseUrl,
    #[error("document is not a recognized directory listing")]
    Unrecognized,
    #[error("invalid nginx directory listing: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid directory entry name: {0}")]
    InvalidName(String),
}

fn validate_base(base: &Url) -> Result<(), Error> {
    if matches!(base.scheme(), "http" | "https") && base.path().ends_with('/') {
        Ok(())
    } else {
        Err(Error::InvalidBaseUrl)
    }
}

/// Parse Apache table/pre/list indexes, nginx autoindex/fancyindex, Caddy,
/// lighttpd-style tables, and server-rendered Name/Size/Modified tables.
/// Recognition uses document structure, never server headers or hostnames.
/// JavaScript shells and arbitrary pages are not treated as empty directories.
pub fn parse_html(base: &Url, text: &str) -> Result<Listing, Error> {
    validate_base(base)?;
    let document = Html::parse_document(text);
    let headings = Selector::parse("h1, h2, title").unwrap();
    let indexed = document.select(&headings).any(|node| {
        let text = node.text().collect::<String>().to_ascii_lowercase();
        text.contains("index of ") || text.contains("directory listing")
    });
    let containers = Selector::parse("table, pre, ul").unwrap();
    let headers = Selector::parse("th").unwrap();
    let anchors = Selector::parse("a[href]").unwrap();
    let warnings = Selector::parse(".warning, [role=alert]").unwrap();
    let mut recognized = false;
    let mut seen = HashSet::new();
    let entries = document
        .select(&containers)
        .filter(|node| {
            let table = node.value().name() == "table";
            let heading = node
                .select(&headers)
                .map(|cell| cell.text().collect::<String>().to_ascii_lowercase())
                .collect::<Vec<_>>();
            let column_listing = table
                && heading.iter().any(|s| s.contains("name"))
                && heading
                    .iter()
                    .any(|s| s.contains("size") || s.contains("modified") || s.contains("date"));
            let known_table = table
                && (matches!(
                    node.value().attr("id"),
                    Some("list" | "indexlist" | "file-table")
                ) || node
                    .value()
                    .attr("summary")
                    .is_some_and(|s| s.eq_ignore_ascii_case("Directory Listing")));
            let plain_listing = !table && indexed;
            // Minimal list indexes sometimes have only the directory name as title.
            let parent_list = node.value().name() == "ul"
                && node
                    .select(&anchors)
                    .any(|a| matches!(a.value().attr("href"), Some("../" | "..")));
            let accept = column_listing || known_table || plain_listing || parent_list;
            recognized |= accept;
            accept
        })
        .flat_map(|node| node.select(&anchors))
        .filter_map(|anchor| child(base, anchor.value().attr("href")?))
        .filter(|entry| seen.insert(entry.url.clone()))
        .collect();
    if !recognized {
        return Err(Error::Unrecognized);
    }
    let truncated = document.select(&warnings).any(|node| {
        node.text()
            .collect::<String>()
            .to_ascii_lowercase()
            .contains("truncat")
    });
    Ok(Listing { entries, truncated })
}

fn child(base: &Url, href: &str) -> Option<Entry> {
    let url = base.join(href).ok()?;
    if url.origin() != base.origin() || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let relative = url.path().strip_prefix(base.path())?;
    let kind = if relative.ends_with('/') {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    let segment = relative.strip_suffix('/').unwrap_or(relative);
    if segment.is_empty() || segment.contains('/') {
        return None;
    }
    let name = percent_decode_str(segment).decode_utf8().ok()?.into_owned();
    if !valid_name(&name) {
        return None;
    }
    Some(Entry { name, url, kind })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\', '\0'])
}

/// Parse nginx `autoindex_format json`. Names are literal filenames, not URL
/// escapes. Unknown fields (such as size and mtime) are ignored.
pub fn parse_nginx_json(base: &Url, text: &str) -> Result<Listing, Error> {
    validate_base(base)?;
    #[derive(Deserialize)]
    struct Record {
        name: String,
        #[serde(rename = "type")]
        kind: String,
    }
    let records: Vec<Record> = serde_json::from_str(text)?;
    let mut seen = HashSet::new();
    let entries = records
        .into_iter()
        .map(|record| {
            if !valid_name(&record.name) {
                return Err(Error::InvalidName(record.name));
            }
            let kind = match record.kind.as_str() {
                "file" => EntryKind::File,
                "directory" => EntryKind::Directory,
                "other" => EntryKind::Other,
                _ => return Err(Error::Unrecognized),
            };
            let mut url = base.clone();
            url.set_query(None);
            url.set_fragment(None);
            {
                let mut path = url
                    .path_segments_mut()
                    .map_err(|()| Error::InvalidBaseUrl)?;
                path.pop_if_empty().push(&record.name);
                if kind == EntryKind::Directory {
                    path.push("");
                }
            }
            Ok(Entry {
                name: record.name,
                url,
                kind,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?
        .into_iter()
        .filter(|entry| seen.insert(entry.url.clone()))
        .collect();
    Ok(Listing {
        entries,
        truncated: false,
    })
}

#[cfg(test)]
mod tests;

use super::*;

#[test]
fn observed_html_flavors_share_entry_semantics() {
    let base = Url::parse("https://example.org/repo/archive/").unwrap();
    for html in [
        include_str!("../tests/fixtures/apache-table.html"),
        include_str!("../tests/fixtures/apache-pre.html"),
        include_str!("../tests/fixtures/apache-list.html"),
        include_str!("../tests/fixtures/nginx-pre.html"),
        include_str!("../tests/fixtures/fancyindex.html"),
        include_str!("../tests/fixtures/caddy.html"),
        include_str!("../tests/fixtures/lighttpd.html"),
        include_str!("../tests/fixtures/custom-table.html"),
    ] {
        let listing = parse_html(&base, html).unwrap();
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| (e.name.as_str(), e.kind))
                .collect::<Vec<_>>(),
            [
                ("example_1.0.tar.gz", EntryKind::File),
                ("nested", EntryKind::Directory)
            ]
        );
        assert_eq!(
            listing.entries[0].url.as_str(),
            "https://example.org/repo/archive/example_1.0.tar.gz"
        );
    }
}

#[test]
fn links_not_display_text_define_names_and_scope() {
    let base = Url::parse("https://example.org/repo/archive/").unwrap();
    let listing = parse_html(
        &base,
        r##"<h1>Index of /repo/archive/</h1><pre>
      <a href="../">parent</a><a href="?C=N&amp;O=D">sort</a>
      <a href="a%20b%26c.tar.gz">truncated...</a><a href="./a%20b%26c.tar.gz"><img></a>
      <a href="entity&amp;&#65;.txt">ignored label</a>
      <a href="/repo/archive/nested/">nested</a>
      <a href="https://example.org/repo/archive/full.txt">full</a>
      <a href="https://other.org/repo/archive/bad">outside</a>
      <a href="/repo/archive-other/bad">prefix mismatch</a>
      <a href="nested/deep.txt">not a child</a><a href="%2e%2e/escape">escape</a>
      <a href="a%2Fb">encoded path</a><a href="#top">fragment</a>
      <script>"fake_1.0.tar.gz"</script>plain_1.0.tar.gz
    </pre>"##,
    )
    .unwrap();
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["a b&c.tar.gz", "entity&A.txt", "nested", "full.txt"]
    );
}

#[test]
fn empty_unknown_and_truncated_are_distinct() {
    let base = Url::parse("https://example.org/").unwrap();
    assert!(
        parse_html(
            &base,
            "<h1>Index of /</h1><pre><a href='../'>parent</a></pre>"
        )
        .unwrap()
        .entries
        .is_empty()
    );
    assert!(matches!(
        parse_html(
            &base,
            "<title>Mirror</title><div id='app'></div><script src='/app.js'></script>"
        ),
        Err(Error::Unrecognized)
    ));
    assert!(matches!(
        parse_html(&base, "<a href='foo'>not an index</a>"),
        Err(Error::Unrecognized)
    ));
    assert!(parse_html(&base, "<p class='warning'>Too many items. This list might be truncated.</p><table id='file-table'></table>").unwrap().truncated);
}

#[test]
fn nginx_json_uses_literal_names_and_typed_entries() {
    let base = Url::parse("https://example.org/archive/").unwrap();
    let listing = parse_nginx_json(
        &base,
        r#"[{"name":"a%20&b","type":"file","size":123},{"name":"nested","type":"directory"}]"#,
    )
    .unwrap();
    assert_eq!(
        listing.entries[0].url.as_str(),
        "https://example.org/archive/a%2520&b"
    );
    assert_eq!(listing.entries[1].kind, EntryKind::Directory);
    assert_eq!(
        listing.entries[1].url.as_str(),
        "https://example.org/archive/nested/"
    );
    assert!(parse_nginx_json(&base, r#"[{"name":"../outside","type":"file"}]"#).is_err());
    assert!(parse_nginx_json(&base, "{}").is_err());
    assert!(parse_nginx_json(&base, "[]").unwrap().entries.is_empty());
}

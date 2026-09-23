# directory-listing

Transport-independent HTML/JSON directory listing parser. Internal workspace
crate with `publish = false`; it has no HTTP, runtime, or package-metadata dependency.

```rust
use directory_listing::{parse_html, EntryKind};
let base = "https://example.org/files/".parse().unwrap();
let listing = parse_html(&base,
    "<h1>Index of /files/</h1><pre><a href='file.tar.gz'>file</a></pre>")?;
assert_eq!(listing.entries[0].name, "file.tar.gz");
assert_eq!(listing.entries[0].kind, EntryKind::File);
# Ok::<(), directory_listing::Error>(())
```

`parse_html(base, html)` uses `scraper` and `html5ever` to recognize Apache
table/pre/list indexes, nginx autoindex/fancyindex, Caddy, lighttpd-style tables,
and custom Name/Modified/Size tables. Recognition is structural rather than tied
to hostnames or Server headers. Entries come from actual links inside the listing,
not rendered names or arbitrary text. `parse_nginx_json(base, json)` handles nginx's
documented JSON autoindex schema separately.

Pass the final directory response URL (after redirects), ending in `/`.
Entries contain a decoded name, resolved URL, and file/directory/other kind.
HTML non-directory links are classified as files; that does not establish their
filesystem type. JSON can explicitly identify `other` entries. Only immediate,
same-origin children are included; parent links, query/fragment controls, nested
paths, and unrelated links are excluded. Duplicate URLs are returned once, in
document order. HTML entities and URL escapes are decoded by their respective
parsers; JSON names are literal filenames, not URL-encoded strings.

Unrecognized documents return an error, distinct from a recognized empty listing.
`Listing::truncated` reports an explicit truncation warning; false does not prove
completeness. Sizes and modification times are not normalized: layouts often
provide rounded sizes or timestamps without time-zone information.

## CRAN mirror survey — 2026-09-23

Requested both `src/contrib/Archive/` and `src/contrib/Archive/digest/` at all
96 entries in the official [mirror CSV](https://cran.r-project.org/CRAN_mirrors.csv),
including inactive entries: 192 curl requests. The per-package results were
79 static HTML listings, one nginx JSON listing, one JavaScript application shell,
and 15 HTTP/connection/TLS/timeout failures. Root requests had a 20-second timeout
and 5 MB cap, so some root responses were partial. These observations describe
one network vantage point, not a permanent mirror availability list.

| Observed layout | Representative mirror |
| --- | --- |
| Apache table | cran.r-project.org (via cran.wu.ac.at redirect) |
| Apache pre | cloud.r-project.org |
| Apache simple list | cran.ma.imperial.ac.uk |
| nginx pre | cran.ms.unimelb.edu.au |
| nginx fancyindex | ftp.belnet.be, mirrors.tuna.tsinghua.edu.cn |
| Caddy browser | mirrors.sjtug.sjtu.edu.cn |
| lighttpd-style table | mirrors.cicku.me |
| custom table | mirrors.ustc.edu.cn |
| nginx JSON | mirrors.zju.edu.cn |

USTC's archive root explicitly warned that its 1,000-entry listing could be
truncated. PKU returned HTTP 200 with a JavaScript shell, without directory links.
Neither situation should silently masquerade as a complete empty listing.

The HTML fixtures are reduced, representative structures from this survey, not
verbatim snapshots. Tests also cover entities, encoded names, duplicate links,
navigation filtering, empty/unknown/truncated pages, and the JSON schema.

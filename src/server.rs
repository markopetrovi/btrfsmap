//! Local HTTP server and JSON API for the web UI.
//!
//! Binds 127.0.0.1 only. Every response is derived from the tree loaded from
//! the cache; the browser asks for one directory at a time, so a 36M-node
//! scan is browsable without shipping the tree to the client.

use crate::model::*;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("web/index.html");

pub struct Server {
    tree: Arc<Tree>,
    tops: Tops,
    /// the cache being served, as reported and cleaned up in the UI
    cache_path: String,
    cache_bytes: u64,
}

/// Biggest nodes by owned bytes, computed once at load.
struct Tops {
    files: Vec<u32>,
    dirs: Vec<u32>,
    snapshots: Vec<u32>,
}

const TOP_N: usize = 200;

fn top_by_owned(tree: &Tree, want: usize, dirs: bool) -> Vec<u32> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::with_capacity(want + 1);
    for i in 0..tree.nodes.len() as u32 {
        let n = &tree.nodes[i as usize];
        let is_dir = n.kind == KIND_DIR;
        if is_dir != dirs || i == tree.root {
            continue;
        }
        let v = n.owned;
        if heap.len() < want {
            heap.push(Reverse((v, i)));
        } else if let Some(Reverse((min, _))) = heap.peek() {
            if v > *min {
                heap.pop();
                heap.push(Reverse((v, i)));
            }
        }
    }
    let mut out: Vec<(u64, u32)> = heap.into_iter().map(|Reverse(x)| x).collect();
    out.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
    out.into_iter().map(|(_, i)| i).collect()
}

impl Tops {
    fn compute(tree: &Tree) -> Tops {
        let mut snapshots: Vec<u32> = Vec::new();
        for i in 0..tree.nodes.len() as u32 {
            let n = &tree.nodes[i as usize];
            if n.kind == KIND_DIR && n.flags & FLAG_SNAPSHOT != 0 {
                snapshots.push(i);
            }
        }
        snapshots.sort_by_key(|&i| std::cmp::Reverse(tree.node(i).owned));
        Tops {
            files: top_by_owned(tree, TOP_N, false),
            dirs: top_by_owned(tree, TOP_N, true),
            snapshots,
        }
    }
}

pub fn run(
    tree: Tree,
    port: u16,
    open_browser: bool,
    cache_path: String,
    cache_bytes: u64,
) -> std::io::Result<()> {
    let tree = Arc::new(tree);
    let tops = Tops::compute(&tree);
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(_) => TcpListener::bind(("127.0.0.1", 0))?,
    };
    let addr = listener.local_addr()?;
    let url = format!("http://127.0.0.1:{}/", addr.port());
    println!();
    println!("  open this in a browser:  {url}");
    println!();
    println!(
        "  scan of {} ({} entries) from {}",
        tree.scanned_roots
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect::<Vec<_>>()
            .join(", "),
        tree.nodes.len(),
        fmt_time(tree.scanned_at)
    );
    println!("  Ctrl-C to stop");
    if open_browser {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
    let server = Arc::new(Server {
        tree,
        tops,
        cache_path,
        cache_bytes,
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            let _ = handle(stream, &server);
        });
    }
    Ok(())
}

fn fmt_time(secs: u64) -> String {
    // days since epoch -> date, without pulling in a date library
    let days = secs / 86400;
    let rem = secs % 86400;
    let mut y = 1970i64;
    let mut d = days as i64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    while d >= months[m] {
        d -= months[m];
        m += 1;
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        y,
        m + 1,
        d + 1,
        rem / 3600,
        (rem % 3600) / 60
    )
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn json_bytes(b: &[u8]) -> String {
    json_escape(&String::from_utf8_lossy(b))
}

#[derive(Clone, Copy, PartialEq)]
enum Metric {
    Owned,
    Compsize,
    Freeable,
    Referenced,
    Uncompressed,
}

impl Metric {
    fn parse(s: &str) -> Metric {
        match s {
            "compsize" => Metric::Compsize,
            "freeable" => Metric::Freeable,
            "referenced" => Metric::Referenced,
            "uncompressed" => Metric::Uncompressed,
            _ => Metric::Owned,
        }
    }
}

fn metric_of(tree: &Tree, id: u32, m: Metric) -> u64 {
    let n = tree.node(id);
    if n.kind == KIND_FILE {
        // Per-file reach/exclusive are not stored; owned is the file's share.
        return match m {
            Metric::Referenced => n.referenced,
            Metric::Uncompressed => n.uncomp,
            _ => n.owned,
        };
    }
    let d = tree.dir(id);
    match m {
        Metric::Owned => n.owned,
        Metric::Compsize => d.compsize,
        Metric::Freeable => d.freeable,
        Metric::Referenced => n.referenced,
        Metric::Uncompressed => d.uncomp_reach,
    }
}

fn node_json(tree: &Tree, id: u32, metric: Metric, with_path: bool) -> String {
    let n = tree.node(id);
    let mut flags: Vec<&str> = Vec::new();
    if n.flags & FLAG_ROOT != 0 {
        flags.push("root");
    }
    if n.flags & FLAG_SUBVOL != 0 {
        flags.push("subvolume");
    }
    if n.flags & FLAG_SNAPSHOT != 0 {
        flags.push("snapshot");
    }
    if n.flags & FLAG_READONLY != 0 {
        flags.push("readonly");
    }
    if n.flags & FLAG_SNAP_CLASS != 0 {
        flags.push("snapclass");
    }
    let flag_str = flags
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(",");
    let metrics = if n.kind == KIND_DIR {
        let d = tree.dir(id);
        format!(
            "\"owned\":{},\"compsize\":{},\"freeable\":{},\"referenced\":{},\"uncompressed\":{},\"files\":{},\"dirs\":{},\"subvol\":{}",
            n.owned, d.compsize, d.freeable, n.referenced, d.uncomp_reach, d.nfiles, d.ndirs, d.subvol
        )
    } else {
        format!(
            "\"owned\":{},\"compsize\":{},\"freeable\":{},\"referenced\":{},\"uncompressed\":{},\"files\":1,\"dirs\":0,\"subvol\":{}",
            n.owned, n.owned, n.owned, n.referenced, n.uncomp, tree.subvols_of(id)
        )
    };
    let path = if with_path {
        let mut parts: Vec<String> = Vec::new();
        let mut cur = id;
        while cur != NO_NODE {
            parts.push(format!(
                "{{\"id\":{},\"name\":\"{}\"}}",
                cur,
                json_bytes(tree.name(cur))
            ));
            if cur == tree.root {
                break;
            }
            cur = tree.node(cur).parent;
        }
        parts.reverse();
        format!(",\"path\":[{}]", parts.join(","))
    } else {
        String::new()
    };
    format!(
        "{{\"id\":{},\"name\":\"{}\",\"kind\":\"{}\",\"mtime\":{},\"nchildren\":{},\"metric\":{},\"flags\":[{}],\"metrics\":{{{}}}{}}}",
        id,
        json_bytes(tree.name(id)),
        if n.kind == KIND_DIR { "dir" } else { "file" },
        n.mtime,
        n.nchildren,
        metric_of(tree, id, metric),
        flag_str,
        metrics,
        path
    )
}

impl Tree {
    /// Subvolume id of the directory containing a node.
    fn subvols_of(&self, id: u32) -> u16 {
        let mut cur = id;
        while cur != NO_NODE {
            let n = self.node(cur);
            if n.kind == KIND_DIR {
                return self.dir(cur).subvol;
            }
            cur = n.parent;
        }
        0
    }
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let mut it = kv.splitn(2, '=');
            let k = it.next().unwrap_or("").to_string();
            let v = it.next().unwrap_or("").to_string();
            (k, urldecode(&v))
        })
        .collect()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn q_get(q: &[(String, String)], key: &str) -> Option<String> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn q_u32(q: &[(String, String)], key: &str, default: u32) -> u32 {
    q_get(q, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn cache_json(path: &str, bytes: u64) -> String {
    let files = crate::cache::list();
    let total: u64 = files.iter().map(|f| f.bytes).sum();
    let items: Vec<String> = files
        .iter()
        .map(|f| {
            format!(
                "{{\"name\":\"{}\",\"bytes\":{},\"mtime\":{}}}",
                json_escape(&f.name),
                f.bytes,
                f.mtime
            )
        })
        .collect();
    format!(
        "{{\"path\":\"{}\",\"bytes\":{},\"dir\":\"{}\",\"total\":{},\"files\":[{}]}}",
        json_escape(path),
        bytes,
        json_escape(&crate::cache::dir().to_string_lossy()),
        total,
        items.join(",")
    )
}

impl Server {
    fn route(&self, path: &str, query: &str) -> (String, String) {
        let tree = &self.tree;
        let q = parse_query(query);
        match path {
            "/" | "/index.html" => ("text/html; charset=utf-8".into(), INDEX_HTML.to_string()),
            "/api/info" => {
                let root = tree.root;
                let mut warnings = String::new();
                for (i, w) in tree.warnings.iter().take(20).enumerate() {
                    if i > 0 {
                        warnings.push(',');
                    }
                    warnings.push('"');
                    warnings.push_str(&json_escape(w));
                    warnings.push('"');
                }
                let snap_owned: u64 = self.tops.snapshots.iter().map(|&s| tree.node(s).owned).sum();
                let snap_free: u64 = self
                    .tops
                    .snapshots
                    .iter()
                    .map(|&s| tree.dir(s).freeable)
                    .sum();
                let roots: Vec<String> = tree
                    .roots
                    .iter()
                    .map(|&r| {
                        format!(
                            "{{\"id\":{},\"name\":\"{}\",\"owned\":{},\"compsize\":{},\"freeable\":{},\"referenced\":{},\"uncompressed\":{},\"files\":{},\"dirs\":{}}}",
                            r,
                            json_bytes(tree.name(r)),
                            tree.node(r).owned,
                            tree.dir(r).compsize,
                            tree.dir(r).freeable,
                            tree.node(r).referenced,
                            tree.dir(r).uncomp_reach,
                            tree.dir(r).nfiles,
                            tree.dir(r).ndirs
                        )
                    })
                    .collect();
                let subvols: Vec<String> = tree
                    .subvols
                    .iter()
                    .map(|s| {
                        format!(
                            "{{\"id\":{},\"name\":\"{}\",\"path\":\"{}\",\"snapshot\":{},\"readonly\":{},\"ctime\":{}}}",
                            s.id,
                            json_bytes(&s.name),
                            json_bytes(&s.path),
                            s.snapshot,
                            s.is_readonly(),
                            s.ctime
                        )
                    })
                    .collect();
                let body = format!(
                    "{{\"scanned_at\":{},\"scanned_roots\":[{}],\"totals\":{{\"owned\":{},\"compsize\":{},\"freeable\":{},\"referenced\":{},\"uncompressed\":{},\"files\":{},\"dirs\":{}}},\
                     \"stats\":{{\"files\":{},\"dirs\":{},\"extents\":{},\"refs\":{},\"subvolumes\":{},\"skipped_nonbtrfs\":{},\"unreadable\":{},\"seconds\":{:.1}}},\
                     \"snapshots\":{{\"count\":{},\"owned\":{},\"freeable\":{}}},\"cache\":{},\"roots\":[{}],\"subvols\":[{}],\"warnings\":[{}]}}",
                    tree.scanned_at,
                    tree.scanned_roots
                        .iter()
                        .map(|r| format!("\"{}\"", json_bytes(r)))
                        .collect::<Vec<_>>()
                        .join(","),
                    tree.node(root).owned,
                    tree.dir(root).compsize,
                    tree.dir(root).freeable,
                    tree.node(root).referenced,
                    tree.dir(root).uncomp_reach,
                    tree.dir(root).nfiles,
                    tree.dir(root).ndirs,
                    tree.stats.files,
                    tree.stats.dirs,
                    tree.stats.extents,
                    tree.stats.refs,
                    tree.stats.subvolumes,
                    tree.stats.skipped_nonbtrfs,
                    tree.stats.unreadable,
                    tree.stats.elapsed.as_secs_f64(),
                    self.tops.snapshots.len(),
                    snap_owned,
                    snap_free,
                    cache_json(&self.cache_path, self.cache_bytes),
                    roots.join(","),
                    subvols.join(","),
                    warnings
                );
                ("application/json".into(), body)
            }
            "/api/node" => {
                let id = q_u32(&q, "id", tree.root);
                let metric = Metric::parse(&q_get(&q, "metric").unwrap_or_default());
                if id as usize >= tree.nodes.len() {
                    return ("application/json".into(), "{\"error\":\"no such node\"}".into());
                }
                ("application/json".into(), node_json(tree, id, metric, true))
            }
            "/api/children" => {
                let id = q_u32(&q, "id", tree.root);
                let metric = Metric::parse(&q_get(&q, "metric").unwrap_or_default());
                let offset = q_u32(&q, "offset", 0) as usize;
                let limit = q_u32(&q, "limit", 500).min(20000) as usize;
                if id as usize >= tree.nodes.len() || tree.node(id).kind != KIND_DIR {
                    return ("application/json".into(), "{\"items\":[],\"total\":0}".into());
                }
                let mut kids: Vec<u32> = tree.child_ids(id).collect();
                let total = kids.len();
                kids.sort_by_key(|&c| std::cmp::Reverse(metric_of(tree, c, metric)));
                let items: Vec<String> = kids
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|&c| node_json(tree, c, metric, false))
                    .collect();
                let body = format!(
                    "{{\"id\":{},\"total\":{},\"offset\":{},\"limit\":{},\"items\":[{}]}}",
                    id,
                    total,
                    offset,
                    limit,
                    items.join(",")
                );
                ("application/json".into(), body)
            }
            "/api/top" => {
                let metric = Metric::parse(&q_get(&q, "metric").unwrap_or_default());
                let kind = q_get(&q, "kind").unwrap_or_else(|| "file".into());
                let limit = q_u32(&q, "limit", 50).min(TOP_N as u32) as usize;
                let list = match kind.as_str() {
                    "dir" => &self.tops.dirs,
                    "snapshot" => &self.tops.snapshots,
                    _ => &self.tops.files,
                };
                let items: Vec<String> = list
                    .iter()
                    .take(limit)
                    .map(|&c| {
                        let mut j = node_json(tree, c, metric, false);
                        // splice in the full path for readability
                        j.pop();
                        j.push_str(&format!(
                            ",\"path\":\"{}\"}}",
                            json_bytes(&tree.path_of(c))
                        ));
                        j
                    })
                    .collect();
                (
                    "application/json".into(),
                    format!("{{\"items\":[{}]}}", items.join(",")),
                )
            }
            "/api/cache/delete" => {
                let name = q_get(&q, "file").filter(|n| !n.is_empty());
                let all = q_get(&q, "all").is_some();
                let deleted = if all {
                    crate::cache::delete(None)
                } else if let Some(n) = name.as_deref() {
                    crate::cache::delete(Some(n))
                } else {
                    Ok(0)
                };
                let body = match deleted {
                    Ok(n) => format!(
                        "{{\"deleted\":{},\"cache\":{}}}",
                        n,
                        cache_json(&self.cache_path, self.cache_bytes)
                    ),
                    Err(e) => format!("{{\"error\":\"{}\"}}", json_escape(&e.to_string())),
                };
                ("application/json".into(), body)
            }
            "/api/search" => {
                let needle = q_get(&q, "q").unwrap_or_default().to_lowercase();
                let limit = q_u32(&q, "limit", 100).min(1000) as usize;
                let mut items: Vec<String> = Vec::new();
                if needle.len() >= 2 {
                    for i in 0..tree.nodes.len() as u32 {
                        if items.len() >= limit {
                            break;
                        }
                        let name = tree.name(i);
                        if name.len() > 512 {
                            continue;
                        }
                        let name_l = String::from_utf8_lossy(name).to_lowercase();
                        if name_l.contains(&needle) {
                            let mut j = node_json(tree, i, Metric::Owned, false);
                            j.pop();
                            j.push_str(&format!(
                                ",\"path\":\"{}\"}}",
                                json_bytes(&tree.path_of(i))
                            ));
                            items.push(j);
                        }
                    }
                }
                (
                    "application/json".into(),
                    format!("{{\"items\":[{}]}}", items.join(",")),
                )
            }
            _ => ("text/plain".into(), "not found".into()),
        }
    }
}

fn handle(stream: TcpStream, server: &Server) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // drain headers
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("GET");
    let target = parts.next().unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let (ctype, body) = server.route(path, query);
    let mut out = stream;
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    out.write_all(header.as_bytes())?;
    out.write_all(body.as_bytes())?;
    out.flush()
}


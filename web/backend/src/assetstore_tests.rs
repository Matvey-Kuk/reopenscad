use super::*;

use crate::assets::Asset;

#[test]
fn asset_names_are_validated() {
    for good in ["scan.stl", "Scan.STL", "a-b_c.1.stl", "x.stl", &format!("{}.stl", "a".repeat(60))] {
        assert!(valid_asset_name(good), "{good}");
    }
    for bad in [
        "", ".stl", ".scan.stl", "scan", "scan.obj", "../x.stl", "a..b.stl", "a/b.stl", "a b.stl", "a\\b.stl",
        "ä.stl", &format!("{}.stl", "a".repeat(61)),
    ] {
        assert!(!valid_asset_name(bad), "{bad}");
    }
}

#[test]
fn budgets_are_clamped() {
    assert_eq!(clamp_budget(None), meshimport::DEFAULT_TRIANGLE_BUDGET);
    assert_eq!(clamp_budget(Some(1)), MIN_TRIANGLE_BUDGET);
    assert_eq!(clamp_budget(Some(u64::MAX)), meshimport::MAX_TRIANGLE_BUDGET);
    assert_eq!(clamp_budget(Some(5000)), 5000);
}

fn meta(name: &str, bytes: u64) -> AssetMeta {
    AssetMeta { name: name.into(), sha256: format!("{name}-sha"), bytes, triangles: 1, created_at: 0, expires_at: 0, report: Value::Null }
}

#[test]
fn limits_count_replacements_and_bytes() {
    let full: Vec<AssetMeta> = (0..MAX_ASSETS_PER_DESIGN).map(|index| meta(&format!("a{index}.stl"), 10)).collect();
    assert_eq!(check_limits(&full, &meta("new.stl", 10), 0, u64::MAX).unwrap_err().status, 409);
    assert!(check_limits(&full, &meta("a0.stl", 10), 0, u64::MAX).is_ok(), "replacing keeps the count");
    assert_eq!(check_limits(&[], &meta("a.stl", 10), 95, 100).unwrap(), 5, "over the total: evict 5 bytes elsewhere");
    assert_eq!(check_limits(&[], &meta("a.stl", 10), 0, 100).unwrap(), 0);
    assert_eq!(check_limits(&[], &meta("a.stl", 101), 0, 100).unwrap_err().status, 503, "can never fit");
}

#[test]
fn digest_matches_the_asset_set() {
    let metas = [meta("b.stl", 1), meta("a.stl", 1)];
    let set = AssetSet::new(metas.iter().map(|meta| Asset {
        name: meta.name.clone(),
        sha256: meta.sha256.clone(),
        mesh: Arc::new(TriMesh::default()),
    }));
    assert_eq!(digest_of(&metas), set.digest());
}

#[test]
fn upload_tokens_are_single_use_and_expire() {
    let token = mint_token().unwrap();
    assert!(valid_token(&token));
    let pending = PendingUpload { design_id: "d".into(), name: "a.stl".into(), budget: 10, expires_at: 1_000, client: String::new() };
    remember_upload(&token, pending.clone(), 0).unwrap();
    assert_eq!(take_upload(&token, 500), Some(pending.clone()));
    assert_eq!(take_upload(&token, 500), None, "second use");
    let expiring = mint_token().unwrap();
    remember_upload(&expiring, pending, 0).unwrap();
    assert_eq!(take_upload(&expiring, 1_000), None, "expired");
}

// -------------------------------------------------------------------
// Server-level tests against the filesystem store.
// -------------------------------------------------------------------

use crate::{
    JobLimiter, RateLimiter, RenderQueue, WorkspaceStore, MAX_CONCURRENT_JOBS, MAX_CONCURRENT_POLLS,
    MAX_QUEUED_JOBS, MAX_QUEUED_PER_CLIENT, MAX_QUEUE_WAIT,
};
use std::net::TcpListener;

fn temporary_state() -> (AppState, PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "reopenscad-asset-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let store = WorkspaceStore::open(root.clone()).unwrap();
    let state = AppState {
        web_root: PathBuf::new(),
        render_queue: RenderQueue::new(MAX_CONCURRENT_JOBS, MAX_QUEUED_JOBS, MAX_QUEUED_PER_CLIENT, MAX_QUEUE_WAIT),
        poll_limiter: JobLimiter::new(MAX_CONCURRENT_POLLS),
        rate_limiter: RateLimiter::new(),
        cancellations: Arc::new(Mutex::new(HashMap::new())),
        workspaces: Store::Filesystem(store),
        public_origin: None,
    };
    (state, root)
}

/// A closed, outward-wound `size` mm cube as binary STL.
fn cube_stl(size: f32) -> Vec<u8> {
    let corner = |index: usize| -> [f32; 3] {
        [
            if index & 1 != 0 { size } else { 0.0 },
            if index & 2 != 0 { size } else { 0.0 },
            if index & 4 != 0 { size } else { 0.0 },
        ]
    };
    // Quads as corner indices, counter-clockwise seen from outside.
    let quads = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]];
    let mut triangles = Vec::new();
    for [a, b, c, d] in quads {
        triangles.push([a, b, c]);
        triangles.push([a, c, d]);
    }
    let mut out = vec![0u8; 80];
    out.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
    for triangle in triangles {
        out.extend_from_slice(&[0u8; 12]);
        for index in triangle {
            for value in corner(index) {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        out.extend_from_slice(&[0, 0]);
    }
    out
}

/// Sends raw bytes through the real dispatcher; `(status, body)`.
fn serve(state: &AppState, raw: Vec<u8>) -> (u16, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        let _ = stream.write_all(&raw);
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        response
    });
    let (mut stream, _) = listener.accept().unwrap();
    crate::serve_request(&mut stream, state, &mut None).unwrap();
    drop(stream);
    let mut response = client.join().unwrap();
    // Skip interim replies: 100 Continue, and the 102 a compute route's
    // client watch sends this half-closed client.
    for interim in [&b"HTTP/1.1 100 Continue\r\n\r\n"[..], b"HTTP/1.1 102 Processing\r\n\r\n"] {
        while response.starts_with(interim) {
            response.drain(..interim.len());
        }
    }
    let split = response.windows(4).position(|window| window == b"\r\n\r\n").unwrap_or_else(|| panic!("response head: {:?}", String::from_utf8_lossy(&response)));
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, response[split + 4..].to_vec())
}

fn raw(method: &str, path: &str, content_type: Option<&str>, body: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nContent-Length: {}\r\n", body.len());
    if let Some(content_type) = content_type {
        out.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

fn json_of(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(body)))
}

fn mcp_request() -> Request {
    Request { method: "POST".into(), path: "/mcp".into(), query: String::new(), headers: vec![("host".into(), "127.0.0.1:5173".into())], body: vec![] }
}

#[test]
fn asset_routes_are_classified() {
    let id = "bouncy-badger-0123456789abcdef0123456789";
    use crate::{route_cost, workspace_action_compiles, RouteCost};
    assert_eq!(route_cost("GET", &format!("/api/designs/{id}/assets")), RouteCost::Cheap);
    assert_eq!(route_cost("GET", &format!("/api/designs/{id}/assets/a.stl")), RouteCost::Evaluator);
    assert_eq!(route_cost("PUT", &format!("/api/workspaces/{id}/assets/a.stl")), RouteCost::Evaluator);
    assert_eq!(route_cost("DELETE", &format!("/api/designs/{id}/assets/a.stl")), RouteCost::Evaluator);
    assert_eq!(route_cost("PUT", "/api/uploads/0123456789abcdef0123456789abcdef"), RouteCost::Evaluator);
    assert_eq!(route_cost("GET", "/api/shared/tok/assets/a.stl"), RouteCost::Evaluator);
    assert!(workspace_action_compiles("render"));
    assert!(workspace_action_compiles("evaluate"));
    assert!(workspace_action_compiles(""));
    for cheap in ["events", "assets", "assets/a.stl", "feedback", "feedback/x", "preview", "code"] {
        assert!(!workspace_action_compiles(cheap), "{cheap}");
    }
}

/// The 128 MiB limit applies to upload routes only; everything else keeps 2 MiB.
#[test]
fn body_limits_depend_on_the_route() {
    let (state, root) = temporary_state();
    let id = "bouncy-badger-0123456789abcdef0123456789";
    let head = |method: &str, path: &str, length: usize| {
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nContent-Length: {length}\r\n\r\n").into_bytes()
    };
    let three_mib = 3 * 1024 * 1024;
    let (status, body) = serve(&state, head("POST", "/api/render", three_mib));
    assert_eq!(status, 413, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("2 MiB"));
    // No such design: refused on the head, before the body or a slot.
    let (status, body) = serve(&state, head("PUT", &format!("/api/designs/{id}/assets/a.stl"), three_mib));
    assert_eq!(status, 404, "{}", String::from_utf8_lossy(&body));
    // Accepted past the header check; the body then ends early (400).
    let real = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let (status, _) = serve(&state, head("PUT", &format!("/api/designs/{real}/assets/a.stl"), three_mib));
    assert_eq!(status, 400);
    let (status, body) = serve(&state, head("PUT", &format!("/api/designs/{id}/assets/a.stl"), meshimport::MAX_UPLOAD_BYTES + 1));
    assert_eq!(status, 413);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("128 MiB") && text.contains("decimates"), "{text}");
    // Over the old 25 MiB limit is admitted on the head.
    let (status, body) = serve(&state, head("PUT", &format!("/api/designs/{real}/assets/a.stl"), 30 * 1024 * 1024));
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    let (status, _) = serve(&state, head("GET", &format!("/api/designs/{id}/assets/a.stl"), three_mib));
    assert_eq!(status, 413, "a GET keeps the 2 MiB limit");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn upload_list_get_delete_round_trip_and_eviction() {
    let (state, root) = temporary_state();
    let workspace = state.workspaces.create("cube(1);".into(), None).unwrap();
    let id = workspace.id.clone();
    let stl = cube_stl(10.0);

    let (status, body) = serve(&state, raw("PUT", &format!("/api/designs/{id}/assets/cube.stl"), None, &stl));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let uploaded = json_of(&body);
    assert_eq!(uploaded["name"], "cube.stl");
    assert_eq!(uploaded["triangles"], 12);
    assert_eq!(uploaded["assetsVersion"], 1);
    assert!(uploaded["report"].is_object() && uploaded["description"].is_object());
    let sha = uploaded["sha256"].as_str().unwrap().to_string();

    let empty = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let (status, body) = serve(&state, raw("PUT", &format!("/api/designs/{empty}/assets/bad.stl"), None, b"not an stl"));
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    let (status, _) = serve(&state, raw("PUT", &format!("/api/designs/{empty}/assets/..x.stl"), None, &stl));
    assert_eq!(status, 400);

    let (status, body) = serve(&state, raw("GET", &format!("/api/workspaces/{id}/assets"), None, b""));
    assert_eq!(status, 200);
    let listed = json_of(&body);
    assert_eq!(listed["assets"].as_array().unwrap().len(), 1);
    assert_eq!(listed["assets"][0]["sha256"], sha.as_str());
    assert_eq!(listed["assetsVersion"], 1);

    let (status, body) = serve(&state, raw("GET", &format!("/api/designs/{id}/assets/cube.stl"), None, b""));
    assert_eq!(status, 200);
    assert_eq!(hex(&rendercache::sha256(&body)), sha);
    assert_eq!(body.len(), 84 + 12 * 50);

    // A share link reads but cannot write.
    let token = state.workspaces.share_token(&id).unwrap();
    let (status, body) = serve(&state, raw("GET", &format!("/api/shared/{token}/assets"), None, b""));
    assert_eq!(status, 200);
    assert!(!String::from_utf8_lossy(&body).contains(&id));
    let (status, _) = serve(&state, raw("PUT", &format!("/api/shared/{token}/assets/cube.stl"), Some("application/json"), &stl));
    assert_eq!(status, 405);

    // The /events poll reports the asset version and ends on a change.
    let (status, body) = serve(&state, raw("GET", &format!("/api/designs/{id}/events?since=99&assets=0"), None, b""));
    assert_eq!(status, 200);
    let events = json_of(&body);
    assert_eq!(events["assetsVersion"], 1);
    assert_eq!(events["assetsChanged"], true);

    let (status, body) = serve(&state, raw("DELETE", &format!("/api/designs/{id}/assets/cube.stl"), None, b""));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(json_of(&body)["assetsVersion"], 2);
    let (status, _) = serve(&state, raw("GET", &format!("/api/designs/{id}/assets/cube.stl"), None, b""));
    assert_eq!(status, 404);
    let (status, _) = serve(&state, raw("DELETE", &format!("/api/designs/{id}/assets/cube.stl"), None, b""));
    assert_eq!(status, 404);

    // Eviction takes the asset directory with it.
    let (status, _) = serve(&state, raw("PUT", &format!("/api/designs/{id}/assets/cube.stl"), None, &stl));
    assert_eq!(status, 200);
    let dir = root.join(format!("{id}{FS_ASSET_DIR_SUFFIX}"));
    assert!(dir.join("index.json").exists());
    let Store::Filesystem(store) = &state.workspaces else { unreachable!() };
    {
        let mut inner = store.inner.lock().unwrap();
        store.forget(&mut inner, &id);
    }
    assert!(!dir.exists(), "eviction left the asset directory behind");
    assert!(state.workspaces.asset_list(&id).unwrap().is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn mcp_compiles_unsaved_code_with_the_designs_assets() {
    let (state, root) = temporary_state();
    let workspace = state.workspaces.create("cube(1);".into(), None).unwrap();
    let id = workspace.id.clone();
    let request = mcp_request();
    let base64 = crate::mcpapp::base64_encode(&cube_stl(10.0));
    let uploaded = crate::mcp_call_tool(&state, &request, "upload_asset", json!({"designId": id, "name": "cube.stl", "base64": base64}));
    assert_eq!(uploaded["isError"], false, "{uploaded}");
    let listed = crate::mcp_call_tool(&state, &request, "list_assets", json!({"designId": id}));
    assert_eq!(listed["structuredContent"]["assets"][0]["name"], "cube.stl");
    let compact = &listed["structuredContent"]["assets"][0];
    assert!(compact.get("description").is_none() && compact.get("report").is_none(), "{compact}");
    assert_eq!(compact["sha256"].as_str().unwrap().len(), 12);
    assert_eq!(compact["size"], json!([10.0, 10.0, 10.0]), "{compact}");
    let detailed = crate::mcp_call_tool(&state, &request, "list_assets", json!({"designId": id, "detail": true}));
    assert!(detailed["structuredContent"]["assets"][0]["description"]["layFlat"].is_array(), "{detailed}");

    let code = "import(\"cube.stl\");\n";
    let with_assets = crate::mcp_call_tool(&state, &request, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, Some(&id), Some(code), None)}));
    assert_eq!(with_assets["isError"], false, "{with_assets}");
    let text = with_assets["structuredContent"].to_string();
    assert!(text.contains("object-1"), "{text}");
    let without = crate::mcp_call_tool(&state, &request, "evaluate", json!({"designId": crate::scancommon::testkit::save_revision(&state, None, Some(code), None)}));
    let failed = without["isError"] == json!(true) || !without["structuredContent"].to_string().contains("object-1");
    assert!(failed, "import resolved without a design: {without}");

    let deleted = crate::mcp_call_tool(&state, &request, "delete_asset", json!({"designId": id, "name": "cube.stl"}));
    assert_eq!(deleted["isError"], false, "{deleted}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn render_cache_key_follows_the_assets() {
    let set = |sha: &str| {
        Some(Arc::new(AssetSet::new([Asset { name: "a.stl".into(), sha256: sha.into(), mesh: Arc::new(TriMesh::default()) }])))
    };
    let key = || rendercache::render_key("preview", "import(\"a.stl\");", "import(\"a.stl\");", &json!([]), None);
    let none = crate::assets::with_assets(None, key);
    let first = crate::assets::with_assets(set("1"), key);
    let second = crate::assets::with_assets(set("2"), key);
    assert_ne!(none, first);
    assert_ne!(first, second);
    assert_eq!(first, crate::assets::with_assets(set("1"), key));
}

#[test]
fn one_time_upload_urls_work_once() {
    let (state, root) = temporary_state();
    let workspace = state.workspaces.create("cube(1);".into(), None).unwrap();
    let request = mcp_request();
    let created = crate::mcp_call_tool(&state, &request, "create_asset_upload", json!({"designId": workspace.id, "name": "big.stl"}));
    assert_eq!(created["isError"], false, "{created}");
    let url = created["structuredContent"]["uploadUrl"].as_str().unwrap().to_string();
    let curl = created["structuredContent"]["curl"].as_str().unwrap();
    assert!(curl.contains(&url) && curl.contains("--data-binary @big.stl"), "{curl}");
    assert_eq!(created["structuredContent"]["maxBytes"], meshimport::MAX_UPLOAD_BYTES);
    assert!(created["structuredContent"]["note"].as_str().unwrap().contains("upload_asset's JSON"));
    assert!(created["structuredContent"].get("chunkedScript").is_none(), "{created}");
    let path = url.split_once("5173").unwrap().1.to_string();
    let mut put = format!("PUT {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n", 684).into_bytes();
    put.extend_from_slice(&cube_stl(5.0));
    // A rejected file does not use the URL up.
    let mut bad = format!("PUT {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nContent-Length: 10\r\n\r\n").into_bytes();
    bad.extend_from_slice(b"not an stl");
    let (status, _) = serve(&state, bad);
    assert_eq!(status, 400);
    let (status, body) = serve(&state, put.clone());
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let reply = json_of(&body);
    assert_eq!(reply["name"], "big.stl");
    assert!(reply.get("designId").is_none(), "the URL holder must not learn the design id: {reply}");
    assert!(!String::from_utf8_lossy(&body).contains(&workspace.id));
    let (status, _) = serve(&state, put);
    assert_eq!(status, 404, "second use");
    let _ = fs::remove_dir_all(root);
}


/// Everything that can refuse an upload runs on its head, before it may
/// hold one of the few upload slots: a wrong host, an unknown upload URL,
/// a missing design or a bad name never take one.
#[test]
fn uploads_are_refused_on_their_head_before_taking_a_slot() {
    let (state, root) = temporary_state();
    let head = |host: &str, path: &str| {
        format!("PUT {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n", 3 * 1024 * 1024).into_bytes()
    };
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let (status, _) = serve(&state, head("evil.example", &format!("/api/designs/{id}/assets/a.stl")));
    assert_eq!(status, 403);
    let (status, _) = serve(&state, head("127.0.0.1:5173", "/api/uploads/0123456789abcdef0123456789abcdef"));
    assert_eq!(status, 404);
    let (status, _) = serve(&state, head("127.0.0.1:5173", &format!("/api/designs/{id}/assets/a.obj")));
    assert_eq!(status, 400);
    let _ = fs::remove_dir_all(root);
}


#[test]
fn upload_asset_over_the_inline_cap_names_the_next_step() {
    let (state, root) = temporary_state();
    let workspace = state.workspaces.create("cube(1);".into(), None).unwrap();
    let request = mcp_request();
    let base64 = "A".repeat(MAX_INLINE_UPLOAD_BYTES / 3 * 4 + 400);
    let result = crate::mcp_call_tool(&state, &request, "upload_asset", json!({"designId": workspace.id, "name": "scan.stl", "base64": base64}));
    assert_eq!(result["isError"], true);
    let text = result.to_string();
    assert!(text.contains("create_asset_upload") && text.contains(&workspace.id) && text.contains("--data-binary @scan.stl"), "{text}");
    assert!(text.contains("128 MiB"), "{text}");
    let _ = fs::remove_dir_all(root);
}

/// A subdivided cube surface: `n` x `n` quads per face.
fn grid_cube_stl(n: usize, size: f32) -> Vec<u8> {
    let count = 6 * n * n * 2;
    let mut out = vec![0u8; 80];
    out.extend_from_slice(&(count as u32).to_le_bytes());
    let step = size / n as f32;
    // (fixed axis, value, u axis, v axis) with u x v pointing outward.
    let faces = [(2, 0.0, 1, 0), (2, size, 0, 1), (0, 0.0, 2, 1), (0, size, 1, 2), (1, 0.0, 0, 2), (1, size, 2, 0)];
    for (axis, value, u, v) in faces {
        let point = |i: usize, j: usize| {
            let mut p = [0f32; 3];
            p[axis] = value;
            p[u] = i as f32 * step;
            p[v] = j as f32 * step;
            p
        };
        for i in 0..n {
            for j in 0..n {
                for triangle in [[point(i, j), point(i + 1, j), point(i + 1, j + 1)], [point(i, j), point(i + 1, j + 1), point(i, j + 1)]] {
                    out.extend_from_slice(&[0u8; 12]);
                    for corner in triangle {
                        for coordinate in corner {
                            out.extend_from_slice(&coordinate.to_le_bytes());
                        }
                    }
                    out.extend_from_slice(&[0u8; 2]);
                }
            }
        }
    }
    out
}

/// Files over the old 25 MiB limit are accepted and decimated.
#[test]
fn upload_over_the_old_limit_is_accepted_and_decimated() {
    let _slots = slot_test();
    let (state, root) = temporary_state();
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let stl = grid_cube_stl(230, 20.0);
    assert!(stl.len() > 30 * 1024 * 1024 && stl.len() > LARGE_UPLOAD_BYTES, "{}", stl.len());
    let (status, body) = serve(&state, raw("PUT", &format!("/api/designs/{id}/assets/scan.stl"), None, &stl));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let uploaded = json_of(&body);
    assert!(uploaded["triangles"].as_u64().unwrap() <= meshimport::DEFAULT_TRIANGLE_BUDGET as u64, "{uploaded}");
    assert_eq!(uploaded["report"]["inputTriangles"], 6 * 230 * 230 * 2);
    assert_eq!(uploaded["report"]["watertight"], true);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn upload_slots_are_capped_per_client() {
    let _slots = slot_test();
    let client: IpAddr = "203.0.113.77".parse().unwrap();
    let held: Vec<UploadSlot> = (0..MAX_UPLOADS_PER_CLIENT).map(|_| UploadSlot::try_acquire(Some(client), false).unwrap()).collect();
    assert!(UploadSlot::try_acquire(Some(client), false).is_err(), "one client cannot hold more");
    drop(held);
    let again = UploadSlot::try_acquire(Some(client), false);
    assert!(again.is_ok(), "released on drop");
    drop(again);
    // One large body per client; a small one may run beside it.
    let large = UploadSlot::try_acquire(Some(client), true).unwrap();
    assert!(UploadSlot::try_acquire(Some(client), true).is_err(), "one large upload per client");
    let small = UploadSlot::try_acquire(Some(client), false);
    assert!(small.is_ok());
    drop((large, small));
    assert!(UploadSlot::try_acquire(Some(client), true).is_ok(), "large slot released on drop");
}

#[test]
fn raw_upload_bytes_are_capped_per_hour() {
    let client: IpAddr = "203.0.113.80".parse().unwrap();
    assert!(raw_upload_admit(client, 60, 0, 100).is_ok());
    let error = raw_upload_admit(client, 60, 1_000, 100).unwrap_err();
    assert_eq!(error.status, 429);
    assert!(error.message.contains("raw files"), "{}", error.message);
    assert!(raw_upload_admit(client, 60, QUOTA_WINDOW_MS + 1, 100).is_ok(), "a new window");
}

#[test]
fn slow_upload_bodies_are_cut_off_after_the_grace_period() {
    assert!(upload_pace_ok(0, Duration::from_secs(5)), "grace period");
    assert!(!upload_pace_ok(100 * 1024, Duration::from_secs(20)));
    assert!(upload_pace_ok(2 * 1024 * 1024, Duration::from_secs(20)));
}

#[test]
fn client_storage_quota_is_per_hour() {
    let client: IpAddr = "203.0.113.78".parse().unwrap();
    let limit = 100;
    assert!(quota_admit(client, 60, 0, limit, true).is_ok());
    assert_eq!(quota_admit(client, 60, 1_000, limit, true).unwrap_err().status, 429);
    assert!(quota_admit(client, 40, 1_000, limit, true).is_ok());
    assert!(quota_admit(client, 1, 2_000, limit, false).is_err());
    assert!(quota_admit(client, 60, QUOTA_WINDOW_MS + 1, limit, true).is_ok(), "a new window");
    let other: IpAddr = "203.0.113.79".parse().unwrap();
    assert!(quota_admit(other, 100, 2_000, limit, false).is_ok(), "per client");
    // A reservation refunds unless committed, and only in its own window.
    let refunded: IpAddr = "192.0.2.77".parse().unwrap();
    assert!(quota_admit(refunded, 70, 5_000, limit, true).is_ok());
    assert!(quota_admit(refunded, 40, 5_000, limit, false).is_err(), "the reservation holds the bytes");
    window_refund(&QUOTA, refunded, 70, 5_000);
    assert!(quota_admit(refunded, 100, 5_000, limit, false).is_ok(), "refunded");
    window_refund(&QUOTA, refunded, 70, 1);
    assert!(quota_admit(refunded, 70, 5_000, limit, true).is_ok());
    window_refund(&QUOTA, refunded, 70, 1);
    assert!(quota_admit(refunded, 31, 5_000, limit, false).is_err(), "another window's refund is ignored");
}

#[test]
fn pending_upload_urls_are_capped_per_design_and_client() {
    let pending = |design: &str, client: &str| PendingUpload {
        design_id: design.into(),
        name: "a.stl".into(),
        budget: 10,
        expires_at: u64::MAX,
        client: client.into(),
    };
    let design = format!("cap-design-{}", mint_token().unwrap());
    for index in 0..MAX_PENDING_UPLOADS_PER_DESIGN {
        remember_upload(&mint_token().unwrap(), pending(&design, &format!("c{index}-{design}")), 0).unwrap();
    }
    assert!(remember_upload(&mint_token().unwrap(), pending(&design, "fresh"), 0).unwrap_err().contains("This design"));
    let client = format!("client-{}", mint_token().unwrap());
    for index in 0..MAX_PENDING_UPLOADS_PER_CLIENT {
        remember_upload(&mint_token().unwrap(), pending(&format!("d{index}-{client}"), &client), 0).unwrap();
    }
    assert!(remember_upload(&mint_token().unwrap(), pending("elsewhere", &client), 0).unwrap_err().contains("You already"));
    assert_eq!(pending_refusal(10, 0, 0, 10).as_deref(), Some("Too many upload URLs are pending; use one or wait for them to expire."));
}

/// The compact reply's way to the chunked script reuses its URL: asking
/// again with `uploadUrl` mints nothing, so it cannot use up the
/// per-design pending limit.
#[test]
fn the_chunked_script_for_an_existing_upload_url_mints_no_new_url() {
    let (state, root) = temporary_state();
    let request = mcp_request();
    let id = state.workspaces.create("cube(1);\n".into(), None).unwrap().id;
    let created = crate::mcp_call_tool(&state, &request, "create_asset_upload", json!({"designId": id, "name": "big.stl"}));
    let structured = &created["structuredContent"];
    let url = structured["uploadUrl"].as_str().unwrap().to_string();
    assert!(structured.get("chunkedScript").is_none(), "{created}");
    assert!(structured["note"].as_str().unwrap().contains("uploadUrl"), "{created}");
    for _ in 0..(MAX_PENDING_UPLOADS_PER_DESIGN + 2) {
        let again = crate::mcp_call_tool(&state, &request, "create_asset_upload", json!({"designId": id, "name": "big.stl", "uploadUrl": url}));
        assert_eq!(again["isError"], false, "{again}");
        assert_eq!(again["structuredContent"]["uploadUrl"], json!(url));
        assert!(again["structuredContent"]["chunkedScript"].as_str().unwrap().contains(&url));
    }
    let other = state.workspaces.create("cube(2);\n".into(), None).unwrap().id;
    let refused = crate::mcp_call_tool(&state, &request, "create_asset_upload", json!({"designId": other, "name": "big.stl", "uploadUrl": url}));
    assert_eq!(refused["isError"], true, "another design's URL: {refused}");
    let refused = crate::mcp_call_tool(&state, &request, "create_asset_upload", json!({"designId": id, "name": "other.stl", "uploadUrl": url}));
    assert_eq!(refused["isError"], true, "another file's URL: {refused}");
    let _ = fs::remove_dir_all(root);
}

/// Routes that do not compile never read or decode a design's meshes (a
/// design read may consult the metadata index for its object-summary
/// key); a compile reads the index once and each imported mesh once.
#[test]
fn non_compiling_reads_never_load_assets() {
    let (state, root) = temporary_state();
    let id = state.workspaces.create("import(\"cube.stl\");\n".into(), None).unwrap().id;
    // A size no other test uses, so the shared decoded cache cannot hit.
    let (status, body) = serve(&state, raw("PUT", &format!("/api/designs/{id}/assets/cube.stl"), None, &cube_stl(10.75)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    for path in [
        format!("/api/designs/{id}?objects=none"),
        format!("/api/designs/{id}/revisions"),
        format!("/api/designs/{id}/assets"),
        format!("/api/designs/{id}/events?since=99&assets=0"),
    ] {
        let (status, body) = serve(&state, raw("GET", &path, None, b""));
        assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
    }
    let request = mcp_request();
    let link = crate::mcp_call_tool(&state, &request, "get_design_link", json!({"designId": id}));
    assert_eq!(link["isError"], false, "{link}");
    assert_eq!(mesh_reads(&id), 0, "a read loaded a mesh");
    let reads_before = index_reads(&id);

    let evaluated = crate::mcp_call_tool(&state, &request, "evaluate", json!({"designId": id}));
    assert_eq!(evaluated["isError"], false, "{evaluated}");
    assert!(evaluated["structuredContent"].to_string().contains("object-1"), "{evaluated}");
    assert_eq!(index_reads(&id) - reads_before, 1, "a compile reads the index once");
    assert_eq!(mesh_reads(&id), 1, "and the imported mesh once");
    let _ = fs::remove_dir_all(root);
}

/// An asset replaced between a compile's index read and its mesh read
/// fails the compile; it is never silently left out or served under the
/// old hash.
#[test]
fn an_asset_replaced_mid_compile_is_an_error() {
    let (state, root) = temporary_state();
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let request = mcp_request();
    let upload = |size: f32| {
        let base64 = crate::mcpapp::base64_encode(&cube_stl(size));
        crate::mcp_call_tool(&state, &request, "upload_asset", json!({"designId": id, "name": "cube.stl", "base64": base64}))
    };
    // Sizes no other test uses, so the shared decoded cache cannot hit.
    assert_eq!(upload(10.5)["isError"], false);
    let set = design_set(&state, &id).unwrap();
    let before = set.digest().to_string();
    assert_eq!(crate::mcp_call_tool(&state, &request, "delete_asset", json!({"designId": id, "name": "cube.stl"}))["isError"], false);
    assert_eq!(upload(5.5)["isError"], false);
    let error = set.try_get("cube.stl").unwrap_err();
    assert!(error.contains("replaced while this compile ran"), "{error}");
    let fresh = design_set(&state, &id).unwrap();
    assert_ne!(fresh.digest(), before);
    assert!(fresh.try_get("cube.stl").unwrap().is_some());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn concurrent_misses_decode_once() {
    let sha = hex(&rendercache::sha256(&cube_stl(3.0)));
    let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (sha, fetches) = (sha.clone(), Arc::clone(&fetches));
            std::thread::spawn(move || {
                decoded_mesh(&sha, || {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    Ok(cube_stl(3.0))
                })
                .unwrap()
            })
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap().triangles.len(), 12);
    }
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
}

/// The summary cache answers only for the assets it was compiled with.
#[test]
fn object_summaries_are_keyed_by_the_asset_digest() {
    let id = format!("summary-{}", mint_token().unwrap());
    let set = |sha: &str| {
        Some(Arc::new(AssetSet::new([Asset { name: "a.stl".into(), sha256: sha.into(), mesh: Arc::new(TriMesh::default()) }])))
    };
    crate::assets::with_assets(set("1"), || crate::agent::remember_compile_failure(&id, 3, "boom"));
    assert!(crate::assets::with_assets(set("1"), || crate::agent::object_cache(&id, 3)).is_some());
    assert!(crate::assets::with_assets(set("2"), || crate::agent::object_cache(&id, 3)).is_none());
    assert!(crate::agent::object_cache(&id, 3).is_none());
}

/// Over the deployment ceiling, an upload evicts the assets of the least
/// recently used other design instead of refusing everyone.
#[test]
fn a_full_store_evicts_least_recently_used_assets() {
    let (state, root) = temporary_state();
    let Store::Filesystem(store) = &state.workspaces else { unreachable!() };
    let processed = process_upload("cube.stl", &cube_stl(10.0), 1000, unix_millis()).unwrap();
    let size = processed.meta.bytes;
    let (old, new) = (
        state.workspaces.create("cube(1);".into(), None).unwrap().id,
        state.workspaces.create("cube(2);".into(), None).unwrap().id,
    );
    store.assets.put(&old, &processed, size, &[]).unwrap();
    let other = process_upload("other.stl", &cube_stl(4.0), 1000, unix_millis()).unwrap();
    assert_eq!(store.assets.put(&new, &other, size, &[]).unwrap_err().status, 503, "nothing to evict");
    let (_, evicted) = store.assets.put(&new, &other, size, &[old.clone(), new.clone()]).unwrap();
    assert_eq!(evicted, vec![old.clone()]);
    assert!(store.assets.list(&old).is_empty());
    assert_eq!(store.assets.version(&old), 2, "the eviction bumps the version browsers watch");
    assert_eq!(store.assets.list(&new).len(), 1);
    let _ = fs::remove_dir_all(root);
}

/// A read-only link exports and checks with the design's assets, and
/// copying it copies them.
#[test]
fn shared_links_compile_and_copy_with_assets() {
    let (state, root) = temporary_state();
    let code = "import(\"cube.stl\");\n";
    let id = state.workspaces.create(code.into(), None).unwrap().id;
    let (status, _) = serve(&state, raw("PUT", &format!("/api/designs/{id}/assets/cube.stl"), None, &cube_stl(10.0)));
    assert_eq!(status, 200);
    let token = state.workspaces.share_token(&id).unwrap();

    let export = |extra: Value| {
        let mut body = json!({"code": code, "format": "stl"});
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serve(&state, raw("POST", "/api/export", Some("application/json"), body.to_string().as_bytes()))
    };
    let (status, body) = export(json!({"shareToken": token}));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert!(!String::from_utf8_lossy(&body).contains(&id));
    let (status, _) = export(json!({}));
    assert_ne!(status, 200, "without the link the import has nothing to read");

    let (status, body) = serve(
        &state,
        raw("POST", &format!("/api/shared/{token}/clone"), Some("application/json"), json!({"code": code}).to_string().as_bytes()),
    );
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let copy = json_of(&body)["designId"].as_str().unwrap().to_string();
    assert_ne!(copy, id);
    let copied = state.workspaces.asset_list(&copy).unwrap();
    assert_eq!(copied.len(), 1);
    assert_eq!(copied[0].sha256, state.workspaces.asset_list(&id).unwrap()[0].sha256);
    let (status, _) = serve(&state, raw("POST", "/api/shared/nope/clone", Some("application/json"), b"{}"));
    assert_eq!(status, 404);
    let _ = fs::remove_dir_all(root);
}

/// A chunk PUT with `Content-Range` (and an `Upload-Id` once known).
fn chunk(path: &str, bytes: &[u8], start: usize, end: usize, upload_id: Option<&str>) -> Vec<u8> {
    let mut out = format!(
        "PUT {path} HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{}/{}\r\n",
        end - start,
        end - 1,
        bytes.len()
    );
    if let Some(upload_id) = upload_id {
        out.push_str(&format!("Upload-Id: {upload_id}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(&bytes[start..end]);
    out
}

const MIB: usize = 1024 * 1024;

/// Held by tests that take upload slots (bodies over 2 MiB), which are
/// counted process-wide, so they cannot starve each other's slots.
static SLOT_TESTS: Mutex<()> = Mutex::new(());

fn slot_test() -> std::sync::MutexGuard<'static, ()> {
    SLOT_TESTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn content_ranges_parse_and_are_limited() {
    assert_eq!(parse_content_range("bytes 0-9/100"), Some(ContentRange { start: 0, end: 10, total: 100 }));
    assert_eq!(parse_content_range(" bytes  90-99/100 "), Some(ContentRange { start: 90, end: 100, total: 100 }));
    for bad in ["bytes 0-9/*", "bytes */100", "bytes 9-0/100", "bytes 0-100/100", "items 0-9/100", "bytes -1-9/100", "bytes 0-9"] {
        assert_eq!(parse_content_range(bad), None, "{bad}");
    }
    let header = |value: &str| vec![("content-range".to_string(), value.to_string())];
    assert_eq!(upload_range(&[], 5).unwrap(), None);
    let total = meshimport::MAX_UPLOAD_BYTES + 1;
    assert_eq!(upload_range(&header(&format!("bytes 0-{}/{total}", MIB - 1)), MIB).unwrap_err().status, 413, "total over the limit");
    let big = MAX_UPLOAD_CHUNK_BYTES + 1;
    assert_eq!(upload_range(&header(&format!("bytes 0-{}/{}", big - 1, 100 * MIB)), big).unwrap_err().status, 413, "chunk too big");
    assert_eq!(upload_range(&header("bytes 0-9/100"), 10).unwrap_err().status, 400, "tiny non-final chunk");
    assert_eq!(upload_range(&header("bytes 0-9/10"), 11).unwrap_err().status, 400, "length mismatch");
    assert_eq!(upload_range(&header("bytes 0-99/abc"), 100).unwrap_err().status, 400);
    assert!(upload_range(&header("bytes 90-99/100"), 10).unwrap().is_some(), "a short last chunk is fine");
}

/// Three chunks (filesystem store) store exactly what one PUT stores;
/// intermediate chunks answer 202 with the next offset.
#[test]
fn chunked_upload_matches_a_single_put() {
    let _slots = slot_test();
    let (state, root) = temporary_state();
    let whole = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let chunked = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let stl = grid_cube_stl(66, 20.0);
    assert!(stl.len() > 2 * MIB && stl.len() < 3 * MIB, "{}", stl.len());
    let (status, body) = serve(&state, raw("PUT", &format!("/api/designs/{whole}/assets/scan.stl?budget=5000"), None, &stl));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let expected = json_of(&body);

    let path = format!("/api/designs/{chunked}/assets/scan.stl?budget=5000");
    let (status, body) = serve(&state, chunk(&path, &stl, 0, MIB, None));
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let first = json_of(&body);
    assert_eq!(first["next"], MIB);
    assert_eq!(first["total"], stl.len());
    let upload_id = first["uploadId"].as_str().unwrap().to_string();
    assert!(valid_token(&upload_id));
    let Store::Filesystem(store) = &state.workspaces else { unreachable!() };
    assert_eq!(fs::metadata(store.assets.part_path(&upload_id)).unwrap().len(), MIB as u64, "staged on disk");
    let (status, body) = serve(&state, chunk(&path, &stl, MIB, 2 * MIB, Some(&upload_id)));
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    assert_eq!(json_of(&body)["next"], 2 * MIB);
    assert!(state.workspaces.asset_list(&chunked).unwrap().is_empty(), "nothing stored before the last chunk");
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let reply = json_of(&body);
    for field in ["sha256", "bytes", "triangles", "report", "description", "import"] {
        assert_eq!(reply[field], expected[field], "{field}");
    }
    assert_eq!(reply["designId"], chunked.as_str());
    assert!(reply["triangles"].as_u64().unwrap() <= 5000, "the first chunk's budget applies");
    assert!(!store.assets.part_path(&upload_id).exists(), "parts are removed once stored");
    // The last chunk again (its reply was lost): the same reply, from the
    // receipt, without storing anything twice.
    let version = state.workspaces.asset_list(&chunked).unwrap().len();
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(without_clock(json_of(&body)), without_clock(reply.clone()), "the replayed reply is the original, but for the time left");
    assert_eq!(state.workspaces.asset_list(&chunked).unwrap().len(), version);
    // Anything but that chunk finds the session gone.
    let (status, _) = serve(&state, chunk(&path, &stl, MIB, 2 * MIB, Some(&upload_id)));
    assert_eq!(status, 404, "the session is gone");
    let other = format!("/api/designs/{chunked}/assets/other.stl");
    let (status, _) = serve(&state, chunk(&other, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 404, "a receipt answers only its own target");
    // The whole file in one request with a Content-Range needs no session.
    let whole = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let (status, body) = serve(&state, chunk(&format!("/api/designs/{whole}/assets/cube.stl"), &cube_stl(9.25), 0, 684, None));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let _ = fs::remove_dir_all(root);
}

/// A last chunk that arrives while another request processes the same
/// file is told to come back (503, retryable), then gets the stored reply;
/// a receipt expires.
#[test]
fn a_concurrent_last_chunk_waits_for_the_finisher() {
    let _slots = slot_test();
    let (state, root) = temporary_state();
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let stl = grid_cube_stl(66, 22.0);
    let path = format!("/api/designs/{id}/assets/scan.stl");
    let (status, body) = serve(&state, chunk(&path, &stl, 0, 2 * MIB, None));
    assert_eq!(status, 202);
    let upload_id = json_of(&body)["uploadId"].as_str().unwrap().to_string();
    // Another request (here: the test) holds the finish claim.
    let staged = state.workspaces.upload_session_append(&upload_id, 2 * MIB as u64, &stl[2 * MIB..], unix_millis()).unwrap();
    assert_eq!(staged, stl.len() as u64);
    assert!(state.workspaces.upload_session_claim_finish(&upload_id, unix_millis()).unwrap());
    assert!(!state.workspaces.upload_session_claim_finish(&upload_id, unix_millis()).unwrap(), "one claim at a time");
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 503, "{}", String::from_utf8_lossy(&body));
    let busy = json_of(&body);
    assert_eq!(busy["reason"], "upload_finishing");
    assert_eq!(busy["retryable"], true);
    assert!(state.workspaces.asset_list(&id).unwrap().is_empty(), "the loser processed nothing");
    // A same-client restart does not tear the session out from under its finisher.
    let Store::Filesystem(store) = &state.workspaces else { unreachable!() };
    assert!(store.assets.part_path(&upload_id).exists());
    // A claim that failed retryably is given back; the re-sent chunk finishes.
    state.workspaces.upload_session_release_finish(&upload_id);
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let reply = json_of(&body);
    let receipt = state.workspaces.upload_receipt(&upload_id, unix_millis()).unwrap().expect("a receipt is kept");
    // Compared as served (serde_json's default float parsing is not exact).
    assert_eq!(json_of(receipt.response.to_string().as_bytes()), reply);
    assert!(state.workspaces.upload_receipt(&upload_id, unix_millis() + UPLOAD_RECEIPT_TTL_MS).unwrap().is_none(), "receipts expire");
    assert!(finish_claimed(unix_millis(), unix_millis()));
    assert!(!finish_claimed(unix_millis() - UPLOAD_FINISH_CLAIM_MS, unix_millis()), "a stale claim can be retaken");
    assert!(!finish_claimed(0, unix_millis()));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn upload_receipts_are_bounded() {
    let root = std::env::temp_dir().join(format!("reopenscad-receipts-{}", mint_token().unwrap()));
    let assets = FsAssets::new(root.clone());
    let receipt = |index: usize, created_at: u64| UploadReceipt {
        upload_id: format!("{index:032x}"),
        target: "design:x/a.stl".into(),
        total: 10,
        final_start: 0,
        response: json!({"n": index}),
        created_at,
    };
    let now = unix_millis();
    for index in 0..MAX_UPLOAD_RECEIPTS + 5 {
        assets.complete_upload(receipt(index, now));
    }
    assert_eq!(assets.receipts.lock().unwrap().len(), MAX_UPLOAD_RECEIPTS);
    assert!(assets.upload_receipt(&format!("{:032x}", 0), now).is_none(), "the oldest went first");
    assert!(assets.upload_receipt(&format!("{:032x}", MAX_UPLOAD_RECEIPTS + 4), now).is_some());
    assets.complete_upload(receipt(9999, now + UPLOAD_RECEIPT_TTL_MS));
    assert_eq!(assets.receipts.lock().unwrap().len(), 1, "expired receipts are dropped on the next insert");
    let _ = fs::remove_dir_all(root);
}

/// A chunk at the wrong offset is answered 409 with `next`; resuming from
/// there, and re-sending the chunk just stored, both work.
#[test]
fn out_of_order_chunks_get_409_and_resume() {
    let _slots = slot_test();
    let (state, root) = temporary_state();
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let stl = grid_cube_stl(66, 21.0);
    let path = format!("/api/designs/{id}/assets/scan.stl");
    let (status, body) = serve(&state, chunk(&path, &stl, 0, MIB, None));
    assert_eq!(status, 202);
    let upload_id = json_of(&body)["uploadId"].as_str().unwrap().to_string();
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));
    let conflict = json_of(&body);
    assert_eq!(conflict["next"], MIB);
    assert_eq!(conflict["uploadId"], upload_id.as_str());
    assert!(conflict["error"].as_str().unwrap().contains("in order"));
    let (status, _) = serve(&state, chunk(&path, &stl, MIB, 2 * MIB, None));
    assert_eq!(status, 400, "a later chunk needs the Upload-Id");
    let (status, _) = serve(&state, chunk(&path, &stl, MIB, 2 * MIB, Some("0123456789abcdef0123456789abcdef")));
    assert_eq!(status, 404, "unknown session");
    let (status, _) = serve(&state, chunk(&format!("/api/designs/{id}/assets/other.stl"), &stl, MIB, 2 * MIB, Some(&upload_id)));
    assert_eq!(status, 404, "a session belongs to one target");
    let (status, body) = serve(&state, chunk(&path, &stl, MIB, 2 * MIB, Some(&upload_id)));
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let (status, body) = serve(&state, chunk(&path, &stl, MIB, 2 * MIB, Some(&upload_id)));
    assert_eq!(status, 202, "re-sending the last chunk is harmless: {}", String::from_utf8_lossy(&body));
    assert_eq!(json_of(&body)["next"], 2 * MIB);
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(json_of(&body)["report"]["inputTriangles"], 6 * 66 * 66 * 2);
    let _ = fs::remove_dir_all(root);
}

/// Oversized chunks and totals are refused on the head, before the body.
#[test]
fn oversized_chunks_and_totals_are_refused_on_the_head() {
    let (state, root) = temporary_state();
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let head = |range: &str, length: usize| {
        format!("PUT /api/designs/{id}/assets/a.stl HTTP/1.1\r\nHost: 127.0.0.1:5173\r\nContent-Length: {length}\r\nContent-Range: {range}\r\n\r\n")
            .into_bytes()
    };
    let big = MAX_UPLOAD_CHUNK_BYTES + MIB;
    let (status, body) = serve(&state, head(&format!("bytes 0-{}/{}", big - 1, 100 * MIB), big));
    assert_eq!(status, 413, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("24 MiB"));
    let total = meshimport::MAX_UPLOAD_BYTES + 1;
    let (status, body) = serve(&state, head(&format!("bytes 0-{}/{total}", 24 * MIB - 1), 24 * MIB));
    assert_eq!(status, 413, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("128 MiB"));
    let (status, _) = serve(&state, head("bytes 0-99/1000", 100));
    assert_eq!(status, 400, "tiny middle chunk");
    let (status, _) = serve(&state, head("nonsense", 100));
    assert_eq!(status, 400);
    let _ = fs::remove_dir_all(root);
}

fn session(upload_id: &str, target: &str, design: &str, client: &str, total: u64, now: u64) -> UploadSession {
    UploadSession {
        upload_id: upload_id.into(),
        target: target.into(),
        design_id: design.into(),
        name: "a.stl".into(),
        budget: 1000,
        total,
        received: 0,
        client: client.into(),
        updated_at: now,
    }
}

/// Sessions are capped per client (count and bytes), per design and in
/// total; a one-time URL takes one session; the same client restarting a
/// target replaces its old session.
#[test]
fn upload_sessions_are_capped() {
    let (state, root) = temporary_state();
    let Store::Filesystem(store) = &state.workspaces else { unreachable!() };
    let assets = &store.assets;
    let token = |index: usize| format!("{index:032x}");
    let total = 100 * MIB as u64;
    assets.open_upload(&session(&token(1), "design:d1/a.stl", "d1", "ip-a", total, 0), 0).unwrap();
    assets.open_upload(&session(&token(2), "design:d2/a.stl", "d2", "ip-a", total, 0), 0).unwrap();
    let third = assets.open_upload(&session(&token(3), "design:d3/a.stl", "d3", "ip-a", 2 * MIB as u64, 0), 0).unwrap_err();
    assert_eq!(third.status, 429, "{}", third.message);
    assert!(third.message.contains("You already have 2"), "{}", third.message);
    // Restarting the same target replaces the old session (and its part).
    assert!(assets.part_path(&token(1)).exists());
    assets.open_upload(&session(&token(4), "design:d1/a.stl", "d1", "ip-a", total, 0), 0).unwrap();
    assert!(assets.upload_session(&token(1), 0).is_none() && !assets.part_path(&token(1)).exists());
    // Per design, whoever the clients are.
    assets.open_upload(&session(&token(5), "design:d1/b.stl", "d1", "ip-b", MIB as u64 * 2, 0), 0).unwrap();
    let design = assets.open_upload(&session(&token(6), "design:d1/c.stl", "d1", "ip-c", MIB as u64 * 2, 0), 0).unwrap_err();
    assert_eq!(design.status, 429);
    assert!(design.message.contains("This design"), "{}", design.message);
    // A one-time URL is bound to its first session.
    assets.open_upload(&session(&token(7), "token:t1", "d7", "ip-d", MIB as u64 * 2, 0), 0).unwrap();
    let bound = assets.open_upload(&session(&token(8), "token:t1", "d7", "ip-e", MIB as u64 * 2, 0), 0).unwrap_err();
    assert_eq!(bound.status, 409, "{}", bound.message);
    let _ = fs::remove_dir_all(root);

    // Bytes: per client and in total.
    let live = [session("x", "design:d/a.stl", "d", "ip-a", 200 * MIB as u64, 0)];
    let refusal = session_refusal(&live, &session("y", "design:e/a.stl", "e", "ip-a", 100 * MIB as u64, 0), MAX_STAGED_BYTES).unwrap();
    assert_eq!(refusal.status, 429);
    assert!(refusal.message.contains("256 MiB"), "{}", refusal.message);
    assert!(session_refusal(&live, &session("y", "design:e/a.stl", "e", "ip-b", 100 * MIB as u64, 0), MAX_STAGED_BYTES).is_none());
    let full = session_refusal(&live, &session("y", "design:e/a.stl", "e", "ip-b", 100 * MIB as u64, 0), 250 * MIB as u64).unwrap();
    assert_eq!(full.status, 503);
}

/// Staged parts are named only by server-minted ids, never follow a
/// planted symlink, and the restart cleanup removes a symlinked staging
/// directory without touching what it points to.
#[cfg(unix)]
#[test]
fn staging_never_leaves_the_parts_directory() {
    use std::os::unix::fs::symlink;
    let base = std::env::temp_dir().join(format!("reopenscad-staging-{}", mint_token().unwrap()));
    let (root, outside) = (base.join("data"), base.join("outside"));
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), b"keep").unwrap();
    symlink(&outside, root.join(UPLOAD_PARTS_DIR)).unwrap();
    let assets = FsAssets::new(root.clone());
    assert!(fs::symlink_metadata(root.join(UPLOAD_PARTS_DIR)).is_err(), "the link is removed");
    assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep", "its target is untouched");

    // Names come from the id alone; a non-token id never names a file.
    assert_eq!(assets.part_path("../../etc/passwd"), root.join(UPLOAD_PARTS_DIR).join("invalid.part"));
    let id = format!("{:032x}", 77);
    assert_eq!(assets.part_path(&id), root.join(UPLOAD_PARTS_DIR).join(format!("{id}.part")));
    let bad = assets.open_upload(&session("../x", "design:d/a.stl", "d", "ip", 2 * MIB as u64, 0), 0).unwrap_err();
    assert_eq!(bad.status, 500);

    // A symlinked staging directory planted while running is refused.
    symlink(&outside, root.join(UPLOAD_PARTS_DIR)).unwrap();
    assert!(assets.open_upload(&session(&id, "design:d/a.stl", "d", "ip", 2 * MIB as u64, 0), 0).is_err());
    assert!(!outside.join(format!("{id}.part")).exists(), "nothing written through the link");
    fs::remove_file(root.join(UPLOAD_PARTS_DIR)).unwrap();

    // A part swapped for a symlink is neither written nor read through.
    assets.open_upload(&session(&id, "design:d/a.stl", "d", "ip", 2 * MIB as u64, 0), 0).unwrap();
    fs::remove_file(assets.part_path(&id)).unwrap();
    symlink(outside.join("keep.txt"), assets.part_path(&id)).unwrap();
    assert_eq!(assets.append_upload(&id, 0, &vec![1u8; MIB], 1).unwrap_err().status, 500);
    assert!(assets.upload_bytes(&id).is_err());
    assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep");
    let _ = fs::remove_dir_all(base);
}

/// An idle session expires: it stops answering, and the next session
/// opened sweeps its staged part away.
#[test]
fn idle_upload_sessions_expire_and_are_swept() {
    let (state, root) = temporary_state();
    let Store::Filesystem(store) = &state.workspaces else { unreachable!() };
    let assets = &store.assets;
    let old = format!("{:032x}", 11);
    assets.open_upload(&session(&old, "design:d1/a.stl", "d1", "ip-a", 3 * MIB as u64, 0), 0).unwrap();
    assert_eq!(assets.append_upload(&old, 0, &vec![7u8; MIB], 1_000).unwrap(), MIB as u64);
    assert!(assets.upload_session(&old, UPLOAD_SESSION_IDLE_MS).is_some(), "a chunk keeps it alive");
    let later = 1_000 + UPLOAD_SESSION_IDLE_MS;
    assert!(assets.upload_session(&old, later).is_none(), "expired");
    assert_eq!(assets.append_upload(&old, MIB as u64, &vec![7u8; MIB], later).unwrap_err().status, 404);
    assert!(assets.part_path(&old).exists(), "not swept yet");
    assets.open_upload(&session(&format!("{:032x}", 12), "design:d2/a.stl", "d2", "ip-b", 3 * MIB as u64, later), later).unwrap();
    assert!(!assets.part_path(&old).exists(), "swept by the next open");
    // A restart drops every staged part.
    drop(state);
    let reopened = FsAssets::new(root.clone());
    assert!(!root.join(UPLOAD_PARTS_DIR).exists());
    drop(reopened);
    let _ = fs::remove_dir_all(root);
}

/// A one-time URL stays valid across every chunk of its upload, is used
/// up by the stored file, and its reply never names the design.
#[test]
fn one_time_urls_take_chunked_uploads() {
    let _slots = slot_test();
    let (state, root) = temporary_state();
    let workspace = state.workspaces.create("cube(1);".into(), None).unwrap();
    let request = mcp_request();
    let created = crate::mcp_call_tool(&state, &request, "create_asset_upload", json!({"designId": workspace.id, "name": "scan.stl", "includeScript": true}));
    assert_eq!(created["isError"], false, "{created}");
    let url = created["structuredContent"]["uploadUrl"].as_str().unwrap().to_string();
    let script = created["structuredContent"]["chunkedScript"].as_str().unwrap();
    assert!(script.contains(&url) && script.contains("Content-Range") && script.contains("Upload-Id"), "{script}");
    assert_eq!(created["structuredContent"]["chunkBytes"], UPLOAD_CHUNK_BYTES);
    let path = url.split_once("5173").unwrap().1.to_string();
    let token = path.rsplit('/').next().unwrap().to_string();
    let stl = grid_cube_stl(66, 22.0);
    let (status, body) = serve(&state, chunk(&path, &stl, 0, MIB, None));
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let upload_id = json_of(&body)["uploadId"].as_str().unwrap().to_string();
    assert!(state.workspaces.peek_upload_token(&token).unwrap().is_some(), "not consumed by a chunk");
    let (status, body) = serve(&state, chunk(&path, &stl, MIB, 2 * MIB, Some(&upload_id)));
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let reply = json_of(&body);
    assert_eq!(reply["name"], "scan.stl");
    assert!(reply.get("designId").is_none() && !String::from_utf8_lossy(&body).contains(&workspace.id));
    assert_eq!(state.workspaces.asset_list(&workspace.id).unwrap().len(), 1);
    // Refused on the head (send only the head, so the unread body cannot
    // reset the connection before the reply is read).
    let mut head = chunk(&path, &stl, 0, MIB, None);
    head.truncate(head.len() - MIB);
    let (status, _) = serve(&state, head);
    assert_eq!(status, 404, "used up");
    // ...except for a re-send of the finished upload's last chunk.
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(without_clock(json_of(&body)), without_clock(reply.clone()));
    assert_eq!(state.workspaces.asset_list(&workspace.id).unwrap().len(), 1);
    let mut middle = chunk(&path, &stl, MIB, 2 * MIB, Some(&upload_id));
    middle.truncate(middle.len() - MIB);
    let (status, _) = serve(&state, middle);
    assert_eq!(status, 404, "only the last chunk replays");
    let _ = fs::remove_dir_all(root);
}

/// A last chunk whose file is rejected drops the session but leaves the
/// one-time URL usable.
#[test]
fn a_rejected_chunked_file_leaves_the_url_usable() {
    let (state, root) = temporary_state();
    let workspace = state.workspaces.create("cube(1);".into(), None).unwrap();
    let created = crate::mcp_call_tool(&state, &mcp_request(), "create_asset_upload", json!({"designId": workspace.id, "name": "scan.stl", "includeScript": true}));
    let url = created["structuredContent"]["uploadUrl"].as_str().unwrap().to_string();
    let path = url.split_once("5173").unwrap().1.to_string();
    let junk = vec![b'x'; MIB + 10];
    let (status, body) = serve(&state, chunk(&path, &junk, 0, MIB, None));
    assert_eq!(status, 202);
    let upload_id = json_of(&body)["uploadId"].as_str().unwrap().to_string();
    let (status, _) = serve(&state, chunk(&path, &junk, MIB, junk.len(), Some(&upload_id)));
    assert_eq!(status, 400);
    let (status, _) = serve(&state, chunk(&path, &junk, MIB, junk.len(), Some(&upload_id)));
    assert_eq!(status, 404, "a rejected file ends its session");
    let (status, body) = serve(&state, raw("PUT", &path, None, &cube_stl(4.75)));
    assert_eq!(status, 200, "the URL still works: {}", String::from_utf8_lossy(&body));
    let _ = fs::remove_dir_all(root);
}

/// The whole chunked protocol against the Postgres store (opt-in, like
/// the pgstore tests: set REOPENSCAD_TEST_DATABASE_URL).
#[cfg(feature = "postgres")]
#[test]
fn chunked_upload_through_postgres() {
    let Ok(url) = std::env::var("REOPENSCAD_TEST_DATABASE_URL") else { return };
    let _slots = slot_test();
    let (mut state, root) = temporary_state();
    state.workspaces = Store::Postgres(crate::pgstore::PgWorkspaceStore::open(&url).unwrap());
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let stl = grid_cube_stl(66, 23.0);
    let path = format!("/api/designs/{id}/assets/scan.stl");
    let (status, body) = serve(&state, chunk(&path, &stl, 0, MIB, None));
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let upload_id = json_of(&body)["uploadId"].as_str().unwrap().to_string();
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 409);
    assert_eq!(json_of(&body)["next"], MIB);
    let (status, _) = serve(&state, chunk(&path, &stl, MIB, 2 * MIB, Some(&upload_id)));
    assert_eq!(status, 202);
    let (status, body) = serve(&state, chunk(&path, &stl, 2 * MIB, stl.len(), Some(&upload_id)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(json_of(&body)["report"]["inputTriangles"], 6 * 66 * 66 * 2);
    assert_eq!(state.workspaces.asset_list(&id).unwrap().len(), 1);
    assert!(state.workspaces.upload_session(&upload_id, unix_millis()).unwrap().is_none(), "session dropped");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn upload_routes_are_recognised() {
    let id = "bouncy-badger-0123456789abcdef0123456789";
    assert!(is_upload_route("PUT", &format!("/api/designs/{id}/assets/scan.stl")));
    assert!(is_upload_route("PUT", &format!("/api/workspaces/{id}/assets/scan.stl")));
    assert!(is_upload_route("PUT", "/api/uploads/0123456789abcdef0123456789abcdef"));
    assert!(!is_upload_route("GET", &format!("/api/designs/{id}/assets/scan.stl")));
    assert!(!is_upload_route("PUT", &format!("/api/designs/{id}/assets")));
    assert!(!is_upload_route("PUT", &format!("/api/designs/{id}/preview")));
    assert!(!is_upload_route("PUT", "/api/uploads/nope"));
}

// -----------------------------------------------------------------------
// Expiry, one STL per design, the render note.
// -----------------------------------------------------------------------

/// A reply without what changes as time passes.
fn without_clock(mut reply: Value) -> Value {
    if let Some(fields) = reply.as_object_mut() {
        for key in ["expiresInSeconds", "expiresIn", "expiry"] {
            fields.remove(key);
        }
    }
    reply
}

fn render_body(state: &AppState, id: &str) -> Vec<u8> {
    json!({"mode": "preview", "requestId": "expiry-test-render", "revision": revision_of(state, id)}).to_string().into_bytes()
}

/// Moves design `id`'s stored assets `ms` into the past, as if that much
/// time had gone by.
fn age_assets(state: &AppState, id: &str, ms: u64) {
    let Store::Filesystem(store) = &state.workspaces else { panic!("filesystem store") };
    let _guard = store.assets.lock();
    let mut index = store.assets.read_index(id);
    for meta in &mut index.assets {
        meta.expires_at = meta.expiry() - ms;
        meta.created_at -= ms;
    }
    store.assets.write_atomic(&store.assets.dir(id).join("index.json"), &serde_json::to_vec(&index).unwrap()).unwrap();
}

fn mcp(state: &AppState, tool: &str, arguments: Value) -> Value {
    crate::mcp_call_tool(state, &mcp_request(), tool, arguments)
}

fn put_stl(state: &AppState, id: &str, name: &str, size: f32) -> (u16, Value) {
    let (status, body) = serve(state, raw("PUT", &format!("/api/designs/{id}/assets/{name}"), None, &cube_stl(size)));
    (status, json_of(&body))
}

fn with_upload(code: &str) -> (AppState, PathBuf, String) {
    let (state, root) = temporary_state();
    let id = state.workspaces.create(code.into(), None).unwrap().id;
    let (status, reply) = put_stl(&state, &id, "scan.stl", 10.0);
    assert_eq!(status, 200, "{reply}");
    (state, root, id)
}

fn revision_of(state: &AppState, id: &str) -> u64 {
    state.workspaces.get(id).unwrap().unwrap().revision
}

const IMPORTING: &str = "import(\"scan.stl\");\ncube(5);\n";
const HOUR: u64 = 60 * 60 * 1000;

#[test]
fn an_upload_states_when_it_expires_and_is_gone_from_every_read_at_that_moment() {
    let (state, root) = temporary_state();
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let before = unix_millis();
    let (status, reply) = put_stl(&state, &id, "scan.stl", 10.0);
    assert_eq!(status, 200, "{reply}");
    let expires = reply["expiresAt"].as_u64().unwrap();
    assert!(expires >= before + ASSET_TTL_MS && expires <= unix_millis() + ASSET_TTL_MS, "{reply}");
    assert!(reply["expiry"].as_str().unwrap().contains(GUIDE_TOOL), "{reply}");
    assert_eq!(reply["expiresIn"], "2h 59m", "{reply}");
    assert!(reply.get("sealed").is_none() && reply["report"].get("sealed").is_none() && reply["report"].get("holesFilled").is_none(), "{reply}");

    let (_, listed) = serve(&state, raw("GET", &format!("/api/designs/{id}/assets"), None, b""));
    let listed = json_of(&listed);
    assert_eq!(listed["assets"][0]["expiresAt"].as_u64(), Some(expires), "{listed}");
    assert!(listed["assets"][0].get("derived").is_none(), "{listed}");
    assert!(listed["serverTime"].as_u64().unwrap() >= before, "{listed}");

    // A millisecond before: still there. At the moment: gone, unswept.
    age_assets(&state, &id, ASSET_TTL_MS - 60_000);
    assert_eq!(state.workspaces.asset_list(&id).unwrap().len(), 1);
    age_assets(&state, &id, 60_000);
    assert!(state.workspaces.asset_list(&id).unwrap().is_empty());
    let (_, listed) = serve(&state, raw("GET", &format!("/api/designs/{id}/assets"), None, b""));
    let listed = json_of(&listed);
    assert_eq!(listed["assets"], json!([]), "{listed}");
    assert_eq!(listed["expired"][0]["name"], "scan.stl", "{listed}");
    let (status, body) = serve(&state, raw("GET", &format!("/api/designs/{id}/assets/scan.stl"), None, b""));
    assert_eq!(status, 404);
    assert!(String::from_utf8_lossy(&body).contains("expired"), "{}", String::from_utf8_lossy(&body));
    let deleted = mcp(&state, "delete_asset", json!({"designId": id, "name": "scan.stl"}));
    assert_eq!(deleted["isError"], true, "{deleted}");
    assert!(deleted["content"][0]["text"].as_str().unwrap().contains("expired"), "{deleted}");
    // Nothing holds the name: a fresh upload starts a new clock.
    let (status, again) = put_stl(&state, &id, "scan.stl", 10.0);
    assert_eq!(status, 200, "{again}");
    assert!(again["expiresAt"].as_u64().unwrap() >= before + ASSET_TTL_MS, "{again}");
    let _ = fs::remove_dir_all(root);
}

/// Every asset a design can hold is remembered when they all expire: a
/// forgotten one would silently stop failing the import.
#[test]
fn every_expired_name_of_a_full_design_is_remembered() {
    assert!(MAX_EXPIRED_NAMES >= MAX_ASSETS_PER_DESIGN);
    let mut index = FsIndex {
        version: 1,
        assets: (0..MAX_ASSETS_PER_DESIGN).map(|i| AssetMeta { expires_at: 5, ..meta(&format!("m{i}.stl"), 1) }).collect(),
        expired: Vec::new(),
    };
    assert_eq!(drain_expired(&mut index, 10).len(), MAX_ASSETS_PER_DESIGN);
    assert_eq!(index.expired.len(), MAX_ASSETS_PER_DESIGN);
}

#[test]
fn nothing_renews_an_upload() {
    let (state, root, id) = with_upload(IMPORTING);
    let expiry = |state: &AppState| state.workspaces.asset_list(&id).unwrap()[0].expiry();
    let first = expiry(&state);
    let rendered = mcp(&state, "render_design", json!({"designId": id, "format": "stl"}));
    assert_eq!(rendered["isError"], false, "{rendered}");
    let (status, _) = serve(&state, raw("GET", &format!("/api/designs/{id}/assets/scan.stl"), None, b""));
    assert_eq!(status, 200);
    assert_eq!(expiry(&state), first);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_asset_from_before_expiry_counts_from_when_it_was_stored() {
    let (state, root, id) = with_upload("cube(1);");
    let Store::Filesystem(store) = &state.workspaces else { panic!() };
    let mut index = store.assets.read_index(&id);
    index.assets[0].expires_at = 0;
    index.assets[0].created_at = unix_millis() - HOUR;
    store.assets.write_atomic(&store.assets.dir(&id).join("index.json"), &serde_json::to_vec(&index).unwrap()).unwrap();
    let meta = &state.workspaces.asset_list(&id).unwrap()[0];
    assert_eq!(meta.expiry(), meta.created_at + ASSET_TTL_MS);
    let mut index = store.assets.read_index(&id);
    index.assets[0].created_at = unix_millis() - 3 * HOUR - 1;
    store.assets.write_atomic(&store.assets.dir(&id).join("index.json"), &serde_json::to_vec(&index).unwrap()).unwrap();
    assert!(state.workspaces.asset_list(&id).unwrap().is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn the_sweep_frees_mesh_and_metadata_and_leaves_live_designs_alone() {
    let (state, root, id) = with_upload("cube(1);");
    let other = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    assert_eq!(put_stl(&state, &other, "keep.stl", 7.0).0, 200);
    let Store::Filesystem(store) = &state.workspaces else { panic!() };
    let version = state.workspaces.assets_version(&id).unwrap();
    let total_before = { let mut guard = store.assets.lock(); store.assets.total(&mut guard) };

    // Not due: the sweep touches nothing.
    assert_eq!(sweep_expired(&state, unix_millis()).unwrap(), Vec::<(String, Vec<String>)>::new());
    assert_eq!(state.workspaces.asset_list(&id).unwrap().len(), 1);

    age_assets(&state, &id, ASSET_TTL_MS);
    let swept = sweep_expired(&state, unix_millis() + HOUR).unwrap();
    assert_eq!(swept.len(), 1, "{swept:?}");
    assert_eq!(swept[0].0, id);
    assert!(swept[0].1.contains(&"scan.stl".to_string()), "{swept:?}");
    assert!(store.assets.read_index(&id).assets.is_empty(), "metadata is gone from disk too");
    let blobs = fs::read_dir(store.assets.dir(&id)).unwrap().flatten().filter(|entry| entry.path().extension().is_some_and(|e| e == "stl")).count();
    assert_eq!(blobs, 0);
    assert_eq!(state.workspaces.assets_version(&id).unwrap(), version + 1, "browsers watching assetsVersion hear of it");
    assert!({ let mut guard = store.assets.lock(); store.assets.total(&mut guard) } < total_before);
    assert_eq!(state.workspaces.asset_list(&other).unwrap().len(), 1, "another design's live upload stays");
    // Idempotent.
    assert!(sweep_expired(&state, unix_millis() + HOUR).unwrap().is_empty());
    assert_eq!(state.workspaces.assets_version(&id).unwrap(), version + 1);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_expired_import_fails_every_render_with_a_clear_error() {
    let (state, root, id) = with_upload(IMPORTING);
    age_assets(&state, &id, ASSET_TTL_MS);
    for tool in ["evaluate", "render_design", "render_image", "render_workspace"] {
        let reply = mcp(&state, tool, json!({"designId": id}));
        assert_eq!(reply["isError"], true, "{tool}: {reply}");
        let text = reply.to_string();
        assert!(text.contains("expired") && text.contains("scan.stl") && text.contains("Remove the import") && text.contains("upload the file again"), "{tool}: {text}");
    }
    // Swept away, the error is the same: the name is remembered.
    sweep_expired(&state, unix_millis()).unwrap();
    let reply = mcp(&state, "evaluate", json!({"designId": id}));
    assert!(reply.to_string().contains("expired"), "{reply}");
    let (status, body) = serve(&state, raw("POST", &format!("/api/designs/{id}/render"), Some("application/json"), &render_body(&state, &id)));
    assert_ne!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("expired"), "{}", String::from_utf8_lossy(&body));
    // A file never uploaded stays OpenSCAD's warning, not this error.
    let other = state.workspaces.create("import(\"nope.stl\");\ncube(1);\n".into(), None).unwrap().id;
    let reply = mcp(&state, "evaluate", json!({"designId": other}));
    assert_eq!(reply["isError"], false, "{reply}");
    assert!(reply.to_string().contains("Can't open import file"), "{reply}");
    // Uploading again brings it back.
    assert_eq!(put_stl(&state, &id, "scan.stl", 10.0).0, 200);
    let reply = mcp(&state, "evaluate", json!({"designId": id}));
    assert_eq!(reply["isError"], false, "{reply}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_tool_naming_an_expired_asset_says_it_expired() {
    let (state, root, id) = with_upload(IMPORTING);
    age_assets(&state, &id, ASSET_TTL_MS);
    let cases = [
        ("compare_surfaces", json!({"designId": id, "reference": {"asset": "scan.stl"}, "candidate": {"asset": "scan.stl"}})),
        ("extract_features", json!({"designId": id, "from": "scan.stl"})),
        ("scan_report", json!({"designId": id, "scan": "scan.stl", "candidate": {"objectIds": ["object-1"]}})),
    ];
    for (tool, arguments) in cases {
        let reply = mcp(&state, tool, arguments);
        assert_eq!(reply["isError"], true, "{tool}: {reply}");
        let text = reply["structuredContent"]["error"].as_str().unwrap_or_default();
        assert!(text.contains("scan.stl expired at") && text.contains("Upload the file again"), "{tool}: {reply}");
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn writes_keeping_an_expired_import_are_refused_and_writes_removing_it_land() {
    let (state, root, id) = with_upload(IMPORTING);
    // A clean revision to restore later, under the import.
    let clean = revision_of(&state, &id);
    let saved = mcp(&state, "update_design_code", json!({"designId": id, "expectedRevision": clean, "revisionName": "Taller", "code": "import(\"scan.stl\");\ncube(6);\n"}));
    assert_eq!(saved["isError"], false, "{saved}");
    let saved = mcp(&state, "update_design_code", json!({"designId": id, "expectedRevision": revision_of(&state, &id), "revisionName": "No scan", "code": "cube(7);\n"}));
    assert_eq!(saved["isError"], false, "{saved}");
    let without = revision_of(&state, &id);
    let back = mcp(&state, "restore_revision", json!({"designId": id, "revision": without - 1, "expectedRevision": without, "revisionName": "Scan again"}));
    assert_eq!(back["isError"], false, "{back}");
    age_assets(&state, &id, ASSET_TTL_MS);

    let refused = |reply: &Value, what: &str| {
        assert!(reply["isError"] == json!(true) || reply["structuredContent"]["ok"] == json!(false), "{what}: {reply}");
        let text = reply.to_string();
        assert!(text.contains("expired") && text.contains("Remove the import"), "{what}: {text}");
    };
    let current = revision_of(&state, &id);
    // Whatever keeps the import is rejected like any failed compile...
    refused(&mcp(&state, "update_design_code", json!({"designId": id, "expectedRevision": current, "revisionName": "Keep", "edits": [{"search": "cube(6)", "replace": "cube(8)"}]})), "edit");
    refused(&mcp(&state, "update_design_code", json!({"designId": id, "expectedRevision": current, "revisionName": "Keep", "startLine": 2, "endLine": 2, "text": "cube(8);"})), "lines");
    refused(&mcp(&state, "evaluate", json!({"designId": id, "expectedRevision": current, "revisionName": "Keep", "edits": [{"search": "cube(6)", "replace": "cube(8)"}]})), "evaluate");
    refused(&mcp(&state, "restore_revision", json!({"designId": id, "revision": clean, "expectedRevision": current, "revisionName": "Back"})), "restore");
    assert_eq!(revision_of(&state, &id), current, "nothing was saved");

    // ...and each way of removing it lands, though the current revision
    // does not compile.
    let edit = mcp(&state, "update_design_code", json!({"designId": id, "expectedRevision": current, "revisionName": "No import", "edits": [{"search": "import(\"scan.stl\");\n", "replace": ""}]}));
    assert_eq!(edit["isError"], false, "{edit}");
    let code = state.workspaces.get(&id).unwrap().unwrap().code;
    assert!(!code.contains("import("), "{code}");
    let history = mcp(&state, "get_design_code", json!({"designId": id, "revision": current}));
    assert!(history.to_string().contains("import("), "old revisions stay as they are: {history}");

    let (state2, root2, id2) = with_upload(IMPORTING);
    age_assets(&state2, &id2, ASSET_TTL_MS);
    let lines = mcp(&state2, "update_design_code", json!({"designId": id2, "expectedRevision": revision_of(&state2, &id2), "revisionName": "No import", "startLine": 1, "endLine": 1, "text": "// scan gone"}));
    assert_eq!(lines["isError"], false, "{lines}");
    assert!(!state2.workspaces.get(&id2).unwrap().unwrap().code.contains("import("));

    let (state3, root3, id3) = with_upload(IMPORTING);
    age_assets(&state3, &id3, ASSET_TTL_MS);
    let evaluated = mcp(&state3, "evaluate", json!({"designId": id3, "expectedRevision": revision_of(&state3, &id3), "revisionName": "No import", "edits": [{"search": "import(\"scan.stl\");\n", "replace": ""}]}));
    assert_eq!(evaluated["isError"], false, "{evaluated}");

    let (state4, root4, id4) = with_upload("cube(3);\n");
    let plain = revision_of(&state4, &id4);
    let imported = mcp(&state4, "update_design_code", json!({"designId": id4, "expectedRevision": plain, "revisionName": "Scan", "code": IMPORTING}));
    assert_eq!(imported["isError"], false, "{imported}");
    age_assets(&state4, &id4, ASSET_TTL_MS);
    let restored = mcp(&state4, "restore_revision", json!({"designId": id4, "revision": plain, "expectedRevision": revision_of(&state4, &id4), "revisionName": "Before the scan"}));
    assert_eq!(restored["isError"], false, "{restored}");
    for root in [root, root2, root3, root4] {
        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn no_cache_serves_a_render_past_the_expiry_or_the_removal() {
    let (state, root, id) = with_upload(IMPORTING);
    let token = state.workspaces.share_token(&id).unwrap();
    let render_design = |state: &AppState| serve(state, raw("POST", &format!("/api/designs/{id}/render"), Some("application/json"), &render_body(state, &id)));
    let render_shared = |state: &AppState| serve(state, raw("POST", &format!("/api/shared/{token}/render"), Some("application/json"), &render_body(state, &id)));
    let export = |state: &AppState| serve(state, raw("POST", "/api/export", Some("application/json"), json!({"code": IMPORTING, "format": "stl", "shareToken": token}).to_string().as_bytes()));
    let app = |state: &AppState| crate::mcp_read_resource(state, &mcp_request(), &format!("ui://reopenscad/design/{id}"));
    // Fill every cache: the same calls twice, the second a hit.
    for _ in 0..2 {
        assert_eq!(render_design(&state).0, 200);
        assert_eq!(render_shared(&state).0, 200);
        assert_eq!(export(&state).0, 200);
        app(&state).unwrap();
        assert_eq!(mcp(&state, "render_image", json!({"designId": id}))["isError"], false);
        assert_eq!(mcp(&state, "render_design", json!({"designId": id, "format": "stl"}))["isError"], false);
    }
    // Under one key: keys carry the asset digest, so a past render cannot match.
    let with_set = |f: &dyn Fn() -> String| with_design_assets(&state, Some(&id), f);
    let digest = with_set(&crate::assets::current_digest);
    assert!(digest.starts_with("scan.stl="), "{digest}");

    age_assets(&state, &id, ASSET_TTL_MS);
    assert_eq!(with_set(&crate::assets::current_digest), "!scan.stl");
    for (what, (status, body)) in [("design", render_design(&state)), ("shared", render_shared(&state)), ("export", export(&state))] {
        let text = String::from_utf8_lossy(&body);
        assert_ne!(status, 200, "{what}: {text}");
        assert!(text.contains("expired"), "{what}: {text}");
    }
    assert!(app(&state).unwrap_err().contains("expired"));
    assert_eq!(mcp(&state, "render_image", json!({"designId": id}))["isError"], true);
    assert_eq!(mcp(&state, "render_design", json!({"designId": id, "format": "stl"}))["isError"], true);
    let design_read = serve(&state, raw("GET", &format!("/api/designs/{id}?objects=compute"), None, b""));
    assert!(String::from_utf8_lossy(&design_read.1).contains("expired"), "{}", String::from_utf8_lossy(&design_read.1));

    // Removal: the digest moves too, so the mesh is never drawn again.
    let (state2, root2, id2) = with_upload(IMPORTING);
    let (status, body) = serve(&state2, raw("POST", &format!("/api/designs/{id2}/render"), Some("application/json"), &render_body(&state2, &id2)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let (status, _) = serve(&state2, raw("DELETE", &format!("/api/designs/{id2}/assets/scan.stl"), None, b""));
    assert_eq!(status, 200);
    let digest = with_design_assets(&state2, Some(&id2), crate::assets::current_digest);
    assert_eq!(digest, "");
    let evaluated = mcp(&state2, "evaluate", json!({"designId": id2}));
    assert!(evaluated.to_string().contains("Can't open import file"), "{evaluated}");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(root2);
}

#[test]
fn a_copy_keeps_the_clock_of_the_original() {
    let (state, root, id) = with_upload(IMPORTING);
    age_assets(&state, &id, HOUR);
    let expiry = state.workspaces.asset_list(&id).unwrap()[0].expiry();
    let token = state.workspaces.share_token(&id).unwrap();
    let (status, body) = serve(&state, raw("POST", &format!("/api/shared/{token}/clone"), Some("application/json"), b"{}"));
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let copy = json_of(&body)["designId"].as_str().unwrap().to_string();
    assert_eq!(state.workspaces.asset_list(&copy).unwrap()[0].expiry(), expiry);
    // And a design whose import expired cannot be copied, as it cannot compile.
    age_assets(&state, &id, 2 * HOUR);
    let (status, _) = serve(&state, raw("POST", &format!("/api/shared/{token}/clone"), Some("application/json"), b"{}"));
    assert_ne!(status, 201);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_design_holds_one_stl_whatever_the_route() {
    let (state, root, id) = with_upload("cube(1);");
    assert_eq!(list_value(&state, &id, false).unwrap()["limits"]["maxAssets"], 1, "the list states the rule");
    let base64 = crate::mcpapp::base64_encode(&cube_stl(5.0));
    let names_it = |text: &str| assert!(text.contains("scan.stl") && text.contains("delete_asset"), "{text}");
    for name in ["other.stl", "scan.stl"] {
        // The browser route: 409, in the ordinary error body.
        let (status, reply) = put_stl(&state, &id, name, 5.0);
        assert_eq!(status, 409, "{reply}");
        assert_eq!(reply["ok"], false, "{reply}");
        names_it(reply["error"].as_str().unwrap());
        // MCP, inline.
        let reply = mcp(&state, "upload_asset", json!({"designId": id, "name": name, "base64": base64}));
        assert_eq!(reply["isError"], true, "{reply}");
        names_it(reply["content"][0]["text"].as_str().unwrap());
        // MCP, a one-time URL is not even minted.
        let reply = mcp(&state, "create_asset_upload", json!({"designId": id, "name": name}));
        assert_eq!(reply["isError"], true, "{reply}");
        names_it(reply["content"][0]["text"].as_str().unwrap());
        // A browser PUT or a first chunk is refused on its head, before any body.
        let head = Request { method: "PUT".into(), path: format!("/api/designs/{id}/assets/{name}"), query: String::new(), headers: Vec::new(), body: Vec::new() };
        let refused = admit_upload_target(&state, &head).unwrap_err();
        assert_eq!(refused.status, 409);
        names_it(&refused.message);
    }
    // Same bytes, same mesh: nothing was overwritten.
    let sha = &state.workspaces.asset_list(&id).unwrap()[0].sha256;
    assert_eq!(state.workspaces.asset_list(&id).unwrap().len(), 1);
    assert_eq!(*sha, hex(&rendercache::sha256(&process_upload("scan.stl", &cube_stl(10.0), meshimport::DEFAULT_TRIANGLE_BUDGET, 0).unwrap().stl)));
    // Removed, the next upload is welcome under any name.
    assert_eq!(mcp(&state, "delete_asset", json!({"designId": id, "name": "scan.stl"}))["isError"], false);
    assert_eq!(put_stl(&state, &id, "other.stl", 5.0).0, 200);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_pending_url_cannot_get_around_one_stl_per_design() {
    let (state, root) = temporary_state();
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    // Both URLs were minted while the design held nothing.
    let url = |name: &str| {
        let created = mcp(&state, "create_asset_upload", json!({"designId": id, "name": name}));
        assert_eq!(created["isError"], false, "{created}");
        created["structuredContent"]["uploadUrl"].as_str().unwrap().split_once("5173").unwrap().1.to_string()
    };
    let (first, second, same) = (url("first.stl"), url("second.stl"), url("first.stl"));
    let (status, body) = serve(&state, raw("PUT", &first, None, &cube_stl(5.0)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    for path in [&second, &same] {
        let (status, body) = serve(&state, raw("PUT", path, None, &cube_stl(6.0)));
        assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));
        let reply = json_of(&body);
        assert!(reply["error"].as_str().unwrap().contains("first.stl"), "{reply}");
        assert!(!reply.to_string().contains(&id), "a URL holder never learns the design: {reply}");
    }
    assert_eq!(state.workspaces.asset_list(&id).unwrap().len(), 1);

    // A chunked session opened before the other upload finished is refused
    // by its last chunk, whichever route it took.
    let (state, root2) = { let _ = fs::remove_dir_all(&root); temporary_state() };
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let big = grid_cube_stl(150, 10.0);
    let mib = 1024 * 1024;
    assert!(big.len() > 2 * mib, "{}", big.len());
    let path = format!("/api/designs/{id}/assets/slow.stl");
    let (status, body) = serve(&state, chunk(&path, &big, 0, mib, None));
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let upload_id = json_of(&body)["uploadId"].as_str().unwrap().to_string();
    assert_eq!(put_stl(&state, &id, "fast.stl", 5.0).0, 200);
    let (status, body) = serve(&state, chunk(&path, &big, mib, big.len(), Some(&upload_id)));
    assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("fast.stl"));
    assert_eq!(state.workspaces.asset_list(&id).unwrap().len(), 1);
    let _ = fs::remove_dir_all(root2);
}

#[test]
fn the_store_settles_a_race_between_two_uploads() {
    let (state, root) = temporary_state();
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    // Both passed the early checks; only the store's lock decides.
    let processed = |name: &str, size: f32| process_upload(name, &cube_stl(size), 1000, unix_millis()).unwrap();
    state.workspaces.asset_put(&id, &processed("a.stl", 5.0)).unwrap();
    let refused = state.workspaces.asset_put(&id, &processed("b.stl", 6.0)).unwrap_err();
    assert_eq!(refused.status, 409);
    assert!(refused.message.contains("a.stl"), "{}", refused.message);
    let refused = state.workspaces.asset_put(&id, &processed("a.stl", 7.0)).unwrap_err();
    assert_eq!(refused.status, 409, "{}", refused.message);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn every_render_of_a_live_import_says_how_long_is_left_and_names_the_guide() {
    let (state, root, id) = with_upload(IMPORTING);
    age_assets(&state, &id, HOUR);
    for (tool, arguments) in [
        ("evaluate", json!({"designId": id})),
        ("render_design", json!({"designId": id, "format": "stl"})),
        ("render_image", json!({"designId": id})),
        ("get_print_layout", json!({"designId": id})),
        ("export_print_parts", json!({"designId": id})),
        ("render_workspace", json!({"workspaceId": id, "format": "stl"})),
    ] {
        let reply = mcp(&state, tool, arguments);
        assert_eq!(reply["isError"], false, "{tool}: {reply}");
        let note = &reply["structuredContent"]["liveStl"];
        assert_eq!(note["files"], json!(["scan.stl"]), "{tool}: {reply}");
        assert!(note["expiresAt"].as_u64().unwrap() > unix_millis(), "{tool}: {note}");
        assert_eq!(note["expiresIn"], "1h 59m", "{tool}: {note}");
        assert_eq!(note["guide"], GUIDE_TOOL, "{tool}: {note}");
        let lines = reply["content"].as_array().unwrap();
        let line = lines.iter().filter_map(|part| part["text"].as_str()).find(|text| text.contains(GUIDE_TOOL)).unwrap_or_else(|| panic!("{tool}: {reply}"));
        assert!(line.contains("1h 59m") && line.to_lowercase().contains("reverse-engineer") && line.contains("remove"), "{tool}: {line}");
        // The colour rule lives in the guide alone.
        assert!(!line.to_lowercase().contains("colour") && !line.to_lowercase().contains("color"), "{tool}: {line}");
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn the_note_follows_the_code_not_the_upload() {
    // Held but not imported, or imported only in a comment: no note.
    for code in ["cube(1);\n", "// import(\"scan.stl\");\n/* import(\"scan.stl\"); */\ncube(1);\n"] {
        let (state, root, id) = with_upload(code);
        let reply = mcp(&state, "render_image", json!({"designId": id}));
        assert_eq!(reply["isError"], false, "{reply}");
        assert!(reply["structuredContent"].get("liveStl").is_none(), "{code}: {reply}");
        let _ = fs::remove_dir_all(root);
    }
    // Through a variable: a note.
    let (state, root, id) = with_upload("file = \"./scan.stl\";\nimport(file);\n");
    let reply = mcp(&state, "render_image", json!({"designId": id}));
    assert_eq!(reply["structuredContent"]["liveStl"]["files"], json!(["scan.stl"]), "{reply}");
    // Not on a reply that is not a render.
    let read = mcp(&state, "get_design_code", json!({"designId": id}));
    assert!(read["structuredContent"].get("liveStl").is_none(), "{read}");
    let _ = fs::remove_dir_all(root);
    assert_eq!(string_literals("a=\"x\\\"y\"; // \"no\"\n/* \"no\" */ b=\"z\";"), vec!["x\\\"y", "z"]);
}

/// The colour rule of the guide relies on this: color() around import()
/// reaches the viewer as the object's colour, closed mesh or open surface.
#[test]
fn color_around_import_reaches_the_viewer() {
    let code = "color(\"LightGrey\") import(\"scan.stl\");\ncolor(\"RoyalBlue\") translate([30, 0, 0]) cube(5);\n";
    let (state, root, id) = with_upload(code);
    let (status, body) = serve(&state, raw("POST", &format!("/api/designs/{id}/render"), Some("application/json"), &render_body(&state, &id)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let text = String::from_utf8_lossy(&body);
    let reply = json_of(&body);
    let objects = reply["objects"].as_array().unwrap_or_else(|| panic!("{}", &text[..text.len().min(800)]));
    assert_eq!(objects.len(), 2, "{objects:?}");
    let grey = objects[0]["color"].as_array().unwrap_or_else(|| panic!("{objects:?}"));
    let blue = objects[1]["color"].as_array().unwrap_or_else(|| panic!("{objects:?}"));
    assert!((grey[0].as_f64().unwrap() - 211.0 / 255.0).abs() < 0.01, "{grey:?}");
    assert!(blue[2].as_f64().unwrap() > blue[0].as_f64().unwrap() + 0.3, "{blue:?}");
    let _ = fs::remove_dir_all(&root);

    // An open sheet keeps its colour too.
    let (state, root) = temporary_state();
    let id = state.workspaces.create(code.into(), None).unwrap().id;
    let sheet = TriMesh {
        points: vec![[0., 0., 0.], [10., 0., 0.], [10., 10., 0.], [0., 10., 0.]],
        triangles: vec![[0, 1, 2], [0, 2, 3]],
    };
    let (status, body) = serve(&state, raw("PUT", &format!("/api/designs/{id}/assets/scan.stl"), None, &meshimport::encode_binary_stl(&sheet)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(json_of(&body)["report"]["watertight"], false);
    let (status, body) = serve(&state, raw("POST", &format!("/api/designs/{id}/render"), Some("application/json"), &render_body(&state, &id)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let reply = json_of(&body);
    assert!(reply["objects"][0]["color"].is_array(), "{}", reply["objects"]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn open_uploads_get_clear_messages_where_a_solid_is_needed() {
    let (state, root) = temporary_state();
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let sheet = TriMesh {
        points: vec![[0., 0., 0.], [10., 0., 0.], [10., 10., 0.], [0., 10., 0.]],
        triangles: vec![[0, 1, 2], [0, 2, 3]],
    };
    let (status, body) = serve(&state, raw("PUT", &format!("/api/designs/{id}/assets/sheet.stl"), None, &meshimport::encode_binary_stl(&sheet)));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let reply = json_of(&body);
    assert!(reply["report"]["warnings"].to_string().contains("open surface"), "{reply}");
    // Rendering, volume and booleans with it: warned, never a crash.
    let saved = mcp(&state, "evaluate", json!({"designId": id, "expectedRevision": revision_of(&state, &id), "revisionName": "Sheet", "code": "import(\"sheet.stl\");\ndifference() { cube(20); import(\"sheet.stl\"); }\n"}));
    assert_eq!(saved["isError"], false, "{saved}");
    assert!(saved.to_string().contains("open surface"), "{saved}");
    assert!(saved.to_string().contains("closed solid") && !saved.to_string().contains("seal"), "{saved}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn upload_tools_and_instructions_point_to_the_guide_and_keep_the_workflow_out() {
    let tools = tool_definitions();
    let tool = |name: &str| tools.iter().find(|tool| tool["name"] == name).unwrap();
    for name in ["upload_asset", "create_asset_upload"] {
        let text = tool(name)["description"].as_str().unwrap();
        assert!(text.contains(GUIDE_TOOL) && text.contains("3 hours") && text.contains("delete_asset"), "{name}: {text}");
        assert!(tool(name)["inputSchema"]["properties"].get("seal").is_none(), "{name}");
        for moved in ["select_scan_region", "compare_surfaces", "align_to"] {
            assert!(!text.contains(moved), "{name} holds workflow: {moved}");
        }
    }
    assert!(!tool("delete_asset")["description"].as_str().unwrap().contains("derived"));
    for moved in ["Reconstructing from a scan", "select_scan_region", "list_scan_regions", "compare_surfaces", "compare_sections", "measure_between", "fit_curve", "scan_report", "extract_features", "shapeAccuracy", "To pose a scan", "seal"] {
        assert!(!crate::MCP_INSTRUCTIONS.contains(moved), "server instructions still hold {moved:?}");
    }
    assert!(crate::MCP_INSTRUCTIONS.contains(GUIDE_TOOL));

    let (state, root) = temporary_state();
    let reply = mcp(&state, GUIDE_TOOL, json!({}));
    assert_eq!(reply["isError"], false, "{reply}");
    let guide = reply["structuredContent"]["guide"].as_str().unwrap();
    for point in ["one at a time", "different colours", "no coordinate dumps", "scan gaps", "delete_asset", "Don't wait for approval"] {
        assert!(guide.contains(point), "{point}: {guide}");
    }
    let id = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    let created = mcp(&state, "create_asset_upload", json!({"designId": id, "name": "big.stl"}));
    let structured = &created["structuredContent"];
    assert_eq!(structured["guide"], GUIDE_TOOL, "{created}");
    assert!(structured["stlExpiry"]["note"].as_str().unwrap().contains(GUIDE_TOOL), "{created}");
    assert!(structured["stlExpiry"]["expiresAtIfUploadedNow"].as_u64().unwrap() > unix_millis(), "{created}");
    let script = mcp(&state, "create_asset_upload", json!({"designId": id, "name": "big2.stl", "includeScript": true}));
    assert_eq!(script["structuredContent"]["guide"], GUIDE_TOOL, "{script}");
    let _ = fs::remove_dir_all(root);
}

/// A closed box, as a legacy entry's mesh.
fn hull_mesh(state: &AppState) -> TriMesh {
    let id = state.workspaces.create("cube(3);".into(), None).unwrap().id;
    crate::scancommon::testkit::compiled_mesh(state, &id)
}

/// A design whose index holds one old entry, as `report` an older version
/// wrote it, with its blob on disk. Returns the entry's name.
fn with_legacy_entry(state: &AppState, report: serde_json::Value) -> (String, &'static str) {
    let id = state.workspaces.create("import(\"hull.stl\");\n".into(), None).unwrap().id;
    let Store::Filesystem(store) = &state.workspaces else { panic!() };
    let dir = store.assets.dir(&id);
    fs::create_dir_all(&dir).unwrap();
    let stl = meshimport::encode_binary_stl(&hull_mesh(state));
    let sha = hex(&crate::rendercache::sha256(&stl));
    fs::write(dir.join(format!("{sha}.stl")), &stl).unwrap();
    let mut index = serde_json::to_value(store.assets.read_index(&id)).unwrap();
    index["assets"] = json!([{
        "name": "hull.stl", "sha256": sha, "bytes": stl.len(), "triangles": 12,
        "createdAt": unix_millis(), "expiresAt": unix_millis() + HOUR, "someFieldFromBefore": true,
        "report": report,
    }]);
    fs::write(dir.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    (id, "hull.stl")
}

#[test]
fn a_legacy_server_made_entry_is_ignored_and_swept() {
    let (state, root) = temporary_state();
    for report in [
        json!({"origin": "derived", "clean": null, "fit": {"parent": "gone.stl"}}),
        json!({"origin": "crop", "clean": null, "crop": {"parent": "gone.stl", "parentSha256": "abc", "fullResolution": true}}),
    ] {
        let (id, name) = with_legacy_entry(&state, report);
        let Store::Filesystem(store) = &state.workspaces else { panic!() };
        // It still parses, extra fields and all, and sits in the index...
        assert_eq!(store.assets.read_index(&id).assets.len(), 1);
        // ...yet is in no list, with no live time left, so no STL row shows it.
        assert!(state.workspaces.asset_list(&id).unwrap().is_empty());
        let (_, body) = serve(&state, raw("GET", &format!("/api/designs/{id}/assets"), None, b""));
        let public = json_of(&body);
        assert_eq!(public["assets"], json!([]), "{public}");
        assert_eq!(public["expired"][0]["name"], name, "{public}");
        assert_eq!(serve(&state, raw("GET", &format!("/api/designs/{id}/assets/{name}"), None, b"")).0, 404);
        // No import: the normal expired error.
        let render = mcp(&state, "evaluate", json!({"designId": id}));
        assert!(render.to_string().contains("expired") && render.to_string().contains(name), "{render}");
        // delete_asset does not reach it.
        assert_eq!(mcp(&state, "delete_asset", json!({"designId": id, "name": name}))["isError"], true);

        // The next sweep frees it, though its old clock had time left.
        let swept = sweep_expired(&state, unix_millis()).unwrap();
        assert!(swept.iter().any(|(design, names)| design == &id && names == &vec![name.to_string()]), "{swept:?}");
        assert!(store.assets.read_index(&id).assets.is_empty());
        let blobs = fs::read_dir(store.assets.dir(&id)).unwrap().flatten().filter(|entry| entry.path().extension().is_some_and(|e| e == "stl")).count();
        assert_eq!(blobs, 0, "the mesh is gone from disk");

        // The name remembered, the import still fails as expired.
        let render = mcp(&state, "evaluate", json!({"designId": id}));
        assert!(render.to_string().contains("expired"), "{render}");
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_legacy_server_made_entry_blocks_no_upload_even_under_its_name() {
    let (state, root) = temporary_state();
    let report = json!({"origin": "derived", "fit": {"parent": "gone.stl"}});
    // Before any sweep: a scan beside it, and a scan taking its very name.
    for (name, chunked) in [("scan.stl", false), ("hull.stl", false), ("big.stl", true)] {
        let (id, _) = with_legacy_entry(&state, report.clone());
        if chunked {
            let created = mcp(&state, "create_asset_upload", json!({"designId": id, "name": name}));
            assert_eq!(created["isError"], false, "{created}");
            continue;
        }
        assert_eq!(put_stl(&state, &id, name, 5.0).0, 200, "{name}");
        let listed = state.workspaces.asset_list(&id).unwrap();
        assert_eq!(listed.iter().map(|meta| meta.name.as_str()).collect::<Vec<_>>(), vec![name]);
        // That upload is the design's one STL, whatever the old entry was.
        assert_eq!(put_stl(&state, &id, "second.stl", 5.0).0, 409);
        assert!(listed[0].report.get("origin").is_none(), "{:?}", listed[0]);
    }
    // A read-only copy takes no legacy entry along.
    let (id, _) = with_legacy_entry(&state, report);
    let copy = state.workspaces.create("cube(1);".into(), None).unwrap().id;
    assert_eq!(copy_assets(&state, &id, &copy).unwrap(), 0);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn the_sweep_removes_old_originals_and_only_their_files() {
    let (state, root, id) = with_upload("cube(1);");
    let Store::Filesystem(store) = &state.workspaces else { panic!() };
    let originals = store.root.join("originals");
    let design = originals.join(&id);
    fs::create_dir_all(&design).unwrap();
    for file in ["scan.stl.aa.orig", "scan.stl.aa.json", "scan.stl.bb.tmp42"] {
        fs::write(design.join(file), b"x").unwrap();
    }
    sweep_expired(&state, unix_millis()).unwrap();
    assert!(!originals.exists(), "the empty directory goes too");
    assert_eq!(state.workspaces.asset_list(&id).unwrap().len(), 1, "live assets stay");

    // A directory an operator named keeps what is not ours.
    fs::create_dir_all(&design).unwrap();
    fs::write(design.join("scan.stl.aa.orig"), b"x").unwrap();
    fs::write(design.join("notes.txt"), b"keep").unwrap();
    fs::write(originals.join("other.txt"), b"keep").unwrap();
    sweep_expired(&state, unix_millis()).unwrap();
    assert!(!design.join("scan.stl.aa.orig").exists());
    assert!(design.join("notes.txt").exists() && originals.join("other.txt").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn removed_mesh_tools_are_gone_from_the_list_and_the_text() {
    let tools = crate::mcp_tool_definitions();
    let names: Vec<&str> = tools.iter().filter_map(|tool| tool["name"].as_str()).collect();
    let text = serde_json::to_string(&tools).unwrap();
    for gone in ["derive_asset", "crop_scan"] {
        assert!(!names.contains(&gone), "{gone} is listed");
        assert!(!text.contains(gone), "a tool's text names {gone}");
        assert!(!crate::MCP_INSTRUCTIONS.contains(gone), "the instructions name {gone}");
    }
    assert!(!text.contains("original is kept") && !text.contains("keeps the original"), "uploads keep no original");
    let (state, root, id) = with_upload("cube(1);");
    for gone in ["derive_asset", "crop_scan"] {
        assert_eq!(mcp(&state, gone, json!({"designId": id, "name": "x.stl"}))["isError"], true, "{gone}");
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn uploads_say_what_is_stored_and_never_invite_a_side_design() {
    let tools = tool_definitions();
    let tool = |name: &str| tools.iter().find(|tool| tool["name"] == name).unwrap();
    for name in ["upload_asset", "create_asset_upload"] {
        let text = tool(name)["description"].as_str().unwrap();
        assert!(text.contains("500000") && text.contains("1000000") && text.contains("cannot be recovered"), "{name}: {text}");
        assert!(text.contains("3 hours") && text.contains("One STL per design"), "{name}: {text}");
    }
    let creating = crate::mcp_tool_definitions().into_iter().find(|tool| tool["name"] == "create_design").unwrap();
    assert!(creating["description"].as_str().unwrap().contains("never to try something out"));
    assert!(crate::MCP_INSTRUCTIONS.contains("never to try something out"));
}

//! End-to-end tests: the real executable, a sandboxed game folder and a local
//! HTTP server standing in for GitHub.
//!
//! Every test gets its own data folder (`SSU_HOME`) and its own fake game
//! folder, and automatic detection is pointed at an empty location, so nothing
//! here can find or touch a real Star Citizen installation.

#[path = "common/zipbuilder.rs"]
mod zipbuilder;

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use zipbuilder::ZipBuilder;

const EXE: &str = env!("CARGO_BIN_EXE_starstrings-updater");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const INI_IN_ZIP: &str = "Data/Localization/english/global.ini";
const INI_REL: &str = r"Data\Localization\english\global.ini";
const DATE_1: &str = "Thu, 12 Nov 2026 17:31:49 GMT";
const DATE_2: &str = "Fri, 20 Nov 2026 08:00:00 GMT";

// ---------------------------------------------------------------------------
// Fake release server
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Response {
    status: u16,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
    location: Option<String>,
    /// Advertise this Content-Length instead of the real one.
    declared_length: Option<usize>,
    delay: Duration,
    /// Send the body a kilobyte at a time, pausing this long in between.
    trickle: Option<Duration>,
}

impl Response {
    fn zip(body: Vec<u8>, etag: &str, date: &str) -> Response {
        Response {
            status: 200,
            body,
            etag: Some(format!("\"{etag}\"")),
            last_modified: Some(date.to_string()),
            ..Default::default()
        }
    }

    fn status(status: u16) -> Response {
        Response {
            status,
            body: b"<html>error</html>".to_vec(),
            ..Default::default()
        }
    }

    fn redirect(location: String) -> Response {
        Response {
            status: 302,
            location: Some(location),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HashMap<String, String>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

struct Server {
    port: u16,
    routes: Arc<Mutex<HashMap<String, Response>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes: Arc<Mutex<HashMap<String, Response>>> = Arc::default();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let (thread_routes, thread_seen) = (routes.clone(), seen.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (routes, seen) = (thread_routes.clone(), thread_seen.clone());
                thread::spawn(move || handle(stream, &routes, &seen));
            }
        });
        Server { port, routes, seen }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn serve(&self, path: &str, response: Response) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_string(), response);
    }

    /// Publishes a release archive for LIVE.
    fn publish(&self, zip: Vec<u8>, etag: &str, date: &str) {
        self.serve("/live.zip", Response::zip(zip, etag, date));
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn requests_to(&self, path: &str) -> Vec<Seen> {
        self.requests()
            .into_iter()
            .filter(|r| r.path == path)
            .collect()
    }
}

fn handle(
    mut stream: TcpStream,
    routes: &Mutex<HashMap<String, Response>>,
    seen: &Mutex<Vec<Seen>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    seen.lock().unwrap().push(Seen {
        path: path.clone(),
        headers: headers.clone(),
    });

    let response = routes.lock().unwrap().get(&path).cloned();
    let Some(response) = response else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    thread::sleep(response.delay);

    // Answer conditional requests the way a real server does.
    let not_modified = response.status == 200
        && match (
            headers.get("if-none-match"),
            headers.get("if-modified-since"),
        ) {
            (Some(tag), _) => response.etag.as_ref() == Some(tag),
            (None, Some(date)) => response.last_modified.as_ref() == Some(date),
            (None, None) => false,
        };
    let (status, body): (u16, &[u8]) = if not_modified {
        (304, b"")
    } else {
        (response.status, &response.body)
    };

    let mut head = format!("HTTP/1.1 {status} Whatever\r\nConnection: close\r\n");
    if status != 304 {
        let length = response.declared_length.unwrap_or(body.len());
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    if let Some(etag) = &response.etag {
        head.push_str(&format!("ETag: {etag}\r\n"));
    }
    if let Some(date) = &response.last_modified {
        head.push_str(&format!("Last-Modified: {date}\r\n"));
    }
    if let Some(location) = &response.location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    match response.trickle {
        None => {
            let _ = stream.write_all(body);
        }
        Some(pause) => {
            for piece in body.chunks(1024) {
                if stream
                    .write_all(piece)
                    .and_then(|_| stream.flush())
                    .is_err()
                {
                    return;
                }
                thread::sleep(pause);
            }
        }
    }
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

struct Ran {
    code: i32,
    out: String,
}

struct Sandbox {
    dir: PathBuf,
    home: PathBuf,
    library: PathBuf,
    root: PathBuf,
}

impl Sandbox {
    /// A fake game folder with LIVE installed.
    fn new(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("ssu-e2e-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let library = dir.join("Roberts Space Industries");
        let sandbox = Sandbox {
            home: dir.join("home"),
            root: library.join("StarCitizen"),
            library,
            dir,
        };
        sandbox.add_channel("LIVE");
        sandbox
    }

    fn add_channel(&self, channel: &str) {
        let dir = self.root.join(channel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Data.p4k"), b"not really the game").unwrap();
    }

    fn ini(&self) -> PathBuf {
        self.root.join("LIVE").join(INI_REL)
    }

    fn cfg(&self) -> PathBuf {
        self.root.join("LIVE").join("USER.cfg")
    }

    /// Where the updater keeps copies of files it found in `game_root`.
    fn backups_for(&self, game_root: &Path) -> PathBuf {
        let key = fnv1a(&game_root.to_string_lossy().to_lowercase()) as u32;
        self.home.join("backup").join(format!("{key:08x}"))
    }

    /// The kept copy of a `global.ini` that was in this sandbox's LIVE folder.
    fn backup(&self) -> PathBuf {
        self.backups_for(&self.root).join("LIVE").join(INI_REL)
    }

    fn state(&self) -> String {
        fs::read_to_string(self.home.join("state.ini")).unwrap_or_default()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.home.join("updater.log")).unwrap_or_default()
    }

    fn command(&self, server: &Server, args: &[&str]) -> Command {
        let mut command = Command::new(EXE);
        command
            .args(args)
            .env("SSU_HOME", &self.home)
            .env("SSU_URL_LIVE", server.url("/live.zip"))
            .env("SSU_URL_PTU", server.url("/ptu.zip"))
            // Automatic detection may only ever look here, where nothing is.
            .env("SSU_SEARCH_PATHS", self.dir.join("nowhere"))
            .stdin(Stdio::null());
        command
    }

    fn run(&self, server: &Server, args: &[&str]) -> Ran {
        finish(self.command(server, args))
    }

    /// `run --path <sandbox game folder>`.
    fn update(&self, server: &Server) -> Ran {
        self.run(server, &["run", "--path", self.root.to_str().unwrap()])
    }

    /// Every file below the sandbox, relative and sorted.
    fn files(&self) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    out.push(
                        path.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.dir, &self.dir, &mut out);
        out.sort();
        out
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = make_writable(&self.ini());
        let _ = make_writable(&self.cfg());
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn finish(mut command: Command) -> Ran {
    let output = command.output().expect("failed to start the updater");
    Ran {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
    }
}

fn make_writable(path: &Path) -> std::io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

/// Localization text for a made-up release.
fn strings(version: u32) -> String {
    format!(
        "\u{feff}release_marker=v{version}\n{}",
        "item_Name=Mil/1/D Tundra\n".repeat(1500 + version as usize)
    )
}

/// A release archive laid out exactly like upstream's.
fn release(ini: &str) -> Vec<u8> {
    ZipBuilder::new()
        .stored("USER.cfg", b"g_language = english")
        .dir("Data/")
        .dir("Data/Localization/")
        .dir("Data/Localization/english/")
        .deflated(INI_IN_ZIP, ini.as_bytes())
        .build()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// The update itself
// ---------------------------------------------------------------------------

#[test]
fn first_run_installs_the_strings_and_the_language_setting() {
    let server = Server::start();
    let sandbox = Sandbox::new("first");
    server.publish(release(&strings(1)), "v1", DATE_1);

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(
        ran.out
            .contains("LIVE: StarStrings updated (release of 12 Nov 2026)"),
        "{}",
        ran.out
    );
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert_eq!(read(&sandbox.cfg()), "g_language = english");

    // Exactly one request, unconditional, identifying itself and nothing else.
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/live.zip");
    assert_eq!(requests[0].header("if-none-match"), None);
    assert_eq!(
        requests[0].header("user-agent"),
        Some(format!("starstrings-updater/{VERSION}").as_str())
    );
    assert_eq!(requests[0].header("cookie"), None);
    assert_eq!(requests[0].header("authorization"), None);

    // What it remembers, and what it wrote down.
    let state = sandbox.state();
    assert!(state.contains("LIVE.etag=\"v1\""), "{state}");
    assert!(
        state.contains(&format!("LIVE.last_modified={DATE_1}")),
        "{state}"
    );
    assert!(state.contains(&format!("|{INI_REL}")), "{state}");
    let log = sandbox.log();
    assert!(log.contains("LIVE: StarStrings updated"), "{log}");
    assert!(
        log.contains("added \"g_language = english\" to USER.cfg"),
        "{log}"
    );

    // Nothing else appeared in the game folder.
    assert_eq!(
        sandbox.files(),
        vec![
            r"Roberts Space Industries\StarCitizen\LIVE\Data.p4k".to_string(),
            format!(r"Roberts Space Industries\StarCitizen\LIVE\{INI_REL}"),
            r"Roberts Space Industries\StarCitizen\LIVE\USER.cfg".to_string(),
            r"home\state.ini".to_string(),
            r"home\updater.log".to_string(),
        ]
    );
}

#[test]
fn an_unchanged_release_costs_one_conditional_request_and_no_disk_writes() {
    let server = Server::start();
    let sandbox = Sandbox::new("noop");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);

    let before: Vec<SystemTime> = [
        sandbox.ini(),
        sandbox.cfg(),
        sandbox.home.join("state.ini"),
        sandbox.home.join("updater.log"),
    ]
    .iter()
    .map(|p| modified(p))
    .collect();
    let files_before = sandbox.files();
    thread::sleep(Duration::from_millis(50));

    // The scheduled task runs plain `run`: the game folder is remembered.
    for _ in 0..3 {
        let ran = sandbox.run(&server, &["run"]);
        assert_eq!(ran.code, 0, "{}", ran.out);
        assert!(
            ran.out
                .contains("LIVE: StarStrings is up to date (release of 12 Nov 2026)"),
            "{}",
            ran.out
        );
    }

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests[1..] {
        assert_eq!(request.header("if-none-match"), Some("\"v1\""));
        assert_eq!(request.header("if-modified-since"), Some(DATE_1));
    }
    let after: Vec<SystemTime> = [
        sandbox.ini(),
        sandbox.cfg(),
        sandbox.home.join("state.ini"),
        sandbox.home.join("updater.log"),
    ]
    .iter()
    .map(|p| modified(p))
    .collect();
    assert_eq!(before, after, "an up-to-date check must not write anything");
    assert_eq!(files_before, sandbox.files());
}

#[test]
fn a_new_release_replaces_the_file() {
    let server = Server::start();
    let sandbox = Sandbox::new("newrelease");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);

    server.publish(release(&strings(2)), "v2", DATE_2);
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(
        ran.out
            .contains("LIVE: StarStrings updated (release of 20 Nov 2026)"),
        "{}",
        ran.out
    );
    assert_eq!(read(&sandbox.ini()), strings(2));
    assert_eq!(read(&sandbox.cfg()), "g_language = english");
    assert!(sandbox.state().contains("LIVE.etag=\"v2\""));
    assert_eq!(
        sandbox.log().matches("LIVE: StarStrings updated").count(),
        2
    );
    assert!(!sandbox.files().iter().any(|f| f.ends_with(".ssu-tmp")));

    // And the check after that is conditional on the new release.
    assert_eq!(sandbox.run(&server, &["run"]).code, 0);
    assert_eq!(
        server.requests().last().unwrap().header("if-none-match"),
        Some("\"v2\"")
    );
}

#[test]
fn a_republished_identical_release_is_not_rewritten() {
    let server = Server::start();
    let sandbox = Sandbox::new("republished");
    server.publish(release(&strings(1)), "first-upload", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);
    let written = modified(&sandbox.ini());
    thread::sleep(Duration::from_millis(50));

    // Upstream re-runs its release job: same content, new upload.
    server.publish(release(&strings(1)), "second-upload", DATE_2);
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(ran.out.contains("up to date"), "{}", ran.out);
    assert_eq!(modified(&sandbox.ini()), written);
    assert!(sandbox.state().contains("LIVE.etag=\"second-upload\""));
}

#[test]
fn files_that_went_missing_are_put_back() {
    let server = Server::start();
    let sandbox = Sandbox::new("restore");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);

    // A game repair (or the player) removes the localization folder.
    fs::remove_dir_all(sandbox.root.join("LIVE").join("Data")).unwrap();
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));
    // "Not modified" would have been the wrong answer, so it must not be asked for.
    assert_eq!(
        server.requests().last().unwrap().header("if-none-match"),
        None
    );

    // USER.cfg replaced by one without the setting: the line comes back, and
    // because the strings themselves are intact the request stays conditional.
    fs::write(sandbox.cfg(), "r_DisplayInfo = 1\r\n").unwrap();
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(
        read(&sandbox.cfg()),
        "r_DisplayInfo = 1\r\ng_language = english\r\n"
    );
    assert_eq!(
        server.requests().last().unwrap().header("if-none-match"),
        Some("\"v1\"")
    );
}

#[test]
fn follows_redirects_the_way_github_serves_release_assets() {
    let server = Server::start();
    let sandbox = Sandbox::new("redirect");
    // github.com answers 302 to a signed URL on another host; mimic both forms.
    server.serve("/live.zip", Response::redirect("/hop?step=1".to_string()));
    server.serve(
        "/hop?step=1",
        Response::redirect(server.url("/blob/asset-1?sig=abc%2Fdef")),
    );
    server.serve(
        "/blob/asset-1?sig=abc%2Fdef",
        Response::zip(release(&strings(1)), "blob-1", DATE_1),
    );

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));

    // The validators must reach the final hop for the check to be cheap.
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(ran.out.contains("up to date"), "{}", ran.out);
    let blob = server.requests_to("/blob/asset-1?sig=abc%2Fdef");
    assert_eq!(blob.len(), 2);
    assert_eq!(blob[1].header("if-none-match"), Some("\"blob-1\""));
}

#[test]
fn redirect_loops_end_in_an_error() {
    let server = Server::start();
    let sandbox = Sandbox::new("loop");
    server.serve("/live.zip", Response::redirect("/live.zip".to_string()));
    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(ran.out.contains("too many redirects"), "{}", ran.out);
    assert!(server.requests().len() <= 7);
    assert!(!sandbox.ini().exists());
}

#[test]
fn works_with_servers_that_only_send_last_modified() {
    let server = Server::start();
    let sandbox = Sandbox::new("lastmodified");
    server.serve(
        "/live.zip",
        Response {
            status: 200,
            body: release(&strings(1)),
            last_modified: Some(DATE_1.to_string()),
            ..Default::default()
        },
    );
    assert_eq!(sandbox.update(&server).code, 0);
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    let last = server.requests().last().cloned().unwrap();
    assert_eq!(last.header("if-none-match"), None);
    assert_eq!(last.header("if-modified-since"), Some(DATE_1));
    assert!(ran.out.contains("up to date"));
}

#[test]
fn without_any_validators_it_does_not_download_every_hour() {
    let server = Server::start();
    let sandbox = Sandbox::new("novalidators");
    server.serve(
        "/live.zip",
        Response {
            status: 200,
            body: release(&strings(1)),
            ..Default::default()
        },
    );
    assert_eq!(sandbox.update(&server).code, 0);
    for _ in 0..3 {
        let ran = sandbox.run(&server, &["run"]);
        assert_eq!(ran.code, 0, "{}", ran.out);
    }
    assert_eq!(
        server.requests().len(),
        1,
        "full downloads must be rate-limited"
    );

    // ...but a missing file is still worth a download.
    fs::remove_file(sandbox.ini()).unwrap();
    assert_eq!(sandbox.run(&server, &["run"]).code, 0);
    assert_eq!(server.requests().len(), 2);
    assert_eq!(read(&sandbox.ini()), strings(1));
}

// ---------------------------------------------------------------------------
// Hostile and broken input
// ---------------------------------------------------------------------------

#[test]
fn a_hostile_archive_cannot_write_outside_the_localization_folder() {
    let server = Server::start();
    let sandbox = Sandbox::new("hostile");
    let absolute = format!("{}/absolute.ini", sandbox.dir.display());
    let hostile = ZipBuilder::new()
        .stored("USER.cfg", b"g_language = english\r\nsys_evil = 1\r\n")
        .stored("../escaped.ini", b"x=1")
        .stored("../../escaped.ini", b"x=1")
        .stored("Data/Localization/../../escaped.ini", b"x=1")
        .stored("Data/Localization/english/../../../escaped.ini", b"x=1")
        .stored(r"Data\Localization\english\..\..\..\escaped.ini", b"x=1")
        .stored(&absolute, b"x=1")
        .stored("/rooted.ini", b"x=1")
        .stored("Bin64/evil.dll", b"MZ")
        .stored("Data/Localization/english/evil.dll", b"MZ")
        .stored("Data/Localization/english/evil.exe", b"MZ")
        .stored("Data/Localization/english/global.ini:stream.ini", b"x=1")
        .stored("Data/evil.ini", b"x=1")
        .stored("StarCitizen_Launcher.exe", b"MZ")
        .deflated(INI_IN_ZIP, strings(1).as_bytes())
        .build();
    server.publish(hostile, "v1", DATE_1);

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));
    // USER.cfg is never taken from the archive.
    assert_eq!(read(&sandbox.cfg()), "g_language = english");
    assert_eq!(
        sandbox.files(),
        vec![
            r"Roberts Space Industries\StarCitizen\LIVE\Data.p4k".to_string(),
            format!(r"Roberts Space Industries\StarCitizen\LIVE\{INI_REL}"),
            r"Roberts Space Industries\StarCitizen\LIVE\USER.cfg".to_string(),
            r"home\state.ini".to_string(),
            r"home\updater.log".to_string(),
        ]
    );
    // Nothing escaped above the sandbox either.
    assert!(!sandbox.dir.parent().unwrap().join("escaped.ini").exists());
}

#[test]
fn an_archive_without_localization_files_changes_nothing() {
    let server = Server::start();
    let sandbox = Sandbox::new("empty");
    server.publish(
        ZipBuilder::new()
            .stored("readme.md", b"moved elsewhere")
            .build(),
        "v1",
        DATE_1,
    );
    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(
        ran.out.contains("contains no localization files"),
        "{}",
        ran.out
    );
    assert!(!sandbox.ini().exists());
    assert!(sandbox.log().contains("ERROR"));
    // Nothing was accepted, so the next check must download again.
    assert!(!sandbox.state().contains("LIVE.etag"));
}

#[test]
fn broken_downloads_never_replace_a_good_file() {
    let server = Server::start();
    let sandbox = Sandbox::new("broken");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);

    let good = release(&strings(2));
    let mut bad_crc = good.clone();
    let middle = bad_crc.len() / 2;
    bad_crc[middle] ^= 0xFF;

    let broken: Vec<(&str, Response)> = vec![
        (
            "an error page served as success",
            Response::zip(
                b"<!DOCTYPE html><html>Service unavailable</html>".to_vec(),
                "x",
                DATE_2,
            ),
        ),
        ("an empty body", Response::zip(Vec::new(), "x", DATE_2)),
        ("corrupted content", Response::zip(bad_crc, "x", DATE_2)),
        (
            "a truncated archive",
            Response::zip(good[..good.len() - 40].to_vec(), "x", DATE_2),
        ),
        (
            "a download cut short",
            Response {
                declared_length: Some(good.len() + 1000),
                ..Response::zip(good.clone(), "x", DATE_2)
            },
        ),
        (
            "an absurdly large download",
            Response {
                declared_length: Some(1 << 30),
                ..Response::zip(good.clone(), "x", DATE_2)
            },
        ),
        (
            "an empty localization file",
            Response::zip(release(""), "x", DATE_2),
        ),
        (
            "a duplicated localization file",
            Response::zip(
                ZipBuilder::new()
                    .deflated(INI_IN_ZIP, strings(2).as_bytes())
                    .deflated(
                        "data/localization/ENGLISH/global.ini",
                        strings(3).as_bytes(),
                    )
                    .build(),
                "x",
                DATE_2,
            ),
        ),
    ];
    for (what, response) in broken {
        server.serve("/live.zip", response);
        let ran = sandbox.run(&server, &["run"]);
        assert_eq!(ran.code, 1, "{what}: {}", ran.out);
        assert_eq!(read(&sandbox.ini()), strings(1), "{what}");
        assert!(sandbox.state().contains("LIVE.etag=\"v1\""), "{what}");
        assert!(
            !sandbox.files().iter().any(|f| f.ends_with(".ssu-tmp")),
            "{what}: left a temporary file behind"
        );
    }

    // Once upstream is healthy again, so is the updater.
    server.publish(good, "v2", DATE_2);
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(2));
}

#[test]
fn server_errors_are_reported_and_leave_everything_alone() {
    let server = Server::start();
    let sandbox = Sandbox::new("servererrors");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);
    let state = sandbox.state();

    for status in [404u16, 403, 429, 500, 503] {
        server.serve("/live.zip", Response::status(status));
        let ran = sandbox.run(&server, &["run"]);
        assert_eq!(ran.code, 1, "{status}: {}", ran.out);
        assert!(ran.out.contains(&format!("HTTP {status}")), "{}", ran.out);
        assert_eq!(read(&sandbox.ini()), strings(1));
        assert_eq!(sandbox.state(), state);
    }
    assert!(
        sandbox
            .log()
            .contains("ERROR LIVE: 127.0.0.1 has no such file (HTTP 404)")
    );
    assert!(
        sandbox
            .log()
            .contains("ERROR LIVE: 127.0.0.1 answered HTTP 500")
    );
}

#[test]
fn being_offline_is_not_worth_a_log_entry() {
    let server = Server::start();
    let sandbox = Sandbox::new("offline");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);
    let log = sandbox.log();
    let state = sandbox.state();

    // A port nobody listens on.
    let free_port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut command = sandbox.command(&server, &["run"]);
    command.env(
        "SSU_URL_LIVE",
        format!("http://127.0.0.1:{free_port}/live.zip"),
    );
    let ran = finish(command);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(ran.out.contains("could not reach 127.0.0.1"), "{}", ran.out);
    assert_eq!(
        sandbox.log(),
        log,
        "an offline check must not write to the log"
    );
    assert_eq!(sandbox.state(), state);
    assert_eq!(read(&sandbox.ini()), strings(1));
}

#[test]
fn unencrypted_sources_are_refused() {
    let server = Server::start();
    let sandbox = Sandbox::new("plainhttp");
    let mut command = sandbox.command(&server, &["run", "--path", sandbox.root.to_str().unwrap()]);
    command.env("SSU_URL_LIVE", "http://example.com/StarStrings-LIVE.zip");
    let ran = finish(command);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(
        ran.out.contains("refusing an unencrypted connection"),
        "{}",
        ran.out
    );
    assert!(!sandbox.ini().exists());
    assert!(server.requests().is_empty());
}

// ---------------------------------------------------------------------------
// The game folder
// ---------------------------------------------------------------------------

#[test]
fn a_file_the_game_holds_open_is_retried_on_the_next_check() {
    let server = Server::start();
    let sandbox = Sandbox::new("locked");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);
    server.publish(release(&strings(2)), "v2", DATE_2);

    // Hold the file open with no sharing at all, like a process reading it.
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(sandbox.ini())
        .unwrap();
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(
        ran.out.contains("it will be retried on the next check"),
        "{}",
        ran.out
    );
    assert!(!sandbox.files().iter().any(|f| f.ends_with(".ssu-tmp")));
    assert!(sandbox.state().contains("LIVE.etag=\"v1\""));
    drop(lock);
    assert_eq!(read(&sandbox.ini()), strings(1));

    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(2));
}

#[test]
fn read_only_files_are_updated_and_stay_read_only() {
    let server = Server::start();
    let sandbox = Sandbox::new("readonly");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);

    for path in [sandbox.ini(), sandbox.cfg()] {
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
    }
    server.publish(release(&strings(2)), "v2", DATE_2);
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(2));
    assert!(
        fs::metadata(sandbox.ini())
            .unwrap()
            .permissions()
            .readonly()
    );
    assert!(
        fs::metadata(sandbox.cfg())
            .unwrap()
            .permissions()
            .readonly()
    );
}

#[test]
fn an_existing_user_cfg_keeps_all_of_its_settings() {
    let server = Server::start();
    let sandbox = Sandbox::new("usercfg");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let mine = "; my settings\r\nr_DisplayInfo = 1\r\nsys_maxFPS = 120\r\n";
    fs::write(sandbox.cfg(), mine).unwrap();

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(
        read(&sandbox.cfg()),
        format!("{mine}g_language = english\r\n")
    );
    let state = sandbox.state();
    assert!(state.contains("LIVE.cfg_added=1"), "{state}");
    assert!(!state.contains("LIVE.cfg_created"), "{state}");
}

#[test]
fn a_scheduled_check_respects_another_language_choice() {
    let server = Server::start();
    let sandbox = Sandbox::new("otherlanguage");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let mine = "g_language = french_(france)\n";
    fs::write(sandbox.cfg(), mine).unwrap();

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.cfg()), mine);
    assert_eq!(read(&sandbox.ini()), strings(1));
}

#[test]
fn an_unreadable_user_cfg_is_a_warning_not_a_blocker() {
    let server = Server::start();
    let sandbox = Sandbox::new("utf16cfg");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain("r_x = 1".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    fs::write(sandbox.cfg(), &utf16).unwrap();

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(ran.out.contains("not a plain text file"), "{}", ran.out);
    // The strings are installed and remembered all the same.
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert_eq!(fs::read(sandbox.cfg()).unwrap(), utf16);
    assert!(sandbox.state().contains("LIVE.etag=\"v1\""));
}

#[test]
fn a_channel_that_is_not_installed_is_skipped() {
    let server = Server::start();
    let sandbox = Sandbox::new("nolive");
    server.publish(release(&strings(1)), "v1", DATE_1);
    // Only PTU exists in this game folder.
    fs::remove_dir_all(sandbox.root.join("LIVE")).unwrap();
    sandbox.add_channel("PTU");

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(
        ran.out
            .contains("LIVE: not installed in the game folder, skipped"),
        "{}",
        ran.out
    );
    assert!(server.requests().is_empty());
    assert!(
        !sandbox.root.join("LIVE").exists(),
        "must not create the channel folder"
    );

    // Once the launcher has installed LIVE, the next check picks it up.
    sandbox.add_channel("LIVE");
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));
}

#[test]
fn ptu_is_only_touched_when_asked_for() {
    let server = Server::start();
    let sandbox = Sandbox::new("ptu");
    sandbox.add_channel("PTU");
    server.publish(release(&strings(1)), "live-1", DATE_1);
    server.serve(
        "/ptu.zip",
        Response::zip(release(&strings(7)), "ptu-1", DATE_2),
    );
    let ptu_ini = sandbox.root.join("PTU").join(INI_REL);

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(!ptu_ini.exists());
    assert!(!sandbox.root.join("PTU").join("USER.cfg").exists());
    assert!(server.requests_to("/ptu.zip").is_empty());

    let ran = sandbox.run(&server, &["run", "--ptu"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(
        ran.out
            .contains("PTU: StarStrings updated (release of 20 Nov 2026)"),
        "{}",
        ran.out
    );
    assert_eq!(read(&ptu_ini), strings(7));
    assert_eq!(
        read(&sandbox.root.join("PTU").join("USER.cfg")),
        "g_language = english"
    );
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert!(sandbox.state().contains("channels=LIVE,PTU"));

    // The choice is remembered; both are now checked conditionally.
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(
        server
            .requests_to("/ptu.zip")
            .last()
            .unwrap()
            .header("if-none-match"),
        Some("\"ptu-1\"")
    );
    assert_eq!(
        server
            .requests_to("/live.zip")
            .last()
            .unwrap()
            .header("if-none-match"),
        Some("\"live-1\"")
    );
}

#[test]
fn one_failing_channel_does_not_stop_the_other() {
    let server = Server::start();
    let sandbox = Sandbox::new("partial");
    sandbox.add_channel("PTU");
    server.serve("/live.zip", Response::status(500));
    server.serve(
        "/ptu.zip",
        Response::zip(release(&strings(7)), "ptu-1", DATE_2),
    );

    let ran = sandbox.run(
        &server,
        &["run", "--path", sandbox.root.to_str().unwrap(), "--ptu"],
    );
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(
        ran.out.contains("LIVE: 127.0.0.1 answered HTTP 500"),
        "{}",
        ran.out
    );
    assert!(ran.out.contains("PTU: StarStrings updated"), "{}", ran.out);
    assert_eq!(read(&sandbox.root.join("PTU").join(INI_REL)), strings(7));
    assert!(sandbox.state().contains("PTU.etag=\"ptu-1\""));
}

#[test]
fn a_file_that_was_there_before_is_backed_up_once() {
    let server = Server::start();
    let sandbox = Sandbox::new("backup");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let mine = "my_own_merged_pack=1\n";
    fs::create_dir_all(sandbox.ini().parent().unwrap()).unwrap();
    fs::write(sandbox.ini(), mine).unwrap();
    let backup = sandbox.backup();

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert_eq!(read(&backup), mine);
    assert!(sandbox.log().contains("kept a copy of the previous"));

    // Later releases replace the updater's own file; the backup stays the original.
    server.publish(release(&strings(2)), "v2", DATE_2);
    assert_eq!(sandbox.run(&server, &["run"]).code, 0);
    assert_eq!(read(&sandbox.ini()), strings(2));
    assert_eq!(read(&backup), mine);
}

#[test]
fn a_file_is_not_replaced_unless_a_copy_of_it_could_be_kept() {
    let server = Server::start();
    let sandbox = Sandbox::new("nobackup");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let mine = "my_own_merged_pack=1\n";
    fs::create_dir_all(sandbox.ini().parent().unwrap()).unwrap();
    fs::write(sandbox.ini(), mine).unwrap();

    // Something is in the way of the backup folder (stands in for a full disk).
    fs::create_dir_all(&sandbox.home).unwrap();
    fs::write(sandbox.home.join("backup"), b"not a folder").unwrap();

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(
        ran.out.contains("a copy of it could not be kept"),
        "{}",
        ran.out
    );
    assert_eq!(read(&sandbox.ini()), mine, "the player's file must survive");
    assert!(
        !sandbox.state().contains("LIVE.etag"),
        "nothing was installed, so nothing may be remembered as installed"
    );

    // Once a copy can be kept, the update goes through.
    fs::remove_file(sandbox.home.join("backup")).unwrap();
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert_eq!(read(&sandbox.backup()), mine);
}

#[test]
fn uninstalling_never_deletes_the_only_copy_of_an_earlier_file() {
    let server = Server::start();
    let sandbox = Sandbox::new("keepbackup");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let mine = "my_own_merged_pack=1\n";
    fs::create_dir_all(sandbox.ini().parent().unwrap()).unwrap();
    fs::write(sandbox.ini(), mine).unwrap();
    assert_eq!(sandbox.update(&server).code, 0);
    assert_eq!(read(&sandbox.backup()), mine);

    // "Remove the updater, keep the strings": the earlier file is not put back,
    // so the copy of it has to stay, and the player is told where it is.
    let ran = sandbox.run(&server, &["uninstall", "--quiet"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert_eq!(read(&sandbox.backup()), mine);
    assert!(
        ran.out
            .contains(&sandbox.home.join("backup").display().to_string()),
        "{}",
        ran.out
    );
    // Everything else of the updater is gone.
    assert_eq!(
        sandbox
            .files()
            .into_iter()
            .filter(|f| f.starts_with("home"))
            .collect::<Vec<_>>(),
        vec![
            sandbox
                .backup()
                .strip_prefix(&sandbox.dir)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        ]
    );
}

#[test]
fn uninstalling_puts_an_earlier_file_back_and_then_lets_go_of_the_copy() {
    let server = Server::start();
    let sandbox = Sandbox::new("restorebackup");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let mine = "my_own_merged_pack=1\n";
    fs::create_dir_all(sandbox.ini().parent().unwrap()).unwrap();
    fs::write(sandbox.ini(), mine).unwrap();
    assert_eq!(sandbox.update(&server).code, 0);

    let ran = sandbox.run(&server, &["uninstall", "--quiet", "--remove-strings"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), mine);
    assert!(!sandbox.home.exists(), "left behind: {:?}", sandbox.files());
}

#[test]
fn a_file_edited_after_installation_is_left_alone_and_its_backup_kept() {
    let server = Server::start();
    let sandbox = Sandbox::new("editedbackup");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let mine = "my_own_merged_pack=1\n";
    fs::create_dir_all(sandbox.ini().parent().unwrap()).unwrap();
    fs::write(sandbox.ini(), mine).unwrap();
    assert_eq!(sandbox.update(&server).code, 0);

    // The player tweaks the installed file, then uninstalls asking for a restore.
    let edited = format!("{}my_tweak=1\n", strings(1));
    fs::write(sandbox.ini(), &edited).unwrap();
    let ran = sandbox.run(&server, &["uninstall", "--quiet", "--remove-strings"]);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(
        ran.out.contains("was changed after it was installed"),
        "{}",
        ran.out
    );
    assert_eq!(
        read(&sandbox.ini()),
        edited,
        "their edit must not be undone"
    );
    assert_eq!(
        read(&sandbox.backup()),
        mine,
        "and the earlier file is still kept"
    );
}

#[test]
fn another_game_folder_is_examined_from_scratch() {
    let server = Server::start();
    let sandbox = Sandbox::new("otherfolder");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);

    // A second installation that already has someone's own pack and settings.
    let other_root = sandbox.dir.join("Other Library").join("StarCitizen");
    let other_live = other_root.join("LIVE");
    let other_ini = other_live.join(INI_REL);
    fs::create_dir_all(other_ini.parent().unwrap()).unwrap();
    fs::write(other_live.join("Data.p4k"), b"not really the game").unwrap();
    let theirs = "someone_elses_pack=1\n";
    fs::write(&other_ini, theirs).unwrap();
    fs::write(other_live.join("USER.cfg"), "g_language = german\n").unwrap();

    // Pointing the updater at it must not carry over anything it knew about
    // the first folder: "not modified" would be the wrong answer here.
    let ran = sandbox.run(&server, &["run", "--path", other_root.to_str().unwrap()]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(
        server.requests().last().unwrap().header("if-none-match"),
        None,
        "the new folder must be checked unconditionally"
    );
    assert_eq!(read(&other_ini), strings(1));
    let other_backup = sandbox.backups_for(&other_root).join("LIVE").join(INI_REL);
    assert_eq!(read(&other_backup), theirs, "their file must be kept");
    assert!(
        sandbox
            .state()
            .contains(&format!("game_root={}", other_root.display()))
    );

    // Undoing it restores what belonged to *that* folder, and leaves the first alone.
    let ran = sandbox.run(&server, &["uninstall", "--quiet", "--remove-strings"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&other_ini), theirs);
    assert_eq!(
        read(&other_live.join("USER.cfg")),
        "g_language = german\n",
        "a scheduled check never changes another language choice"
    );
    assert_eq!(read(&sandbox.ini()), strings(1));
}

#[test]
fn a_disconnected_drive_does_not_make_it_settle_on_another_folder() {
    let server = Server::start();
    let sandbox = Sandbox::new("unplugged");
    server.publish(release(&strings(1)), "v1", DATE_1);

    // The game is recorded as living on a drive that is not there right now.
    let unused = (b'D'..=b'Z')
        .rev()
        .map(|letter| format!("{}:\\", letter as char))
        .find(|root| !Path::new(root).exists())
        .expect("a free drive letter");
    let recorded = format!("{unused}Games\\StarCitizen");
    fs::create_dir_all(&sandbox.home).unwrap();
    let state = format!(
        "# StarStrings Updater state. Managed automatically.\ngame_root={recorded}\nchannels=LIVE\n"
    );
    fs::write(sandbox.home.join("state.ini"), &state).unwrap();

    // Another installation could be found - but the recorded one is only away.
    let mut command = sandbox.command(&server, &["run"]);
    command.env("SSU_SEARCH_PATHS", &sandbox.library);
    let ran = finish(command);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(ran.out.contains("skipped"), "{}", ran.out);
    assert_eq!(sandbox.state(), state);
    assert!(server.requests().is_empty());
    assert!(!sandbox.ini().exists());
}

#[test]
fn archives_that_are_too_big_are_refused_before_anything_is_unpacked() {
    let server = Server::start();
    let sandbox = Sandbox::new("limits");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);
    let state = sandbox.state();
    let files = sandbox.files();

    // Too many files.
    let mut many = ZipBuilder::new();
    for index in 0..33 {
        many = many.stored(
            &format!("Data/Localization/english/part{index}.ini"),
            b"a=1",
        );
    }
    // Small files that claim to unpack to far more than is allowed in total...
    let padding = vec![b'a'; 4096];
    let bloated = ZipBuilder::new()
        .deflated_claiming("Data/Localization/english/a.ini", &padding, 60 << 20)
        .deflated_claiming("Data/Localization/english/b.ini", &padding, 60 << 20)
        .deflated_claiming("Data/Localization/english/c.ini", &padding, 60 << 20)
        .build();
    // ...and one that claims more than a single file may be.
    let huge = ZipBuilder::new()
        .deflated_claiming(INI_IN_ZIP, &padding, 65 << 20)
        .build();

    for (what, archive, expected) in [
        ("too many files", many.build(), "more than the 32"),
        ("too much in total", bloated, "more than the 128 MB"),
        (
            "one file too large",
            huge,
            "larger than this updater accepts",
        ),
    ] {
        server.publish(archive, "big", DATE_2);
        let ran = sandbox.run(&server, &["run"]);
        assert_eq!(ran.code, 1, "{what}: {}", ran.out);
        assert!(ran.out.contains(expected), "{what}: {}", ran.out);
        assert_eq!(sandbox.state(), state, "{what}");
        assert_eq!(
            sandbox
                .files()
                .into_iter()
                .filter(|f| !f.ends_with("updater.log"))
                .collect::<Vec<_>>(),
            files
                .iter()
                .filter(|f| !f.ends_with("updater.log"))
                .cloned()
                .collect::<Vec<_>>(),
            "{what}: nothing may be written"
        );
    }
}

#[test]
fn a_download_that_crawls_is_abandoned() {
    let server = Server::start();
    let sandbox = Sandbox::new("crawl");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);

    // A new release served a kilobyte every quarter second: far too slow to
    // finish within the two seconds this test allows a download.
    server.serve(
        "/live.zip",
        Response {
            trickle: Some(Duration::from_millis(250)),
            ..Response::zip(
                ZipBuilder::new()
                    .stored(INI_IN_ZIP, strings(2).repeat(3).as_bytes())
                    .build(),
                "v2",
                DATE_2,
            )
        },
    );
    let started = std::time::Instant::now();
    let mut command = sandbox.command(&server, &["run"]);
    command.env("SSU_DOWNLOAD_TIMEOUT", "2");
    let ran = finish(command);
    let took = started.elapsed();
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(ran.out.contains("took too long"), "{}", ran.out);
    // Well under the time the whole body would take, and close to the limit:
    // the limit has to bite between small pieces, not once a large buffer fills.
    assert!(took < Duration::from_secs(8), "gave up only after {took:?}");
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert!(sandbox.state().contains("LIVE.etag=\"v1\""));
}

#[test]
fn a_relative_path_is_stored_as_the_full_path() {
    let server = Server::start();
    let sandbox = Sandbox::new("relative");
    server.publish(release(&strings(1)), "v1", DATE_1);

    // `--path .` typed inside the game folder.
    let mut command = sandbox.command(&server, &["run", "--path", "."]);
    command.current_dir(&sandbox.root);
    let ran = finish(command);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(
        sandbox
            .state()
            .contains(&format!("game_root={}\n", sandbox.root.display())),
        "{}",
        sandbox.state()
    );

    // The scheduled task runs from another folder entirely and must still find it.
    server.publish(release(&strings(2)), "v2", DATE_2);
    let mut command = sandbox.command(&server, &["run"]);
    command.current_dir(std::env::temp_dir());
    let ran = finish(command);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(2));
}

#[test]
fn a_user_cfg_shared_between_channels_stays_shared() {
    let server = Server::start();
    let sandbox = Sandbox::new("sharedcfg");
    sandbox.add_channel("PTU");
    server.publish(release(&strings(1)), "v1", DATE_1);

    // One settings file for both channels, as players set up with a link.
    let ptu_cfg = sandbox.root.join("PTU").join("USER.cfg");
    fs::write(sandbox.cfg(), "r_DisplayInfo = 1\r\n").unwrap();
    fs::hard_link(sandbox.cfg(), &ptu_cfg).unwrap();

    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 0, "{}", ran.out);
    let expected = "r_DisplayInfo = 1\r\ng_language = english\r\n";
    assert_eq!(read(&sandbox.cfg()), expected);
    assert_eq!(read(&ptu_cfg), expected, "the link must not have been cut");

    // And it is still one file afterwards.
    fs::write(&ptu_cfg, "changed through the other name").unwrap();
    assert_eq!(read(&sandbox.cfg()), "changed through the other name");
}

#[test]
fn a_change_to_user_cfg_is_only_remembered_if_it_was_really_made() {
    let server = Server::start();
    let sandbox = Sandbox::new("cfgfailed");
    server.publish(release(&strings(1)), "v1", DATE_1);
    fs::write(sandbox.cfg(), "r_DisplayInfo = 1\r\n").unwrap();

    // Another program has the file open and allows reading but not writing.
    const FILE_SHARE_READ: u32 = 1;
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(sandbox.cfg())
        .unwrap();
    let ran = sandbox.update(&server);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(ran.out.contains("could not update USER.cfg"), "{}", ran.out);
    drop(held);
    assert_eq!(read(&sandbox.cfg()), "r_DisplayInfo = 1\r\n");
    assert!(
        !sandbox.state().contains("cfg_added"),
        "nothing was added, so nothing may be recorded as added:\n{}",
        sandbox.state()
    );

    // The player adds the line themselves; uninstalling must leave their line alone.
    fs::write(
        sandbox.cfg(),
        "r_DisplayInfo = 1\r\ng_language = english\r\n",
    )
    .unwrap();
    assert_eq!(sandbox.run(&server, &["run"]).code, 0);
    let ran = sandbox.run(&server, &["uninstall", "--quiet", "--remove-strings"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(
        read(&sandbox.cfg()),
        "r_DisplayInfo = 1\r\ng_language = english\r\n"
    );
}

#[test]
fn a_library_planted_beside_the_program_is_not_loaded() {
    // Windows normally prefers libraries lying next to a program. Someone who
    // can drop a file into the Downloads folder must not get their code loaded
    // when the updater is started from there.
    unsafe extern "system" {
        fn SetErrorMode(mode: u32) -> u32;
    }
    // If this ever regresses, fail instead of putting an error dialog on screen.
    const SEM_FAILCRITICALERRORS: u32 = 0x0001;
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS) };

    let server = Server::start();
    let sandbox = Sandbox::new("planted");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let downloads = sandbox.dir.join("Downloads");
    fs::create_dir_all(&downloads).unwrap();
    let program = downloads.join("starstrings-updater.exe");
    fs::copy(EXE, &program).unwrap();
    // Not libraries at all: loading any of them would stop the program from starting.
    for name in [
        "winhttp.dll",
        "version.dll",
        "cryptbase.dll",
        "taskschd.dll",
    ] {
        fs::write(downloads.join(name), b"this is not a library").unwrap();
    }

    let mut version = Command::new(&program);
    version.arg("--version").stdin(Stdio::null());
    let ran = finish(version);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(ran.out.trim(), format!("StarStrings Updater {VERSION}"));

    // A real check exercises the networking library as well.
    let mut check = Command::new(&program);
    check
        .args(["run", "--path", sandbox.root.to_str().unwrap()])
        .env("SSU_HOME", &sandbox.home)
        .env("SSU_URL_LIVE", server.url("/live.zip"))
        .env("SSU_SEARCH_PATHS", sandbox.dir.join("nowhere"))
        .stdin(Stdio::null());
    let ran = finish(check);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));
}

#[test]
fn files_dropped_from_a_later_release_are_removed_unless_modified() {
    let server = Server::start();
    let sandbox = Sandbox::new("dropped");
    let extra = |name: &str| {
        sandbox
            .root
            .join("LIVE")
            .join(r"Data\Localization\english")
            .join(name)
    };
    let with_extras = ZipBuilder::new()
        .deflated(INI_IN_ZIP, strings(1).as_bytes())
        .stored("Data/Localization/english/extra_a.ini", b"a=1")
        .stored("Data/Localization/english/extra_b.ini", b"b=1")
        .build();
    server.publish(with_extras, "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);
    assert_eq!(read(&extra("extra_a.ini")), "a=1");
    assert_eq!(read(&extra("extra_b.ini")), "b=1");

    // The player edits one of them; the next release ships neither.
    fs::write(extra("extra_b.ini"), "b=mine").unwrap();
    server.publish(release(&strings(2)), "v2", DATE_2);
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(2));
    assert!(
        !extra("extra_a.ini").exists(),
        "unmodified leftovers are cleaned up"
    );
    assert_eq!(
        read(&extra("extra_b.ini")),
        "b=mine",
        "the player's edit is kept"
    );
}

// ---------------------------------------------------------------------------
// Finding the game, and the command line
// ---------------------------------------------------------------------------

#[test]
fn finds_the_game_by_itself_and_again_after_it_moved() {
    let server = Server::start();
    let sandbox = Sandbox::new("detect");
    server.publish(release(&strings(1)), "v1", DATE_1);

    // Nothing to find: a clear error, nothing created.
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(
        ran.out.contains("Star Citizen folder could not be found"),
        "{}",
        ran.out
    );
    assert!(server.requests().is_empty());

    // Found through the search path (the library folder, one level up).
    let mut command = sandbox.command(&server, &["run"]);
    command.env("SSU_SEARCH_PATHS", &sandbox.library);
    let ran = finish(command);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert!(
        sandbox
            .state()
            .contains(&format!("game_root={}", sandbox.root.display()))
    );

    // The player moves the library to another folder.
    let moved_library = sandbox.dir.join("Moved Library");
    fs::rename(&sandbox.library, &moved_library).unwrap();
    let mut command = sandbox.command(&server, &["run"]);
    command.env("SSU_SEARCH_PATHS", &moved_library);
    let ran = finish(command);
    assert_eq!(ran.code, 0, "{}", ran.out);
    let moved_root = moved_library.join("StarCitizen");
    assert!(
        sandbox
            .state()
            .contains(&format!("game_root={}", moved_root.display())),
        "{}",
        sandbox.state()
    );
    assert!(ran.out.contains("up to date"), "{}", ran.out);
}

#[test]
fn a_moved_game_is_found_even_when_the_old_folder_is_still_there() {
    let server = Server::start();
    let sandbox = Sandbox::new("leftover");
    server.publish(release(&strings(1)), "v1", DATE_1);
    assert_eq!(sandbox.update(&server).code, 0);

    // The game is uninstalled, but its old folder stays behind (empty of channels).
    fs::remove_dir_all(sandbox.root.join("LIVE")).unwrap();
    assert!(sandbox.root.is_dir());

    // Nowhere else to be found: wait for it quietly instead of failing every hour.
    let ran = sandbox.run(&server, &["run"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(
        ran.out
            .contains("LIVE: not installed in the game folder, skipped"),
        "{}",
        ran.out
    );
    assert_eq!(server.requests().len(), 1);

    // Then it is installed again in a new library folder.
    let new_library = sandbox.dir.join("New Library");
    let new_live = new_library.join("StarCitizen").join("LIVE");
    fs::create_dir_all(&new_live).unwrap();
    fs::write(new_live.join("Data.p4k"), b"not really the game").unwrap();
    let mut command = sandbox.command(&server, &["run"]);
    command.env("SSU_SEARCH_PATHS", &new_library);
    let ran = finish(command);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&new_live.join(INI_REL)), strings(1));
    assert_eq!(read(&new_live.join("USER.cfg")), "g_language = english");
    assert!(
        sandbox.state().contains(&format!(
            "game_root={}",
            new_library.join("StarCitizen").display()
        )),
        "{}",
        sandbox.state()
    );
    assert!(
        !sandbox.root.join("LIVE").exists(),
        "the old folder must be left alone"
    );
}

#[test]
fn accepts_the_channel_folder_or_the_library_folder_as_path() {
    let server = Server::start();
    let sandbox = Sandbox::new("pathforms");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let expected = format!("game_root={}", sandbox.root.display());

    for given in [
        sandbox.root.join("LIVE"),
        sandbox.library.clone(),
        sandbox.root.clone(),
    ] {
        let _ = fs::remove_dir_all(&sandbox.home);
        let ran = sandbox.run(&server, &["run", "--path", given.to_str().unwrap()]);
        assert_eq!(ran.code, 0, "{}: {}", given.display(), ran.out);
        assert!(sandbox.state().contains(&expected), "{}", sandbox.state());
    }

    let ran = sandbox.run(&server, &["run", "--path", sandbox.home.to_str().unwrap()]);
    assert_eq!(ran.code, 2, "{}", ran.out);
    assert!(
        ran.out.contains("does not look like a Star Citizen folder"),
        "{}",
        ran.out
    );
}

#[test]
fn two_checks_at_the_same_time_do_not_collide() {
    let server = Server::start();
    let sandbox = Sandbox::new("concurrent");
    server.serve(
        "/live.zip",
        Response {
            delay: Duration::from_millis(2500),
            ..Response::zip(release(&strings(1)), "v1", DATE_1)
        },
    );

    let mut slow = sandbox.command(&server, &["run", "--path", sandbox.root.to_str().unwrap()]);
    let slow = slow.stdout(Stdio::piped()).spawn().unwrap();
    // Wait until the first check is talking to the server, then start a second.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server.requests().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the first check never started"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let second = sandbox.run(&server, &["run"]);
    assert_eq!(second.code, 0, "{}", second.out);
    assert!(second.out.contains("already working"), "{}", second.out);

    let first = slow.wait_with_output().unwrap();
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(server.requests().len(), 1);
    assert_eq!(read(&sandbox.ini()), strings(1));
}

#[test]
fn status_describes_the_installation() {
    let server = Server::start();
    let sandbox = Sandbox::new("status");
    server.publish(release(&strings(1)), "v1", DATE_1);

    let ran = sandbox.run(&server, &["status"]);
    assert_eq!(ran.code, 0);
    assert!(
        ran.out.contains(&format!("StarStrings Updater {VERSION}")),
        "{}",
        ran.out
    );
    assert!(ran.out.contains("Not installed"), "{}", ran.out);
    assert!(
        ran.out.contains("Star Citizen was not found automatically"),
        "{}",
        ran.out
    );
    // Pointed at the library, it says which folder it would use - still read-only.
    let mut command = sandbox.command(&server, &["status"]);
    command.env("SSU_SEARCH_PATHS", &sandbox.library);
    let found = finish(command);
    assert!(
        found.out.contains(&format!(
            "Star Citizen found at: {}",
            sandbox.root.display()
        )),
        "{}",
        found.out
    );
    assert!(!sandbox.home.exists(), "status must not create anything");
    assert!(!sandbox.ini().exists(), "status must not install anything");

    assert_eq!(sandbox.update(&server).code, 0);
    let ran = sandbox.run(&server, &["status"]);
    assert_eq!(ran.code, 0);
    for expected in [
        format!("Game folder: {}", sandbox.root.display()),
        "Scheduled task: NOT registered".to_string(),
        "LIVE: StarStrings release of 12 Nov 2026, installed ".to_string(),
        format!("{INI_REL} ({} bytes): ok", strings(1).len()),
        "USER.cfg: g_language = english".to_string(),
    ] {
        assert!(
            ran.out.contains(&expected),
            "missing {expected:?} in:\n{}",
            ran.out
        );
    }

    fs::write(sandbox.ini(), "edited=1").unwrap();
    fs::write(sandbox.cfg(), "g_language = german").unwrap();
    let ran = sandbox.run(&server, &["status"]);
    assert!(
        ran.out.contains("changed since it was installed"),
        "{}",
        ran.out
    );
    assert!(
        ran.out
            .contains("StarStrings is not active with this setting"),
        "{}",
        ran.out
    );

    fs::remove_file(sandbox.ini()).unwrap();
    fs::remove_file(sandbox.cfg()).unwrap();
    let ran = sandbox.run(&server, &["status"]);
    assert!(ran.out.contains("MISSING"), "{}", ran.out);
    assert!(ran.out.contains("g_language is not set"), "{}", ran.out);
    // Status only reports; it never contacts the server.
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn command_line_basics() {
    let server = Server::start();
    let sandbox = Sandbox::new("cli");

    let ran = sandbox.run(&server, &["--version"]);
    assert_eq!(ran.code, 0);
    assert_eq!(ran.out.trim(), format!("StarStrings Updater {VERSION}"));

    let ran = sandbox.run(&server, &["--help"]);
    assert_eq!(ran.code, 0);
    for expected in [
        "Usage:",
        "install",
        "uninstall",
        "--remove-strings",
        "status",
        "--ptu",
    ] {
        assert!(ran.out.contains(expected), "missing {expected:?} in help");
    }

    let ran = sandbox.run(&server, &["frobnicate"]);
    assert_eq!(ran.code, 2);
    assert!(
        ran.out.contains("unknown command: frobnicate"),
        "{}",
        ran.out
    );
    assert!(ran.out.contains("Usage:"), "{}", ran.out);

    let ran = sandbox.run(&server, &["run", "--path"]);
    assert_eq!(ran.code, 2);
    assert!(ran.out.contains("--path needs a value"), "{}", ran.out);

    assert!(
        !sandbox.home.exists(),
        "none of this may create the data folder"
    );
    assert!(server.requests().is_empty());
}

// ---------------------------------------------------------------------------
// Installation (system tests)
// ---------------------------------------------------------------------------
//
// These register a real scheduled task and a real "Installed apps" entry for
// the current user, both under a per-sandbox development name, and remove
// them again. They are skipped by default; run them with
// `cargo test -- --include-ignored`.

fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The names a development copy with this data folder registers under.
fn dev_names(home: &Path) -> (String, String) {
    let tag = format!(
        "{:08x}",
        fnv1a(&home.to_string_lossy().to_lowercase()) as u32
    );
    (
        format!("StarStrings Updater (dev {tag})"),
        format!(
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\StarStringsUpdater-dev-{tag}"
        ),
    )
}

fn tool(program: &str, args: &[&str]) -> (bool, String) {
    let output = Command::new(program).args(args).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

/// Removes whatever a failed system test may have left registered.
struct Cleanup {
    task: String,
    key: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = tool("schtasks", &["/delete", "/tn", &self.task, "/f"]);
        let _ = tool("reg", &["delete", &self.key, "/f"]);
    }
}

#[test]
#[ignore = "registers a (development-named) scheduled task and Installed-apps entry"]
fn install_sets_everything_up_and_uninstall_removes_it() {
    let server = Server::start();
    let sandbox = Sandbox::new("install");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let (task, key) = dev_names(&sandbox.home);
    let _cleanup = Cleanup {
        task: task.clone(),
        key: key.clone(),
    };
    let installed_exe = sandbox.home.join("starstrings-updater.exe");
    let mine = "sys_maxFPS = 120\r\ng_language = german\r\n";
    fs::write(sandbox.cfg(), mine).unwrap();

    // --- install -----------------------------------------------------------
    let ran = sandbox.run(
        &server,
        &[
            "install",
            "--quiet",
            "--path",
            sandbox.root.join("LIVE").to_str().unwrap(),
        ],
    );
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(fs::read(&installed_exe).unwrap(), fs::read(EXE).unwrap());
    assert_eq!(read(&sandbox.ini()), strings(1));
    // Installing StarStrings is asking for English, so the installer switches it.
    assert_eq!(
        read(&sandbox.cfg()),
        "sys_maxFPS = 120\r\ng_language = english\r\n"
    );
    assert!(sandbox.state().contains("LIVE.cfg_previous=german"));

    let (ok, xml) = tool("schtasks", &["/query", "/tn", &task, "/xml"]);
    assert!(ok, "{xml}");
    for expected in [
        format!("<Command>\"{}\"</Command>", installed_exe.display()),
        "<Arguments>run</Arguments>".to_string(),
        "<LogonTrigger>".to_string(),
        "<Delay>PT1M</Delay>".to_string(),
        "<Interval>PT1H</Interval>".to_string(),
        "<LogonType>InteractiveToken</LogonType>".to_string(),
        "<StartWhenAvailable>true</StartWhenAvailable>".to_string(),
        "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>".to_string(),
        "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>".to_string(),
    ] {
        assert!(xml.contains(&expected), "missing {expected} in:\n{xml}");
    }
    // Task Scheduler leaves defaults out of its export, so the unobtrusive
    // settings show up as the absence of their opposites.
    for unwanted in [
        "HighestAvailable",
        "<WakeToRun>true",
        "<Hidden>true",
        "<Password>",
    ] {
        assert!(!xml.contains(unwanted), "unexpected {unwanted} in:\n{xml}");
    }

    let (ok, entry) = tool("reg", &["query", &key]);
    assert!(ok, "{entry}");
    assert!(entry.contains(&task), "{entry}");
    assert!(
        entry.contains(&format!("\"{}\" uninstall", installed_exe.display())),
        "{entry}"
    );
    assert!(entry.contains(VERSION), "{entry}");

    let status = sandbox.run(&server, &["status"]);
    assert!(
        status.out.contains("Scheduled task: registered"),
        "{}",
        status.out
    );
    assert!(status.out.contains("last run: not yet"), "{}", status.out);
    assert!(status.out.contains("next run: "), "{}", status.out);

    // --- the installed copy knows where it lives ---------------------------
    // The scheduled task starts it with no environment overrides at all.
    let mut bare = Command::new(&installed_exe);
    bare.arg("status").stdin(Stdio::null());
    let bare = finish(bare);
    assert_eq!(bare.code, 0);
    assert!(
        bare.out
            .contains(&format!("Data folder: {}", sandbox.home.display())),
        "{}",
        bare.out
    );
    assert!(
        bare.out
            .contains(&format!("Game folder: {}", sandbox.root.display())),
        "{}",
        bare.out
    );

    // --- installing again is a harmless repair ------------------------------
    let ran = sandbox.run(&server, &["install", "--quiet"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(ran.out.contains("up to date"), "{}", ran.out);

    // --- uninstall, run from the installed copy itself ----------------------
    let mut uninstall = Command::new(&installed_exe);
    uninstall
        .args(["uninstall", "--quiet"])
        .env("SSU_SEARCH_PATHS", sandbox.dir.join("nowhere"))
        .stdin(Stdio::null());
    let ran = finish(uninstall);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(
        !tool("schtasks", &["/query", "/tn", &task]).0,
        "the task must be gone"
    );
    assert!(
        !tool("reg", &["query", &key]).0,
        "the registry entry must be gone"
    );
    assert!(!sandbox.home.exists(), "left behind: {:?}", sandbox.files());
    // By default the strings stay in the game.
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert_eq!(
        read(&sandbox.cfg()),
        "sys_maxFPS = 120\r\ng_language = english\r\n"
    );

    // A program cannot delete itself while running, so the uninstaller parks
    // its own file in the temporary folder. Tidy that up after the test.
    for entry in fs::read_dir(std::env::temp_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("starstrings-updater-removed-")
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[test]
#[ignore = "registers a (development-named) scheduled task and Installed-apps entry"]
fn uninstall_can_put_the_game_folder_back_as_it_was() {
    let server = Server::start();
    let sandbox = Sandbox::new("restoregame");
    sandbox.add_channel("PTU");
    server.publish(release(&strings(1)), "v1", DATE_1);
    server.serve(
        "/ptu.zip",
        Response::zip(release(&strings(7)), "ptu-1", DATE_2),
    );
    let (task, key) = dev_names(&sandbox.home);
    let _cleanup = Cleanup {
        task: task.clone(),
        key,
    };

    // LIVE: the player had their own pack and their own language setting.
    let own_pack = "my_own_pack=1\n";
    fs::create_dir_all(sandbox.ini().parent().unwrap()).unwrap();
    fs::write(sandbox.ini(), own_pack).unwrap();
    let own_cfg = "sys_maxFPS = 120\r\ng_language = german\r\n";
    fs::write(sandbox.cfg(), own_cfg).unwrap();
    // PTU: nothing there before.
    let ptu = sandbox.root.join("PTU");

    let ran = sandbox.run(
        &server,
        &[
            "install",
            "--quiet",
            "--ptu",
            "--path",
            sandbox.root.to_str().unwrap(),
        ],
    );
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));
    assert_eq!(read(&ptu.join(INI_REL)), strings(7));
    assert_eq!(read(&ptu.join("USER.cfg")), "g_language = english");

    let ran = sandbox.run(&server, &["uninstall", "--quiet", "--remove-strings"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(!tool("schtasks", &["/query", "/tn", &task]).0);

    // LIVE is exactly as the player had it.
    assert_eq!(read(&sandbox.ini()), own_pack);
    assert_eq!(read(&sandbox.cfg()), own_cfg);
    // PTU is back to a pristine game folder: no strings, no cfg, no empty folders.
    assert_eq!(
        fs::read_dir(&ptu)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        vec!["Data.p4k".to_string()]
    );
    assert!(!sandbox.home.exists(), "left behind: {:?}", sandbox.files());
}

#[test]
#[ignore = "has Task Scheduler run a (development-named) task against the real release on github.com"]
fn task_scheduler_really_runs_the_installed_copy() {
    let server = Server::start();
    let sandbox = Sandbox::new("scheduled");
    server.publish(release(&strings(1)), "v1", DATE_1);
    let (task, key) = dev_names(&sandbox.home);
    let _cleanup = Cleanup {
        task: task.clone(),
        key,
    };

    let ran = sandbox.run(
        &server,
        &[
            "install",
            "--quiet",
            "--path",
            sandbox.root.to_str().unwrap(),
        ],
    );
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert_eq!(read(&sandbox.ini()), strings(1));

    // Have Task Scheduler start the task now, exactly as it will every hour.
    // A task carries none of this test's overrides, so the run finds its data
    // folder by itself, asks the real release on github.com, sees that the
    // stand-in strings are not it, and installs the real ones.
    let (ok, text) = tool("schtasks", &["/run", "/tn", &task]);
    assert!(ok, "{text}");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let state = sandbox.state();
        if state.contains("LIVE.etag=") && !state.contains("LIVE.etag=\"v1\"") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the scheduled run did not update anything; state:\n{state}\nlog:\n{}",
            sandbox.log()
        );
        thread::sleep(Duration::from_millis(500));
    }
    let installed = fs::read(sandbox.ini()).unwrap();
    assert!(
        installed.len() > 1_000_000,
        "expected the real StarStrings file, got {} bytes",
        installed.len()
    );
    assert!(installed.starts_with(&[0xEF, 0xBB, 0xBF]));
    assert!(
        server.requests().len() == 1,
        "the task must not use the test server"
    );

    // Task Scheduler itself reports the run as successful.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let status = sandbox.run(&server, &["status"]).out;
        if status.contains("(succeeded)") {
            assert!(status.contains("last run: 20"), "{status}");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{status}");
        thread::sleep(Duration::from_millis(500));
    }

    let ran = sandbox.run(&server, &["uninstall", "--quiet"]);
    assert_eq!(ran.code, 0, "{}", ran.out);
    assert!(!tool("schtasks", &["/query", "/tn", &task]).0);
}

#[test]
#[ignore = "registers a (development-named) scheduled task and Installed-apps entry"]
fn install_reports_a_missing_game_and_changes_nothing() {
    let server = Server::start();
    let sandbox = Sandbox::new("nogame");
    let (task, key) = dev_names(&sandbox.home);
    let _cleanup = Cleanup {
        task: task.clone(),
        key,
    };

    let ran = sandbox.run(&server, &["install", "--quiet"]);
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(
        ran.out.contains("Star Citizen was not found"),
        "{}",
        ran.out
    );

    let ran = sandbox.run(
        &server,
        &[
            "install",
            "--quiet",
            "--path",
            sandbox.home.to_str().unwrap(),
        ],
    );
    assert_eq!(ran.code, 1, "{}", ran.out);
    assert!(
        ran.out.contains("does not look like a Star Citizen folder"),
        "{}",
        ran.out
    );

    assert!(
        !tool("schtasks", &["/query", "/tn", &task]).0,
        "no task may be registered"
    );
    assert!(!sandbox.home.join("starstrings-updater.exe").exists());
    assert!(server.requests().is_empty());
}

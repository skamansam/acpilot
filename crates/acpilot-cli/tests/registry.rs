//! The signed-index verbs at the process boundary.
//!
//! The index is a local file signed by a throwaway key the test config trusts, and
//! the archive is served by a one-shot HTTP server: the command under test does
//! the signing check, the download, the archive check, and the store install for
//! real, so a change that verifies the index but skips the archive (or the other
//! way round) fails here.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("acpilot-registry-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn acpilot(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_acpilot"))
        .args(args)
        .output()
        .expect("run acpilot")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

/// A deterministic key and its public half, so a test's config can trust it.
fn signing(seed: u8) -> (SigningKey, String) {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let public = hex(&key.verifying_key().to_bytes());
    (key, public)
}

/// Write `index.json` and a detached signature over exactly those bytes.
fn write_index(directory: &Path, entries: &str, key: &SigningKey, key_id: &str) -> PathBuf {
    let body = format!("{{\n  \"schema_version\": 1,\n  \"extensions\": [{entries}]\n}}\n");
    let path = directory.join("index.json");
    fs::write(&path, &body).expect("write the index");
    let signature = ed25519_dalek::Signer::sign(key, body.as_bytes());
    let detached = serde_json::json!({
        "key": key_id,
        "signature": hex(&signature.to_bytes()),
    });
    fs::write(
        format!("{}.sig", path.display()),
        serde_json::to_vec(&detached).expect("a signature file"),
    )
    .expect("write the signature");
    path
}

/// A config root whose global file trusts one test key.
fn config_trusting(directory: &Path, key_id: &str, public_key: &str) -> PathBuf {
    let config = directory.join("config");
    fs::create_dir_all(&config).expect("config dir");
    fs::write(
        config.join("config.toml"),
        format!("[[registry.trusted_keys]]\nid = {key_id:?}\npublic_key = {public_key:?}\n"),
    )
    .expect("write the config");
    config
}

/// The workspace's night-flight extension, packed the way a release would be.
fn pack_fixture() -> Vec<u8> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions/acpilot.night-flight");
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    append_tree(&mut builder, &root, &root);
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

fn append_tree<W: Write>(builder: &mut tar::Builder<W>, root: &Path, directory: &Path) {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .expect("the fixture is readable")
        .map(|entry| entry.expect("an entry"))
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).expect("inside the fixture");
        if path.is_dir() {
            append_tree(builder, root, &path);
        } else {
            builder
                .append_path_with_name(&path, relative)
                .expect("append a fixture file");
        }
    }
}

/// A listener that answers every request with the same bytes.
struct OneFile {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OneFile {
    fn serve(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the archive server");
        let url = format!(
            "http://{}/night-flight-1.0.0.tar.gz",
            listener.local_addr().expect("address")
        );
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut head = Vec::new();
                        let mut buffer = [0u8; 1024];
                        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                            match stream.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(read) => head.extend_from_slice(&buffer[..read]),
                            }
                        }
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.write_all(&body);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for OneFile {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn add_downloads_checks_and_installs_an_indexed_archive() {
    let directory = TempDir::new();
    let (key, public) = signing(11);
    let bytes = pack_fixture();
    let server = OneFile::serve(bytes.clone());
    let entry = format!(
        r#"{{ "id": "acpilot.night-flight", "version": "1.0.0", "kind": "theme", "publisher": "acpilot", "checksum": "sha256:{}", "size": {}, "download": {:?} }}"#,
        sha256_hex(&bytes),
        bytes.len(),
        server.url
    );
    let index = write_index(directory.path(), &entry, &key, "test-key");
    let config = config_trusting(directory.path(), "test-key", &public);
    let store = directory.path().join("extensions");

    let output = acpilot(&[
        "ext",
        "add",
        "acpilot.night-flight",
        "--index",
        index.to_str().expect("index path"),
        "--store",
        store.to_str().expect("store path"),
        "--config",
        config.to_str().expect("config path"),
    ]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("installed acpilot.night-flight@1.0.0"),
        "{out}"
    );
    assert!(
        store
            .join("acpilot.night-flight/1.0.0/extension.toml")
            .is_file(),
        "the archive was unpacked into the store"
    );
}

#[test]
fn an_index_signed_by_an_untrusted_key_is_refused_by_name() {
    let directory = TempDir::new();
    let (key, _) = signing(12);
    let index = write_index(directory.path(), "", &key, "test-key");
    let config = directory.path().join("config");
    fs::create_dir_all(&config).expect("config dir");

    let output = acpilot(&[
        "ext",
        "search",
        "--index",
        index.to_str().expect("index path"),
        "--config",
        config.to_str().expect("config path"),
    ]);
    assert!(!output.status.success(), "an untrusted key is a failure");
    let err = stderr(&output);
    assert!(err.contains("test-key"), "{err}");
    assert!(err.contains("registry.trusted_keys"), "{err}");
}

#[test]
fn an_index_edited_after_signing_is_refused() {
    let directory = TempDir::new();
    let (key, public) = signing(13);
    let index = write_index(directory.path(), "", &key, "test-key");
    let config = config_trusting(directory.path(), "test-key", &public);
    let mut body = fs::read_to_string(&index).expect("read the index");
    body.push('\n');
    fs::write(&index, body).expect("edit the index");

    let output = acpilot(&[
        "ext",
        "search",
        "--index",
        index.to_str().expect("index path"),
        "--config",
        config.to_str().expect("config path"),
    ]);
    assert!(!output.status.success(), "a tampered index is a failure");
    let err = stderr(&output);
    assert!(err.contains("not signed by \"test-key\""), "{err}");
}

#[test]
fn the_default_index_is_the_one_this_build_ships() {
    let directory = TempDir::new();
    let output = acpilot(&[
        "ext",
        "search",
        "--config",
        directory.path().to_str().expect("config path"),
    ]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("this index lists no extensions"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn show_reports_the_index_beside_the_store() {
    let directory = TempDir::new();
    let (key, public) = signing(14);
    let bytes = b"not downloaded by show".to_vec();
    let entry = format!(
        r#"{{ "id": "acpilot.night-flight", "version": "1.0.0", "kind": "theme", "publisher": "acpilot", "checksum": "sha256:{}", "size": {}, "download": "https://example.invalid/night-flight.tar.gz" }}"#,
        sha256_hex(&bytes),
        bytes.len()
    );
    let index = write_index(directory.path(), &entry, &key, "test-key");
    let config = config_trusting(directory.path(), "test-key", &public);
    let store = directory.path().join("extensions");

    let output = acpilot(&[
        "ext",
        "show",
        "acpilot.night-flight",
        "--index",
        index.to_str().expect("index path"),
        "--store",
        store.to_str().expect("store path"),
        "--config",
        config.to_str().expect("config path"),
    ]);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(out.contains("acpilot.night-flight 1.0.0"), "{out}");
    assert!(out.contains("not installed"), "{out}");
}

#[test]
fn remove_deletes_what_was_installed_from_a_directory() {
    let directory = TempDir::new();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions/acpilot.night-flight");
    let store = directory.path().join("extensions");
    let config = directory.path().join("config");
    fs::create_dir_all(&config).expect("config dir");

    let added = acpilot(&[
        "ext",
        "add",
        "--from",
        fixture.to_str().expect("fixture path"),
        "--store",
        store.to_str().expect("store path"),
        "--config",
        config.to_str().expect("config path"),
    ]);
    assert!(
        added.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&added),
        stderr(&added)
    );

    let removed = acpilot(&[
        "ext",
        "remove",
        "acpilot.night-flight",
        "--store",
        store.to_str().expect("store path"),
        "--config",
        config.to_str().expect("config path"),
    ]);
    assert!(
        removed.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&removed),
        stderr(&removed)
    );
    assert!(
        stdout(&removed).contains("removed acpilot.night-flight@1.0.0"),
        "{}",
        stdout(&removed)
    );

    let listed = acpilot(&[
        "ext",
        "list",
        "--store",
        store.to_str().expect("store path"),
        "--config",
        config.to_str().expect("config path"),
    ]);
    assert!(listed.status.success());
    assert!(
        stdout(&listed).contains("no extensions installed"),
        "{}",
        stdout(&listed)
    );
}

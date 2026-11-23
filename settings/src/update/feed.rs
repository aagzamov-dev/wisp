use serde_json::Value;
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::ptr::{NonNull, null, null_mut};
use std::time::{Duration, Instant};

const REPOSITORY: &str = "aagzamov-dev/wisp";
const API_HOST: &str = "api.github.com";
const ASSET_HOST: &str = "github.com";
const MAX_RELEASE_BYTES: usize = 1024 * 1024;
const MAX_ASSET_BYTES: usize = 32 * 1024 * 1024;
const BINARIES: [&str; 2] = ["wisp.exe", "wisp-settings.exe"];

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    assets: [Asset; 2],
}

#[derive(Clone, Debug)]
struct Asset {
    url: String,
    fingerprint: Fingerprint,
}

#[derive(Clone, Copy, Debug)]
pub struct Fingerprint {
    pub size: usize,
    pub digest: [u8; 32],
}

impl Release {
    pub fn fingerprints(&self) -> [Fingerprint; 2] {
        [self.assets[0].fingerprint, self.assets[1].fingerprint]
    }
}

pub fn available() -> bool {
    !cfg!(debug_assertions) && !cfg!(feature = "nightly") && option_env!("WISP_RELEASE_REPOSITORY") == Some(REPOSITORY)
}

pub fn check() -> Result<Option<Release>, String> {
    if !available() {
        return Err("This build has no update feed.".into());
    }

    let client = HttpClient::new()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let request = client.get(
        API_HOST,
        &format!("/repos/{REPOSITORY}/releases/latest"),
        Some("Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n"),
        deadline,
    )?;
    match request.status()? {
        200 => {}
        404 => return Err("No stable Wisp release is currently published.".into()),
        status => return Err(format!("GitHub could not provide the latest release (HTTP {status}).")),
    }
    let body = request.body(MAX_RELEASE_BYTES, deadline)?;
    let document = serde_json::from_slice(&body).map_err(|_| "GitHub returned invalid release metadata.")?;
    parse_release(&document, env!("CARGO_PKG_VERSION"))
}

pub fn download(release: &Release, staging: &Path) -> Result<(), String> {
    if !available() {
        return Err("This build has no update feed.".into());
    }

    let client = HttpClient::new()?;
    let deadline = Instant::now() + Duration::from_secs(120);
    download_with(release, staging, |asset| client.download(asset, deadline))
}

pub fn verify_files(staging: &Path, fingerprints: &[Fingerprint; 2]) -> Result<(), String> {
    for (name, fingerprint) in BINARIES.into_iter().zip(fingerprints) {
        let read_error = || format!("Cannot read staged {name}.");
        if !(1..=MAX_ASSET_BYTES).contains(&fingerprint.size) {
            return Err(format!("Invalid size for staged {name}."));
        }
        let path = staging.join(name);
        let metadata = fs::symlink_metadata(&path).map_err(|_| read_error())?;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!("Staged {name} must be a regular file."));
        }
        const FILE_SHARE_READ: u32 = 1;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .map_err(|_| read_error())?;
        let mut bytes = Vec::new();
        file.take(fingerprint.size as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| read_error())?;
        verify_payload(name, fingerprint, &bytes)?;
    }
    Ok(())
}

fn stable_version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.split('.');
    let mut version = [0; 3];
    for component in &mut version {
        let part = parts.next()?;
        if part.len() > 1 && part.starts_with('0') || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *component = part.parse().ok()?;
    }
    parts.next().is_none().then_some(version)
}

fn parse_release(document: &Value, installed: &str) -> Result<Option<Release>, String> {
    if document["draft"].as_bool() != Some(false) || document["prerelease"].as_bool() != Some(false) {
        return Err("GitHub did not return a published stable release.".into());
    }
    let tag = document["tag_name"].as_str().ok_or("The release has no version tag.")?;
    let version = tag
        .strip_prefix('v')
        .ok_or("The release tag is not a stable version.")?;
    let remote = stable_version(version).ok_or("The release tag is not a stable version.")?;
    let local = stable_version(installed).ok_or("The installed Wisp version is not a stable version.")?;
    if remote <= local {
        return Ok(None);
    }
    let assets = document["assets"]
        .as_array()
        .ok_or("The release has no executable assets.")?;
    Ok(Some(Release {
        version: version.into(),
        assets: [
            parse_asset(assets, tag, "wisp")?,
            parse_asset(assets, tag, "wisp-settings")?,
        ],
    }))
}

fn parse_asset(assets: &[Value], tag: &str, stem: &str) -> Result<Asset, String> {
    let name = format!("{stem}-{tag}-x86_64.exe");
    let mut matches = assets.iter().filter(|asset| asset["name"].as_str() == Some(&name));
    let asset = matches
        .next()
        .ok_or_else(|| format!("The release is missing {name}."))?;
    if matches.next().is_some() || asset["state"].as_str() != Some("uploaded") {
        return Err(format!("The release has invalid metadata for {name}."));
    }
    let url = format!("https://{ASSET_HOST}/{REPOSITORY}/releases/download/{tag}/{name}");
    if asset["browser_download_url"].as_str() != Some(&url) {
        return Err(format!("The release has an unexpected download URL for {name}."));
    }
    let size = asset["size"]
        .as_u64()
        .filter(|&size| (1..=MAX_ASSET_BYTES as u64).contains(&size))
        .ok_or_else(|| format!("The release has an invalid size for {name}."))? as usize;
    let digest = asset["digest"]
        .as_str()
        .and_then(parse_digest)
        .ok_or_else(|| format!("The release has no valid SHA-256 digest for {name}."))?;
    Ok(Asset {
        url,
        fingerprint: Fingerprint { size, digest },
    })
}

fn parse_digest(value: &str) -> Option<[u8; 32]> {
    let hex = value.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

fn download_with(
    release: &Release,
    staging: &Path,
    mut fetch: impl FnMut(&Asset) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    let mut payloads = Vec::with_capacity(BINARIES.len());
    for (name, asset) in BINARIES.into_iter().zip(&release.assets) {
        let payload = fetch(asset)?;
        verify_payload(name, &asset.fingerprint, &payload)?;
        payloads.push(payload);
    }

    // Stage the pair only after both downloads have passed verification.
    let mut created = Vec::with_capacity(BINARIES.len());
    for (name, bytes) in BINARIES.into_iter().zip(payloads) {
        let path = staging.join(name);
        let write = (|| -> io::Result<()> {
            let mut file = OpenOptions::new().create_new(true).write(true).open(&path)?;
            created.push(path);
            file.write_all(&bytes)?;
            file.sync_all()
        })();
        if let Err(error) = write {
            for path in created {
                let _ = fs::remove_file(path);
            }
            return Err(format!("Cannot stage {name}: {error}"));
        }
    }
    Ok(())
}

fn verify_payload(name: &str, fingerprint: &Fingerprint, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() != fingerprint.size {
        return Err(format!("{name} does not match the expected download size."));
    }
    if sha256(bytes)? != fingerprint.digest {
        return Err(format!("{name} does not match the GitHub release digest."));
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> Result<[u8; 32], String> {
    const BCRYPT_SHA256_ALG_HANDLE: usize = 0x41;
    let mut digest = [0; 32];
    let length = u32::try_from(bytes.len()).map_err(|_| "The file is too large to verify.")?;
    // The Windows 10+ pseudo-handle is predefined and does not need closing.
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE as *mut c_void,
            null(),
            0,
            bytes.as_ptr(),
            length,
            digest.as_mut_ptr(),
            digest.len() as u32,
        )
    };
    if status < 0 {
        return Err(format!("Windows could not verify a release file (0x{status:08x})."));
    }
    Ok(digest)
}

fn asset_redirect(url: &str) -> Result<(&str, &str), String> {
    const CDN_HOSTS: [&str; 2] = ["release-assets.githubusercontent.com", "objects.githubusercontent.com"];
    let (host, path) = url
        .strip_prefix("https://")
        .and_then(|url| url.split_once('/'))
        .ok_or("GitHub returned an invalid asset redirect.")?;
    if !CDN_HOSTS.contains(&host)
        || path.is_empty()
        || !path
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'\\' | b'#'))
    {
        return Err("GitHub returned an untrusted asset redirect.".into());
    }
    Ok((host, &url[url.len() - path.len() - 1..]))
}

// A small WinHTTP binding keeps release builds on Windows' native TLS stack.
const WINHTTP_ACCESS_TYPE_NO_PROXY: u32 = 1;
const WINHTTP_OPTION_CONNECT_RETRIES: u32 = 4;
const WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT: u32 = 7;
const WINHTTP_OPTION_DISABLE_FEATURE: u32 = 63;
const WINHTTP_OPTION_SECURE_PROTOCOLS: u32 = 84;
const WINHTTP_OPTION_MAX_RESPONSE_HEADER_SIZE: u32 = 91;
const WINHTTP_DISABLE_COOKIES: u32 = 1;
const WINHTTP_DISABLE_REDIRECTS: u32 = 2;
const WINHTTP_DISABLE_AUTHENTICATION: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_FLAG_ESCAPE_DISABLE: u32 = 0x40;
const WINHTTP_FLAG_ESCAPE_DISABLE_QUERY: u32 = 0x80;
const WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2: u32 = 0x800;
const WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3: u32 = 0x2000;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_LOCATION: u32 = 33;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const IO_TIMEOUT_MS: i32 = 5_000;

struct HttpClient(InternetHandle);
struct InternetHandle(NonNull<c_void>);

impl HttpClient {
    fn new() -> Result<Self, String> {
        let user_agent = wide(concat!("Wisp-Settings/", env!("CARGO_PKG_VERSION")));
        let session = InternetHandle::new(
            unsafe { WinHttpOpen(user_agent.as_ptr(), WINHTTP_ACCESS_TYPE_NO_PROXY, null(), null(), 0) },
            "open the update session",
        )?;
        let handle = session.0.as_ptr();
        winhttp(
            unsafe { WinHttpSetTimeouts(handle, IO_TIMEOUT_MS, IO_TIMEOUT_MS, IO_TIMEOUT_MS, IO_TIMEOUT_MS) },
            "set update timeouts",
        )?;
        session.set(WINHTTP_OPTION_CONNECT_RETRIES, 2)?;
        session.set(WINHTTP_OPTION_MAX_RESPONSE_HEADER_SIZE, 32 * 1024)?;
        session.set(
            WINHTTP_OPTION_SECURE_PROTOCOLS,
            WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3,
        )?;
        Ok(Self(session))
    }

    fn get(&self, host: &str, path: &str, headers: Option<&str>, deadline: Instant) -> Result<Request, String> {
        check_deadline(deadline)?;
        let host = wide(host);
        let connection = InternetHandle::new(
            unsafe { WinHttpConnect(self.0.0.as_ptr(), host.as_ptr(), 443, 0) },
            "connect to GitHub",
        )?;
        let path = wide(path);
        let request = InternetHandle::new(
            unsafe {
                WinHttpOpenRequest(
                    connection.0.as_ptr(),
                    null(),
                    path.as_ptr(),
                    null(),
                    null(),
                    null(),
                    WINHTTP_FLAG_SECURE | WINHTTP_FLAG_ESCAPE_DISABLE | WINHTTP_FLAG_ESCAPE_DISABLE_QUERY,
                )
            },
            "open the HTTPS request",
        )?;
        request.set(
            WINHTTP_OPTION_DISABLE_FEATURE,
            WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_AUTHENTICATION,
        )?;
        request.set(WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT, IO_TIMEOUT_MS as u32)?;
        let headers = headers.map(wide);
        let (headers, count) = headers
            .as_ref()
            .map_or((null(), 0), |wide| (wide.as_ptr(), (wide.len() - 1) as u32));
        let handle = request.0.as_ptr();
        winhttp(
            unsafe { WinHttpSendRequest(handle, headers, count, null(), 0, 0, 0) },
            "send the HTTPS request",
        )?;
        check_deadline(deadline)?;
        winhttp(
            unsafe { WinHttpReceiveResponse(handle, null_mut()) },
            "read the GitHub response headers",
        )?;
        check_deadline(deadline)?;
        Ok(Request {
            handle: request,
            _connection: connection,
        })
    }

    fn download(&self, asset: &Asset, deadline: Instant) -> Result<Vec<u8>, String> {
        // Asset URLs have already been matched to the repository, tag, and asset name.
        let path = asset
            .url
            .strip_prefix("https://github.com")
            .ok_or("Invalid GitHub asset URL.")?;
        let origin = self.get(ASSET_HOST, path, None, deadline)?;
        let status = origin.status()?;
        match status {
            200 => origin.body(asset.fingerprint.size, deadline),
            301 | 302 | 303 | 307 | 308 => {
                let location = origin.location()?;
                let (host, path) = asset_redirect(&location)?;
                let cdn = self.get(host, path, None, deadline)?;
                match cdn.status()? {
                    200 => cdn.body(asset.fingerprint.size, deadline),
                    status => Err(format!("The GitHub download server returned HTTP {status}.")),
                }
            }
            _ => Err(format!("GitHub could not provide the release asset (HTTP {status}).")),
        }
    }
}

struct Request {
    handle: InternetHandle,
    // Close the request before its parent connection.
    _connection: InternetHandle,
}

impl Request {
    fn status(&self) -> Result<u32, String> {
        let mut status = 0_u32;
        let mut length = size_of::<u32>() as u32;
        winhttp(
            unsafe {
                WinHttpQueryHeaders(
                    self.handle.0.as_ptr(),
                    WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                    null(),
                    (&mut status as *mut u32).cast(),
                    &mut length,
                    null_mut(),
                )
            },
            "read the GitHub status code",
        )?;
        Ok(status)
    }

    fn location(&self) -> Result<String, String> {
        let mut buffer = [0_u16; 8192];
        let mut length = size_of_val(&buffer) as u32;
        winhttp(
            unsafe {
                WinHttpQueryHeaders(
                    self.handle.0.as_ptr(),
                    WINHTTP_QUERY_LOCATION,
                    null(),
                    buffer.as_mut_ptr().cast(),
                    &mut length,
                    null_mut(),
                )
            },
            "read the GitHub redirect",
        )?;
        // WinHTTP's returned byte count excludes the terminating NUL.
        let count = length as usize / size_of::<u16>();
        String::from_utf16(&buffer[..count]).map_err(|_| "GitHub returned an invalid redirect.".into())
    }

    fn body(&self, limit: usize, deadline: Instant) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            check_deadline(deadline)?;
            let mut read = 0;
            winhttp(
                unsafe {
                    WinHttpReadData(
                        self.handle.0.as_ptr(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        &mut read,
                    )
                },
                "read the GitHub response body",
            )?;
            check_deadline(deadline)?;
            if read == 0 {
                return Ok(bytes);
            }
            if bytes.len() + read as usize > limit {
                return Err("The GitHub response exceeded its expected size.".into());
            }
            bytes.extend_from_slice(&buffer[..read as usize]);
        }
    }
}

impl InternetHandle {
    fn new(raw: *mut c_void, action: &str) -> Result<Self, String> {
        NonNull::new(raw).map(Self).ok_or_else(|| winhttp_error(action))
    }

    fn set(&self, option: u32, value: u32) -> Result<(), String> {
        winhttp(
            unsafe {
                WinHttpSetOption(
                    self.0.as_ptr(),
                    option,
                    (&value as *const u32).cast(),
                    size_of::<u32>() as u32,
                )
            },
            "configure the HTTPS request",
        )
    }
}

impl Drop for InternetHandle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0.as_ptr()) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn check_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("The update request timed out.".into());
    }
    Ok(())
}

fn winhttp(result: i32, action: &str) -> Result<(), String> {
    if result == 0 {
        return Err(winhttp_error(action));
    }
    Ok(())
}

fn winhttp_error(action: &str) -> String {
    let code = io::Error::last_os_error().raw_os_error().unwrap_or_default();
    format!("Windows could not {action} (error {code}).")
}

#[link(name = "winhttp")]
unsafe extern "system" {
    fn WinHttpOpen(agent: *const u16, access: u32, proxy: *const u16, bypass: *const u16, flags: u32) -> *mut c_void;
    fn WinHttpConnect(session: *mut c_void, server: *const u16, port: u16, reserved: u32) -> *mut c_void;
    fn WinHttpOpenRequest(
        connection: *mut c_void,
        verb: *const u16,
        path: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept_types: *const *const u16,
        flags: u32,
    ) -> *mut c_void;
    fn WinHttpSetTimeouts(handle: *mut c_void, resolve: i32, connect: i32, send: i32, receive: i32) -> i32;
    fn WinHttpSetOption(handle: *mut c_void, option: u32, buffer: *const c_void, length: u32) -> i32;
    fn WinHttpSendRequest(
        request: *mut c_void,
        headers: *const u16,
        header_length: u32,
        optional: *const c_void,
        optional_length: u32,
        total_length: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: *mut c_void, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: *mut c_void,
        level: u32,
        name: *const u16,
        buffer: *mut c_void,
        length: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(request: *mut c_void, buffer: *mut c_void, length: u32, read: *mut u32) -> i32;
    fn WinHttpCloseHandle(handle: *mut c_void) -> i32;
}

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptHash(
        algorithm: *mut c_void,
        secret: *const u8,
        secret_length: u32,
        input: *const u8,
        input_length: u32,
        output: *mut u8,
        output_length: u32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SHA256_ABC: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SHA256_XYZ: &str = "sha256:3608bca1e44ea6c4d268eb6db02260269892c0b42b86bbf1e77a6fa16c3c9282";

    fn metadata() -> Value {
        json!({
            "draft": false,
            "prerelease": false,
            "tag_name": "v0.10.0",
            "assets": (["wisp", "wisp-settings"].into_iter()
                .zip([SHA256_ABC, SHA256_XYZ])
                .map(|(stem, digest)| {
                    let name = format!("{stem}-v0.10.0-x86_64.exe");
                    json!({
                        "name": name,
                        "state": "uploaded",
                        "browser_download_url": format!("https://github.com/aagzamov-dev/wisp/releases/download/v0.10.0/{name}"),
                        "size": 3,
                        "digest": digest,
                    })
                })
                .collect::<Vec<_>>())
        })
    }

    #[test]
    fn compares_stable_versions_numerically_and_rejects_unstable_tags() {
        let document = metadata();
        assert_eq!(parse_release(&document, "0.9.12").unwrap().unwrap().version, "0.10.0");
        assert!(parse_release(&document, "0.10.0").unwrap().is_none());
        assert!(parse_release(&document, "1.0.0").unwrap().is_none());
        for tag in ["v01.2.3", "v1.2.3-beta", "v1.2", "v1.2.+3", "v1.2.3+meta", "1.2.3"] {
            let mut document = metadata();
            document["tag_name"] = json!(tag);
            assert!(parse_release(&document, "0.1.0").is_err(), "{tag}");
        }
        for flag in ["draft", "prerelease"] {
            let mut document = metadata();
            document[flag] = json!(true);
            assert!(parse_release(&document, "0.1.0").is_err(), "{flag}");
        }
    }

    #[test]
    fn rejects_untrusted_or_ambiguous_asset_metadata() {
        for (key, bad_value) in [
            ("digest", json!(null)),
            ("digest", json!("sha256:qqq")),
            ("state", json!("open")),
            ("size", json!(MAX_ASSET_BYTES + 1)),
            ("name", json!("wisp-v0.11.0-x86_64.exe")),
            ("browser_download_url", json!("https://example.org/wisp.exe")),
            (
                "browser_download_url",
                json!("https://github.com/attacker/wisp/releases/download/v0.10.0/wisp-v0.10.0-x86_64.exe"),
            ),
            (
                "browser_download_url",
                json!("https://github.com/aagzamov-dev/wisp/releases/download/v0.11.0/wisp-v0.10.0-x86_64.exe"),
            ),
        ] {
            let mut document = metadata();
            document["assets"][0][key] = bad_value;
            assert!(parse_release(&document, "0.1.0").is_err(), "{key}");
        }

        let mut document = metadata();
        let duplicate = document["assets"][0].clone();
        document["assets"].as_array_mut().unwrap().push(duplicate);
        assert!(parse_release(&document, "0.1.0").is_err());
    }

    #[test]
    fn follows_only_allowed_https_asset_redirects() {
        let path = "/github-production-release-asset/123?token=abc%2Bxyz";
        assert_eq!(
            asset_redirect(&format!("https://release-assets.githubusercontent.com{path}")).unwrap(),
            ("release-assets.githubusercontent.com", path),
        );
        for url in [
            "http://release-assets.githubusercontent.com/file",
            "https://release-assets.githubusercontent.com.evil.example/file",
            "https://release-assets.githubusercontent.com@evil.example/file",
            "https://release-assets.githubusercontent.com:444/file",
            "https://evil.example/file",
            "https://release-assets.githubusercontent.com/file#fragment",
            "/relative/path",
        ] {
            assert!(asset_redirect(url).is_err(), "{url}");
        }
    }

    #[test]
    fn stages_both_verified_payloads_and_rechecks_them() {
        let stage = tempfile::tempdir().unwrap();
        let release = parse_release(&metadata(), "0.1.0").unwrap().unwrap();
        let result = download_with(&release, stage.path(), |asset| {
            Ok(if asset.url.contains("wisp-settings") {
                b"xyz"
            } else {
                b"abc"
            }
            .to_vec())
        });
        result.unwrap();
        assert_eq!(std::fs::read(stage.path().join("wisp.exe")).unwrap(), b"abc");
        assert_eq!(std::fs::read(stage.path().join("wisp-settings.exe")).unwrap(), b"xyz");
        assert!(verify_files(stage.path(), &release.fingerprints()).is_ok());
        std::fs::write(stage.path().join("wisp-settings.exe"), b"abc").unwrap();
        assert!(verify_files(stage.path(), &release.fingerprints()).is_err());
    }

    #[test]
    fn writes_nothing_on_a_partial_or_mismatched_download() {
        let stage = tempfile::tempdir().unwrap();
        let release = parse_release(&metadata(), "0.1.0").unwrap().unwrap();
        for invalid in [b"abc".as_slice(), b"x", b"xyz\0"] {
            let result = download_with(&release, stage.path(), |asset| {
                Ok(if asset.url.contains("wisp-settings") {
                    invalid
                } else {
                    b"abc"
                }
                .to_vec())
            });
            assert!(result.is_err());
            assert_eq!(std::fs::read_dir(stage.path()).unwrap().count(), 0);
        }
    }
}

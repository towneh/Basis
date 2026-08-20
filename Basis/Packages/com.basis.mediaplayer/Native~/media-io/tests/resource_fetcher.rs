//! `ResourceFetcher`: the byte cap that bounds how much attacker-chosen
//! data a playlist lane will buffer, and the origin split that decides
//! whether the lane has a filesystem arm at all.

use std::path::PathBuf;
use std::sync::Arc;

use media_hls::SegmentFetcher;
use media_io::{CancelToken, IoLimits, PublicAddressGate, ResourceFetcher};

/// Per-process scratch directory, created on first use.
fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bm-fetch-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A fetcher for a playlist opened from disk, rooted at the scratch
/// directory the fixtures below are written into.
fn local_fetcher() -> ResourceFetcher {
    ResourceFetcher::local(
        &scratch_dir(),
        IoLimits::default(),
        Arc::new(PublicAddressGate),
        CancelToken::new(),
    )
    .expect("scratch dir canonicalises")
}

/// A fetcher for a playlist that came off the network.
fn remote_fetcher() -> ResourceFetcher {
    ResourceFetcher::remote(
        IoLimits::default(),
        Arc::new(PublicAddressGate),
        CancelToken::new(),
    )
}

/// A file of `len` bytes in the scratch directory, returned as the string
/// the fetcher takes.
fn scratch_file(name: &str, len: usize) -> (PathBuf, String) {
    let path = scratch_dir().join(name);
    std::fs::write(&path, vec![b'x'; len]).expect("write scratch file");
    let url = path.to_str().expect("scratch path is UTF-8").to_owned();
    (path, url)
}

/// A resource at exactly the cap is served whole: the bound is inclusive,
/// so the boundary case is playable rather than a spurious refusal.
#[test]
fn a_file_at_the_cap_is_served_whole() {
    let (path, url) = scratch_file("at-cap.bin", 64);
    let bytes = local_fetcher()
        .fetch(&url, 64)
        .expect("at the cap, so served");
    assert_eq!(bytes.len(), 64, "every byte of the resource");
    let _ = std::fs::remove_file(path);
}

/// One byte past the cap refuses. The read is bounded at `cap + 1`, so
/// this is the first length that can trip the post-read check, and an
/// off-by-one in either direction shows up here.
#[test]
fn a_file_past_the_cap_refuses() {
    let (path, url) = scratch_file("past-cap.bin", 65);
    let err = local_fetcher()
        .fetch(&url, 64)
        .expect_err("past the cap, so refused");
    assert!(
        err.to_string().contains("exceeds the 64-byte cap"),
        "refusal names the cap it enforced: {err}"
    );
    let _ = std::fs::remove_file(path);
}

/// The cap binds the bytes returned, not the length the filesystem stated
/// for the path. Nothing the fetcher hands back may exceed it, which is
/// what keeps a file that grows (or is swapped) after the size is taken
/// from turning a bounded fetch into an unbounded allocation. The race
/// itself is not reproducible in-process; this pins the invariant that
/// makes losing it harmless.
#[test]
fn no_fetch_returns_more_than_its_cap() {
    let (path, url) = scratch_file("bound.bin", 4096);
    for cap in [0u64, 1, 255, 4095, 4096] {
        match local_fetcher().fetch(&url, cap) {
            Ok(bytes) => assert!(
                bytes.len() as u64 <= cap,
                "served {} bytes against a {cap}-byte cap",
                bytes.len()
            ),
            Err(e) => assert!(
                e.to_string().contains("exceeds the"),
                "refused for a reason other than the cap: {e}"
            ),
        }
    }
    let _ = std::fs::remove_file(path);
}

/// A resource that is not there fails as an I/O error naming the path,
/// not as a cap refusal — the two are different diagnoses and the lane
/// reports them separately.
#[test]
fn a_missing_file_is_an_io_error_not_a_cap_refusal() {
    let missing = scratch_dir().join("not-here.bin");
    let url = missing.to_str().expect("path is UTF-8");
    let err = local_fetcher().fetch(url, 64).expect_err("no such file");
    assert!(
        !err.to_string().contains("exceeds the"),
        "a missing file is not a cap refusal: {err}"
    );
}

/// A resource outside the playlist's own directory is refused even
/// though the fetcher has a filesystem arm and the file is readable. The
/// disk-origin lane is confined to where the playlist sits, not given
/// the whole filesystem.
#[test]
fn a_disk_playlist_cannot_read_outside_its_directory() {
    let outside = scratch_dir().join("outside.bin");
    std::fs::write(&outside, b"secret").expect("write outside the root");
    let root = scratch_dir().join("playlist-dir");
    std::fs::create_dir_all(&root).expect("playlist dir");
    let fetcher = || {
        ResourceFetcher::local(
            &root,
            IoLimits::default(),
            Arc::new(PublicAddressGate),
            CancelToken::new(),
        )
        .expect("root canonicalises")
    };

    // Absolute, and a walk back out through the parent. Both name the
    // same readable file; neither is inside the root — and they are
    // caught by different screens, so each row names the one it expects.
    // "directory" alone appears in all three refusals, and a
    // reclassification between them would pass while the URLs had swapped
    // which screen caught them.
    let absolute = outside.to_str().expect("UTF-8").to_owned();
    let traversal = root
        .join("..")
        .join("outside.bin")
        .to_str()
        .expect("UTF-8")
        .to_owned();
    for (url, expected) in [
        (absolute, "outside the playlist's directory"),
        (traversal, "walks out of its directory"),
    ] {
        let Err(err) = fetcher().fetch(&url, 64) else {
            panic!("{url:?} must refuse, not serve");
        };
        assert!(
            err.to_string().contains(expected),
            "{url:?} refused as {err}, not {expected:?}"
        );
    }

    // The control: the same bytes inside the root do serve, so the
    // refusals above are the confinement and not a broken lane.
    let inside = root.join("inside.bin");
    std::fs::write(&inside, b"secret").expect("write inside the root");
    let url = inside.to_str().expect("UTF-8").to_owned();
    assert_eq!(
        fetcher().fetch(&url, 64).expect("inside the root").len(),
        6,
        "the confined lane still serves its own directory"
    );
}

/// Remove a planted link by what it actually is. A Unix symlink needs
/// `remove_file` — `remove_dir` refuses it with ENOTDIR and `remove_dir_all`
/// will not follow it either — while a Windows junction needs `remove_dir`.
/// Getting this wrong leaves the link behind, and the next run then fails
/// to plant one, prints SKIPPED and asserts nothing.
fn remove_planted_link(link: &std::path::Path) {
    if link.symlink_metadata().is_err() {
        return;
    }
    if std::fs::remove_file(link).is_ok() {
        return;
    }
    let _ = std::fs::remove_dir(link);
}

/// Plant a directory link inside `root` pointing at `target`. Windows
/// symlinks need Developer Mode or elevation, but a directory junction
/// needs neither and `canonicalize` resolves both, so fall back to one
/// rather than let the row quietly stop running on an ordinary box.
/// Returns false when the host allows neither.
#[cfg(unix)]
fn plant_directory_link(link: &std::path::Path, target: &std::path::Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

/// As above. Defined per platform rather than branched inside one body,
/// so neither arm carries a `return` the other one needs.
#[cfg(windows)]
fn plant_directory_link(link: &std::path::Path, target: &std::path::Path) -> bool {
    if std::os::windows::fs::symlink_dir(target, link).is_ok() {
        return true;
    }
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// A link planted inside the playlist's directory that points outside it
/// is refused. The lexical screen cannot see this one — every component
/// is an ordinary name under the root — so it is the canonicalising
/// screen that catches it, which is exactly why that screen is there.
#[test]
fn a_link_out_of_the_root_is_refused() {
    let outside = scratch_dir().join("link-target-dir");
    std::fs::create_dir_all(&outside).expect("target dir");
    std::fs::write(outside.join("secret.bin"), b"secret").expect("write the target");
    let root = scratch_dir().join("link-root");
    std::fs::create_dir_all(&root).expect("playlist dir");
    let link = root.join("escape");
    remove_planted_link(&link);
    if !plant_directory_link(&link, &outside) {
        eprintln!("SKIPPED: this host allows neither a symlink nor a junction");
        return;
    }

    let mut fetcher = ResourceFetcher::local(
        &root,
        IoLimits::default(),
        Arc::new(PublicAddressGate),
        CancelToken::new(),
    )
    .expect("root canonicalises");
    let through_link = link.join("secret.bin");
    let url = through_link.to_str().expect("UTF-8").to_owned();
    let Err(err) = fetcher.fetch(&url, 64) else {
        panic!("a link out of the root must refuse, not serve");
    };
    assert!(
        err.to_string().contains("resolves outside"),
        "caught by the wrong screen, so the link was never resolved: {err}"
    );
    remove_planted_link(&link);
}

/// A playlist that came off the network has no filesystem arm, so a URI
/// it names cannot be read off disk however it is spelled. The file is
/// real and within what the local fetcher would serve, which is the
/// point: the refusal is the fetcher's origin, not a missing file.
#[test]
fn a_network_playlist_cannot_read_a_local_file() {
    let (path, url) = scratch_file("readable.bin", 16);
    assert_eq!(
        local_fetcher()
            .fetch(&url, 64)
            .expect("local lane serves it")
            .len(),
        16,
        "the same resource is served from a disk-origin playlist"
    );
    let err = remote_fetcher()
        .fetch(&url, 64)
        .expect_err("network origin has no filesystem arm");
    assert!(
        err.to_string().contains("may not name a local resource"),
        "refused as an origin violation: {err}"
    );
    let _ = std::fs::remove_file(path);
}

/// The spellings a hostile playlist reaches for. None of them are
/// `http://` or `https://`, so all of them land on the arm a
/// network-origin fetcher does not have — including the Windows
/// drive-relative form that reads as a URL but resolves as a path, and
/// the UNC form, which would otherwise be an outbound SMB connect the
/// address gate never sees.
#[test]
fn a_network_playlist_refuses_every_non_http_spelling() {
    for url in [
        "c://Windows/win.ini",
        "C://Windows//System32/drivers/etc/hosts",
        "file:///etc/passwd",
        "\\\\attacker.example\\share\\clip.ts",
        "/etc/passwd",
        "../../../etc/passwd",
        "ftp://attacker.example/clip.ts",
    ] {
        let Err(err) = remote_fetcher().fetch(url, 64) else {
            panic!("{url:?} must refuse, not serve");
        };
        assert!(
            err.to_string().contains("may not name a local resource"),
            "{url:?} refused for the wrong reason: {err}"
        );
    }
}

/// The other side of that seam: a fetcher rooted at the current
/// directory serves a resource resolved against it. Cargo runs a test
/// with the package directory as the working directory, so the
/// manifest is a file that is certainly there and certainly inside the
/// root — the row is about the two spellings agreeing, not the bytes.
#[test]
fn a_root_of_the_current_directory_serves_what_resolves_against_it() {
    let mut fetcher = ResourceFetcher::local(
        std::path::Path::new("."),
        IoLimits::default(),
        Arc::new(PublicAddressGate),
        CancelToken::new(),
    )
    .expect("the current directory canonicalises");
    let resolved = std::path::Path::new(".").join("Cargo.toml");
    let url = resolved.to_str().expect("UTF-8");
    let bytes = fetcher
        .fetch(url, 1024 * 1024)
        .unwrap_or_else(|e| panic!("{url:?} is inside the root and must serve: {e}"));
    assert!(!bytes.is_empty(), "served the manifest");
}

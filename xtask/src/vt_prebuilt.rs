//! Fetches the prebuilt `libghostty-vt` archive the `ghostty` feature links, and verifies it.
//!
//! **The point is that the build does not do this.** `ghostty-vt-rs-sys`' own `build.rs` will download and
//! check the same archive on a first build, which is mechanically what happens here — the difference is
//! *when*, and it is the whole of the decision this feature rests on (`goal-one-vt-core.md`, D4): a build
//! that reaches the network fails differently on a machine that has none, and cannot be reproduced. So the
//! fetch happens here, once, and the build is handed the result through
//! `GHOSTTY_VT_RS_SYS_PREBUILT_DIR` with `GHOSTTY_VT_RS_SYS_BUILD=prebuilt` beside it, which makes a
//! build-from-source a refusal rather than a silent attempt that would need a Zig this repository does not
//! pin.
//!
//! **Nothing about the release is written here.** The version comes from `Cargo.lock`, which is the pin;
//! the Ghostty commit and the SHA-256 of every target's archive come from the dependency's own
//! `prebuilt.rs`, which its CI writes. A number copied into this file would be a second place for the pin
//! to live and a second place for it to go stale.
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the archives are published: the `ghostty-<commit12>` release of the crate's own repository.
const RELEASES: &str = "https://github.com/nklmilojevic/ghostty-vt-rs/releases/download";

/// The dependency whose pin and checksums this reads.
const CRATE: &str = "ghostty-vt-rs-sys";

pub fn fetch(root: &Path) -> Result<(), String> {
    let triple = host_triple()?;
    let version = locked_version(root)?;
    let source = crate_source(&version)?;
    let (commit, checksum) = pinned(&source, &triple)?;

    let short = &commit[..12];
    let into = root.join("target").join("ghostty-vt").join(format!("{short}-{triple}"));
    let library = into.join("lib").join("libghostty-vt.a");

    if library.exists() {
        println!("already fetched: {}", into.display());
    } else {
        let archive = format!("ghostty-vt-{short}-{triple}.tar.gz");
        let url = format!("{RELEASES}/ghostty-{short}/{archive}");
        // **Beside where it will be placed, not in the system's temporary directory**: the unpacked tree
        // is put in place by a rename, which cannot cross filesystems, and a build directory on a volume
        // of its own is the ordinary case in a container
        let beside = into.parent().unwrap_or(root);
        std::fs::create_dir_all(beside).map_err(|error| format!("{}: {error}", beside.display()))?;
        let staged = tempfile::tempdir_in(beside).map_err(|error| format!("no temporary directory: {error}"))?;
        let downloaded = staged.path().join(&archive);

        println!("fetching {url}");
        download(&url, &downloaded)?;
        verify(&downloaded, &checksum)?;

        // Unpacked into the temporary directory first and renamed, so an interrupted fetch cannot leave a
        // half-extracted tree that the `library.exists()` check above would then accept
        let unpacked = staged.path().join("unpacked");
        std::fs::create_dir_all(&unpacked).map_err(|error| format!("{}: {error}", unpacked.display()))?;
        untar(&downloaded, &unpacked)?;
        if !unpacked.join("lib").join("libghostty-vt.a").exists() {
            return Err(format!("{archive} holds no lib/libghostty-vt.a; the archive layout has changed"));
        }
        std::fs::remove_dir_all(&into).ok();
        std::fs::rename(&unpacked, &into).map_err(|error| format!("could not place {}: {error}", into.display()))?;
    }

    println!();
    println!("libghostty-vt {version} for {triple}, from ghostty {short}");
    println!("export GHOSTTY_VT_RS_SYS_BUILD=prebuilt");
    println!("export GHOSTTY_VT_RS_SYS_PREBUILT_DIR={}", into.display());
    Ok(())
}

/// The triple this machine builds for, asked of the compiler rather than assembled from `cfg!`.
fn host_triple() -> Result<String, String> {
    let output = Command::new("rustc").arg("-vV").output().map_err(|error| format!("could not run rustc: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| "rustc -vV printed no host triple".to_owned())
}

/// The version the lockfile pins, which is the version the build will use.
fn locked_version(root: &Path) -> Result<String, String> {
    let path = root.join("Cargo.lock");
    let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim() != format!("name = \"{CRATE}\"") {
            continue;
        }
        for line in lines.by_ref().take(3) {
            if let Some(version) = line.trim().strip_prefix("version = \"").and_then(|rest| rest.strip_suffix('"')) {
                return Ok(version.to_owned());
            }
        }
        return Err(format!("{CRATE} is in Cargo.lock with no version beside it"));
    }
    Err(format!(
        "{CRATE} is not in Cargo.lock: run `cargo metadata --features ghostty` once so the feature's \
         dependency is resolved and locked"
    ))
}

/// The unpacked crate in the registry, which is where its `prebuilt.rs` is.
///
/// Found by path rather than through `cargo metadata`, which would mean parsing JSON for one string in a
/// workspace whose tasks have no serde: `mutants.rs` reads a TOML block the same way, by hand, for the
/// same reason.
fn crate_source(version: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))
        .ok_or_else(|| "neither CARGO_HOME nor a home directory".to_owned())?;
    let registry = home.join("registry").join("src");
    let wanted = format!("{CRATE}-{version}");
    let indexes = std::fs::read_dir(&registry).map_err(|error| format!("{}: {error}", registry.display()))?;
    for index in indexes.flatten() {
        let candidate = index.path().join(&wanted);
        if candidate.join("prebuilt.rs").is_file() {
            return Ok(candidate);
        }
    }
    Err(format!("{wanted} is not unpacked under {}: run `cargo fetch --features ghostty` first", registry.display()))
}

/// The Ghostty commit the archives were built from, and this target's archive checksum.
///
/// Read out of the dependency's `prebuilt.rs`, whose own header says a commit other than its `build.rs`'s
/// means there is no prebuilt library for any target — so the pair is read together or not at all.
fn pinned(source: &Path, triple: &str) -> Result<(String, String), String> {
    let path = source.join("prebuilt.rs");
    let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let commit = text
        .lines()
        .find_map(|line| {
            line.trim().strip_prefix("const PREBUILT_COMMIT: &str = \"")?.strip_suffix("\";").map(str::to_owned)
        })
        .ok_or_else(|| format!("{}: no PREBUILT_COMMIT", path.display()))?;
    if commit.len() < 12 {
        return Err(format!("{}: PREBUILT_COMMIT is not a commit: {commit}", path.display()));
    }
    let checksum = text
        .lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix(&format!("(\"{triple}\", \""))?;
            rest.split('"').next().map(str::to_owned)
        })
        .ok_or_else(|| {
            format!(
                "no prebuilt libghostty-vt for {triple} at ghostty {}. The published targets are Apple, \
                 Linux (gnu and musl) and wasm32; Windows has no prebuilt from anyone and has to be built \
                 from the signed source and dependency tarballs (goal-one-vt-core.md, D4)",
                &commit[..12]
            )
        })?;
    Ok((commit, checksum))
}

/// `curl`, because the alternative is a TLS stack in a task that fetches one file.
///
/// Present on macOS, on Linux and on Windows 10 and later, which is every platform this broker builds for.
fn download(url: &str, into: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        .args(["--fail", "--location", "--silent", "--show-error", "--output"])
        .arg(into)
        .arg(url)
        .status()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if !status.success() {
        return Err(format!("could not download {url}"));
    }
    Ok(())
}

/// Checked against the dependency's own pinned digest, which is the only reason this is safe to fetch.
fn verify(archive: &Path, expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        return Err(format!(
            "{} does not match the checksum {CRATE} pins:\n  expected {expected}\n  got      {actual}",
            archive.display()
        ));
    }
    println!("checksum ok");
    Ok(())
}

/// `tar`, for the same reason as `curl`: it is on every platform this builds for.
fn untar(archive: &Path, into: &Path) -> Result<(), String> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .status()
        .map_err(|error| format!("could not run tar: {error}"))?;
    if !status.success() {
        return Err(format!("could not unpack {}", archive.display()));
    }
    Ok(())
}

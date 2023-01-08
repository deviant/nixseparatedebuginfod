use crate::db::{Cache, Entry, Timestamp};
use anyhow::Context;
use object::read::Object;
use once_cell::unsync::Lazy;
use sqlx::{sqlite::SqliteConnectOptions, ConnectOptions, Connection, Row};
use std::{
    ffi::OsString,
    os::unix::prelude::OsStringExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc::Sender;

pub async fn realise(path: &Path) -> anyhow::Result<()> {
    use tokio::fs::metadata;
    use tokio::process::Command;
    if metadata(path).await.is_ok() {
        return Ok(());
    };
    let mut command = Command::new("nix-store");
    command.arg("--realise").arg(path);
    log::info!("Running {:?}", &command);
    let _ = command.status().await;
    if metadata(path).await.is_ok() {
        return Ok(());
    };
    anyhow::bail!("nix-store --realise {} failed", path.display());
}

/// Walks a store path and attempts to register everything that has a buildid in it.
fn register_store_path(storepath: &Path, sendto: Sender<Entry>) {
    log::info!("examining {}", storepath.display());
    if !storepath.is_dir() {
        return;
    }
    let deriver_source = Lazy::new(|| match get_deriver(storepath) {
        Err(e) => {
            log::info!("no deriver for {}: {:#}", storepath.display(), e);
            (None, None)
        }
        Ok(deriver) => {
            if deriver.is_file() {
                (Some(deriver), None)
            } else {
                (None, None)
            }
        }
    });
    if storepath.ends_with("-debug") {
        let mut root = storepath.to_owned();
        root.push("lib");
        root.push("debug");
        if !root.is_dir() {
            return;
        };
        let readroot = match std::fs::read_dir(&root) {
            Err(e) => {
                log::info!("could not list {}: {:#}", root.display(), e);
                return;
            }
            Ok(r) => r,
        };
        for mid in readroot {
            let mid = match mid {
                Err(e) => {
                    log::info!("could not list {}: {:#}", root.display(), e);
                    continue;
                }
                Ok(mid) => mid,
            };
            if mid.file_type().map(|x| x.is_dir()).unwrap_or(false) {
                continue;
            };
            let mid_path = mid.path();
            let mid_name_os = mid.file_name();
            let mid_name = match mid_name_os.to_str() {
                None => continue,
                Some(x) => x,
            };
            let read_mid = match std::fs::read_dir(&mid_path) {
                Err(e) => {
                    log::info!("could not list {}: {:#}", mid_path.display(), e);
                    continue;
                }
                Ok(r) => r,
            };
            for end in read_mid {
                let end = match end {
                    Err(e) => {
                        log::info!("could not list {}: {:#}", mid_path.display(), e);
                        continue;
                    }
                    Ok(end) => end,
                };
                if !end.file_type().map(|x| x.is_file()).unwrap_or(false) {
                    continue;
                };
                let end_name_os = end.file_name();
                let end_name = match end_name_os.to_str() {
                    None => continue,
                    Some(x) => x,
                };
                if !end_name.ends_with(".debug") {
                    continue;
                };
                let buildid = format!(
                    "{}{}",
                    &mid_name,
                    &end_name[..(end_name.len() - ".debug".len())]
                );
                let (_, source) = &*deriver_source;
                let entry = Entry {
                    debuginfo: end.path().to_str().map(|s| s.to_owned()),
                    executable: None,
                    source: source.clone(),
                    buildid,
                };
                if let Err(e) = sendto.blocking_send(entry) {
                    log::warn!("failed to send entry: {:#}", e);
                };
            }
        }
    } else {
        let debug_output = Lazy::new(|| {
            let (deriver, _) = &*deriver_source;
            match deriver {
                None => None,
                Some(deriver) => match get_debug_output(deriver.as_path()) {
                    Ok(None) => None,
                    Err(e) => {
                        log::info!(
                            "could not determine if the deriver {} of {} has a debug output: {}",
                            storepath.display(),
                            deriver.display(),
                            e
                        );
                        None
                    }
                    Ok(Some(d)) => Some(d),
                },
            }
        });
        for file in walkdir::WalkDir::new(storepath) {
            let file = match file {
                Err(_) => continue,
                Ok(file) => file,
            };
            if !file.file_type().is_file() {
                continue;
            };
            let path = file.path();
            let buildid = match get_buildid(path) {
                Err(e) => {
                    log::info!("cannot get buildid of {}: {:#}", path.display(), e);
                    continue;
                }
                Ok(Some(buildid)) => buildid,
                Ok(None) => continue,
            };
            let debuginfo = match &*debug_output {
                None => None,
                Some(storepath) => {
                    let theoretical = debuginfo_path_for(&buildid, storepath.as_path());
                    if storepath.is_dir() {
                        // the store path is available, check the prediction
                        if !theoretical.is_file() {
                            log::warn!(
                                "{} has buildid {}, and {} exists but not {}",
                                path.display(),
                                buildid,
                                storepath.display(),
                                theoretical.display()
                            );
                            None
                        } else {
                            Some(theoretical)
                        }
                    } else {
                        Some(theoretical)
                    }
                }
            };
            let (_, source) = &*deriver_source;
            let entry = Entry {
                buildid,
                source: source.clone(),
                executable: path.to_str().map(|s| s.to_owned()),
                debuginfo: debuginfo.and_then(|path| path.to_str().map(|s| s.to_owned())),
            };
            if let Err(e) = sendto.blocking_send(entry) {
                log::warn!("failed to send entry: {:#}", e);
            };
        }
    }
}

/// Return the path where separate debuginfo is to be found in a debug output for a buildid
fn debuginfo_path_for(buildid: &str, debug_output: &Path) -> PathBuf {
    let mut res = debug_output.to_path_buf();
    res.push("lib");
    res.push("debug");
    res.push(".build-id");
    res.push(&buildid[..2]);
    res.push(format!("{}.debug", &buildid[2..]));
    res
}

/// Obtains the derivation of a store path.
///
/// The store path must exist.
fn get_deriver(storepath: &Path) -> anyhow::Result<PathBuf> {
    let mut cmd = std::process::Command::new("nix-store");
    cmd.arg("--query").arg("--deriver").arg(storepath);
    log::info!("Running {:?}", &cmd);
    let out = cmd.output().with_context(|| format!("running {:?}", cmd))?;
    if !out.status.success() {
        anyhow::bail!("{:?} failed: {}", cmd, String::from_utf8_lossy(&out.stderr));
    }
    let n = out.stdout.len();
    if n <= 1 || out.stdout[n - 1] != b'\n' {
        anyhow::bail!(
            "{:?} returned weird output: {}",
            cmd,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let path = PathBuf::from(OsString::from_vec(out.stdout[..n - 1].to_owned()));
    if !path.is_absolute() {
        // nix returns `unknown-deriver` when it does not know
        anyhow::bail!("no deriver");
    };
    Ok(path)
}

/// Obtains the debug output corresponding to this derivation
///
/// The derivation must exist.
fn get_debug_output(drvpath: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut cmd = std::process::Command::new("nix-store");
    cmd.arg("--query").arg("--outputs").arg(drvpath);
    log::info!("Running {:?}", &cmd);
    let out = cmd.output().with_context(|| format!("running {:?}", cmd))?;
    if !out.status.success() {
        anyhow::bail!("{:?} failed: {}", cmd, String::from_utf8_lossy(&out.stderr));
    }
    for output in out.stdout.split(|&elt| elt == b'\n') {
        if output.ends_with(b"-debug") {
            return Ok(Some(PathBuf::from(OsString::from_vec(output.to_owned()))));
        }
    }
    return Ok(None);
}

/// Return the build id of this file.
///
/// If the file is not an executable returns Ok(None).
/// Errors are only for errors returned from the fs.
fn get_buildid(path: &Path) -> anyhow::Result<Option<String>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to get its buildid", path.display()))?;
    let reader = object::read::ReadCache::new(file);
    let object = match object::read::File::parse(&reader) {
        Err(_) => {
            // object::read::Error is opaque, so no way to distinguish "this is not elf" and a real
            // error
            return Ok(None);
        }
        Ok(o) => o,
    };
    match object
        .build_id()
        .with_context(|| format!("parsing {} for buildid", path.display()))?
    {
        None => Ok(None),
        Some(data) => {
            let buildid = base16::encode_lower(&data);
            Ok(Some(buildid))
        }
    }
}

pub fn spawn_store_watcher(cache: &'static Cache) {
    let threadpool = threadpool::ThreadPool::new(8);
    let (path_sender, mut path_receiver) = tokio::sync::mpsc::channel::<PathBuf>(200);
    let (path_done_sender, mut path_done_receiver) = tokio::sync::mpsc::channel(200);
    let (entry_sender, mut entry_receiver) = tokio::sync::mpsc::channel(200);
    tokio::spawn(async move {
        while let Some(entry) = entry_receiver.recv().await {
            log::info!("found {:?}", &entry);
            if let Err(e) = cache.register(&entry).await {
                log::warn!("failed to register {:?}: {:#}", &entry, e);
            }
        }
    });
    std::thread::spawn(move || {
        while let Some(path) = path_receiver.blocking_recv() {
            let entry_sender_moved = entry_sender.clone();
            let path_done_sender_moved = path_done_sender.clone();
            threadpool.execute(move || {
                register_store_path(path.as_path(), entry_sender_moved);
                if let Err(e) = path_done_sender_moved.blocking_send(()) {
                    log::warn!("failed to send {:?}: {:#}", (), e);
                };
            });
        }
    });
    tokio::spawn(async move {
        let mut from_timestamp = cache
            .get_registration_timestamp()
            .await
            .expect("problem with cache db");
        loop {
            match get_new_store_path_batch(from_timestamp).await {
                Err(e) => {
                    log::warn!("could not read nix db: {}", dbg!(e));
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Ok((paths, _)) if paths.is_empty() => {
                    log::info!("done reading store");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                Ok((paths, time)) => {
                    let n = paths.len();
                    for path in paths {
                        if let Err(e) = path_sender.send(path).await {
                            log::warn!("failed to send path: {:#}", e);
                        };
                    }
                    for _ in 0..n {
                        path_done_receiver.recv().await;
                    }
                    if let Err(e) = cache.set_registration_timestamp(time).await {
                        log::warn!("could not store timestamp to cache db: {}", dbg!(e));
                    }

                    from_timestamp = time;
                }
            }
        }
    });
}

async fn get_new_store_path_batch(
    from_timestamp: Timestamp,
) -> anyhow::Result<(Vec<PathBuf>, Timestamp)> {
    // note: this is a hack. One cannot open a sqlite db read only with WAL if the underlying
    // file is not writable. So we promise sqlite that the db will not be modified with
    // immutable=1, but it's false.
    let mut db = SqliteConnectOptions::new()
        .filename("/nix/var/nix/db/db.sqlite")
        .immutable(true)
        .read_only(true)
        .connect()
        .await
        .context("opening nix db")?;
    let rows =
        sqlx::query("select path, registrationTime from ValidPaths where registrationTime >= $1 and registrationTime <= (with candidates(registrationTime) as (select registrationTime from ValidPaths where registrationTime >= $1 order by registrationTime asc limit 100) select max(registrationTime) from candidates)")
            .bind(from_timestamp)
            .fetch_all(&mut db)
            .await
            .context("reading nix db")?;
    let mut paths = Vec::new();
    let mut max_time = 0;
    for row in rows {
        let path: &str = row.try_get("path").context("parsing path in nix db")?;
        if !path.starts_with("/nix/store") || path.chars().filter(|&x| x == '/').count() != 3 {
            anyhow::bail!(
                "read corrupted stuff from nix db: {}, concurrent write?",
                path
            );
        }
        paths.push(PathBuf::from(path));
        let time: Timestamp = row
            .try_get("registrationTime")
            .context("parsing timestamp in nix db")?;
        max_time = time.max(max_time);
    }
    // As we lie about the database being immutable let's not keep the connection open
    if let Err(e) = db.close().await {
        log::warn!("failed to close nix db {:#}", e)
    };
    if (max_time == 0) ^ paths.is_empty() {
        anyhow::bail!("read paths with 0 registration time...");
    }
    Ok((paths, max_time + 1))
}

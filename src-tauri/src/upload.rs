use anyhow::Context;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use iroh_bytes::{
    format::collection::Collection,
    provider::{handle_connection, Event, EventSender},
    store::{ExportMode, ImportMode},
    BlobFormat, Hash, TempTag,
};
use iroh_net::{key::SecretKey, ticket::BlobTicket, MagicEndpoint};
use rand::Rng;
use std::{
    fmt::{Display, Formatter},
    path::{Component, Path, PathBuf},
    str::FromStr,
};
use tokio::task::JoinHandle;
use tokio_util::task::LocalPoolHandle;
use walkdir::WalkDir;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    #[default]
    Hex,
    Cid,
}

impl FromStr for Format {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "hex" => Ok(Format::Hex),
            "cid" => Ok(Format::Cid),
            _ => Err(anyhow::anyhow!("invalid format")),
        }
    }
}

impl Display for Format {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Format::Hex => write!(f, "hex"),
            Format::Cid => write!(f, "cid"),
        }
    }
}

fn print_hash(hash: &Hash, format: Format) -> String {
    match format {
        Format::Hex => hash.to_hex().to_string(),
        Format::Cid => hash.to_string(),
    }
}

/// Get the secret key or generate a new one.
///
/// Print the secret key to stderr if it was generated, so the user can save it.
fn get_or_create_secret() -> anyhow::Result<SecretKey> {
    match std::env::var("IROH_SECRET") {
        Ok(secret) => SecretKey::from_str(&secret).context("invalid secret"),
        Err(_) => {
            let key = SecretKey::generate();
            Ok(key)
        }
    }
}

fn validate_path_component(component: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !component.contains('/'),
        "path components must not contain the only correct path separator, /"
    );
    Ok(())
}

/// This function converts an already canonicalized path to a string.
///
/// If `must_be_relative` is true, the function will fail if any component of the path is
/// `Component::RootDir`
///
/// This function will also fail if the path is non canonical, i.e. contains
/// `..` or `.`, or if the path components contain any windows or unix path
/// separators.
pub fn canonicalized_path_to_string(
    path: impl AsRef<Path>,
    must_be_relative: bool,
) -> anyhow::Result<String> {
    let mut path_str = String::new();
    let parts = path
        .as_ref()
        .components()
        .filter_map(|c| match c {
            Component::Normal(x) => {
                let c = match x.to_str() {
                    Some(c) => c,
                    None => return Some(Err(anyhow::anyhow!("invalid character in path"))),
                };

                if !c.contains('/') && !c.contains('\\') {
                    Some(Ok(c))
                } else {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                }
            }
            Component::RootDir => {
                if must_be_relative {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                } else {
                    path_str.push('/');
                    None
                }
            }
            _ => Some(Err(anyhow::anyhow!("invalid path component {:?}", c))),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let parts = parts.join("/");
    path_str.push_str(&parts);
    Ok(path_str)
}

/// Import from a file or directory into the database.
///
/// The returned tag always refers to a collection. If the input is a file, this
/// is a collection with a single blob, named like the file.
///
/// If the input is a directory, the collection contains all the files in the
/// directory.
async fn import(
    path: PathBuf,
    db: impl iroh_bytes::store::Store,
) -> anyhow::Result<(TempTag, u64, Collection)> {
    let path = path.canonicalize()?;
    anyhow::ensure!(path.exists(), "path {} does not exist", path.display());
    let root = path.parent().context("context get parent")?;
    // walkdir also works for files, so we don't need to special case them
    let files = WalkDir::new(path.clone()).into_iter();
    // flatten the directory structure into a list of (name, path) pairs.
    // ignore symlinks.
    let data_sources: Vec<(String, PathBuf)> = files
        .map(|entry| {
            let entry = entry?;
            if !entry.file_type().is_file() {
                // Skip symlinks. Directories are handled by WalkDir.
                return Ok(None);
            }
            let path = entry.into_path();
            let relative = path.strip_prefix(root)?;
            let name = canonicalized_path_to_string(relative, true)?;
            anyhow::Ok(Some((name, path)))
        })
        .filter_map(Result::transpose)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let (send, recv) = flume::bounded(32);
    let progress = iroh_bytes::util::progress::FlumeProgressSender::new(send);
    // import all the files, using num_cpus workers, return names and temp tags
    let names_and_tags = futures::stream::iter(data_sources)
        .map(|(name, path)| {
            let db = db.clone();
            let progress = progress.clone();
            async move {
                let (temp_tag, file_size) = db
                    .import_file(path, ImportMode::TryReference, BlobFormat::Raw, progress)
                    .await?;
                anyhow::Ok((name, temp_tag, file_size))
            }
        })
        .buffer_unordered(num_cpus::get())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;
    drop(progress);
    // total size of all files
    let size = names_and_tags.iter().map(|(_, _, size)| *size).sum::<u64>();
    // collect the (name, hash) tuples into a collection
    // we must also keep the tags around so the data does not get gced.
    let (collection, tags) = names_and_tags
        .into_iter()
        .map(|(name, tag, _)| ((name, *tag.hash()), tag))
        .unzip::<_, _, Collection, Vec<_>>();
    let temp_tag = collection.clone().store(&db).await?;
    // now that the collection is stored, we can drop the tags
    // data is protected by the collection
    drop(tags);
    Ok((temp_tag, size, collection))
}

fn get_export_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let parts = name.split('/');
    let mut path = root.to_path_buf();
    for part in parts {
        validate_path_component(part)?;
        path.push(part);
    }
    Ok(path)
}

async fn export(db: impl iroh_bytes::store::Store, collection: Collection) -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    for (name, hash) in collection.iter() {
        let target = get_export_path(&root, name)?;
        db.export(*hash, target, ExportMode::TryReference, |_position| Ok(()))
            .await?;
    }
    Ok(())
}

pub async fn provide(path: PathBuf) -> anyhow::Result<(BlobTicket, JoinHandle<()>)> {
    let secret_key = get_or_create_secret()?;
    // create a magicsocket endpoint
    let endpoint_fut = MagicEndpoint::builder()
        .alpns(vec![iroh_bytes::protocol::ALPN.to_vec()])
        .secret_key(secret_key)
        .bind(0);
    // use a flat store - todo: use a partial in mem store instead
    let suffix = rand::thread_rng().gen::<[u8; 16]>();
    let iroh_data_dir = path
        .parent()
        .unwrap()
        .join(format!(".sendme-provide-{}", hex::encode(suffix)));
    if iroh_data_dir.exists() {
        println!("can not share twice from the same directory");
        std::process::exit(1);
    }
    std::fs::create_dir_all(&iroh_data_dir)?;
    let db = iroh_bytes::store::flat::Store::load(&iroh_data_dir).await?;
    let (temp_tag, size, collection) = import(path.clone(), db.clone()).await?;
    let hash = *temp_tag.hash();
    // wait for the endpoint to be ready
    let endpoint = endpoint_fut.await?;
    // wait for the endpoint to figure out its address before making a ticket
    while endpoint.my_derp().is_none() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // make a ticket
    let addr = endpoint.my_addr().await?;
    let ticket = BlobTicket::new(addr, hash, BlobFormat::HashSeq)?;
    let entry_type = if path.is_file() { "file" } else { "directory" };
    println!(
        "imported {} {}, {}, hash {}",
        entry_type,
        path.display(),
        size,
        print_hash(&hash, Format::Hex)
    );
    for (name, hash) in collection.iter() {
        println!("    {} {name}", print_hash(hash, Format::Hex));
    }

    println!("to get this data, use");

    let handle = tokio::task::spawn(async move {
        let rt = LocalPoolHandle::new(1);
        loop {
            let Some(connecting) = endpoint.accept().await else {
                break;
            };
            let db = db.clone();
            let rt = rt.clone();
            tokio::spawn(handle_connection(connecting, db, Events {}, rt));
        }
        drop(temp_tag);
        std::fs::remove_dir_all(iroh_data_dir).ok();
    });
    Ok((ticket, handle))
}

#[derive(Debug, Clone)]
struct Events {}

impl EventSender for Events {
    fn send(&self, event: Event) -> BoxFuture<()> {
        println!("ev: {:?}", event);
        async move { () }.boxed()
    }
}

// async fn get(args: GetArgs) -> anyhow::Result<()> {
//     let ticket = args.ticket;
//     let addr = ticket.node_addr().clone();
//     let secret_key = get_or_create_secret(args.common.verbose > 0)?;
//     let endpoint = MagicEndpoint::builder()
//         .alpns(vec![])
//         .secret_key(secret_key)
//         .bind(args.common.magic_port)
//         .await?;
//     let dir_name = format!(".sendme-get-{}", ticket.hash().to_hex());
//     let iroh_data_dir = std::env::current_dir()?.join(dir_name);
//     let db = iroh_bytes::store::flat::Store::load(&iroh_data_dir).await?;
//     let mp = MultiProgress::new();
//     let connect_progress = mp.add(ProgressBar::hidden());
//     connect_progress.set_draw_target(ProgressDrawTarget::stderr());
//     connect_progress.set_style(ProgressStyle::default_spinner());
//     connect_progress.set_message(format!("connecting to {}", addr.node_id));
//     let connection = endpoint.connect(addr, iroh_bytes::protocol::ALPN).await?;
//     let hash_and_format = HashAndFormat {
//         hash: ticket.hash(),
//         format: ticket.format(),
//     };
//     connect_progress.finish_and_clear();
//     let (send, recv) = flume::bounded(32);
//     let progress = iroh_bytes::util::progress::FlumeProgressSender::new(send);
//     let (_hash_seq, sizes) =
//         get_hash_seq_and_sizes(&connection, &hash_and_format.hash, 1024 * 1024 * 32)
//             .await
//             .map_err(show_get_error)?;
//     let total_size = sizes.iter().sum::<u64>();
//     let total_files = sizes.len().saturating_sub(1);
//     let payload_size = sizes.iter().skip(1).sum::<u64>();
//     eprintln!(
//         "getting collection {} {} files, {}",
//         print_hash(&ticket.hash(), args.common.format),
//         total_files,
//         HumanBytes(payload_size)
//     );
//     // print the details of the collection only in verbose mode
//     if args.common.verbose > 0 {
//         eprintln!(
//             "getting {} blobs in total, {}",
//             sizes.len(),
//             HumanBytes(total_size)
//         );
//     }
//     let _task = tokio::spawn(show_download_progress(recv.into_stream(), total_size));
//     let stats = iroh_bytes::get::db::get_to_db(&db, connection, &hash_and_format, progress)
//         .await
//         .map_err(show_get_error)?;
//     let collection = Collection::load(&db, &hash_and_format.hash).await?;
//     if args.common.verbose > 0 {
//         for (name, hash) in collection.iter() {
//             println!("    {} {name}", print_hash(hash, args.common.format));
//         }
//     }
//     if let Some((name, _)) = collection.iter().next() {
//         if let Some(first) = name.split('/').next() {
//             println!("downloading to: {};", first);
//         }
//     }
//     export(db, collection).await?;
//     std::fs::remove_dir_all(iroh_data_dir)?;
//     if args.common.verbose > 0 {
//         println!(
//             "downloaded {} files, {}. took {} ({}/s)",
//             total_files,
//             HumanBytes(payload_size),
//             HumanDuration(stats.elapsed),
//             HumanBytes((stats.bytes_read as f64 / stats.elapsed.as_secs_f64()) as u64),
//         );
//     }
//     Ok(())
// }

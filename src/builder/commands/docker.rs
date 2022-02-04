use std::collections::{BTreeMap, HashSet};
use std::fs::{remove_dir_all, remove_file};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(feature="containers")]
use dkregistry::v2::Client as RegistryClient;

#[cfg(feature="containers")]
use futures::stream::StreamExt;

#[cfg(feature="containers")]
use tar::{Entry, EntryType};

#[cfg(feature="containers")]
use tokio::{
    io::AsyncWriteExt,
    sync::Semaphore,
};

#[cfg(feature="containers")]
use quire::{
    validate as V,
    ast::{Ast, ScalarKind, Tag},
};

#[cfg(feature="containers")]
use crate::{
    builder::commands::tarcmd::TarCmd,
    capsule::packages as capsule,
    container::util::clean_dir,
    file_util::{Dir, Lock},
};
use crate::build_step::{BuildStep, Config, Digest, Guard, StepError, VersionError};

const DEFAULT_REGISTRY_HOST: &str = "registry-1.docker.io";
const DEFAULT_IMAGE_NAMESPACE: &str = "library";
const DEFAULT_IMAGE_TAG: &str = "latest";

const DOCKER_LAYERS_CACHE_PATH: &str = "/vagga/cache/docker-layers";
const DOCKER_LAYERS_DOWNLOAD_CONCURRENCY: usize = 2;

#[derive(Serialize, Deserialize, Debug)]
pub struct DockerImage {
    pub registry: String,
    pub image: String,
    pub tag: String,
    pub insecure: Option<bool>,
    pub path: PathBuf,
}

impl DockerImage {
    pub fn config() -> V::Structure<'static> {
        V::Structure::new()
        .member("registry", V::Scalar::new().default(DEFAULT_REGISTRY_HOST))
        .member("image", V::Scalar::new())
        .member("tag", V::Scalar::new().default(DEFAULT_IMAGE_TAG))
        .member("insecure", V::Scalar::new().optional())
        .member("path", V::Directory::new().absolute(true).default("/"))
        .parser(parse_image)
    }
}

fn parse_image(ast: Ast) -> BTreeMap<String, Ast> {
    match ast {
        Ast::Scalar(pos, _, _, value) => {
            let mut map = BTreeMap::new();

            let (image, registry) = if let Some((registry, image)) = value.split_once('/') {
                if registry == "localhost" || registry.contains(|c| c == '.' || c == ':') {
                    map.insert(
                        "registry".to_string(),
                        Ast::Scalar(pos.clone(), Tag::NonSpecific, ScalarKind::Plain, registry.to_string())
                    );
                    (image, Some(registry))
                } else {
                    (value.as_str(), None)
                }
            } else {
                (value.as_str(), None)
            };

            let image = if let Some((image, tag)) = image.rsplit_once(':') {
                map.insert(
                    "tag".to_string(),
                    Ast::Scalar(pos.clone(), Tag::NonSpecific, ScalarKind::Plain, tag.to_string())
                );
                image
            } else {
                image
            };

            let image = if !image.contains('/') && registry.is_none() {
                format!("{}/{}", DEFAULT_IMAGE_NAMESPACE, image)
            } else {
                image.to_string()
            };

            map.insert(
                "image".to_string(),
                Ast::Scalar(pos.clone(), Tag::NonSpecific, ScalarKind::Plain, image)
            );

            map
        },
        _ => unreachable!(),
    }
}

impl BuildStep for DockerImage {
    fn name(&self) -> &'static str {
        "DockerImage"
    }

    #[cfg(feature="containers")]
    fn hash(&self, _cfg: &Config, hash: &mut Digest) -> Result<(), VersionError> {
        hash.field("registry", &self.registry);
        hash.field("image", &self.image);
        hash.field("tag", &self.tag);
        hash.opt_field("insecure", &self.insecure);
        hash.field("path", &self.path);
        Ok(())
    }

    #[cfg(feature="containers")]
    fn build(&self, guard: &mut Guard, _build: bool) -> Result<(), StepError> {
        let insecure = self.insecure.unwrap_or_else(||
            is_insecure_registry(&self.registry, &guard.ctx.settings.docker_insecure_registries)
        );
        if !insecure {
            capsule::ensure(&mut guard.ctx.capsule, &[capsule::Https])?;
        }
        Dir::new(DOCKER_LAYERS_CACHE_PATH)
            .recursive(true)
            .create()
            .map_err(|e|
                format!("Cannot create docker layers cache directory: {}", e)
            )?;
        let dst_path = Path::new("/vagga/root").join(&self.path.strip_prefix("/").unwrap());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("Error creating tokio runtime: {}", e))?
            .block_on(download_and_unpack_image(
                &self.registry, insecure, &self.image, &self.tag, &dst_path
            ))?;
        Ok(())
    }

    fn is_dependent_on(&self) -> Option<&str> {
        None
    }
}

fn is_insecure_registry(
    registry: &str, insecure_registries: &HashSet<String>
) -> bool {
    let registry_host = match registry.split_once(':') {
        Some((host, _port)) => host,
        None => registry,
    };
    insecure_registries.contains(registry_host)
}

/// See:
/// - https://github.com/moby/moby/blob/v20.10.11/pkg/archive/whiteouts.go
/// - https://github.com/moby/moby/blob/v20.10.11/pkg/archive/diff.go#L131
#[cfg(feature="containers")]
fn whiteout_entry_handler(entry: &Entry<Box<dyn Read>>, dst_path: &Path) -> Result<bool, String> {
    let file_name = dst_path.file_name()
        .and_then(|fname| fname.to_str());
    let file_name = if let Some(file_name) = file_name {
        file_name
    } else {
        return Ok(false);
    };

    if entry.header().entry_type() != EntryType::Regular {
        return Ok(false);
    }

    if let Some(whiteout) = file_name.strip_prefix(".wh.") {
        let dir = dst_path.parent().unwrap();
        if whiteout == ".wh..opq" {
            // TODO: Track and keep files that were unpacked from the current archive
            clean_dir(dir, false)?
        } else {
            let mut whiteout_path = dir.to_path_buf();
            whiteout_path.push(whiteout);
            if whiteout_path.is_dir() {
                remove_dir_all(&whiteout_path)
                    .map_err(|e| format!("Cannot remove directory: {}", e))?;
            } else {
                remove_file(whiteout_path)
                    .map_err(|e| format!("Cannot delete file: {}", e))?;
            }
        }
        return Ok(true);
    }

    Ok(false)
}

#[cfg(feature="containers")]
async fn download_and_unpack_image(
    registry: &str, insecure: bool, image: &str, tag: &str, dst_path: &Path
) -> Result<(), StepError> {
    let auth_scope = format!("repository:{}:pull", image);
    let client = build_client(registry, insecure, &[&auth_scope]).await?;

    println!("Downloading docker image: {}/{}:{}", registry, image, tag);
    let manifest = client.get_manifest(&image, &tag).await?;

    let layers_digests = manifest.layers_digests(None)?;

    let layers_download_semaphore = Arc::new(
        Semaphore::new(DOCKER_LAYERS_DOWNLOAD_CONCURRENCY)
    );

    use tokio::sync::oneshot;

    let mut layers_futures = vec!();
    let mut unpack_channels = vec!();
    for digest in &layers_digests {
        let image = image.to_string();
        let digest = digest.clone();
        let client = client.clone();
        let sem = layers_download_semaphore.clone();
        let (tx, rx) = oneshot::channel();
        unpack_channels.push(rx);
        let download_future = tokio::spawn(async move {
            if let Ok(_guard) = sem.acquire().await {
                println!("Downloading docker layer: {}", &digest);
                match download_blob(&client, &image, &digest).await {
                    Ok(layer_path) => {
                        if let Err(_) = tx.send((digest.clone(), layer_path)) {
                            return Err(format!("Error sending downloaded layer"));
                        }
                        Ok(())
                    }
                    Err(e) => Err(e)
                }
            } else {
                panic!("Semaphore was closed unexpectedly")
            }
        });
        layers_futures.push(download_future);
    }

    let dst_path = dst_path.to_path_buf();
    let unpack_future = tokio::spawn(async move {
        for ch in unpack_channels {
            match ch.await {
                Ok((digest, layer_path)) => {
                    let dst_path = dst_path.clone();
                    if let Err(e) = unpack_layer(digest, layer_path, dst_path).await {
                        return Err(e);
                    }
                }
                Err(e) => return Err(
                    format!("Error waiting downloaded layer: {}", e)
                ),
            }
        }
        Ok(())
    });

    let mut layers_paths = vec!();
    let mut layers_errors = vec!();
    for layer_res in futures::future::join_all(layers_futures).await.into_iter() {
        match layer_res {
            Ok(Ok(layer)) => layers_paths.push(layer),
            Ok(Err(client_err)) => layers_errors.push(client_err),
            Err(join_err) => layers_errors.push(format!("{}", join_err)),
        }
    }

    unpack_future.await
        .map_err(|e| format!("Error waiting unpack future: {}", e))??;

    if !layers_errors.is_empty() {
        Err(layers_errors.into())
    } else {
        Ok(())
    }
}

async fn unpack_layer(
    digest: String, layer_path: PathBuf, dst_path: PathBuf
) -> Result<(), String> {
    let unpack_future_res = tokio::task::spawn_blocking(move || {
        println!("Unpacking docker layer: {}", digest);
        TarCmd::new(&layer_path, &dst_path)
            .preserve_owner(true)
            .entry_handler(whiteout_entry_handler)
            .unpack()
    }).await;
    unpack_future_res
        .map_err(|e| format!("Error waiting a unpack layer future: {}", e))?
        .map_err(|e| format!("Error unpacking docker layer: {}", e))
}

#[cfg(feature="containers")]
async fn build_client(
    registry: &str, insecure: bool, auth_scopes: &[&str]
) -> Result<Arc<RegistryClient>, StepError> {
    let client_config = RegistryClient::configure()
        .registry(registry)
        .insecure_registry(insecure)
        .username(None)
        .password(None);
    let client = client_config.build()?;

    let client = match client.is_auth().await {
        Ok(true) => client,
        Ok(false) => client.authenticate(auth_scopes).await?,
        Err(e) => return Err(e.into()),
    };
    Ok(Arc::new(client))
}

#[cfg(feature="containers")]
async fn download_blob(
    client: &RegistryClient, image: &str, layer_digest: &str
) -> Result<PathBuf, String> {
    let digest = layer_digest.split_once(':')
        .ok_or(format!("Invalid layer digest: {}", layer_digest))?
        .1;
    let short_digest = &digest[..12];

    let layers_cache = Path::new(DOCKER_LAYERS_CACHE_PATH);
    let blob_file_name = format!("{}.tar.gz", digest);
    let blob_path = layers_cache.join(&blob_file_name);
    match tokio::fs::symlink_metadata(&blob_path).await {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let lock_file_name = format!(".{}.lock", &blob_file_name);
            let lock_msg = format!("Another process downloads blob: {}", &short_digest);
            let lock_fut = tokio::task::spawn_blocking(move || {
                let lockfile = layers_cache.join(lock_file_name);
                Lock::exclusive_wait(lockfile, true, &lock_msg)
            });
            let _lock = lock_fut.await
                .map_err(|e| format!("Error waiting a lock file future: {}", e))?
                .map_err(|e| format!("Error taking exclusive lock: {}", e))?;

            match tokio::fs::symlink_metadata(&blob_path).await {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    let blob_tmp_file_name = format!(".{}.tmp", &blob_file_name);
                    let blob_tmp_path = layers_cache.join(&blob_tmp_file_name);

                    println!("Downloading docker blob: {}", &short_digest);
                    let mut blob_stream = client.get_blob_stream(image, layer_digest).await
                        .map_err(|e| format!("Error getting docker blob response: {}", e))?;
                    let mut blob_file = tokio::fs::File::create(&blob_tmp_path).await
                        .map_err(|e| format!("Cannot create layer file: {}", e))?;
                    while let Some(chunk) = blob_stream.next().await {
                        let chunk = chunk.map_err(|e| format!("Error fetching layer chunk: {}", e))?;
                        blob_file.write_all(&chunk).await
                            .map_err(|e| format!("Cannot write blob file: {}", e))?;
                    }
                    tokio::fs::rename(&blob_tmp_path, &blob_path).await
                        .map_err(|e| format!("Cannot rename docker blob: {}", e))?;
                }
                Err(e) => return Err(format!("{}", e)),
            }

        }
        Err(e) => return Err(format!("{}", e)),
    }
    Ok(blob_path)
}
// Copyright (c) 2023 Microsoft Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

// Allow Docker image config field names.
#![allow(non_snake_case)]

use crate::policy;

use anyhow::{anyhow, Result};
use docker_credential::{CredentialRetrievalError, DockerCredential};
use log::warn;
use log::{debug, info, LevelFilter};
use oci_distribution::client::{linux_amd64_resolver, ClientConfig};
use oci_distribution::{manifest, secrets::RegistryAuth, Client, Reference};
use serde::{Deserialize, Serialize};
use sha2::{digest::typenum::Unsigned, digest::OutputSizeUser, Sha256};
use std::{io, io::Seek, io::Write, path::Path};
use tokio::{fs, io::AsyncWriteExt};

#[derive(Clone, Debug, Default)]
pub struct Container {
    config_layer: DockerConfigLayer,
    image_layers: Vec<ImageLayer>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DockerConfigLayer {
    architecture: String,
    config: DockerImageConfig,
    rootfs: DockerRootfs,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DockerImageConfig {
    User: Option<String>,
    Tty: Option<bool>,
    Env: Vec<String>,
    Cmd: Option<Vec<String>>,
    WorkingDir: Option<String>,
    Entrypoint: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DockerRootfs {
    r#type: String,
    diff_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ImageLayer {
    pub diff_id: String,
    pub verity_hash: String,
}

impl Container {
    pub async fn new(use_cached_files: bool, image: &str) -> Result<Self> {
        info!("============================================");
        info!("Pulling manifest and config for {:?}", image);
        let reference: Reference = image.to_string().parse().unwrap();
        let auth = build_auth(&reference);

        let mut client = Client::new(ClientConfig {
            platform_resolver: Some(Box::new(linux_amd64_resolver)),
            ..Default::default()
        });

        match client.pull_manifest_and_config(&reference, &auth).await {
            Ok((manifest, digest_hash, config_layer_str)) => {
                debug!("digest_hash: {:?}", digest_hash);
                debug!(
                    "manifest: {}",
                    serde_json::to_string_pretty(&manifest).unwrap()
                );

                // Log the contents of the config layer.
                if log::max_level() >= LevelFilter::Debug {
                    let mut deserializer = serde_json::Deserializer::from_str(&config_layer_str);
                    let mut serializer = serde_json::Serializer::pretty(io::stderr());
                    serde_transcode::transcode(&mut deserializer, &mut serializer).unwrap();
                }

                let config_layer: DockerConfigLayer =
                    serde_json::from_str(&config_layer_str).unwrap();
                let image_layers = get_image_layers(
                    use_cached_files,
                    &mut client,
                    &reference,
                    &manifest,
                    &config_layer,
                )
                .await
                .unwrap();

                Ok(Container {
                    config_layer,
                    image_layers,
                })
            }
            Err(oci_distribution::errors::OciDistributionError::AuthenticationFailure(message)) => {
                panic!("Container image registry authentication failure ({}). Are docker credentials set-up for current user?", &message);
            }
            Err(e) => {
                panic!(
                    "Failed to pull container image manifest and config - error: {:#?}",
                    &e
                );
            }
        }
    }

    // Convert Docker image config to policy data.
    pub fn get_process(
        &self,
        process: &mut policy::KataProcess,
        yaml_has_command: bool,
        yaml_has_args: bool,
    ) {
        debug!("Getting process field from docker config layer...");
        let docker_config = &self.config_layer.config;

        if let Some(image_user) = &docker_config.User {
            if !image_user.is_empty() {
                debug!("Splitting Docker config user = {:?}", image_user);
                let user: Vec<&str> = image_user.split(':').collect();
                if !user.is_empty() {
                    debug!("Parsing uid from user[0] = {}", &user[0]);
                    match user[0].parse() {
                        Ok(id) => process.User.UID = id,
                        Err(e) => {
                            // "image: prom/prometheus" has user = "nobody", but
                            // process.User.UID is an u32 value.
                            warn!(
                                "Failed to parse {} as u32, using uid = 0 - error {:?}",
                                &user[0], &e
                            );
                            process.User.UID = 0;
                        }
                    }
                }
                if user.len() > 1 {
                    debug!("Parsing gid from user[1] = {:?}", user[1]);
                    process.User.GID = user[1].parse().unwrap();
                }
            }
        }

        if let Some(terminal) = docker_config.Tty {
            process.Terminal = terminal;
        } else {
            process.Terminal = false;
        }

        for env in &docker_config.Env {
            process.Env.push(env.clone());
        }

        let policy_args = &mut process.Args;
        debug!("Already existing policy args: {:?}", policy_args);

        if let Some(entry_points) = &docker_config.Entrypoint {
            debug!("Image Entrypoint: {:?}", entry_points);
            if !yaml_has_command {
                debug!("Inserting Entrypoint into policy args");

                let mut reversed_entry_points = entry_points.clone();
                reversed_entry_points.reverse();

                for entry_point in reversed_entry_points {
                    policy_args.insert(0, entry_point.clone());
                }
            } else {
                debug!("Ignoring image Entrypoint because YAML specified the container command");
            }
        } else {
            debug!("No image Entrypoint");
        }

        debug!("Updated policy args: {:?}", policy_args);

        if yaml_has_command {
            debug!("Ignoring image Cmd because YAML specified the container command");
        } else if yaml_has_args {
            debug!("Ignoring image Cmd because YAML specified the container args");
        } else if let Some(commands) = &docker_config.Cmd {
            debug!("Adding to policy args the image Cmd: {:?}", commands);

            for cmd in commands {
                policy_args.push(cmd.clone());
            }
        } else {
            debug!("Image Cmd field is not present");
        }

        debug!("Updated policy args: {:?}", policy_args);

        if let Some(working_dir) = &docker_config.WorkingDir {
            if !working_dir.is_empty() {
                process.Cwd = working_dir.clone();
            }
        }

        debug!("get_process succeeded.");
    }

    pub fn get_image_layers(&self) -> Vec<ImageLayer> {
        self.image_layers.clone()
    }
}

async fn get_image_layers(
    use_cached_files: bool,
    client: &mut Client,
    reference: &Reference,
    manifest: &manifest::OciImageManifest,
    config_layer: &DockerConfigLayer,
) -> Result<Vec<ImageLayer>> {
    let mut layer_index = 0;
    let mut layers = Vec::new();

    for layer in &manifest.layers {
        if layer
            .media_type
            .eq(manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE)
        {
            if layer_index < config_layer.rootfs.diff_ids.len() {
                layers.push(ImageLayer {
                    diff_id: config_layer.rootfs.diff_ids[layer_index].clone(),
                    verity_hash: get_verity_hash(
                        use_cached_files,
                        client,
                        reference,
                        &layer.digest,
                    )
                    .await?,
                });
            } else {
                return Err(anyhow!("Too many Docker gzip layers"));
            }

            layer_index += 1;
        }
    }

    Ok(layers)
}

fn delete_files(decompressed_path: &Path, compressed_path: &Path, verity_path: &Path) {
    let _ = fs::remove_file(&decompressed_path);
    let _ = fs::remove_file(&compressed_path);
    let _ = fs::remove_file(&verity_path);
}

async fn get_verity_hash(
    use_cached_files: bool,
    client: &mut Client,
    reference: &Reference,
    layer_digest: &str,
) -> Result<String> {
    let base_dir = std::path::Path::new("layers_cache");

    // Use file names supported by both Linux and Windows.
    let file_name = str::replace(&layer_digest, ":", "-");

    let mut decompressed_path = base_dir.join(file_name);
    decompressed_path.set_extension("tar");

    let mut compressed_path = decompressed_path.clone();
    compressed_path.set_extension("gz");

    let mut verity_path = decompressed_path.clone();
    verity_path.set_extension("verity");

    let mut verity_hash = "".to_string();
    let mut error_message = "".to_string();
    let mut error = false;

    if use_cached_files && verity_path.exists() {
        info!("Using cached file {:?}", &verity_path);
    } else if let Err(e) = create_verity_hash_file(
        use_cached_files,
        client,
        reference,
        layer_digest,
        &base_dir,
        &decompressed_path,
        &compressed_path,
        &verity_path,
    )
    .await
    {
        error = true;
        error_message = format!(
            "Failed to create verity hash for {}, error {:?}",
            layer_digest, &e
        );
    }

    if !error {
        match std::fs::read_to_string(&verity_path) {
            Err(e) => {
                error = true;
                error_message = format!("Failed to read {:?}, error {:?}", &verity_path, &e);
            }
            Ok(v) => {
                verity_hash = v;
                info!("dm-verity root hash: {}", &verity_hash);
            }
        }
    }

    if !use_cached_files {
        let _ = std::fs::remove_dir_all(&base_dir);
    } else if error {
        delete_files(&decompressed_path, &compressed_path, &verity_path);
    }

    if error {
        panic!("{}", &error_message);
    } else {
        Ok(verity_hash)
    }
}

async fn create_verity_hash_file(
    use_cached_files: bool,
    client: &mut Client,
    reference: &Reference,
    layer_digest: &str,
    base_dir: &Path,
    decompressed_path: &Path,
    compressed_path: &Path,
    verity_path: &Path,
) -> Result<()> {
    if use_cached_files && decompressed_path.exists() {
        info!("Using cached file {:?}", &decompressed_path);
    } else {
        std::fs::create_dir_all(&base_dir)?;

        create_decompressed_layer_file(
            use_cached_files,
            client,
            reference,
            layer_digest,
            &decompressed_path,
            &compressed_path,
        )
        .await?;
    }

    do_create_verity_hash_file(decompressed_path, verity_path)
}

async fn create_decompressed_layer_file(
    use_cached_files: bool,
    client: &mut Client,
    reference: &Reference,
    layer_digest: &str,
    decompressed_path: &Path,
    compressed_path: &Path,
) -> Result<()> {
    if use_cached_files && compressed_path.exists() {
        info!("Using cached file {:?}", &compressed_path);
    } else {
        info!("Pulling layer {:?}", layer_digest);
        let mut file = tokio::fs::File::create(&compressed_path)
            .await
            .map_err(|e| anyhow!(e))?;
        client
            .pull_blob(&reference, layer_digest, &mut file)
            .await
            .map_err(|e| anyhow!(e))?;
        file.flush().await.map_err(|e| anyhow!(e))?;
    }

    info!("Decompressing layer");
    let compressed_file = std::fs::File::open(&compressed_path).map_err(|e| anyhow!(e))?;
    let mut decompressed_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&decompressed_path)?;
    let mut gz_decoder = flate2::read::GzDecoder::new(compressed_file);
    std::io::copy(&mut gz_decoder, &mut decompressed_file).map_err(|e| anyhow!(e))?;

    info!("Adding tarfs index to layer");
    decompressed_file.seek(std::io::SeekFrom::Start(0))?;
    tarindex::append_index(&mut decompressed_file).map_err(|e| anyhow!(e))?;
    decompressed_file.flush().map_err(|e| anyhow!(e))?;

    Ok(())
}

fn do_create_verity_hash_file(path: &Path, verity_path: &Path) -> Result<()> {
    info!("Calculating dm-verity root hash");
    let mut file = std::fs::File::open(path)?;
    let size = file.seek(std::io::SeekFrom::End(0))?;
    if size < 4096 {
        return Err(anyhow!("Block device {:?} is too small: {}", path, size));
    }

    let salt = [0u8; <Sha256 as OutputSizeUser>::OutputSize::USIZE];
    let v = verity::Verity::<Sha256>::new(size, 4096, 4096, &salt, 0)?;
    let hash = verity::traverse_file(&mut file, 0, false, v, &mut verity::no_write)?;
    let result = format!("{:x}", hash);

    let mut verity_file = std::fs::File::create(verity_path).map_err(|e| anyhow!(e))?;
    verity_file
        .write_all(result.as_bytes())
        .map_err(|e| anyhow!(e))?;
    verity_file.flush().map_err(|e| anyhow!(e))?;

    Ok(())
}

pub async fn get_container(use_cache: bool, image: &str) -> Result<Container> {
    Container::new(use_cache, image).await
}

fn build_auth(reference: &Reference) -> RegistryAuth {
    debug!("build_auth: {:?}", reference);

    let server = reference
        .resolve_registry()
        .strip_suffix("/")
        .unwrap_or_else(|| reference.resolve_registry());

    match docker_credential::get_credential(server) {
        Ok(DockerCredential::UsernamePassword(username, password)) => {
            debug!("build_auth: Found docker credentials");
            return RegistryAuth::Basic(username, password);
        }
        Ok(DockerCredential::IdentityToken(_)) => {
            warn!("build_auth: Cannot use contents of docker config, identity token not supported. Using anonymous access.");
        }
        Err(CredentialRetrievalError::ConfigNotFound) => {
            debug!("build_auth: Docker config not found - using anonymous access.");
        }
        Err(CredentialRetrievalError::NoCredentialConfigured) => {
            debug!("build_auth: Docker credentials not configured - using anonymous access.");
        }
        Err(CredentialRetrievalError::ConfigReadError) => {
            debug!("build_auth: Cannot read docker credentials - using anonymous access.");
        }
        Err(CredentialRetrievalError::HelperFailure { stdout, stderr }) => {
            if stdout == "credentials not found in native keychain\n" {
                // On WSL, this error is generated when credentials are not
                // available in ~/.docker/config.json.
                debug!("build_auth: Docker credentials not found - using anonymous access.");
            } else {
                warn!("build_auth: Docker credentials not found - using anonymous access. stderr = {}, stdout = {}",
                    &stderr, &stdout);
            }
        }
        Err(e) => panic!("Error handling docker configuration file: {}", e),
    }

    RegistryAuth::Anonymous
}

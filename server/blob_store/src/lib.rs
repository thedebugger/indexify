use std::{env, fmt::Debug, sync::Arc};

use anyhow::{anyhow, Result};
use bytes::{Bytes, BytesMut};
use futures::{stream::BoxStream, StreamExt};
use object_store::{parse_url, path::Path, ObjectStore, WriteMultipart};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::info;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobStorageConfig {
    pub path: Option<String>,
}

impl BlobStorageConfig {
    pub fn new(path: &str) -> Self {
        BlobStorageConfig {
            path: Some(format!("file://{}", path)),
        }
    }
}

impl Default for BlobStorageConfig {
    fn default() -> Self {
        let blob_store_path = format!(
            "file://{}",
            env::current_dir()
                .unwrap()
                .join("indexify_storage/blobs")
                .to_str()
                .unwrap()
        );
        info!("using blob store path: {}", blob_store_path);
        BlobStorageConfig {
            path: Some(blob_store_path),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PutResult {
    pub url: String,
    pub size_bytes: u64,
    pub sha256_hash: String,
}

#[derive(Clone)]
pub struct BlobStorage {
    object_store: Arc<dyn ObjectStore>,
    path: Path,
}

impl BlobStorage {
    pub fn new(config: BlobStorageConfig) -> Result<Self> {
        let (object_store, path) = parse_url(&config.path.clone().unwrap().parse::<Url>()?)?;
        Ok(Self {
            object_store: Arc::new(object_store),
            path,
        })
    }

    pub async fn put(
        &self,
        key: &str,
        data: impl futures::Stream<Item = Result<Bytes>> + Send + Unpin,
    ) -> Result<PutResult, anyhow::Error> {
        let mut hasher = Sha256::new();
        let mut hashed_stream = data.map(|item| {
            item.map(|bytes| {
                hasher.update(&bytes);
                bytes
            })
        });

        let path = self.path.child(key);
        let m = self.object_store.put_multipart(&path).await?;
        let mut w = WriteMultipart::new(m);
        let mut size_bytes = 0;
        while let Some(chunk) = hashed_stream.next().await {
            w.wait_for_capacity(1).await?;
            let chunk = chunk?;
            size_bytes += chunk.len() as u64;
            w.write(&chunk);
        }
        w.finish().await?;

        let hash = format!("{:x}", hasher.finalize());
        Ok(PutResult {
            url: path.to_string(),
            size_bytes,
            sha256_hash: hash,
        })
    }

    pub async fn get(&self, path: &str) -> Result<BoxStream<'static, Result<Bytes>>> {
        let client_clone = self.object_store.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let get_result = client_clone
            .get(&path.into())
            .await
            .map_err(|e| anyhow!("can't get s3 object {:?}: {:?}", path, e))?;
        let path = path.to_string();
        tokio::spawn(async move {
            let mut stream = get_result.into_stream();
            while let Some(chunk) = stream.next().await {
                let _ =
                    tx.send(chunk.map_err(|e| {
                        anyhow!("error reading s3 object {:?}: {:?}", path.clone(), e)
                    }));
            }
        });
        Ok(Box::pin(UnboundedReceiverStream::new(rx)))
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.object_store
            .delete(&object_store::path::Path::from(key))
            .await?;
        Ok(())
    }

    pub async fn read_bytes(&self, key: &str) -> Result<Bytes> {
        let mut reader = self.get(key).await?;
        let mut bytes = BytesMut::new();
        while let Some(chunk) = reader.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        Ok(bytes.into())
    }
}

use std::collections::HashMap;
use std::path::{PathBuf};
use std::sync::Arc;

use derive_more::From;
use futures::stream::Stream;
use tokio::fs::{self, File};
use tokio::io::{self, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::{task, runtime};
use uuid::Uuid;

use mixlab_protocol::{WorkspaceState, PerformanceInfo};

use crate::engine::{self, EngineHandle, EngineEvents, EngineError, EngineSession, WorkspaceEmbryo};
use crate::persist;

#[derive(Clone)]
pub struct ProjectHandle {
    base: ProjectBaseRef,
    engine: EngineHandle,
}

struct ProjectBase {
    path: PathBuf,
    library: HashMap<Uuid, MediaInfo>,
    uploads: HashMap<Uuid, InProgressUpload>,
}

type ProjectBaseRef = Arc<Mutex<ProjectBase>>;

#[derive(From, Debug)]
pub enum OpenError {
    Io(io::Error),
    Json(serde_json::Error),
    NotDirectory,
}

impl ProjectBase {
    fn open_at(path: PathBuf) -> Self {
        ProjectBase {
            path,
            library: HashMap::new(),
            uploads: HashMap::new(),
        }
    }

    async fn read_workspace(&self) -> Result<persist::Workspace, io::Error> {
        let workspace_path = self.path.join("workspace.json");

        let workspace = match fs::read(&workspace_path).await {
            Ok(serialized) => {
                serde_json::from_slice(&serialized)?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                persist::Workspace::default()
            }
            Err(e) => {
                return Err(e)
            }
        };

        Ok(workspace)
    }

    async fn write_workspace(&mut self, workspace: &persist::Workspace) -> Result<(), io::Error> {
        let workspace_tmp_path = self.path.join(".workspace.json.tmp");
        let workspace_path = self.path.join("workspace.json");

        let serialized = serde_json::to_vec(workspace).expect("serde_json::to_vec");

        // write to temporary file and rename into place. this is atomic on unix,
        // maybe it is on windows too?
        fs::write(&workspace_tmp_path, &serialized).await?;
        fs::rename(&workspace_tmp_path, &workspace_path).await?;

        Ok(())
    }

    async fn begin_media_upload(&mut self, info: UploadInfo) -> Result<(Uuid, File), io::Error> {
        let media_path = self.path.join("media");

        match fs::create_dir(&media_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => { return Err(e); }
        }

        let id = Uuid::new_v4();
        let file = File::create(media_path.join(id.to_hyphenated_ref().to_string())).await?;

        self.uploads.insert(id, InProgressUpload {
            info,
            uploaded_bytes: 0,
        });

        Ok((id, file))
    }
}

pub async fn open_or_create(path: PathBuf) -> Result<ProjectHandle, OpenError> {
    match fs::create_dir(&path).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // TODO - this is racey! we need an atomic way of asserting that a directory exists
            match fs::metadata(&path).await {
                Ok(meta) if meta.is_dir() => {
                    // already exists!
                }
                Ok(_) => {
                    return Err(OpenError::NotDirectory);
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
        Err(e) => {
            return Err(e.into());
        }
    }

    let base = ProjectBase::open_at(path);
    let workspace = base.read_workspace().await?;

    // start engine update thread
    let (embryo, mut persist_rx) = WorkspaceEmbryo::new(workspace);
    let engine = engine::start(runtime::Handle::current(), embryo);

    let base = Arc::new(Mutex::new(base));

    task::spawn({
        let base = base.clone();
        async move {
            while let Some(workspace) = persist_rx.recv().await {
                match base.lock().await.write_workspace(&workspace).await {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("project: could not persist workspace: {:?}", e);
                    }
                }
            }
        }
    });

    Ok(ProjectHandle {
        base,
        engine,
    })
}

impl ProjectHandle {
    pub async fn connect_engine(&self) -> Result<(WorkspaceState, EngineEvents, EngineSession), EngineError> {
        self.engine.connect().await
    }

    pub fn performance_info(&self) -> impl Stream<Item = Arc<PerformanceInfo>> {
        self.engine.performance_info()
    }

    pub async fn begin_media_upload(&self, info: UploadInfo) -> Result<MediaUpload, io::Error> {
        let (id, file) = self.base.lock().await.begin_media_upload(info).await?;

        Ok(MediaUpload {
            base: self.base.clone(),
            id,
            file,
        })
    }
}

struct InProgressUpload {
    info: UploadInfo,
    uploaded_bytes: usize,
}

pub struct UploadInfo {
    pub name: String,
    pub kind: String,
    pub size: usize,
}

pub struct MediaUploadId(pub Uuid);

pub struct MediaUpload {
    base: ProjectBaseRef,
    id: Uuid,
    file: File,
}

#[derive(From)]
pub enum UploadError {
    Io(io::Error),
    Cancelled,
}

impl MediaUpload {
    pub async fn receive_bytes(&mut self, bytes: &[u8]) -> Result<(), UploadError> {
        self.file.write_all(bytes).await?;

        let mut base = self.base.lock().await;

        let mut uploaded_bytes = &mut base
            .uploads.get_mut(&self.id).ok_or(UploadError::Cancelled)?
            .uploaded_bytes;

        *uploaded_bytes += bytes.len();

        // TODO - should we error if uploaded bytes exceeds what we think the size will be?

        Ok(())
    }

    pub async fn finalize(self) -> Result<(), UploadError> {
        let mut base = self.base.lock().await;

        let upload = base.uploads.remove(&self.id)
            .ok_or(UploadError::Cancelled)?;

        base.library.insert(self.id, MediaInfo {
            name: upload.info.name,
            kind: upload.info.kind,
        });

        Ok(())
    }
}

pub struct MediaInfo {
    pub name: String,
    pub kind: String,
}

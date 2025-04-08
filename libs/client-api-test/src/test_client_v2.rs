use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Error};
use assert_json_diff::{
  assert_json_eq, assert_json_include, assert_json_matches_no_panic, CompareMode, Config,
};
use bytes::Bytes;
use collab::core::collab::DataSource;
use collab::core::collab_state::SyncState;
use collab::core::origin::{CollabClient, CollabOrigin};
use collab::entity::EncodedCollab;
use collab::lock::{Mutex, RwLock};
use collab::preclude::{Collab, Prelim};
use collab_database::database::{Database, DatabaseContext};
use collab_database::workspace_database::WorkspaceDatabase;
use collab_document::document::Document;
use collab_entity::CollabType;
use collab_folder::hierarchy_builder::NestedChildViewBuilder;
use collab_folder::{Folder, ViewLayout};
use collab_user::core::UserAwareness;
use mime::Mime;
use serde::Deserialize;
use serde_json::{json, Value};
use shared_entity::dto::publish_dto::PublishViewMetaData;
use tempfile::TempDir;
use tokio::time::{sleep, timeout, Duration};
use tokio_stream::StreamExt;
use tracing::trace;
use uuid::Uuid;

use client_api::entity::id::user_awareness_object_id;
use client_api::entity::{
  CompletionStream, CompletionStreamValue, PublishCollabItem, PublishCollabMetadata,
  QueryWorkspaceMember, QuestionStream, QuestionStreamValue, UpdateCollabWebParams,
};
use client_api::v2::{WorkspaceController, WorkspaceControllerOptions};
use database_entity::dto::{
  AFCollabEmbedInfo, AFRole, AFSnapshotMeta, AFSnapshotMetas, AFUserProfile, AFUserWorkspaceInfo,
  AFWorkspace, AFWorkspaceInvitationStatus, AFWorkspaceMember, BatchQueryCollabResult,
  CollabParams, CreateCollabParams, QueryCollab, QueryCollabParams, QuerySnapshotParams,
  SnapshotData,
};
use shared_entity::dto::ai_dto::CalculateSimilarityParams;
use shared_entity::dto::search_dto::SearchDocumentResponseItem;
use shared_entity::dto::workspace_dto::{
  BlobMetadata, CollabResponse, EmbeddedCollabQuery, PublishedDuplicate, WorkspaceMemberChangeset,
  WorkspaceMemberInvitation, WorkspaceSpaceUsage,
};
use shared_entity::response::AppResponseError;

use crate::database_util::TestDatabaseCollabService;
use crate::user::{generate_unique_registered_user, User};
use crate::{load_env, localhost_client_with_device_id, setup_log, LOCALHOST_WS_V2};

pub type CollabRef = Arc<RwLock<dyn BorrowMut<Collab> + Send + Sync + 'static>>;

pub struct TestClient {
  pub user: User,
  pub workspace: WorkspaceController,
  pub api_client: client_api::Client,
  pub collabs: HashMap<Uuid, TestCollab>,
  pub device_id: String,
  _temp_dir: TempDir,
}

impl TestClient {
  pub async fn new(registered_user: User, start_ws_conn: bool) -> Self {
    load_env();
    setup_log();
    let device_id = Uuid::new_v4().to_string();
    Self::new_with_device_id(&device_id, registered_user, start_ws_conn).await
  }

  pub async fn insert_into<S: Prelim>(&self, object_id: &Uuid, key: &str, value: S) {
    let mut lock = self.collabs.get(object_id).unwrap().collab.write().await;
    let collab = (*lock).borrow_mut();
    collab.insert(key, value);
  }

  pub async fn new_with_device_id(
    device_id: &str,
    registered_user: User,
    start_ws_conn: bool,
  ) -> Self {
    setup_log();
    let temp_dir = TempDir::new().unwrap();
    let api_client = localhost_client_with_device_id(device_id);
    api_client
      .sign_in_password(&registered_user.email, &registered_user.password)
      .await
      .unwrap();
    let uid = api_client.get_profile().await.unwrap().uid;
    let workspace_id = api_client
      .get_workspaces()
      .await
      .unwrap()
      .first()
      .unwrap()
      .workspace_id;
    let device_id = api_client.device_id.clone();

    // Connect to server via websocket
    let workspace = WorkspaceController::new(WorkspaceControllerOptions {
      url: LOCALHOST_WS_V2.to_string(),
      workspace_id,
      uid,
      workspace_db_path: temp_dir.path().to_str().unwrap().to_owned(),
      device_id: device_id.clone(),
    })
    .unwrap();
    if start_ws_conn {
      workspace.connect().await.unwrap();
    }
    Self {
      user: registered_user,
      workspace,
      api_client,
      collabs: Default::default(),
      device_id,
      _temp_dir: temp_dir,
    }
  }

  pub async fn new_user() -> Self {
    let registered_user = generate_unique_registered_user().await;
    let this = Self::new(registered_user, true).await;
    let uid = this.uid().await;
    trace!("🤖New user created: {}", uid);
    this
  }

  pub async fn new_user_without_ws_conn() -> Self {
    let registered_user = generate_unique_registered_user().await;
    Self::new(registered_user, false).await
  }

  pub fn disable_receive_message(&mut self) {
    self.workspace.disable_receive_message();
  }

  pub fn enable_receive_message(&mut self) {
    self.workspace.enable_receive_message();
  }

  pub async fn insert_view_to_general_space(
    &self,
    workspace_id: &Uuid,
    view_id: &str,
    view_name: &str,
    view_layout: ViewLayout,
  ) {
    let mut folder = self.get_folder(*workspace_id).await;
    let general_space_id = folder
      .get_view(&workspace_id.to_string())
      .unwrap()
      .children
      .first()
      .unwrap()
      .clone();
    let view = NestedChildViewBuilder::new(self.uid().await, general_space_id.id.clone())
      .with_view_id(view_id.to_string())
      .with_name(view_name)
      .with_layout(view_layout)
      .build()
      .view;
    {
      let mut txn = folder.collab.transact_mut();
      folder.body.views.insert(&mut txn, view, None);
    }
    let folder_collab_type = CollabType::Folder;
    self
      .api_client
      .update_web_collab(
        workspace_id,
        workspace_id,
        UpdateCollabWebParams {
          doc_state: folder
            .encode_collab_v1(|c| folder_collab_type.validate_require_data(c))
            .unwrap()
            .doc_state
            .to_vec(),
          collab_type: CollabType::Folder,
        },
      )
      .await
      .unwrap();
  }

  pub async fn get_folder(&self, workspace_id: Uuid) -> Folder {
    let uid = self.uid().await;
    let folder_collab = self
      .api_client
      .get_collab(QueryCollabParams::new(
        workspace_id,
        CollabType::Folder,
        workspace_id,
      ))
      .await
      .unwrap()
      .encode_collab;
    Folder::from_collab_doc_state(
      uid,
      CollabOrigin::Client(CollabClient::new(uid, self.device_id.clone())),
      folder_collab.into(),
      &workspace_id.to_string(),
      vec![],
    )
    .unwrap()
  }

  pub async fn get_database(&self, workspace_id: Uuid, database_id: &str) -> Database {
    let service = TestDatabaseCollabService {
      api_client: self.api_client.clone(),
      workspace_id,
    };
    let context = DatabaseContext::new(Arc::new(service));
    Database::open(database_id, context).await.unwrap()
  }

  pub async fn get_document(&self, workspace_id: Uuid, document_id: Uuid) -> Document {
    let collab = self
      .get_collab_to_collab(workspace_id, document_id, CollabType::Document)
      .await
      .unwrap();
    Document::open(collab).unwrap()
  }

  pub async fn get_workspace_database(&self, workspace_id: Uuid) -> WorkspaceDatabase {
    let workspaces = self.api_client.get_workspaces().await.unwrap();
    let workspace_database_id = workspaces
      .iter()
      .find(|w| w.workspace_id == workspace_id)
      .unwrap()
      .database_storage_id;

    let collab = self
      .api_client
      .get_collab(QueryCollabParams::new(
        workspace_database_id,
        CollabType::WorkspaceDatabase,
        workspace_id,
      ))
      .await
      .unwrap();

    WorkspaceDatabase::from_collab_doc_state(
      &workspace_database_id.to_string(),
      CollabOrigin::Empty,
      collab.encode_collab.into(),
    )
    .unwrap()
  }

  pub async fn get_connect_users(&self, object_id: &Uuid) -> Vec<i64> {
    #[derive(Deserialize)]
    struct UserId {
      pub uid: i64,
    }

    let lock = self.collabs.get(object_id).unwrap().collab.read().await;
    lock
      .get_awareness()
      .iter()
      .flat_map(|(_a, client)| match &client.data {
        None => None,
        Some(json) => {
          let user: UserId = serde_json::from_str(json).unwrap();
          Some(user.uid)
        },
      })
      .collect()
  }

  pub async fn clean_awareness_state(&self, object_id: &Uuid) {
    let test_collab = self.collabs.get(object_id).unwrap();
    let mut lock = test_collab.collab.write().await;
    let collab = (*lock).borrow_mut();
    collab.clean_awareness_state();
  }

  pub async fn emit_awareness_state(&self, object_id: &Uuid) {
    let test_collab = self.collabs.get(object_id).unwrap();
    let mut lock = test_collab.collab.write().await;
    let collab = (*lock).borrow_mut();
    collab.emit_awareness_state();
  }

  pub async fn user_with_new_device(registered_user: User) -> Self {
    Self::new(registered_user, true).await
  }

  pub async fn get_user_workspace_info(&self) -> AFUserWorkspaceInfo {
    self.api_client.get_user_workspace_info().await.unwrap()
  }

  pub async fn open_workspace(&self, workspace_id: &Uuid) -> AFWorkspace {
    self.api_client.open_workspace(workspace_id).await.unwrap()
  }

  pub async fn get_user_folder(&self) -> Folder {
    let uid = self.uid().await;
    let workspace_id = self.workspace_id().await;
    let data = self
      .api_client
      .get_collab(QueryCollabParams::new(
        workspace_id,
        CollabType::Folder,
        workspace_id,
      ))
      .await
      .unwrap();

    Folder::from_collab_doc_state(
      uid,
      CollabOrigin::Empty,
      data.encode_collab.into(),
      &workspace_id.to_string(),
      vec![],
    )
    .unwrap()
  }

  pub async fn get_workspace_database_collab(&self, workspace_id: Uuid) -> Collab {
    let db_storage_id = self.open_workspace(&workspace_id).await.database_storage_id;
    let collab_resp = self
      .get_collab(workspace_id, db_storage_id, CollabType::WorkspaceDatabase)
      .await
      .unwrap();
    Collab::new_with_source(
      CollabOrigin::Server,
      &db_storage_id.to_string(),
      collab_resp.encode_collab.into(),
      vec![],
      false,
    )
    .unwrap()
  }

  pub async fn create_document_collab(&self, workspace_id: Uuid, object_id: Uuid) -> Document {
    let collab_resp = self
      .get_collab(workspace_id, object_id, CollabType::Document)
      .await
      .unwrap();
    let collab = Collab::new_with_source(
      CollabOrigin::Server,
      &object_id.to_string(),
      collab_resp.encode_collab.into(),
      vec![],
      false,
    )
    .unwrap();
    Document::open(collab).unwrap()
  }

  pub async fn get_db_collab_from_view(&mut self, workspace_id: Uuid, view_id: &Uuid) -> Collab {
    let ws_db_collab = self.get_workspace_database_collab(workspace_id).await;
    let ws_db_body = WorkspaceDatabase::open(ws_db_collab).unwrap();
    let db_id = ws_db_body
      .get_all_database_meta()
      .into_iter()
      .find(|db_meta| db_meta.linked_views.contains(&view_id.to_string()))
      .unwrap()
      .database_id
      .parse::<Uuid>()
      .unwrap();
    let db_collab_collab_resp = self
      .get_collab(workspace_id, db_id, CollabType::Database)
      .await
      .unwrap();
    Collab::new_with_source(
      CollabOrigin::Server,
      &db_id.to_string(),
      db_collab_collab_resp.encode_collab.into(),
      vec![],
      false,
    )
    .unwrap()
  }

  pub async fn get_user_awareness(&self) -> UserAwareness {
    let workspace_id = self.workspace_id().await;
    let profile = self.get_user_profile().await;
    let awareness_object_id = user_awareness_object_id(&profile.uuid, &workspace_id);
    let data = self
      .api_client
      .get_collab(QueryCollabParams::new(
        awareness_object_id,
        CollabType::UserAwareness,
        workspace_id,
      ))
      .await
      .unwrap();
    let collab = Collab::new_with_source(
      CollabOrigin::Empty,
      &awareness_object_id.to_string(),
      DataSource::DocStateV1(data.encode_collab.doc_state.to_vec()),
      vec![],
      false,
    )
    .unwrap();

    UserAwareness::open(collab, None).unwrap()
  }

  pub async fn try_update_workspace_member(
    &self,
    workspace_id: &Uuid,
    other_client: &TestClient,
    role: AFRole,
  ) -> Result<(), AppResponseError> {
    let email = other_client.email().await;
    self
      .api_client
      .update_workspace_member(
        workspace_id,
        WorkspaceMemberChangeset::new(email).with_role(role),
      )
      .await
  }

  pub async fn invite_and_accepted_workspace_member(
    &self,
    workspace_id: &Uuid,
    other_client: &TestClient,
    role: AFRole,
  ) -> Result<(), AppResponseError> {
    let email = other_client.email().await;

    self
      .api_client
      .invite_workspace_members(
        workspace_id,
        vec![WorkspaceMemberInvitation {
          email,
          role,
          skip_email_send: true,
          ..Default::default()
        }],
      )
      .await?;

    let invitations = other_client
      .api_client
      .list_workspace_invitations(Some(AFWorkspaceInvitationStatus::Pending))
      .await
      .unwrap();

    let target_invitation = invitations
      .iter()
      .find(|inv| &inv.workspace_id == workspace_id)
      .unwrap();

    other_client
      .api_client
      .accept_workspace_invitation(target_invitation.invite_id.to_string().as_str())
      .await
      .unwrap();

    Ok(())
  }

  pub async fn try_remove_workspace_member(
    &self,
    workspace_id: &Uuid,
    other_client: &TestClient,
  ) -> Result<(), AppResponseError> {
    let email = other_client.email().await;
    self
      .api_client
      .remove_workspace_members(workspace_id, vec![email])
      .await
  }

  pub async fn get_workspace_members(&self, workspace_id: &Uuid) -> Vec<AFWorkspaceMember> {
    self
      .api_client
      .get_workspace_members(workspace_id)
      .await
      .unwrap()
  }

  pub async fn try_get_workspace_members(
    &self,
    workspace_id: &Uuid,
  ) -> Result<Vec<AFWorkspaceMember>, AppResponseError> {
    self.api_client.get_workspace_members(workspace_id).await
  }

  pub async fn get_workspace_member(&self, workspace_id: Uuid, user_id: i64) -> AFWorkspaceMember {
    let params = QueryWorkspaceMember {
      workspace_id,
      uid: user_id,
    };
    self.api_client.get_workspace_member(params).await.unwrap()
  }

  pub async fn try_get_workspace_member(
    &self,
    workspace_id: Uuid,
    user_id: i64,
  ) -> Result<AFWorkspaceMember, AppResponseError> {
    let params = QueryWorkspaceMember {
      workspace_id,
      uid: user_id,
    };

    self.api_client.get_workspace_member(params).await
  }

  pub async fn wait_object_sync_complete(&self, object_id: &Uuid) -> Result<(), Error> {
    self
      .wait_object_sync_complete_with_secs(object_id, 60)
      .await
  }

  pub async fn wait_object_sync_complete_with_secs(
    &self,
    object_id: &Uuid,
    secs: u64,
  ) -> Result<(), Error> {
    let mut sync_state = {
      let lock = self.collabs.get(object_id).unwrap().collab.read().await;
      lock.subscribe_sync_state()
    };

    let duration = Duration::from_secs(secs);
    while let Ok(Some(state)) = timeout(duration, sync_state.next()).await {
      if state == SyncState::SyncFinished {
        return Ok(());
      }
    }

    Err(anyhow!(
      "Timeout or SyncState stream ended before reaching SyncFinished"
    ))
  }

  #[allow(dead_code)]
  pub async fn get_blob_metadata(&self, workspace_id: &Uuid, file_id: &str) -> BlobMetadata {
    let url = self.api_client.get_blob_url(workspace_id, file_id);
    self.api_client.get_blob_metadata(&url).await.unwrap()
  }

  pub async fn upload_blob<T: Into<Bytes>>(&self, file_id: &str, data: T, mime: &Mime) {
    let workspace_id = self.workspace_id().await;
    let url = self.api_client.get_blob_url(&workspace_id, file_id);
    self.api_client.put_blob(&url, data, mime).await.unwrap()
  }

  pub async fn delete_file(&self, file_id: &str) {
    let workspace_id = self.workspace_id().await;
    let url = self.api_client.get_blob_url(&workspace_id, file_id);
    self.api_client.delete_blob(&url).await.unwrap();
  }

  pub async fn get_workspace_usage(&self) -> WorkspaceSpaceUsage {
    let workspace_id = self.workspace_id().await;
    self
      .api_client
      .get_workspace_usage(&workspace_id)
      .await
      .unwrap()
  }

  pub async fn workspace_id(&self) -> Uuid {
    self
      .api_client
      .get_workspaces()
      .await
      .unwrap()
      .first()
      .unwrap()
      .workspace_id
  }

  pub async fn email(&self) -> String {
    self.api_client.get_profile().await.unwrap().email.unwrap()
  }

  pub async fn uid(&self) -> i64 {
    self.api_client.get_profile().await.unwrap().uid
  }

  pub async fn get_user_profile(&self) -> AFUserProfile {
    self.api_client.get_profile().await.unwrap()
  }

  pub async fn wait_until_all_embedding(
    &self,
    workspace_id: &Uuid,
    query: Vec<EmbeddedCollabQuery>,
  ) -> Vec<AFCollabEmbedInfo> {
    let timeout_duration = Duration::from_secs(30);
    let poll_interval = Duration::from_millis(2000);
    let poll_fut = async {
      loop {
        match self
          .api_client
          .batch_get_collab_embed_info(workspace_id, query.clone())
          .await
        {
          Ok(items) if items.len() == query.len() => return Ok::<_, Error>(items),
          _ => tokio::time::sleep(poll_interval).await,
        }
      }
    };

    // Enforce timeout
    match timeout(timeout_duration, poll_fut).await {
      Ok(Ok(items)) => items,
      Ok(Err(e)) => panic!("Test failed: {}", e),
      Err(_) => panic!("Test failed: Timeout after 30 seconds."),
    }
  }

  pub async fn wait_until_get_embedding(&self, workspace_id: &Uuid, object_id: &Uuid) {
    let result = timeout(Duration::from_secs(30), async {
      while self
        .api_client
        .get_collab_embed_info(workspace_id, object_id)
        .await
        .is_err()
      {
        tokio::time::sleep(Duration::from_millis(2000)).await;
      }
      self
        .api_client
        .get_collab_embed_info(workspace_id, object_id)
        .await
    })
    .await;

    match result {
      Ok(Ok(_)) => {},
      Ok(Err(e)) => panic!("Test failed: API returned an error: {:?}", e),
      Err(_) => panic!("Test failed: Timeout after 30 seconds."),
    }
  }

  pub async fn wait_unit_get_search_result(
    &self,
    workspace_id: &Uuid,
    query: &str,
    limit: u32,
  ) -> Vec<SearchDocumentResponseItem> {
    timeout(Duration::from_secs(30), async {
      loop {
        let response = self
          .api_client
          .search_documents(workspace_id, query, limit, 200)
          .await
          .unwrap();

        if response.is_empty() {
          tokio::time::sleep(Duration::from_millis(1500)).await;
          continue;
        } else {
          return response;
        }
      }
    })
    .await
    .unwrap()
  }

  pub async fn assert_similarity(
    &self,
    workspace_id: &Uuid,
    input: &str,
    expected: &str,
    score: f64,
    use_embedding: bool,
  ) {
    let params = CalculateSimilarityParams {
      workspace_id: *workspace_id,
      input: input.to_string(),
      expected: expected.to_string(),
      use_embedding,
    };
    let resp = self.api_client.calculate_similarity(params).await.unwrap();
    assert!(
      resp.score > score,
      "Similarity score is too low: {}.\nexpected: {},\ninput: {},\nexpected:{}",
      resp.score,
      score,
      input,
      expected
    );
  }

  pub async fn get_snapshot(
    &self,
    workspace_id: &Uuid,
    object_id: &Uuid,
    snapshot_id: &i64,
  ) -> Result<SnapshotData, AppResponseError> {
    self
      .api_client
      .get_snapshot(
        workspace_id,
        object_id,
        QuerySnapshotParams {
          snapshot_id: *snapshot_id,
        },
      )
      .await
  }

  pub async fn create_snapshot(
    &self,
    workspace_id: &Uuid,
    object_id: &Uuid,
    collab_type: CollabType,
  ) -> Result<AFSnapshotMeta, AppResponseError> {
    self
      .api_client
      .create_snapshot(workspace_id, object_id, collab_type)
      .await
  }

  pub async fn get_snapshot_list(
    &self,
    workspace_id: &Uuid,
    object_id: &Uuid,
  ) -> Result<AFSnapshotMetas, AppResponseError> {
    self
      .api_client
      .get_snapshot_list(workspace_id, object_id)
      .await
  }

  pub async fn get_snapshot_list_until(
    &self,
    workspace_id: &Uuid,
    object_id: &Uuid,
    f: impl Fn(&AFSnapshotMetas) -> bool + Send + Sync + 'static,
    timeout_secs: u64,
  ) -> Result<AFSnapshotMetas, AppResponseError> {
    let duration = Duration::from_secs(timeout_secs);
    #[allow(clippy::blocks_in_conditions)]
    match timeout(duration, async {
      let mut snapshot_metas = self.get_snapshot_list(workspace_id, object_id).await?;
      // Loop until the condition `f` returns true or the timeout is reached
      while !f(&snapshot_metas) {
        sleep(Duration::from_secs(5)).await;
        snapshot_metas = self.get_snapshot_list(workspace_id, object_id).await?;
      }
      Ok(snapshot_metas)
    })
    .await
    {
      Ok(result) => result,
      Err(_) => panic!("Operation timed out after {} seconds", timeout_secs),
    }
  }

  pub async fn create_collab_list(
    &mut self,
    workspace_id: &Uuid,
    params: Vec<CollabParams>,
  ) -> Result<(), AppResponseError> {
    self
      .api_client
      .create_collab_list(workspace_id, params)
      .await
  }

  pub async fn get_collab(
    &self,
    workspace_id: Uuid,
    object_id: Uuid,
    collab_type: CollabType,
  ) -> Result<CollabResponse, AppResponseError> {
    self
      .api_client
      .get_collab(QueryCollabParams {
        workspace_id,
        inner: QueryCollab {
          object_id,
          collab_type,
        },
      })
      .await
  }

  pub async fn get_collab_to_collab(
    &self,
    workspace_id: Uuid,
    object_id: Uuid,
    collab_type: CollabType,
  ) -> Result<Collab, AppResponseError> {
    let resp = self
      .get_collab(workspace_id, object_id, collab_type)
      .await?;
    let collab = Collab::new_with_source(
      CollabOrigin::Server,
      &object_id.to_string(),
      resp.encode_collab.into(),
      vec![],
      false,
    )
    .unwrap();
    Ok(collab)
  }

  pub async fn batch_get_collab(
    &mut self,
    workspace_id: &Uuid,
    params: Vec<QueryCollab>,
  ) -> Result<BatchQueryCollabResult, AppResponseError> {
    self.api_client.batch_get_collab(workspace_id, params).await
  }

  #[allow(clippy::await_holding_lock)]
  pub async fn create_and_edit_collab(
    &mut self,
    workspace_id: Uuid,
    collab_type: CollabType,
  ) -> Uuid {
    let object_id = Uuid::new_v4();
    self
      .create_and_edit_collab_with_data(object_id, workspace_id, collab_type, None)
      .await;
    object_id
  }

  #[allow(unused_variables)]
  pub async fn create_and_edit_collab_with_data(
    &mut self,
    object_id: Uuid,
    workspace_id: Uuid,
    collab_type: CollabType,
    encoded_collab_v1: Option<EncodedCollab>,
  ) {
    // Subscribe to object
    let origin = CollabOrigin::Client(CollabClient::new(self.uid().await, self.device_id.clone()));
    let mut collab = match encoded_collab_v1 {
      None => Collab::new_with_origin(origin.clone(), object_id.to_string(), vec![], false),
      Some(data) => Collab::new_with_source(
        origin.clone(),
        &object_id.to_string(),
        DataSource::DocStateV1(data.doc_state.to_vec()),
        vec![],
        false,
      )
      .unwrap(),
    };

    collab.emit_awareness_state();
    let encoded_collab_v1 = collab
      .encode_collab_v1(|collab| collab_type.validate_require_data(collab))
      .unwrap()
      .encode_to_bytes()
      .unwrap();

    self
      .api_client
      .create_collab(CreateCollabParams {
        object_id,
        encoded_collab_v1,
        collab_type,
        workspace_id,
      })
      .await
      .unwrap();

    let collab = Arc::new(RwLock::from(collab));
    let collab_ref = collab.clone() as CollabRef;
    #[cfg(feature = "collab-sync")]
    {
      self.workspace.bind(&collab_ref, collab_type).await.unwrap();
    }
    {
      let mut lock = collab.write().await;
      let collab = (*lock).borrow_mut();
      collab.initialize();
    }
    let test_collab = TestCollab { origin, collab };
    self.collabs.insert(object_id, test_collab);
    self.wait_object_sync_complete(&object_id).await.unwrap();
  }

  pub async fn open_workspace_collab(&mut self, workspace_id: Uuid) {
    self
      .open_collab(workspace_id, workspace_id, CollabType::Unknown)
      .await;
  }

  #[allow(clippy::await_holding_lock)]
  pub async fn open_collab(
    &mut self,
    workspace_id: Uuid,
    object_id: Uuid,
    collab_type: CollabType,
  ) {
    self
      .open_collab_with_doc_state(workspace_id, object_id, collab_type, vec![])
      .await
  }

  #[allow(unused_variables)]
  pub async fn open_collab_with_doc_state(
    &mut self,
    workspace_id: Uuid,
    object_id: Uuid,
    collab_type: CollabType,
    doc_state: Vec<u8>,
  ) {
    // Subscribe to object
    let origin = CollabOrigin::Client(CollabClient::new(self.uid().await, self.device_id.clone()));
    let mut collab = Collab::new_with_source(
      origin.clone(),
      &object_id.to_string(),
      DataSource::DocStateV1(doc_state),
      vec![],
      false,
    )
    .unwrap();
    collab.emit_awareness_state();
    let collab = Arc::new(RwLock::from(collab));
    let collab_ref = collab.clone() as CollabRef;

    #[cfg(feature = "collab-sync")]
    {
      self.workspace.bind(&collab_ref, collab_type).await.unwrap();
    }
    {
      let mut lock = collab.write().await;
      let collab = (*lock).borrow_mut();
      collab.initialize();
    }
    let test_collab = TestCollab { origin, collab };
    self.collabs.insert(object_id, test_collab);
  }

  #[allow(unused_variables)]
  pub async fn create_collab_with_data(
    &mut self,
    workspace_id: Uuid,
    object_id: Uuid,
    collab_type: CollabType,
    encoded_collab_v1: EncodedCollab,
  ) -> Result<(), AppResponseError> {
    // Subscribe to object
    let origin = CollabOrigin::Client(CollabClient::new(self.uid().await, self.device_id.clone()));
    let collab = Collab::new_with_source(
      origin.clone(),
      &object_id.to_string(),
      DataSource::DocStateV1(encoded_collab_v1.doc_state.to_vec()),
      vec![],
      false,
    )
    .unwrap();

    let encoded_collab_v1 = collab
      .encode_collab_v1(|collab| collab_type.validate_require_data(collab))
      .unwrap()
      .encode_to_bytes()
      .unwrap();

    self
      .api_client
      .create_collab(CreateCollabParams {
        object_id,
        encoded_collab_v1,
        collab_type,
        workspace_id,
      })
      .await
  }

  #[cfg(not(target_arch = "wasm32"))]
  pub async fn post_realtime_binary(&self, message: Vec<u8>) -> Result<(), AppResponseError> {
    let message = client_websocket::Message::binary(message);
    self
      .api_client
      .post_realtime_msg(&self.device_id, message)
      .await
  }

  pub async fn disconnect(&self) {
    self.workspace.disconnect().await.unwrap();
  }

  pub async fn reconnect(&self) {
    self.workspace.connect().await.unwrap();
  }

  pub async fn get_edit_collab_json(&self, object_id: &Uuid) -> Value {
    let lock = self.collabs.get(object_id).unwrap().collab.read().await;
    lock.to_json_value()
  }

  /// data: [(view_id, meta_json, blob_hex)]
  pub async fn publish_collabs(
    &self,
    workspace_id: &Uuid,
    data: Vec<(Uuid, &str, &str)>,
    comments_enabled: bool,
    duplicate_enabled: bool,
  ) {
    let pub_items = data
      .into_iter()
      .map(|(view_id, meta_json, blob_hex)| {
        let meta: PublishViewMetaData = serde_json::from_str(meta_json).unwrap();
        let blob = hex::decode(blob_hex).unwrap();
        PublishCollabItem {
          meta: PublishCollabMetadata {
            view_id,
            publish_name: uuid::Uuid::new_v4().to_string(),
            metadata: meta,
          },
          data: blob,
          comments_enabled,
          duplicate_enabled,
        }
      })
      .collect();

    self
      .api_client
      .publish_collabs(workspace_id, pub_items)
      .await
      .unwrap();
  }

  pub async fn duplicate_published_to_workspace(
    &self,
    dest_workspace_id: Uuid,
    src_view_id: Uuid,
    dest_view_id: Uuid,
  ) {
    self
      .api_client
      .duplicate_published_to_workspace(
        dest_workspace_id,
        &PublishedDuplicate {
          published_view_id: src_view_id,
          dest_view_id,
        },
      )
      .await
      .unwrap();

    // wait a while for folder collab to be synced
    tokio::time::sleep(Duration::from_secs(1)).await;
  }
}

pub struct TestCollab {
  #[allow(dead_code)]
  pub origin: CollabOrigin,
  pub collab: Arc<RwLock<Collab>>,
}

impl TestCollab {
  pub async fn encode_collab(&self) -> EncodedCollab {
    let lock = self.collab.read().await;
    lock
      .encode_collab_v1(|_| Ok::<(), anyhow::Error>(()))
      .unwrap()
  }
}

pub async fn assert_server_snapshot(
  client: &client_api::Client,
  workspace_id: &Uuid,
  object_id: &Uuid,
  snapshot_id: &i64,
  expected: Value,
) {
  let mut retry_count = 0;
  loop {
    tokio::select! {
       _ = tokio::time::sleep(Duration::from_secs(10)) => {
         panic!("Query snapshot timeout");
       },
       result = client.get_snapshot(workspace_id, object_id, QuerySnapshotParams {snapshot_id: *snapshot_id },
        ) => {
        retry_count += 1;
        match &result {
          Ok(snapshot_data) => {
          let encoded_collab_v1 =
            EncodedCollab::decode_from_bytes(&snapshot_data.encoded_collab_v1).unwrap();
          let json = Collab::new_with_source(
            CollabOrigin::Empty,
            &object_id.to_string(),
            DataSource::DocStateV1(encoded_collab_v1.doc_state.to_vec()),
            vec![],
            false,
          )
          .unwrap()
          .to_json_value();
            if retry_count > 10 {
              assert_json_eq!(json, expected);
              break;
            }

            if assert_json_matches_no_panic(&json, &expected, Config::new(CompareMode::Inclusive)).is_ok() {
              break;
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
          },
          Err(e) => {
            if retry_count > 10 {
              panic!("Query snapshot failed: {}", e);
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
          }
        }
       },
    }
  }
}

pub async fn assert_server_collab(
  workspace_id: Uuid,
  client: &mut client_api::Client,
  object_id: Uuid,
  collab_type: &CollabType,
  timeout_secs: u64,
  expected: Value,
) -> Result<(), Error> {
  let duration = Duration::from_secs(timeout_secs);
  let collab_type = *collab_type;
  let final_json = Arc::new(Mutex::from(json!({})));

  // Use tokio::time::timeout to apply a timeout to the entire operation
  let cloned_final_json = final_json.clone();
  let operation = async {
    loop {
      let result = client
        .get_collab(QueryCollabParams::new(object_id, collab_type, workspace_id))
        .await;

      match &result {
        Ok(data) => {
          let json = Collab::new_with_source(
            CollabOrigin::Empty,
            &object_id.to_string(),
            DataSource::DocStateV1(data.encode_collab.doc_state.to_vec()),
            vec![],
            false,
          )
          .unwrap()
          .to_json_value();

          *cloned_final_json.lock().await = json.clone();
          if assert_json_matches_no_panic(&json, &expected, Config::new(CompareMode::Inclusive))
            .is_ok()
          {
            return;
          }
        },
        Err(e) => {
          // Instead of panicking immediately, log or handle the error and continue the loop
          // until the timeout is reached.
          eprintln!("Query collab failed: {}", e);
        },
      }

      // Sleep before retrying. Adjust the sleep duration as needed.
      tokio::time::sleep(Duration::from_millis(1000)).await;
    }
  };

  if timeout(duration, operation).await.is_err() {
    eprintln!("json:{}\nexpected:{}", final_json.lock().await, expected);
    return Err(anyhow!("time out for the action"));
  }
  Ok(())
}

pub async fn assert_client_collab_within_secs(
  client: &mut TestClient,
  object_id: &Uuid,
  key: &str,
  expected: Value,
  secs: u64,
) {
  let mut retry_count = 0;
  loop {
    tokio::select! {
       _ = tokio::time::sleep(Duration::from_secs(secs)) => {
         panic!("timeout");
       },
       json = async {
        let lock = client
          .collabs
          .get_mut(object_id)
          .unwrap()
          .collab
          .read()
          .await;
        lock.to_json_value()
      } => {
        retry_count += 1;
        if retry_count > 60 {
            assert_eq!(json[key], expected[key], "object_id: {}", object_id);
            break;
          }
        if json[key] == expected[key] {
          break;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
      }
    }
  }
}

pub async fn assert_client_collab_include_value(
  client: &mut TestClient,
  object_id: &Uuid,
  expected: Value,
) -> Result<(), Error> {
  let secs = 60;
  let mut retry_count = 0;
  loop {
    tokio::select! {
       _ = tokio::time::sleep(Duration::from_secs(secs)) => {
        return Err(anyhow!("timeout"));
       },
       json = async {
        let lock = client
          .collabs
          .get_mut(object_id)
          .unwrap()
          .collab
          .read()
          .await;
        lock.to_json_value()
      } => {
        retry_count += 1;
        if retry_count > 30 {
          assert_json_include!(actual: json, expected: expected);
          return Ok(());
          }
        if assert_json_matches_no_panic(&json, &expected, Config::new(CompareMode::Inclusive)).is_ok() {
          return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
      }
    }
  }
}

pub async fn collect_answer(mut stream: QuestionStream) -> String {
  let mut answer = String::new();
  while let Some(value) = stream.next().await {
    match value.unwrap() {
      QuestionStreamValue::Answer { value } => {
        answer.push_str(&value);
      },
      QuestionStreamValue::Metadata { .. } => {},
      QuestionStreamValue::KeepAlive => {},
    }
  }
  answer
}

pub async fn collect_completion_v2(mut stream: CompletionStream) -> (String, String) {
  let mut answer = String::new();
  let mut comment = String::new();
  while let Some(value) = stream.next().await {
    match value.unwrap() {
      CompletionStreamValue::Answer { value } => {
        answer.push_str(&value);
      },
      CompletionStreamValue::Comment { value } => {
        comment.push_str(&value);
      },
    }
  }
  (answer, comment)
}

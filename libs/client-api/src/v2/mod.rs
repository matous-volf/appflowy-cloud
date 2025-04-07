mod controller;
mod db;

pub type WorkspaceController = controller::WorkspaceController;
pub type WorkspaceControllerOptions = controller::Options;

pub type WorkspaceId = uuid::Uuid;
pub type ObjectId = uuid::Uuid;

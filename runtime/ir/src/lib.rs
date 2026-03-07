pub mod error;
pub mod validate;
pub mod workflow;

pub use error::IrError;
pub use validate::validate_workflow;
pub use workflow::{EdgeDef, NodeDef, WorkflowIr};
